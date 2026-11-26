//! OldUsersConverter: the pre-1.7 text user lists (`banned-ips.txt`,
//! `banned-players.txt`, `ops.txt`, `white-list.txt`) a server converts when it
//! first starts on a world from those versions, before the json lists are
//! loaded, in `DedicatedServer.convertOldUsers`' order.
use super::{
    profiles::{self, NoAnswer},
    user_lists::{
        BanEntry, BanList, Entry, IpBanList, NameAndId, OpEntry, OpList, UserList, WhiteEntry,
        WhiteList, parse_date, read_utf8,
    },
};
use horizon_std::fs::PathExt as _;
use std::path::Path;

/// `OldUsersConverter.OLD_USERBANLIST`.
pub const OLD_USER_BAN_LIST: &str = "banned-players.txt";
/// `OldUsersConverter.OLD_IPBANLIST`.
pub const OLD_IP_BAN_LIST: &str = "banned-ips.txt";
/// `OldUsersConverter.OLD_OPLIST`.
pub const OLD_OP_LIST: &str = "ops.txt";
/// `OldUsersConverter.OLD_WHITELIST`.
pub const OLD_WHITE_LIST: &str = "white-list.txt";

/// `OldUsersConverter.readOldListFormat`: the lines that are not a comment or
/// blank, each split on `|` (Java's split drops the trailing empty fields), the
/// first field lowercased as the map key (Locale.ROOT).
pub fn read_old_list_format(text: &str) -> Vec<(String, Vec<String>)> {
    let mut users = Vec::new();
    for line in text.split(['\n', '\r']) {
        // Java's String.trim: only characters up to U+0020.
        let line = line.trim_matches(|c: char| c <= ' ');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts: Vec<String> = line.split('|').map(str::to_owned).collect();
        while parts.len() > 1 && parts.last().is_some_and(String::is_empty) {
            parts.pop();
        }
        let key = parts[0].to_lowercase();
        users.push((key, parts));
    }
    users
}

/// `Files.readLines` for the lists of bare names (`convertOpsList` and
/// `convertWhiteList`): every line, split as Java splits them; the empty ones
/// are dropped by `lookupPlayers`. Vanilla does not trim these lines or treat
/// `#` specially, unlike `readOldListFormat`.
fn read_old_lines(text: &str) -> Vec<String> {
    text.split(['\n', '\r']).map(str::to_owned).collect()
}

/// Where an online conversion asks for profiles: `None` for an offline server.
type Lookup<'a> = Option<&'a str>;

/// The discovery document an online server's conversion asks, `None` offline.
fn lookup(online: bool) -> Option<String> {
    online.then(profiles::discovery_url)
}

/// `OldUsersConverter.lookupPlayers` with its callback's lookup: the names that
/// are not empty, each the offline profile without authentication, else the
/// account the services know (asked for the name in lower case). A name the
/// services do not know (`ProfileNotFoundException`) is left out with vanilla's
/// warning; any other failure is vanilla's `ConversionError`, which ends the
/// conversion before the json list is saved. The callback's
/// `nameToIdCache().add` is not done here.
fn lookup_players(
    names: impl IntoIterator<Item = String>,
    discovery: Lookup<'_>,
    what: &str,
) -> Result<Vec<NameAndId>, NoAnswer> {
    let mut found = Vec::new();
    for name in names.into_iter().filter(|name| !name.is_empty()) {
        let Some(discovery) = discovery else {
            found.push(NameAndId::offline(&name));
            continue;
        };
        // The profile repository skips names that are not valid player names.
        let answer = if profiles::valid_player_name(&name) {
            profiles::look_up_by_name(discovery, &name.to_ascii_lowercase())
        } else {
            Ok(None)
        };
        match answer {
            Ok(Some(user)) => found.push(user),
            Ok(None) => log::warn!("Could not lookup {what} entry for {name}"),
            Err(e) => {
                log::warn!("Could not lookup {what} entry for {name}: the services {e}");
                return Err(e);
            }
        }
    }
    Ok(found)
}

/// The shell of one conversion (`convertUserBanlist`, `convertIpBanlist`,
/// `convertOpsList`, `convertWhiteList`): merge into the json list, as the
/// list's constructor and `load` do (a broken existing file is only warned
/// about), then save it and rename the old file to `.converted`
/// (`renameOldFile`, whose result is ignored). False on a read or save failure,
/// or a profile lookup the services did not answer, which vanilla retries: the
/// json list is then not written and the old file stays for the next start.
fn convert<E: Entry>(
    dir: &Path,
    old_name: &str,
    mut list: UserList<E>,
    what: &str,
    fill: impl FnOnce(&mut UserList<E>, &str) -> Result<(), NoAnswer>,
) -> bool {
    let old = dir.join(old_name);
    if !old.fs_is_file() {
        return true;
    }
    if list.file.fs_exists()
        && let Err(e) = list.load()
    {
        log::warn!("Could not load existing file {}: {e}", list.file.display());
    }
    let text = match horizon_std::fs::read(&old) {
        Ok(bytes) => read_utf8(&bytes),
        Err(e) => {
            log::warn!("Could not parse old {what} to convert it! {e}");
            return false;
        }
    };
    if let Err(e) = fill(&mut list, &text) {
        log::warn!("Could not convert {what} because of a lookup failure: the services {e}");
        return false;
    }
    if let Err(e) = list.save() {
        log::warn!("Could not parse old {what} to convert it! {e}");
        return false;
    }
    // Vanilla ignores a rename that fails; the write above is the conversion.
    let _ = horizon_std::fs::rename(&old, dir.join(format!("{old_name}.converted")));
    true
}

/// `OldUsersConverter.convertUserBanlist` for the working directory, as
/// vanilla's `File("banned-players.txt")` reads it.
pub fn convert_user_banlist(online: bool) -> bool {
    convert_user_banlist_in(Path::new("."), online)
}

/// Convert `dir/banned-players.txt` into `dir/banned-players.json`, keeping the
/// entries the json file already has, then rename the old file to `.converted`.
/// The profile lookup is `server.usesAuthentication()`: an offline server makes
/// the offline profile of the lower-case key, an online one asks the services.
pub fn convert_user_banlist_in(dir: &Path, online: bool) -> bool {
    let discovery = lookup(online);
    convert(
        dir,
        OLD_USER_BAN_LIST,
        BanList::new(dir.join("banned-players.json")),
        "user banlist",
        |bans, text| {
            for (key, parts) in read_old_list_format(text) {
                let created = parts.get(1).and_then(|s| parse_date(s));
                let source = parts.get(2).cloned();
                let expires = parts.get(3).and_then(|s| parse_date(s));
                let reason = parts.get(4).cloned();
                if let Some(user) =
                    lookup_players([key], discovery.as_deref(), "user banlist")?.pop()
                {
                    bans.add(BanEntry::with_created(
                        user, created, source, expires, reason,
                    ));
                }
            }
            Ok(())
        },
    )
}

/// `OldUsersConverter.convertIpBanlist` for the working directory, as vanilla's
/// `File("banned-ips.txt")` reads it.
pub fn convert_ip_banlist() -> bool {
    convert_ip_banlist_in(Path::new("."))
}

/// Convert `dir/banned-ips.txt` into `dir/banned-ips.json`, keeping the entries
/// the json file already has, then rename the old file to `.converted`.
pub fn convert_ip_banlist_in(dir: &Path) -> bool {
    convert(
        dir,
        OLD_IP_BAN_LIST,
        IpBanList::new(dir.join("banned-ips.json")),
        "ip banlist",
        |bans, text| {
            for (ip, parts) in read_old_list_format(text) {
                let created = parts.get(1).and_then(|s| parse_date(s));
                let source = parts.get(2).cloned();
                let expires = parts.get(3).and_then(|s| parse_date(s));
                let reason = parts.get(4).cloned();
                bans.add(BanEntry::with_created(ip, created, source, expires, reason));
            }
            Ok(())
        },
    )
}

/// `OldUsersConverter.convertOpsList` for the working directory, as vanilla's
/// `File("ops.txt")` reads it.
pub fn convert_ops_list(online: bool, level: i32) -> bool {
    convert_ops_list_in(Path::new("."), online, level)
}

/// Convert `dir/ops.txt` into `dir/ops.json`, keeping the entries the json file
/// already has, then rename the old file to `.converted`. Every name gets
/// `server.operatorUserPermissions()` as its level and does not bypass the
/// player limit, as vanilla's `ServerOpListEntry(profile, permissions, false)`.
pub fn convert_ops_list_in(dir: &Path, online: bool, level: i32) -> bool {
    convert_ops_list_with(dir, lookup(online).as_deref(), level)
}

fn convert_ops_list_with(dir: &Path, discovery: Lookup<'_>, level: i32) -> bool {
    convert(
        dir,
        OLD_OP_LIST,
        OpList::new(dir.join("ops.json")),
        "oplist",
        |ops, text| {
            for user in lookup_players(read_old_lines(text), discovery, "oplist")? {
                ops.add(OpEntry {
                    user,
                    level,
                    bypasses_player_limit: false,
                });
            }
            Ok(())
        },
    )
}

/// `OldUsersConverter.convertWhiteList` for the working directory, as vanilla's
/// `File("white-list.txt")` reads it.
pub fn convert_white_list(online: bool) -> bool {
    convert_white_list_in(Path::new("."), online)
}

/// Convert `dir/white-list.txt` into `dir/whitelist.json`, keeping the entries
/// the json file already has, then rename the old file to `.converted`.
pub fn convert_white_list_in(dir: &Path, online: bool) -> bool {
    let discovery = lookup(online);
    convert(
        dir,
        OLD_WHITE_LIST,
        WhiteList::new(dir.join("whitelist.json")),
        "whitelist",
        |white, text| {
            for user in lookup_players(read_old_lines(text), discovery.as_deref(), "whitelist")? {
                white.add(WhiteEntry { user });
            }
            Ok(())
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use horizon_protocol::crypto::uuid_string;

    /// One old list of names each: the line and field mapping, the profile
    /// lookup (offline, so the lower-case ban key and the raw op/whitelist
    /// lines), the written json and the `.converted` rename.
    #[test]
    fn converts_the_old_name_lists() {
        let dir =
            horizon_std::env::temp_dir().join(format!("horizon-old-users-{}", std::process::id()));
        let _ = horizon_std::fs::remove_dir_all(&dir);
        horizon_std::fs::create_dir_all(&dir).unwrap();
        let written = |name: &str| horizon_std::fs::read_to_string(dir.join(name)).unwrap();

        // banned-players.txt: comments and blank lines skipped, the line
        // trimmed (so the name's surrounding spaces go) but a reason's inner
        // spaces kept. Of the fields after the name: created, source, expires,
        // reason. A line with no fields takes the constructor's defaults.
        horizon_std::fs::write(
            dir.join(OLD_USER_BAN_LIST),
            "# old bans\r\n\r\n  Griefer|2020-02-03 04:05:06 +0000|Server|2031-01-01 00:00:00 +0000|a reason with spaces  \nQuiet\n",
        )
        .unwrap();
        assert!(convert_user_banlist_in(&dir, false));
        assert!(!dir.join(OLD_USER_BAN_LIST).exists());
        assert!(dir.join("banned-players.txt.converted").exists());
        let mut bans = BanList::new(dir.join("banned-players.json"));
        bans.load().unwrap();
        let ban = |name: &str| {
            bans.entries()
                .into_iter()
                .find(|e| e.user.name == name)
                .cloned()
                .unwrap()
        };
        let griefer = ban("griefer"); // the lookup's lower-case key
        assert_eq!(griefer.user.id, NameAndId::offline("griefer").id);
        assert_eq!(
            griefer.created,
            parse_date("2020-02-03 04:05:06 +0000").unwrap()
        );
        assert_eq!(griefer.source, "Server");
        assert_eq!(griefer.expires, parse_date("2031-01-01 00:00:00 +0000"));
        assert_eq!(griefer.reason.as_deref(), Some("a reason with spaces"));
        let quiet = ban("quiet");
        assert!(quiet.created > 0);
        assert_eq!(quiet.source, "(Unknown)");
        assert_eq!(quiet.expires, None);
        assert_eq!(quiet.reason, None);
        let bans_text = written("banned-players.json");
        assert!(bans_text.contains("\"created\": \"2020-02-03 04:05:06 +0000\""));
        assert!(bans_text.contains("\"expires\": \"forever\""));

        // ops.txt: blank lines dropped, the line not trimmed, every op gets
        // the level given and no player-limit bypass.
        horizon_std::fs::write(dir.join(OLD_OP_LIST), "Notch\n\n").unwrap();
        assert!(convert_ops_list_in(&dir, false, 3));
        assert!(!dir.join(OLD_OP_LIST).exists());
        assert!(dir.join("ops.txt.converted").exists());
        let uuid = uuid_string(NameAndId::offline("Notch").id);
        assert_eq!(
            written("ops.json"),
            format!(
                "[\n  {{\n    \"uuid\": \"{uuid}\",\n    \"name\": \"Notch\",\n    \"level\": 3,\n    \"bypassesPlayerLimit\": false\n  }}\n]"
            )
        );

        // white-list.txt: a bare name, looked up as written (offline keeps its
        // case, unlike the ban list's lower-case key).
        horizon_std::fs::write(dir.join(OLD_WHITE_LIST), "Alex\n\n").unwrap();
        assert!(convert_white_list_in(&dir, false));
        assert!(!dir.join(OLD_WHITE_LIST).exists());
        assert!(dir.join("white-list.txt.converted").exists());
        let mut white = WhiteList::new(dir.join("whitelist.json"));
        white.load().unwrap();
        let names: Vec<&str> = white
            .entries()
            .iter()
            .map(|e| e.user.name.as_str())
            .collect();
        assert_eq!(names, ["Alex"]);
        assert!(written("whitelist.json").contains(&uuid_string(NameAndId::offline("Alex").id)));

        let _ = horizon_std::fs::remove_dir_all(&dir);
    }

    /// An online conversion whose profile lookup gets no answer writes no json
    /// list and keeps the old file for the next start, as vanilla's
    /// `ConversionError` makes `convertOpsList` return false.
    #[test]
    fn a_failed_lookup_keeps_the_old_list() {
        let dir =
            horizon_std::env::temp_dir().join(format!("horizon-old-ops-{}", std::process::id()));
        let _ = horizon_std::fs::remove_dir_all(&dir);
        horizon_std::fs::create_dir_all(&dir).unwrap();
        horizon_std::fs::write(dir.join(OLD_OP_LIST), "Notch\nJeb_\n").unwrap();
        // Nothing listens on port 1, so the discovery request fails at once.
        assert!(!convert_ops_list_with(
            &dir,
            Some("http://127.0.0.1:1/discovery"),
            4
        ));
        assert_eq!(
            horizon_std::fs::read_to_string(dir.join(OLD_OP_LIST)).unwrap(),
            "Notch\nJeb_\n"
        );
        assert!(!dir.join("ops.json").exists());
        assert!(!dir.join("ops.txt.converted").exists());
        let _ = horizon_std::fs::remove_dir_all(&dir);
    }

    /// The old IP line format: comments and blanks skipped, fields split on `|`
    /// with trailing empties dropped, the key lowercased and the whole line
    /// trimmed of characters up to U+0020 only; then a conversion merges into
    /// the json list and renames the old file.
    #[test]
    fn converts_an_old_ip_ban_list() {
        let users = read_old_list_format(
            "# comment\r\n\r\n 10.0.0.1|2020-02-03 04:05:06 +0000|Server|2021-01-01 00:00:00 +0000|grief|  \n10.0.0.2|",
        );
        assert_eq!(
            users,
            [
                (
                    "10.0.0.1".to_owned(),
                    [
                        "10.0.0.1",
                        "2020-02-03 04:05:06 +0000",
                        "Server",
                        "2021-01-01 00:00:00 +0000",
                        "grief"
                    ]
                    .map(str::to_owned)
                    .to_vec()
                ),
                ("10.0.0.2".to_owned(), vec!["10.0.0.2".to_owned()]),
            ]
        );
        let dir =
            horizon_std::env::temp_dir().join(format!("horizon-old-ips-{}", std::process::id()));
        let _ = horizon_std::fs::remove_dir_all(&dir);
        horizon_std::fs::create_dir_all(&dir).unwrap();
        horizon_std::fs::write(
            dir.join("banned-ips.json"),
            "[{\"ip\":\"9.9.9.9\",\"created\":\"2020-01-01 00:00:00 +0000\",\"source\":\"Server\",\"expires\":\"forever\",\"reason\":null}]",
        )
        .unwrap();
        horizon_std::fs::write(
            dir.join(OLD_IP_BAN_LIST),
            "# old bans\n10.0.0.1|2020-02-03 04:05:06 +0000|Server|2031-01-01 00:00:00 +0000|grief\n10.0.0.2\n",
        )
        .unwrap();
        assert!(convert_ip_banlist_in(&dir));
        assert!(!dir.join(OLD_IP_BAN_LIST).exists());
        assert!(dir.join("banned-ips.txt.converted").exists());
        let mut after = IpBanList::new(dir.join("banned-ips.json"));
        after.load().unwrap();
        let found = |ip: &str| {
            after
                .entries()
                .into_iter()
                .find(|e| e.user == ip)
                .cloned()
                .unwrap()
        };
        assert_eq!(found("9.9.9.9").reason, None);
        let converted = found("10.0.0.1");
        assert_eq!(
            converted.created,
            parse_date("2020-02-03 04:05:06 +0000").unwrap()
        );
        assert_eq!(converted.source, "Server");
        assert_eq!(converted.expires, parse_date("2031-01-01 00:00:00 +0000"));
        assert_eq!(converted.reason.as_deref(), Some("grief"));
        // A line with just the address gets the constructor's defaults.
        let bare = found("10.0.0.2");
        assert_eq!(bare.source, "(Unknown)");
        assert_eq!(bare.expires, None);
        assert_eq!(bare.reason, None);
        let _ = horizon_std::fs::remove_dir_all(&dir);
    }
}
