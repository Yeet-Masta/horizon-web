//! An opt-in preflight of a real NeoForge server's FancyModLoader (FML).
//!
//! The native interpreter ([`super::run_java_mods`]) stays the path every
//! startup takes. When `HORIZON_NEOFORGE_SERVER` (an installed NeoForge server)
//! and `HORIZON_JVM_BIN` (the `horizon-jvm` executable) are both set and the
//! role is a server, this runs
//!
//! ```text
//! horizon-jvm --real-loader --entry-kind neoforge --libs <server>/libraries <jar>...
//! ```
//!
//! in a child process, bounded in time and output, and reports what FML saw:
//! `mods=N`, each `mod=...` id, and its completion line. A client never runs
//! it; a missing binary or server directory is a report line rather than a
//! startup failure, and only `HORIZON_NEOFORGE_STRICT=1` turns a failure into
//! the startup error.
use super::super::execution_role::ExecutionRole;
use horizon_platform::sync::RwLock;
use horizon_platform::time::{Duration, Instant};
use std::{
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
    sync::OnceLock,
};

/// An installed NeoForge server: its `libraries` folder is passed as `--libs`.
pub(super) const SERVER_ENV: &str = "HORIZON_NEOFORGE_SERVER";
/// The `horizon-jvm` executable that runs the real loader.
pub(super) const JVM_ENV: &str = "HORIZON_JVM_BIN";
/// `=1` makes a failed preflight stop startup; anything else is log-only.
pub(super) const STRICT_ENV: &str = "HORIZON_NEOFORGE_STRICT";

/// The line `horizon-jvm --real-loader` prints once FML has initialized.
const COMPLETION: &str = "real loader: FML probe completed";
/// Output kept per stream; the rest is drained so the child never blocks.
const MAX_CAPTURE_BYTES: usize = 32 * 1024;
/// Mod ids kept in a report after sorting and deduplicating.
const MAX_MOD_IDS: usize = 64;
/// The most characters of a child's line a failure reason repeats.
const MAX_REASON_CHARS: usize = 200;
/// A real-loader startup is slower than a native run; this bounds it.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(120);
/// How often the child is asked whether it exited.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// The role as its log lines and `server-status` name it.
pub(in crate::game) fn role_name(role: ExecutionRole) -> &'static str {
    match role {
        ExecutionRole::Client => "client",
        ExecutionRole::IntegratedServer => "integrated-server",
        ExecutionRole::DedicatedServer => "dedicated-server",
    }
}

/// What the environment asked for, once both paths are known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FmlConfig {
    role: ExecutionRole,
    server: PathBuf,
    jvm: PathBuf,
    strict: bool,
    jars: Vec<PathBuf>,
}

impl FmlConfig {
    /// The environment as [`horizon_platform::env`] has it.
    fn from_env(role: ExecutionRole, jars: &[PathBuf]) -> Option<Self> {
        Self::from_lookup(role, jars, |name| horizon_platform::env::var(name).ok())
    }

    /// The gating, separated from the environment so a test can say what is
    /// set: a client never runs, and both paths have to be non-blank.
    fn from_lookup(
        role: ExecutionRole,
        jars: &[PathBuf],
        get: impl Fn(&str) -> Option<String>,
    ) -> Option<Self> {
        if role.is_client() {
            return None;
        }
        let server = non_blank(get(SERVER_ENV))?;
        let jvm = non_blank(get(JVM_ENV))?;
        let strict = get(STRICT_ENV).is_some_and(|value| env_flag(&value));
        Some(Self {
            role,
            server: PathBuf::from(server),
            jvm: PathBuf::from(jvm),
            strict,
            jars: jars.to_vec(),
        })
    }

    fn libraries(&self) -> PathBuf {
        self.server.join("libraries")
    }

    /// The arguments after the executable: the real loader, the NeoForge entry
    /// kind, the installed server's libraries, then the jars to probe.
    fn command_args(&self) -> Vec<String> {
        let mut args = vec![
            "--real-loader".to_owned(),
            "--entry-kind".to_owned(),
            "neoforge".to_owned(),
            "--libs".to_owned(),
            self.libraries().display().to_string(),
        ];
        args.extend(self.jars.iter().map(|jar| jar.display().to_string()));
        args
    }
}

/// A blank variable is not a path: it reads as unset rather than as "".
fn non_blank(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The values `HORIZON_NEOFORGE_STRICT` accepts as on.
fn env_flag(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// What the child's output said, parsed apart from the process that wrote it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct FmlProbeOutput {
    /// The `mods=N` line, the first one printed.
    pub(super) mods: Option<usize>,
    /// `mod=<id>` lines, in output order, bounded, then sorted and deduplicated.
    pub(super) ids: Vec<String>,
    /// Whether the completion line was seen.
    pub(super) completed: bool,
}

impl FmlProbeOutput {
    /// Reads `mods=N`, `mod=<id>` and the completion line out of the child's
    /// stdout and stderr. A line counts wherever it landed: a loader that
    /// logs to either stream still reports the same probe.
    fn parse(stdout: &str, stderr: &str) -> Self {
        let mut output = Self::default();
        for line in stdout.lines().chain(stderr.lines()) {
            let line = line.trim();
            if let Some(count) = line.strip_prefix("mods=") {
                if output.mods.is_none() {
                    output.mods = count.trim().parse().ok();
                }
            } else if let Some(id) = line.strip_prefix("mod=") {
                // `horizon-jvm` appends the mod's version after a tab; the id
                // is the part before it, as FML's own listing splits it.
                let id = id.split(['\t', ' ']).next().unwrap_or_default().trim();
                if !id.is_empty() && output.ids.len() < MAX_MOD_IDS {
                    output.ids.push(id.to_owned());
                }
            } else if line.contains(COMPLETION) {
                output.completed = true;
            }
        }
        output.ids.sort();
        output.ids.dedup();
        output
    }
}

/// One role's preflight, as its log lines and `server-status` report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FmlReport {
    pub(in crate::game) role: ExecutionRole,
    /// How many explicit jars were offered to the probe.
    pub(in crate::game) jars: usize,
    /// FML's own count; absent when the probe did not print one.
    pub(in crate::game) mods: Option<usize>,
    /// The mod ids FML listed, sorted and bounded.
    pub(in crate::game) mod_ids: Vec<String>,
    /// Why the probe failed; `None` when it completed.
    pub(in crate::game) failure: Option<String>,
}

impl FmlReport {
    fn failed(role: ExecutionRole, jars: usize, why: impl Into<String>) -> Self {
        Self {
            role,
            jars,
            mods: None,
            mod_ids: Vec::new(),
            failure: Some(why.into()),
        }
    }

    pub(crate) fn succeeded(&self) -> bool {
        self.failure.is_none()
    }

    /// The startup log's lines: a fixed order over fixed fields, with the ids
    /// bounded and sorted.
    pub(crate) fn summary(&self) -> Vec<String> {
        let mods = self
            .mods
            .map_or_else(|| "unknown".to_owned(), |mods| mods.to_string());
        let mut lines = vec![format!(
            "fml-preflight: role={} jars={} mods={mods} real-loader={}",
            role_name(self.role),
            self.jars,
            if self.succeeded() {
                "completed"
            } else {
                "failed"
            },
        )];
        if !self.mod_ids.is_empty() {
            lines.push(format!("fml-preflight: ids=[{}]", self.mod_ids.join(", ")));
        }
        if let Some(failure) = &self.failure {
            lines.push(format!("fml-preflight: failed: {failure}"));
        }
        lines
    }

    /// The precise error `HORIZON_NEOFORGE_STRICT=1` returns.
    pub(super) fn error(&self) -> String {
        format!(
            "NeoForge FML preflight failed for {} ({} jar(s)): {}",
            role_name(self.role),
            self.jars,
            self.failure.as_deref().unwrap_or("unknown"),
        )
    }
}

/// A preflight that ran, and whether the environment asked for it to be
/// strict. Kept together so a caller logs the report before failing startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FmlOutcome {
    pub(crate) report: FmlReport,
    pub(crate) strict: bool,
}

impl FmlOutcome {
    /// The error a strict failure returns; `None` when the probe succeeded or
    /// the environment only asked for a log line.
    pub(crate) fn strict_error(&self) -> Option<String> {
        (self.strict && !self.report.succeeded()).then(|| self.report.error())
    }
}

/// The last report a preflight in this process produced, for `server-status`'s
/// `fml` section. `None` means no preflight ran yet: a skip is not a failure,
/// and a later startup overwrites an earlier report.
static LAST_REPORT: OnceLock<RwLock<Option<FmlReport>>> = OnceLock::new();

fn last_reports() -> &'static RwLock<Option<FmlReport>> {
    LAST_REPORT.get_or_init(|| RwLock::new(None))
}

/// The last preflight's report, or `None` when none ran in this process.
pub(in crate::game) fn last_report() -> Option<FmlReport> {
    last_reports()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Records what a preflight found, replacing whatever was there before.
fn store(report: Option<FmlReport>) {
    *last_reports()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = report;
}

/// Whether `HORIZON_NEOFORGE_STRICT` asked a failure to stop startup. Read
/// again for `server-status`, because [`FmlReport`] does not carry it.
pub(in crate::game) fn strict_requested() -> bool {
    horizon_platform::env::var(STRICT_ENV).is_ok_and(|value| env_flag(&value))
}

/// Runs the preflight when the environment asks for it and this role is a
/// server. `None` means it was skipped, which is the normal startup; a failure
/// is a report here, and only [`FmlOutcome::strict_error`] makes it fatal.
pub(crate) fn preflight(role: ExecutionRole, jars: &[PathBuf]) -> Option<FmlOutcome> {
    Some(preflight_with(FmlConfig::from_env(role, jars)?))
}

/// One configured preflight, kept for `server-status` whatever it found: a
/// missing `libraries` folder is a report too, never a silent skip.
fn preflight_with(config: FmlConfig) -> FmlOutcome {
    let report = if config.libraries().is_dir() {
        run(&config)
    } else {
        FmlReport::failed(
            config.role,
            config.jars.len(),
            format!("{} has no libraries folder", config.server.display()),
        )
    };
    store(Some(report.clone()));
    FmlOutcome {
        report,
        strict: config.strict,
    }
}

/// Runs the child process, bounded in time and output, and turns what it said
/// into a report. Every failure is a reason, never a panic.
fn run(config: &FmlConfig) -> FmlReport {
    let jars = config.jars.len();
    let mut command = Command::new(&config.jvm);
    command
        .args(config.command_args())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return FmlReport::failed(
                config.role,
                jars,
                format!("could not start {}: {error}", config.jvm.display()),
            );
        }
    };
    // Both pipes get their own reader, so a chatty child cannot fill one and
    // block while this thread waits for it to exit.
    let stdout = child.stdout.take().map(capture);
    let stderr = child.stderr.take().map(capture);
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() < PREFLIGHT_TIMEOUT => {
                horizon_platform::thread::sleep(POLL_INTERVAL);
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return FmlReport::failed(
                    config.role,
                    jars,
                    format!("lost track of {}: {error}", config.jvm.display()),
                );
            }
        }
    };
    let stdout = stdout.map(join).unwrap_or_default();
    let stderr = stderr.map(join).unwrap_or_default();
    let parsed = FmlProbeOutput::parse(&stdout, &stderr);
    let Some(status) = status else {
        return FmlReport::failed(config.role, jars, timed_out());
    };
    if !status.success() {
        let why = format!("{} exited with {status}", config.jvm.display());
        let why = last_line(&stderr).map_or(why.clone(), |line| format!("{why}: {line}"));
        return FmlReport::failed(config.role, jars, why);
    }
    if !parsed.completed {
        return FmlReport::failed(config.role, jars, incomplete(stdout.len() + stderr.len()));
    }
    FmlReport {
        role: config.role,
        jars,
        mods: parsed.mods,
        mod_ids: parsed.ids,
        failure: None,
    }
}

/// Reads a stream to its end, keeping at most [`MAX_CAPTURE_BYTES`].
fn capture(mut pipe: impl Read + Send + 'static) -> horizon_platform::thread::JoinHandle<String> {
    horizon_platform::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let room = MAX_CAPTURE_BYTES.saturating_sub(kept.len());
                    kept.extend_from_slice(&buf[..read.min(room)]);
                }
            }
        }
        String::from_utf8_lossy(&kept).into_owned()
    })
}

fn join(handle: horizon_platform::thread::JoinHandle<String>) -> String {
    handle.join().unwrap_or_default()
}

fn timed_out() -> String {
    format!("timed out after {}s", PREFLIGHT_TIMEOUT.as_secs())
}

fn incomplete(bytes: usize) -> String {
    format!("the FML probe did not complete ({bytes} byte(s) of output)")
}

/// The last non-empty line of a stream, clipped, for a failure reason.
fn last_line(text: &str) -> Option<String> {
    let line = text.lines().rev().map(str::trim).find(|l| !l.is_empty())?;
    Some(line.chars().take(MAX_REASON_CHARS).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(jars: &[&str]) -> FmlConfig {
        FmlConfig {
            role: ExecutionRole::DedicatedServer,
            server: PathBuf::from("/srv/neoforge"),
            jvm: PathBuf::from("/opt/horizon/bin/horizon-jvm"),
            strict: false,
            jars: jars.iter().map(PathBuf::from).collect(),
        }
    }

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn command_names_the_real_loader_and_the_installed_server() {
        let args = config(&["/mods/a.jar", "/mods/b.jar"]).command_args();
        assert_eq!(
            args,
            [
                "--real-loader",
                "--entry-kind",
                "neoforge",
                "--libs",
                "/srv/neoforge/libraries",
                "/mods/a.jar",
                "/mods/b.jar",
            ]
        );
    }

    #[test]
    fn probe_output_reads_the_count_ids_and_completion() {
        let output = FmlProbeOutput::parse(
            "mods=2\nmod=b\nmod=a\nmod=a\n",
            "some warning\nreal loader: FML probe completed\n",
        );
        assert_eq!(output.mods, Some(2));
        assert_eq!(output.ids, ["a", "b"]);
        assert!(output.completed);
    }

    /// `horizon-jvm` prints `<id>\t<version>`; the version is not part of the
    /// id, and the same id listed twice is one.
    #[test]
    fn probe_output_keeps_the_id_before_the_version() {
        let output = FmlProbeOutput::parse("mod=amod\t1.0\nmod=bmod 2.3\n", "");
        assert_eq!(output.ids, ["amod", "bmod"]);
    }

    #[test]
    fn probe_output_keeps_the_first_count_and_bounds_the_ids() {
        let lines: Vec<String> = (0..MAX_MOD_IDS + 4)
            .map(|index| format!("mod=m{index:02}"))
            .collect();
        let output = FmlProbeOutput::parse("mods=1\nmods=9\n", &lines.join("\n"));
        assert_eq!(output.mods, Some(1));
        assert_eq!(output.ids.len(), MAX_MOD_IDS);
        assert_eq!(output.ids.first().map(String::as_str), Some("m00"));
        assert_eq!(output.ids.last().map(String::as_str), Some("m63"));
        assert!(!output.completed);
    }

    #[test]
    fn a_client_never_reads_the_environment() {
        let set = lookup(&[(SERVER_ENV, "/srv/neoforge"), (JVM_ENV, "/bin/horizon-jvm")]);
        assert!(FmlConfig::from_lookup(ExecutionRole::Client, &[], set).is_none());
    }

    #[test]
    fn a_missing_or_blank_path_skips_the_preflight() {
        let only_server = lookup(&[(SERVER_ENV, "/srv/neoforge")]);
        let only_jvm = lookup(&[(JVM_ENV, "/bin/horizon-jvm")]);
        let blank = lookup(&[(SERVER_ENV, "  "), (JVM_ENV, " ")]);
        for get in [only_server, only_jvm, blank] {
            assert!(FmlConfig::from_lookup(ExecutionRole::DedicatedServer, &[], get).is_none());
        }
        let both = lookup(&[(SERVER_ENV, "/srv/neoforge"), (JVM_ENV, "/bin/horizon-jvm")]);
        let config = FmlConfig::from_lookup(ExecutionRole::IntegratedServer, &[], both).unwrap();
        assert_eq!(config.role, ExecutionRole::IntegratedServer);
        assert!(!config.strict);
    }

    #[test]
    fn strict_is_an_explicit_opt_in() {
        for (value, strict) in [
            ("1", true),
            ("true", true),
            ("YES", true),
            ("on", true),
            ("0", false),
            ("", false),
            ("maybe", false),
        ] {
            let pairs = [
                (SERVER_ENV, "/srv/neoforge"),
                (JVM_ENV, "/bin/horizon-jvm"),
                (STRICT_ENV, value),
            ];
            let get = lookup(&pairs);
            let config = FmlConfig::from_lookup(ExecutionRole::DedicatedServer, &[], get).unwrap();
            assert_eq!(config.strict, strict, "{value:?}");
        }
    }

    #[test]
    fn a_report_names_its_role_counts_and_success() {
        let report = FmlReport {
            role: ExecutionRole::IntegratedServer,
            jars: 2,
            mods: Some(3),
            mod_ids: vec!["a".into(), "b".into()],
            failure: None,
        };
        assert_eq!(
            report.summary(),
            [
                "fml-preflight: role=integrated-server jars=2 mods=3 real-loader=completed",
                "fml-preflight: ids=[a, b]",
            ]
        );
    }

    #[test]
    fn a_failure_says_why_and_only_strict_turns_it_into_an_error() {
        for why in [
            timed_out(),
            incomplete(12),
            "could not start /bin/horizon-jvm: not found".to_owned(),
        ] {
            let report = FmlReport::failed(ExecutionRole::DedicatedServer, 1, why.clone());
            assert!(!report.succeeded());
            let expected = format!("fml-preflight: failed: {why}");
            assert_eq!(
                report.summary().last().map(String::as_str),
                Some(expected.as_str())
            );
            assert!(report.error().contains(&why));
            let logged = FmlOutcome {
                report,
                strict: false,
            };
            assert!(logged.strict_error().is_none());
        }
        let failed = FmlReport::failed(ExecutionRole::DedicatedServer, 1, timed_out());
        let strict = FmlOutcome {
            report: failed,
            strict: true,
        };
        assert!(
            strict
                .strict_error()
                .is_some_and(|error| error.contains("dedicated-server") && error.contains("120s"))
        );
        let passed = FmlOutcome {
            report: FmlReport {
                role: ExecutionRole::DedicatedServer,
                jars: 0,
                mods: Some(1),
                mod_ids: Vec::new(),
                failure: None,
            },
            strict: true,
        };
        assert!(passed.strict_error().is_none());
    }

    #[test]
    fn a_failure_reason_keeps_only_the_last_output_line() {
        assert_eq!(last_line("a\n\n  b  \n").as_deref(), Some("b"));
        assert_eq!(last_line(" \n"), None);
    }

    /// The tests below share the process-wide report.
    static LAST: horizon_platform::sync::Mutex<()> = horizon_platform::sync::Mutex::new(());

    fn report(jars: usize) -> FmlReport {
        FmlReport {
            role: ExecutionRole::DedicatedServer,
            jars,
            mods: Some(jars),
            mod_ids: Vec::new(),
            failure: None,
        }
    }

    #[test]
    fn the_last_report_is_idempotent_and_last_write_wins() {
        let _guard = LAST.lock().unwrap();
        store(None);
        assert_eq!(last_report(), None);
        let first = report(1);
        store(Some(first.clone()));
        assert_eq!(last_report(), Some(first.clone()));
        // Storing the same report again changes nothing.
        store(Some(first.clone()));
        assert_eq!(last_report(), Some(first.clone()));
        // A later preflight replaces it instead of adding to it.
        let second = report(2);
        store(Some(second.clone()));
        assert_eq!(last_report(), Some(second));
        store(None);
        assert_eq!(last_report(), None);
    }

    /// A missing server folder is a report, not a skip: `server-status` has to
    /// say the preflight ran and failed, and nothing is spawned to find out.
    #[test]
    fn a_missing_libraries_folder_is_recorded_as_failed() {
        let _guard = LAST.lock().unwrap();
        store(None);
        let config = FmlConfig {
            server: PathBuf::from("/horizon-absent-neoforge"),
            strict: true,
            jars: vec![PathBuf::from("/mods/a.jar")],
            ..config(&[])
        };
        let outcome = preflight_with(config);
        assert!(!outcome.report.succeeded());
        assert!(outcome.strict);
        let recorded = last_report().expect("the failure is kept for server-status");
        assert_eq!(recorded, outcome.report);
        assert!(
            recorded
                .failure
                .as_deref()
                .is_some_and(|why| why.contains("no libraries folder"))
        );
        assert_eq!(recorded.jars, 1);
    }

    /// A fake `horizon-jvm` proves the process path without a JVM: the report
    /// comes from the child's real output, and a non-zero exit is a reason.
    #[cfg(unix)]
    #[test]
    fn a_child_process_success_and_failure_become_reports() {
        use std::os::unix::fs::PermissionsExt;

        let root = horizon_platform::env::temp_dir().join(format!(
            "horizon-fml-{}-{}",
            std::process::id(),
            horizon_platform::time::SystemTime::now()
                .duration_since(horizon_platform::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let libraries = root.join("libraries");
        horizon_platform::fs::create_dir_all(&libraries).unwrap();
        let script = root.join("fake-jvm");
        horizon_platform::fs::write(
            &script,
            "#!/bin/sh\necho mods=2\necho 'mod=beta\t2.0'\necho mod=alpha\necho 'real loader: FML probe completed'\n",
        )
        .unwrap();
        horizon_platform::fs::set_permissions(
            &script,
            horizon_platform::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let config = FmlConfig {
            role: ExecutionRole::DedicatedServer,
            server: root.clone(),
            jvm: script,
            strict: false,
            jars: Vec::new(),
        };
        let report = run(&config);
        assert!(report.succeeded(), "{report:?}");
        assert_eq!(report.mods, Some(2));
        assert_eq!(report.mod_ids, ["alpha", "beta"]);
        assert_eq!(
            report.summary().first().map(String::as_str),
            Some("fml-preflight: role=dedicated-server jars=0 mods=2 real-loader=completed")
        );

        horizon_platform::fs::write(
            &config.jvm,
            "#!/bin/sh\necho 'loader blew up' >&2\nexit 3\n",
        )
        .unwrap();
        horizon_platform::fs::set_permissions(
            &config.jvm,
            horizon_platform::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let failed = run(&config);
        assert!(!failed.succeeded());
        let failure = failed.failure.as_deref().unwrap_or_default();
        assert!(failure.contains("exited with"), "{failure}");
        assert!(failure.contains("loader blew up"), "{failure}");
        let _ = horizon_platform::fs::remove_dir_all(&root);
    }
}
