use super::{Marker, check_deadline, pause};
use crate::runtime_leases::open_directory;
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use horizon_std::time::Instant;
use std::{
    io,
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
};
pub type Stream = UnixStream;

pub struct Endpoint {
    parent: Dir,
    path: PathBuf,
    listener: Option<UnixListener>,
}
impl Marker {
    /// Fresh short-lived control address, independent of installed data paths.
    /// The endpoint creates its own private directory; no secret is encoded in its name.
    pub fn runtime() -> io::Result<Self> {
        Self::runtime_at(
            horizon_std::env::var_os("XDG_RUNTIME_DIR")
                .as_deref()
                .map(std::path::Path::new),
        )
    }

    fn runtime_at(runtime: Option<&std::path::Path>) -> io::Result<Self> {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|_| io::ErrorKind::Other)?;
        let name = format!("hz-{:032x}", u128::from_ne_bytes(nonce));
        let suitable = |path: &std::path::Path| -> bool {
            socket2::SockAddr::unix(path.join(&name).join("control")).is_ok()
                && open_directory(path).is_ok_and(|dir| check_directory(&dir, path, true).is_ok())
                && path.ancestors().skip(1).all(|ancestor| {
                    open_directory(ancestor)
                        .is_ok_and(|dir| check_directory(&dir, ancestor, false).is_ok())
                })
        };
        if let Some(runtime) = runtime.filter(|path| suitable(path)) {
            return Self::new(runtime.join(name));
        }
        // Deliberately not temp_dir(): TMPDIR may be arbitrarily long or unsafe.
        // Endpoint::bind checks /tmp and every ancestor and creates a 0700 leaf
        // with exclusive mkdir, rejecting collisions rather than reusing anything.
        #[cfg(target_os = "macos")]
        let fallback = "/private/tmp"; // /tmp is a symlink on macOS.
        #[cfg(not(target_os = "macos"))]
        let fallback = "/tmp";
        Self::new(std::path::Path::new(fallback).join(name))
    }
}

impl Endpoint {
    pub fn bind(marker: &Marker) -> io::Result<Self> {
        let parent_path = marker
            .directory
            .parent()
            .ok_or(io::ErrorKind::InvalidInput)?;
        let parent = open_directory(parent_path).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "open control socket parent {}: {error:?}",
                    parent_path.display()
                ),
            )
        })?;
        check_directory(&parent, parent_path, false)?;
        for ancestor in marker.directory.ancestors().skip(2) {
            let directory = open_directory(ancestor).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "open control socket ancestor {}: {error:?}",
                        ancestor.display()
                    ),
                )
            })?;
            check_directory(&directory, ancestor, false)?;
        }
        let name = marker
            .directory
            .file_name()
            .ok_or(io::ErrorKind::InvalidInput)?;
        rustix::fs::mkdirat(&parent, name, rustix::fs::Mode::RWXU).map_err(|error| {
            let error = io::Error::from(error);
            io::Error::new(
                error.kind(),
                format!(
                    "create control socket directory {}: {error}",
                    marker.directory.display()
                ),
            )
        })?;
        let mut endpoint = Self {
            parent,
            path: marker.directory.clone(),
            listener: None,
        };
        let directory = endpoint.parent.open_dir_nofollow(name)?;
        check_directory(&directory, &endpoint.path, true)?;
        let socket_path = endpoint.path.join("control");
        let listener = UnixListener::bind(&socket_path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("bind control socket {}: {error}", socket_path.display()),
            )
        })?;
        listener.set_nonblocking(true)?;
        endpoint.listener = Some(listener);
        Ok(endpoint)
    }
    /// Consumes the sole listener: no second client can ever be accepted.
    pub fn accept(&mut self, deadline: Instant) -> io::Result<Stream> {
        let listener = self.listener.take().ok_or(io::ErrorKind::NotConnected)?;
        loop {
            check_deadline(deadline)?;
            match listener.accept() {
                Ok((stream, _)) => {
                    peer(&stream)?;
                    // A concurrent fork can keep the listening descriptor open
                    // until exec. Remove its address before returning so even
                    // that inherited listener cannot admit another connection.
                    let directory = self.parent.open_dir_nofollow(
                        self.path
                            .file_name()
                            .expect("bound endpoint has a directory name"),
                    )?;
                    directory.remove_file("control")?;
                    return Ok(stream);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => pause(deadline)?,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }
}
pub fn connect(marker: &Marker, deadline: Instant) -> io::Result<Stream> {
    loop {
        check_deadline(deadline)?;
        let directory = match open_directory(&marker.directory) {
            Ok(directory) => directory,
            Err(crate::runtime_leases::Error::Io(error))
                if error.kind() == io::ErrorKind::NotFound =>
            {
                pause(deadline)?;
                continue;
            }
            Err(_) => return Err(io::ErrorKind::PermissionDenied.into()),
        };
        check_directory(&directory, &marker.directory, true)?;
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
        let address = socket2::SockAddr::unix(marker.directory.join("control"))?;
        // AF_UNIX backlog exhaustion can report EAGAIN without a pending connection.
        let connected = socket
            .connect_timeout(&address, deadline.saturating_duration_since(Instant::now()))
            .and_then(|()| socket.peer_addr());
        match connected {
            Ok(_) => {
                let stream = Stream::from(std::os::fd::OwnedFd::from(socket));
                peer(&stream)?;
                return Ok(stream);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound
                        | io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::NotConnected
                ) =>
            {
                pause(deadline)?
            }
            Err(error) => return Err(error),
        }
    }
}
/// Whether `owner` may own a control socket directory.
///
/// The current user (`euid`) may own any control socket directory. A parent or
/// ancestor directory (`private` false) may also be owned by root (uid = 0), or by
/// `root_as`: a different uid that root shows up as (typically `None` on a normal system,
/// or `Some(65534)` in a sandbox where root is unmapped).
fn trusted_owner(owner: u32, euid: u32, private: bool, root_as: Option<u32>) -> bool {
    owner == euid || (!private && (owner == 0 || Some(owner) == root_as))
}
/// Whether the text of a `uid_map` leaves root unmapped. Each line is
/// `inside outside count`; root is mapped when a line starts at inside uid 0.
/// Text that doesn't parse counts as mapped, keeping it strict.
#[cfg(target_os = "linux")]
fn leaves_root_unmapped(uid_map: &str) -> bool {
    let mut lines = uid_map.lines().peekable();
    lines.peek().is_some()
        && lines.all(|line| {
            let fields: Option<Vec<u32>> = line
                .split_whitespace()
                .map(|field| field.parse().ok())
                .collect();
            matches!(fields.as_deref(), Some(&[inside, _, count]) if inside != 0 || count == 0)
        })
}
/// The uid root shows up as in this process's user namespace when that namespace
/// leaves root unmapped: the kernel's overflow uid, typically 65534. `None` when
/// root is mapped or either file can't be read.
#[cfg(target_os = "linux")]
fn root_as() -> Option<u32> {
    let uid_map = horizon_std::fs::read_to_string("/proc/self/uid_map").ok()?;
    if !leaves_root_unmapped(&uid_map) {
        return None;
    }
    horizon_std::fs::read_to_string("/proc/sys/kernel/overflowuid")
        .ok()?
        .trim()
        .parse()
        .ok()
}
#[cfg(not(target_os = "linux"))]
fn root_as() -> Option<u32> {
    None
}
fn check_directory(directory: &Dir, path: &Path, private: bool) -> io::Result<()> {
    let metadata = rustix::fs::fstat(directory)?;
    let mode = metadata.st_mode & 0o7777;
    let reason = if !trusted_owner(
        metadata.st_uid,
        unsafe { libc::geteuid() },
        private,
        root_as(),
    ) {
        "unexpected owner"
    } else if mode & 0o022 != 0 && (private || mode & 0o1000 == 0) {
        "group or other users can write to it"
    } else if private && mode & 0o777 != 0o700 {
        "expected a private directory (mode 0700)"
    } else {
        return Ok(());
    };
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "untrusted control socket directory {} (mode {mode:04o}): {reason}",
            path.display()
        ),
    ))
}

fn peer(stream: &Stream) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    let uid = rustix::net::sockopt::socket_peercred(stream)?.uid.as_raw();
    #[cfg(target_os = "macos")]
    let uid = {
        use std::os::fd::AsRawFd;
        let (mut uid, mut gid) = (0, 0);
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        uid
    };
    #[cfg(test)]
    let uid = TEST_PEER_UID.with(|source| source.get().unwrap_or(uid));
    check_peer_uid(uid)
}
fn check_peer_uid(uid: u32) -> io::Result<()> {
    if uid != unsafe { libc::geteuid() } {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(())
}
#[cfg(test)]
std::thread_local! {
    // Per-thread override keeps parallel real-credential tests isolated.
    static TEST_PEER_UID: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}
#[test]
fn foreign_peer_uid_is_refused_on_connect_and_accept() {
    for server in [false, true] {
        let (_root, marker) = super::tests::marker();
        let mut endpoint = Endpoint::bind(&marker).unwrap();
        let deadline = Instant::now() + horizon_std::time::Duration::from_secs(3);
        let _client = server.then(|| connect(&marker, deadline).unwrap());
        TEST_PEER_UID.set(Some(unsafe { libc::geteuid() } ^ 1));
        let result = if server {
            endpoint.accept(deadline)
        } else {
            connect(&marker, deadline)
        };
        TEST_PEER_UID.set(None);
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::PermissionDenied));
    }
}
#[test]
fn consumed_listener_refuses_connections_even_while_an_inherited_copy_is_open() {
    let (_root, marker) = super::tests::marker();
    let mut endpoint = Endpoint::bind(&marker).unwrap();
    // A concurrent fork retains this reference until exec, even with CLOEXEC.
    // Duplicate it explicitly so the regression never depends on scheduling.
    let inherited = endpoint.listener.as_ref().unwrap().try_clone().unwrap();
    let deadline = Instant::now() + horizon_std::time::Duration::from_secs(3);
    let _client = connect(&marker, deadline).unwrap();
    let _accepted = endpoint.accept(deadline).unwrap();
    assert!(
        UnixStream::connect(marker.directory.join("control")).is_err(),
        "a consumed endpoint still accepts connections through an inherited listener"
    );
    drop(inherited);
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.listener.take();
        if let Some(name) = self.path.file_name() {
            if let Ok(directory) = self.parent.open_dir_nofollow(name) {
                let _ = directory.remove_file("control");
            }
            let _ = self.parent.remove_dir(name);
        }
    }
}

#[cfg(test)]
#[test]
fn runtime_addresses_validate_candidates_and_exchange() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    #[cfg(target_os = "macos")]
    let fallback = "/private/tmp";
    #[cfg(not(target_os = "macos"))]
    let fallback = "/tmp";
    let root = tempfile::tempdir_in(fallback).unwrap();
    // Owner-only whatever the umask: a candidate whose parent others can write to is refused.
    horizon_std::fs::set_permissions(root.path(), horizon_std::fs::Permissions::from_mode(0o700))
        .unwrap();
    let safe = root.path().canonicalize().unwrap().join("runtime");
    horizon_std::fs::create_dir(&safe).unwrap();
    horizon_std::fs::set_permissions(&safe, horizon_std::fs::Permissions::from_mode(0o700))
        .unwrap();
    let first = Marker::runtime_at(Some(&safe)).unwrap();
    let second = Marker::runtime_at(Some(&safe)).unwrap();
    assert_ne!(first.directory, second.directory);
    assert_eq!(first.directory.parent(), Some(safe.as_path()));
    let second_endpoint = Endpoint::bind(&second).unwrap();
    let mut endpoint = Endpoint::bind(&first).unwrap();
    let deadline = Instant::now() + horizon_std::time::Duration::from_secs(3);
    let client = connect(&first, deadline).unwrap();
    let server = endpoint.accept(deadline).unwrap();
    let client = super::Channel::authenticate(client, &first, false, deadline).unwrap();
    let server = super::Channel::authenticate(server, &first, true, deadline).unwrap();
    client.send(&42u32).unwrap();
    assert_eq!(server.receive::<u32>().unwrap().1, 42);
    let link = root.path().join("link");
    symlink(&safe, &link).unwrap();
    let long = safe.join("long-home-".repeat(18));
    horizon_std::fs::create_dir(&long).unwrap();
    horizon_std::fs::set_permissions(&long, horizon_std::fs::Permissions::from_mode(0o700))
        .unwrap();
    assert!(socket2::SockAddr::unix(long.join("control")).is_err());
    let old_marker = Marker::new(long.join("holder-control-123-456")).unwrap();
    assert!(Endpoint::bind(&old_marker).is_err());
    assert!(!old_marker.directory.exists());
    drop(second_endpoint);
    horizon_std::fs::set_permissions(&safe, horizon_std::fs::Permissions::from_mode(0o777))
        .unwrap();
    for candidate in [
        Some(safe.as_path()),
        Some(link.as_path()),
        Some(long.as_path()),
        None,
    ] {
        let marker = Marker::runtime_at(candidate).unwrap();
        assert_ne!(marker.directory.parent(), candidate);
        let endpoint = Endpoint::bind(&marker).unwrap();
        drop(endpoint);
        assert!(!marker.directory.exists());
    }
}
#[cfg(target_os = "linux")]
#[test]
fn overflow_uid_owns_only_parents_and_only_when_root_is_unmapped() {
    let (me, nobody) = (1000, 65534);
    // A sandbox's map (only the user) leaves root unmapped; a host's maps it.
    assert!(leaves_root_unmapped("      1000       1000          1\n"));
    assert!(!leaves_root_unmapped("         0          0 4294967295\n"));
    assert!(!leaves_root_unmapped("0 100000 65536\n1000 1000 1\n"));
    assert!(!leaves_root_unmapped("1000 1000 1\n0 100000 65536\n"));
    // Text that doesn't parse, or no lines at all, keeps the strict rule.
    assert!(!leaves_root_unmapped(""));
    assert!(!leaves_root_unmapped("1000 1000\n"));

    let sandboxed = Some(nobody);
    // Root unmapped: the overflow uid may own a parent, never a private directory.
    assert!(trusted_owner(nobody, me, false, sandboxed));
    assert!(!trusted_owner(nobody, me, true, sandboxed));
    // Root mapped: the overflow uid is a real user and owns nothing.
    assert!(!trusted_owner(nobody, me, false, None));
    // What held before still holds.
    assert!(trusted_owner(me, me, true, sandboxed));
    assert!(trusted_owner(0, me, false, None));
    assert!(!trusted_owner(0, me, true, None));
    assert!(!trusted_owner(1001, me, false, sandboxed));
    assert!(trusted_owner(0, me, false, sandboxed));
}
