//! What `horizon-server` asks the dedicated server on its control endpoint (`--control-socket`, docs/server.md
//! section 2): `server-status` answers with the tick rate, the tick times `/tick query` reports, the players
//! (including their positions), the loaded chunks, the Java jars this role started and the last opt-in
//! NeoForge FML preflight; `console <line>` runs a console command and answers with the lines it logged;
//! `server-events <since>` answers the typed events after that number (events.rs); `management <connection>
//! <json>` edits the lists as vanilla's management protocol does (management.rs).
//! `player-expect <uuid> [seconds]`, `player-drop <uuid>`, `player-export <uuid> [freeze]`,
//! `player-release <uuid> ...`, `player-import <uuid> <json>` and `player-confirm <uuid> ...` are a proxy's,
//! for a player it moves between two servers (transfer.rs).
use super::super::java_mods::{
    JavaCompatibilityReport, JavaEngine, JavaLoader,
    fml::{self, role_name},
};
use super::{ServerState, capture_log};
use crate::game::Game;
use serde_json::{Map, Value, json};
use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

/// The window the tick rate is measured over.
const TPS_WINDOW: Duration = Duration::from_secs(5);

/// The most jars `server-status` lists one by one; the counts stay whole.
const MAX_STATUS_JARS: usize = 8;
/// The most payload channel ids the `java` section names; the count stays whole.
const MAX_STATUS_ROWS: usize = 12;
/// The most mod ids the `fml` section lists; FML's count stays whole.
const MAX_FML_IDS: usize = 16;
/// The most characters of a failure the java and fml sections repeat.
const MAX_STATUS_ERROR_CHARS: usize = 200;

/// The engine labels `engines` counts, in the order it prints them. The
/// restored runner has no sidecar engine, so the old label stays at zero.
const ENGINE_LABELS: [JavaEngine; 3] = [
    JavaEngine::Native,
    JavaEngine::NativeQualified,
    JavaEngine::Refused,
];

/// The backends `backends` counts, in the order it prints them: what actually
/// executed a jar, as opposed to how complete the result was. A native result
/// stays in process and a refused jar ran on nothing, so `jvm` is always zero
/// until a sidecar engine comes back.
const BACKEND_LABELS: [&str; 3] = ["native", "jvm", "none"];

/// What the load weave did with a jar, as the engine the run recorded says:
/// `Native` applied every plan, `NativeQualified` applied some and recorded the
/// rest as skipped, and a refused jar was never woven.
fn weave_name(engine: JavaEngine) -> &'static str {
    match engine {
        JavaEngine::Native => "applied",
        JavaEngine::NativeQualified => "qualified",
        JavaEngine::Refused => "none",
    }
}

/// What ran a jar, not how complete the run was: a qualified native result
/// still costs no JVM.
fn backend_name(engine: JavaEngine) -> &'static str {
    match engine {
        JavaEngine::Native | JavaEngine::NativeQualified => "native",
        JavaEngine::Refused => "none",
    }
}

// The batch store belongs to the runner (`java_mods`): the integrated server
// thread records into it too, and this module only answers from it. The
// re-export keeps the dedicated startup's `status::record_java_reports` call.
pub(super) use super::super::java_mods::record_java_reports;

/// The batch a role recorded: the runner keeps it because both roles that
/// start jars answer `server-status` from the same process, not because a
/// status request runs on their thread.
fn java_rows() -> Vec<JavaCompatibilityReport> {
    super::super::java_mods::java_reports()
}

/// The `java` key of `server-status`.
///
/// The per-jar rows are the runner's reports; the payload channels are the
/// content root's declarations, which the transport keeps process-wide and
/// does not attribute to a jar, so they are named once for the section.
pub(super) fn java_status() -> Value {
    let channels: Vec<String> = super::super::java_payloads::declared()
        .declarations()
        .map(|declaration| declaration.id.clone())
        .collect();
    java_section(&java_rows(), &channels)
}

/// The `java` section of `server-status`: which explicitly selected jars the
/// loader started, each one's loader, engine, backend, weave outcome and the
/// work it cost (entrypoints executed, registrations recorded, how many of
/// those stayed opaque), and whether any was isolated. The section totals
/// those over every jar, so the bounded entry list never hides the whole.
/// Sorted by jar path and bounded, so two servers that started the same jars
/// answer the same. Every key is fixed, so a server with no jars answers zeros
/// and an empty entry list.
pub(super) fn java_section(reports: &[JavaCompatibilityReport], channels: &[String]) -> Value {
    let mut sorted: Vec<&JavaCompatibilityReport> = reports.iter().collect();
    sorted.sort_by(|a, b| a.jar.cmp(&b.jar));
    let count = |of: fn(&JavaCompatibilityReport) -> usize| {
        sorted.iter().map(|report| of(report)).sum::<usize>()
    };
    // Every engine label with its count over every jar, refused ones included:
    // the entries list is bounded, these counts are not.
    let mut engines = Map::new();
    for engine in ENGINE_LABELS {
        let count = sorted
            .iter()
            .filter(|report| report.engine == engine)
            .count();
        engines.insert(engine.name().to_owned(), json!(count));
    }
    engines.insert("sidecar".to_owned(), json!(0));
    // The same jars by what ran them, not by how complete the run was.
    let mut backends = Map::new();
    for backend in BACKEND_LABELS {
        let count = sorted
            .iter()
            .filter(|report| backend_name(report.engine) == backend)
            .count();
        backends.insert(backend.to_owned(), json!(count));
    }
    let mut weave = Map::new();
    for (label, name) in [
        (JavaEngine::Native, "applied"),
        (JavaEngine::NativeQualified, "qualified"),
        (JavaEngine::Refused, "none"),
    ] {
        let count = sorted
            .iter()
            .filter(|report| report.engine == label)
            .count();
        weave.insert(name.to_owned(), json!(count));
    }
    // The content root's channels, in id order, named once: the runner's
    // reports carry no per-jar channel attribution.
    let mut channel_ids: Vec<&str> = channels.iter().map(String::as_str).collect();
    channel_ids.sort_unstable();
    channel_ids.dedup();
    let total_channels = channel_ids.len();
    channel_ids.truncate(MAX_STATUS_ROWS);
    let entries: Vec<Value> = sorted
        .iter()
        .take(MAX_STATUS_JARS)
        .map(|report| {
            json!({
                "jar": report.jar.display().to_string(),
                "loader": report.loader.map_or("unknown", JavaLoader::name),
                "engine": report.engine.name(),
                "backend": backend_name(report.engine),
                "weave": weave_name(report.engine),
                "entrypoints": report.entrypoints,
                "registrations": report.registrations,
                "opaque": report.opaque,
                "isolation_failure": report.isolation_failure.as_deref().map(bounded_error),
            })
        })
        .collect();
    json!({
        "jars": sorted.len(),
        "listed": entries.len(),
        "isolated": sorted.iter().filter(|r| r.isolation_failure.is_some()).count(),
        "engines": engines,
        "backends": backends,
        "weave": weave,
        "entrypoints": count(|r| r.entrypoints),
        "registrations": count(|r| r.registrations),
        "opaque": count(|r| r.opaque),
        "payload_channels": channel_ids,
        "payload_channels_total": total_channels,
        "entries": entries,
    })
}

/// The `fml` section of `server-status`: the last opt-in FancyModLoader
/// preflight — its role, the jars offered to it, FML's own mod count and ids,
/// why it failed and whether the environment asked for it to be strict. The
/// keys are fixed: a server that never ran one answers `not-run` with empty
/// fields, which is not the same as a failed preflight.
fn fml_section(report: Option<&fml::FmlReport>, strict: bool) -> Value {
    let Some(report) = report else {
        return json!({
            "status": "not-run",
            "role": null,
            "jars": 0,
            "mods": null,
            "ids": [],
            "error": null,
            "strict": strict,
        });
    };
    let mut ids = report.mod_ids.clone();
    ids.sort();
    ids.dedup();
    ids.truncate(MAX_FML_IDS);
    json!({
        "status": if report.succeeded() { "ok" } else { "failed" },
        "role": role_name(report.role),
        "jars": report.jars,
        "mods": report.mods,
        "ids": ids,
        "error": report.failure.as_deref().map(bounded_error),
        "strict": strict,
    })
}

/// A failure reason clipped for the status reply: control characters become
/// spaces and only [`MAX_STATUS_ERROR_CHARS`] characters are kept.
fn bounded_error(failure: &str) -> String {
    let mut bounded: String = failure
        .chars()
        .take(MAX_STATUS_ERROR_CHARS)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if failure.chars().count() > MAX_STATUS_ERROR_CHARS {
        bounded.push_str("...");
    }
    bounded
}

impl ServerState {
    /// One pass of the server loop ended.
    pub(super) fn ticked(&mut self, at: Instant) {
        self.ticks.push_back(at);
        while self
            .ticks
            .front()
            .is_some_and(|t| at.duration_since(*t) > TPS_WINDOW)
        {
            self.ticks.pop_front();
        }
    }
    /// Ticks per second over the last few seconds.
    fn tps(&self) -> f64 {
        match (self.ticks.front(), self.ticks.back()) {
            (Some(first), Some(last)) if self.ticks.len() > 1 => {
                (self.ticks.len() - 1) as f64 / last.duration_since(*first).as_secs_f64().max(1e-9)
            }
            _ => 0.0,
        }
    }
}

impl Game<'_> {
    /// Answers the requests waiting on the control endpoint.
    pub(super) fn poll_server_control(&mut self, state: &mut ServerState) {
        while let Some(request) = self.control.as_ref().and_then(|c| c.try_recv()) {
            let reply = match request.line.trim().split_once(' ') {
                _ if request.line.trim() == "server-status" => self.server_status(state),
                Some(("server-events", since)) => {
                    super::events::since(since.trim().parse().unwrap_or(0))
                }
                Some(("management", rest)) => self.management(state, rest),
                // A proxy moving a player here or away (transfer.rs, lan/arrivals.rs).
                Some((verb, rest)) if verb.starts_with("player-") => {
                    self.player_request(verb, rest)
                }
                Some(("console", line)) => {
                    // Command feedback is logged as system chat; the reply carries the text alone.
                    let output: Vec<String> = capture_log(|| self.console_command(state, line))
                        .into_iter()
                        .map(|l| {
                            l.strip_prefix("System chat: ")
                                .map_or(l.clone(), str::to_owned)
                        })
                        .collect();
                    json!({ "output": output })
                }
                // An RCON command: run as the console, with the `Rcon` source, whose feedback the
                // operators are told of when `broadcast-rcon-to-ops` is on.
                Some(("rcon", line)) => {
                    let output: Vec<String> = capture_log(|| self.rcon_line(state, line))
                        .into_iter()
                        .map(|l| {
                            l.strip_prefix("System chat: ")
                                .map_or(l.clone(), str::to_owned)
                        })
                        .collect();
                    json!({ "output": output })
                }
                _ => json!({ "error": format!("unknown request {:?}", request.line) }),
            };
            let _ = request.reply.send(reply.to_string());
        }
    }

    /// Each level's loaded chunks and entities (players and block entities apart), the level in place
    /// first: what the Prometheus text reports by dimension.
    fn status_levels(&self) -> Value {
        let mut levels = vec![json!({
            "dimension": self.dim.id(),
            "chunks": self.world.chunks.len(),
            "entities": self.entity_count(),
        })];
        for dim in self.levels.parked_dims() {
            if let (Some(world), Some(entities)) = (
                self.levels.parked_world(dim),
                self.levels.parked_entity_count(dim),
            ) {
                levels.push(json!({
                    "dimension": dim.id(),
                    "chunks": world.chunks.len(),
                    "entities": entities,
                }));
            }
        }
        Value::Array(levels)
    }

    /// The entities of the level in place: mobs, items, orbs, projectiles, vehicles, falling blocks,
    /// hangings, armor stands, cushions, tridents, rockets and primed TNT.
    fn entity_count(&self) -> usize {
        self.mobs.iter().filter(|m| !m.removed).count()
            + self.dropped.len()
            + self.orbs.len()
            + self.creatures.projectiles.len()
            + self.carts.len()
            + self.boats.len()
            + self.falling.len()
            + self.paintings.len()
            + self.item_frames.len()
            + self.armor_stands.len()
            + self.cushions.len()
            + self.tridents.len()
            + self.rockets.len()
            + self.tnts.len()
    }

    fn server_status(&self, state: &ServerState) -> Value {
        let millis = |nanos: u64| nanos as f64 / 1e6;
        let samples = self.clock.sorted_samples();
        // The percentiles `/tick query` prints (TickCommand.percentileToString).
        let at = |q: f64| {
            let i = ((samples.len() as f64 * q) as usize).min(samples.len().saturating_sub(1));
            samples.get(i).copied().map_or(0.0, millis)
        };
        let (players, max_players) = self.lan.as_ref().map_or((Vec::new(), 0), |h| {
            let players = h
                .hub
                .guests
                .iter()
                .map(|g| {
                    player_status(
                        &g.name,
                        g.uuid,
                        &g.address.ip().to_string(),
                        g.latency.load(Ordering::Relaxed),
                        &g.dimension,
                        g.position,
                    )
                })
                .collect();
            (players, h.hub.lan.shared.max_players())
        });
        json!({
            "tps": state.tps(),
            "target_tps": self.clock.rate(),
            "mspt": {
                "average": millis(self.clock.average_nanos()),
                "p50": at(0.5),
                "p95": at(0.95),
                "p99": at(0.99),
            },
            "players": players,
            "max_players": max_players,
            "chunks": self.world.chunks.len(),
            "levels": self.status_levels(),
            "paused": state.paused,
            // Additive: a reader written before this key existed reads the
            // fields above exactly as it did.
            "java": java_status(),
            "fml": fml_section(fml::last_report().as_ref(), fml::strict_requested()),
        })
    }
}

fn player_status(
    name: &str,
    uuid: u128,
    address: &str,
    ping_ms: i32,
    dimension: &str,
    position: [f64; 3],
) -> Value {
    json!({
        "name": name,
        "uuid": horizon_protocol::crypto::uuid_string(uuid),
        "address": address,
        "ping_ms": ping_ms,
        "dimension": dimension,
        "position": position,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_FML_IDS, MAX_STATUS_ERROR_CHARS, MAX_STATUS_JARS, MAX_STATUS_ROWS, fml_section,
        java_section, java_status, player_status,
    };
    use crate::game::execution_role::ExecutionRole;
    use crate::game::java_mods::fml;
    use crate::game::java_mods::{
        JavaCompatibilityReport, JavaEngine, JavaLoader, record_java_reports,
    };
    use serde_json::{Value, json};
    use std::path::PathBuf;

    /// A compatibility report built in memory from the fields the status
    /// section reads, so the rows are testable without compiling a jar or
    /// starting a loader.
    fn java_report(
        jar: &str,
        engine: JavaEngine,
        entrypoints: usize,
        registrations: usize,
        opaque: usize,
        isolation_failure: Option<&str>,
    ) -> JavaCompatibilityReport {
        JavaCompatibilityReport {
            jar: PathBuf::from(jar),
            loader: Some(JavaLoader::Fabric),
            engine,
            entrypoints,
            registrations,
            opaque,
            isolation_failure: isolation_failure.map(str::to_owned),
        }
    }

    #[test]
    fn player_status_includes_coordinates() {
        assert_eq!(
            player_status(
                "Alex",
                1,
                "203.0.113.7",
                42,
                "minecraft:the_nether",
                [1.5, 64.0, -2.25]
            ),
            json!({
                "name": "Alex",
                "uuid": "00000000-0000-0000-0000-000000000001",
                "address": "203.0.113.7",
                "ping_ms": 42,
                "dimension": "minecraft:the_nether",
                "position": [1.5, 64.0, -2.25],
            })
        );
    }

    /// One started jar fills one bounded row, and every count is its own.
    #[test]
    fn one_jar_fills_the_bounded_java_rows() {
        let report = java_report(
            "/mods/demo.jar",
            JavaEngine::NativeQualified,
            2,
            5,
            1,
            Some("panic\nwhile invoking entrypoint"),
        );
        let channels = [
            "demo:ping".to_owned(),
            "demo:pong".to_owned(),
            "demo:ping".to_owned(),
        ];
        let section = java_section(&[report], &channels);

        assert_eq!(section["jars"], 1);
        assert_eq!(section["listed"], 1);
        assert_eq!(section["isolated"], 1);
        // Fixed label sets, so a reader never has to guess at a shape.
        assert_eq!(
            section["engines"],
            json!({"native": 0, "native-qualified": 1, "refused": 0, "sidecar": 0})
        );
        assert_eq!(
            section["backends"],
            json!({"native": 1, "jvm": 0, "none": 0})
        );
        assert_eq!(
            section["weave"],
            json!({"applied": 0, "qualified": 1, "none": 0})
        );
        assert_eq!(section["entrypoints"], 2);
        assert_eq!(section["registrations"], 5);
        assert_eq!(section["opaque"], 1);
        // Repeated channels are one, in id order.
        assert_eq!(
            section["payload_channels"],
            json!(["demo:ping", "demo:pong"])
        );
        assert_eq!(section["payload_channels_total"], 2);

        let entry = &section["entries"][0];
        assert_eq!(entry["jar"], "/mods/demo.jar");
        assert_eq!(entry["loader"], "fabric");
        assert_eq!(entry["engine"], "native-qualified");
        assert_eq!(entry["backend"], "native");
        assert_eq!(entry["weave"], "qualified");
        assert_eq!(entry["entrypoints"], 2);
        assert_eq!(entry["registrations"], 5);
        assert_eq!(entry["opaque"], 1);
        // A failure reason is one bounded line.
        assert_eq!(
            entry["isolation_failure"],
            "panic while invoking entrypoint"
        );
    }

    /// A server that started no jar answers zeros and an empty list, not a
    /// shape a reader has to special-case.
    #[test]
    fn no_jars_answer_a_stable_empty_shape() {
        assert_eq!(
            java_section(&[], &[]),
            json!({
                "jars": 0,
                "listed": 0,
                "isolated": 0,
                "engines": {"native": 0, "native-qualified": 0, "refused": 0, "sidecar": 0},
                "backends": {"native": 0, "jvm": 0, "none": 0},
                "weave": {"applied": 0, "qualified": 0, "none": 0},
                "entrypoints": 0,
                "registrations": 0,
                "opaque": 0,
                "payload_channels": [],
                "payload_channels_total": 0,
                "entries": [],
            })
        );
    }

    /// The entry list is bounded; the counts are not, so a server with more
    /// jars than the listing still answers for all of them. Rows are sorted by
    /// jar, whatever order the runner reported them in.
    #[test]
    fn java_rows_are_sorted_and_the_counts_survive_the_entry_bound() {
        let reports: Vec<JavaCompatibilityReport> = (0..MAX_STATUS_JARS + 2)
            .rev()
            .map(|i| {
                java_report(
                    &format!("/mods/{i:02}.jar"),
                    JavaEngine::Native,
                    2,
                    3,
                    1,
                    None,
                )
            })
            .collect();
        let section = java_section(&reports, &[]);
        assert_eq!(section["jars"], json!(MAX_STATUS_JARS + 2));
        assert_eq!(section["listed"], json!(MAX_STATUS_JARS));
        assert_eq!(section["entrypoints"], json!(2 * (MAX_STATUS_JARS + 2)));
        assert_eq!(section["registrations"], json!(3 * (MAX_STATUS_JARS + 2)));
        assert_eq!(section["opaque"], json!(MAX_STATUS_JARS + 2));
        assert_eq!(section["backends"]["native"], json!(MAX_STATUS_JARS + 2));
        let entries = section["entries"].as_array().unwrap();
        assert_eq!(entries.len(), MAX_STATUS_JARS);
        assert_eq!(entries[0]["jar"], "/mods/00.jar");
        assert_eq!(entries[MAX_STATUS_JARS - 1]["jar"], "/mods/07.jar");
    }

    /// More channels than the row bound: the list is cut, the count is not.
    #[test]
    fn java_payload_channels_are_bounded_but_counted_whole() {
        let channels: Vec<String> = (0..MAX_STATUS_ROWS + 4)
            .rev()
            .map(|i| format!("demo:c{i:02}"))
            .collect();
        let section = java_section(&[], &channels);
        let listed = section["payload_channels"].as_array().unwrap();
        assert_eq!(listed.len(), MAX_STATUS_ROWS);
        assert_eq!(listed[0], "demo:c00");
        assert_eq!(section["payload_channels_total"], MAX_STATUS_ROWS + 4);
    }

    /// A jar the loader refused never claims native work, and its row says so.
    #[test]
    fn a_refused_jar_is_never_counted_as_native_work() {
        let report = java_report("/mods/refused.jar", JavaEngine::Refused, 0, 0, 0, None);
        let section = java_section(&[report], &[]);
        assert_eq!(
            section["backends"],
            json!({"native": 0, "jvm": 0, "none": 1})
        );
        assert_eq!(
            section["weave"],
            json!({"applied": 0, "qualified": 0, "none": 1})
        );
        assert_eq!(section["entries"][0]["backend"], "none");
        assert_eq!(section["entries"][0]["weave"], "none");
        assert_eq!(section["entries"][0]["isolation_failure"], Value::Null);
    }

    /// A failure reason from a report that was not built here is still cut to
    /// the bound the section documents.
    #[test]
    fn a_long_isolation_failure_stays_one_bounded_line() {
        let failure = format!("\t{}\nsecond line", "x".repeat(MAX_STATUS_ERROR_CHARS + 40));
        let report = java_report(
            "/mods/demo.jar",
            JavaEngine::Native,
            1,
            0,
            0,
            Some(&failure),
        );
        let section = java_section(&[report], &[]);
        let line = section["entries"][0]["isolation_failure"].as_str().unwrap();
        assert_eq!(line.chars().count(), MAX_STATUS_ERROR_CHARS + 3);
        assert!(line.starts_with(" xxx"), "{line}");
        assert!(line.ends_with("..."), "{line}");
        assert!(!line.chars().any(char::is_control), "{line}");
    }

    /// The new keys are additive: a reader written against the section as the
    /// old server sent it finds every field it knows, unchanged, after a wire
    /// round trip that drops the keys it does not.
    #[test]
    fn old_java_keys_still_decode_after_a_round_trip() {
        let report = java_report("/mods/a.jar", JavaEngine::Native, 1, 2, 0, None);
        let section = java_section(&[report], &["demo:ping".to_owned()]);
        let mut wire: Value = serde_json::from_str(&section.to_string()).unwrap();

        let object = wire.as_object_mut().unwrap();
        for key in [
            "weave",
            "entrypoints",
            "registrations",
            "payload_channels",
            "payload_channels_total",
        ] {
            object.remove(key);
        }
        for entry in object["entries"].as_array_mut().unwrap() {
            let entry = entry.as_object_mut().unwrap();
            for key in ["weave", "entrypoints", "registrations"] {
                entry.remove(key);
            }
        }
        assert_eq!(
            wire,
            json!({
                "jars": 1,
                "listed": 1,
                "isolated": 0,
                "engines": {"native": 1, "native-qualified": 0, "refused": 0, "sidecar": 0},
                "backends": {"native": 1, "jvm": 0, "none": 0},
                "opaque": 0,
                "entries": [{
                    "jar": "/mods/a.jar",
                    "loader": "fabric",
                    "engine": "native",
                    "backend": "native",
                    "opaque": 0,
                    "isolation_failure": null,
                }],
            })
        );
    }

    /// The runner's batch reaches the status key: the loader hands the rows
    /// over, and the answer changes with it. Both roles record into the one
    /// process-wide store, so the Java tests take turns.
    #[test]
    fn the_recorded_batch_answers_the_status_key() {
        let _guard = crate::game::java_mods::test_lock();
        record_java_reports(&[java_report(
            "/mods/recorded.jar",
            JavaEngine::Native,
            1,
            2,
            0,
            None,
        )]);
        let section = java_status();
        assert_eq!(section["jars"], 1);
        assert_eq!(section["entries"][0]["jar"], "/mods/recorded.jar");
        assert_eq!(section["entrypoints"], 1);

        record_java_reports(&[]);
        assert_eq!(java_status()["jars"], 0);
        assert_eq!(java_status()["entries"], json!([]));
    }

    /// A preflight built in memory with just the fields the section reads.
    fn fml_report(
        jars: usize,
        mods: Option<usize>,
        mod_ids: &[&str],
        failure: Option<&str>,
    ) -> fml::FmlReport {
        fml::FmlReport {
            role: ExecutionRole::DedicatedServer,
            jars,
            mods,
            mod_ids: mod_ids.iter().map(|id| (*id).to_owned()).collect(),
            failure: failure.map(str::to_owned),
        }
    }

    /// A completed preflight says ok with FML's own counts, sorted and
    /// deduplicated whatever order the probe printed.
    #[test]
    fn a_completed_preflight_says_ok_with_its_counts() {
        let report = fml_report(2, Some(3), &["beta", "alpha", "alpha"], None);
        let section = fml_section(Some(&report), false);
        assert_eq!(
            section,
            json!({
                "status": "ok",
                "role": "dedicated-server",
                "jars": 2,
                "mods": 3,
                "ids": ["alpha", "beta"],
                "error": null,
                "strict": false,
            })
        );
        // Fixed keys, so a status reader never has to guess at a shape.
        assert_eq!(
            section.to_string(),
            "{\"error\":null,\"ids\":[\"alpha\",\"beta\"],\"jars\":2,\"mods\":3,\
             \"role\":\"dedicated-server\",\"status\":\"ok\",\"strict\":false}"
        );
    }

    /// A failed preflight says failed, keeps no made-up count, and bounds both
    /// its id list and its reason.
    #[test]
    fn a_failed_preflight_says_failed_and_bounds_its_ids_and_error() {
        let ids: Vec<String> = (0..MAX_FML_IDS + 8)
            .rev()
            .map(|i| format!("m{i:02}"))
            .collect();
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        let failure = format!("\t{}\nsecond line", "x".repeat(MAX_STATUS_ERROR_CHARS + 40));
        let report = fml_report(1, None, &ids, Some(&failure));
        let section = fml_section(Some(&report), true);
        assert_eq!(section["status"], "failed");
        assert_eq!(section["role"], "dedicated-server");
        assert_eq!(section["jars"], 1);
        // No count from FML: null, not a made-up zero.
        assert_eq!(section["mods"], json!(null));
        assert_eq!(section["strict"], true);
        let listed = section["ids"].as_array().unwrap();
        assert_eq!(listed.len(), MAX_FML_IDS);
        assert_eq!(listed[0], "m00");
        assert_eq!(listed[MAX_FML_IDS - 1], "m15");
        let error = section["error"].as_str().unwrap();
        assert_eq!(error.chars().count(), MAX_STATUS_ERROR_CHARS + 3);
        assert!(error.starts_with(" xxx"), "{error}");
        assert!(error.ends_with("..."), "{error}");
        assert!(!error.chars().any(char::is_control), "{error}");
    }

    /// A server that never ran the preflight answers `not-run`, which is not a
    /// failure, and still says whether the environment asked for strictness.
    #[test]
    fn no_preflight_answers_not_run_with_empty_fields() {
        assert_eq!(
            fml_section(None, false),
            json!({
                "status": "not-run",
                "role": null,
                "jars": 0,
                "mods": null,
                "ids": [],
                "error": null,
                "strict": false,
            })
        );
        assert_eq!(fml_section(None, true)["strict"], true);
    }
}
