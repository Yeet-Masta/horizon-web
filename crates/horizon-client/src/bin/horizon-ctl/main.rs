//! horizon-ctl: play and script a running Horizon client through its control
//! socket, from a shell, a script or an AI agent (`horizon-ctl mcp`).
#![forbid(unsafe_code)]
extern crate horizon_client_lib as horizon_client;
mod commands;
mod launch;
mod mcp;

use std::{io::{BufRead, BufReader, Write}, path::{Path, PathBuf}, process::ExitCode};
use horizon_platform::{thread, time::{Duration, Instant}};

use horizon_client::control::Stream;
use serde_json::{Value, json};

/// Exit statuses.
const GAME_ERROR: u8 = 1;
const USAGE: u8 = 2;
const UNREACHABLE: u8 = 3;
/// No AI is on (`horizon_core::no_ai`): agent control is off.
const DENIED: u8 = 4;
/// `--api LINE`: the game speaks another protocol line.
const API_MISMATCH: u8 = 5;

/// A failure, with the exit status it maps to.
#[derive(Debug)]
pub struct Failure {
    pub reply: Value,
    pub status: u8,
}
impl Failure {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            reply: json!({ "error": message.into(), "kind": "usage" }),
            status: USAGE,
        }
    }
    /// No AI is on in the game's home.
    fn denied() -> Self {
        Self {
            reply: json!({ "error": horizon_core::no_ai::REFUSAL, "kind": "denied" }),
            status: DENIED,
        }
    }
    fn unreachable(message: impl Into<String>) -> Self {
        Self {
            reply: json!({ "error": message.into(), "kind": "connection" }),
            status: UNREACHABLE,
        }
    }
}

pub struct Client {
    writer: Stream,
    reader: BufReader<Stream>,
}

impl Client {
    fn connect(path: &Path, timeout: Option<Duration>) -> Result<Self, Failure> {
        let stream = horizon_client::control::connect(path).map_err(|e| {
            Failure::unreachable(format!(
                "cannot connect to {} ({e}); is the game running with --control? Start one with: horizon-ctl launch --world NAME",
                path.display()
            ))
        })?;
        stream
            .set_read_timeout(timeout)
            .map_err(|e| Failure::unreachable(e.to_string()))?;
        let reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| Failure::unreachable(e.to_string()))?,
        );
        Ok(Self {
            writer: stream,
            reader,
        })
    }
    /// Send one line and read its JSON reply.
    fn send(&mut self, line: &str) -> Result<Value, Failure> {
        writeln!(self.writer, "{line}").map_err(|e| Failure::unreachable(e.to_string()))?;
        let mut reply = String::new();
        let n = self.reader.read_line(&mut reply).map_err(|e| {
            Failure::unreachable(
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) {
                    "timed out waiting for the game's reply (--timeout)".to_owned()
                } else {
                    e.to_string()
                },
            )
        })?;
        if n == 0 {
            return Err(Failure::unreachable("the game closed the connection"));
        }
        serde_json::from_str(reply.trim_end()).map_err(|_| Failure {
            reply: json!({ "error": "the game sent a reply that is not JSON", "reply": reply.trim_end() }),
            status: GAME_ERROR,
        })
    }
}

/// Whether a game's protocol (`MAJOR.MINOR.PATCH`) is on the requested line (`MAJOR.MINOR`, or a full version):
/// every part the line names must match. Before 1.0 a patch only adds, so `0.1` takes `0.1.0` and `0.1.4`.
fn on_line(protocol: &str, line: &str) -> bool {
    let game: Vec<&str> = protocol.split('.').collect();
    let want: Vec<&str> = line.split('.').collect();
    want.len() <= game.len() && want.iter().zip(&game).all(|(w, g)| w == g)
}

/// Ask the game `hello` and refuse it unless it speaks `line`. A game without the command
/// (older than protocol 0.1.0) has no protocol line and is refused too.
fn check_api(client: &mut Client, line: &str) -> Result<(), Failure> {
    let refuse = |why: String| Failure {
        reply: json!({ "error": format!("horizon-ctl --api {line}: {why}"), "kind": "api", "wanted": line }),
        status: API_MISMATCH,
    };
    let reply = client.send("hello")?;
    match reply.get("protocol").and_then(Value::as_str) {
        Some(p) if on_line(p, line) => Ok(()),
        Some(p) => Err(refuse(format!(
            "the game speaks control protocol {p}, not the {line} line; use a horizon-ctl built for it (docs/api/agent-control.md)"
        ))),
        None => Err(refuse(
            "the game does not report a control protocol (it predates hello); update the game or drop --api".to_owned(),
        )),
    }
}

/// Global options and the connection, opened on first use.
pub struct Session {
    pub socket: PathBuf,
    pub timeout: Option<Duration>,
    /// The game's home, where No AI is read (`horizon_core::no_ai`).
    pub home: PathBuf,
    /// `--api LINE`: the protocol line this run speaks; a game on another line is refused.
    pub api: Option<String>,
    client: Option<Client>,
}

impl Session {
    pub fn new(socket: PathBuf, timeout: Option<Duration>) -> Self {
        Self {
            socket,
            timeout,
            home: horizon_client::logging::game_home(),
            api: None,
            client: None,
        }
    }
    /// A failure while No AI is on: every command refuses (help aside).
    pub fn allowed(&self) -> Result<(), Failure> {
        horizon_core::no_ai::check(&self.home).map_err(|_| Failure::denied())
    }
    fn client(&mut self) -> Result<&mut Client, Failure> {
        if self.client.is_none() {
            let mut client = Client::connect(&self.socket, self.timeout)?;
            if let Some(line) = &self.api {
                check_api(&mut client, line)?;
            }
            self.client = Some(client);
        }
        Ok(self.client.as_mut().unwrap())
    }
    /// Send a line to the game; a reply with "error" is a failure.
    pub fn game(&mut self, line: &str) -> Result<Value, Failure> {
        let result = self.client()?.send(line);
        if result.as_ref().is_err_and(|f| f.status == UNREACHABLE) {
            // Reconnect next time (the game may have restarted).
            self.client = None;
        }
        let reply = result?;
        if reply.get("error").is_some() {
            return Err(Failure {
                reply,
                status: GAME_ERROR,
            });
        }
        Ok(reply)
    }

    /// Run one command (CLI words); Ok(reply) or a failure.
    pub fn run(&mut self, words: &[String]) -> Result<Value, Failure> {
        let Some(name) = words.first().map(String::as_str) else {
            return Err(Failure::usage("no command; try horizon-ctl help"));
        };
        self.allowed()?;
        if words.iter().skip(1).any(|w| w == "--help" || w == "-h") {
            return Err(Failure::usage(
                "use horizon-ctl COMMAND --help outside scripts",
            ));
        }
        if name == "payload" {
            return payload_command(self, words);
        }
        let number = |i: usize, what: &str| -> Result<f64, Failure> {
            words
                .get(i)
                .ok_or_else(|| Failure::usage(format!("missing {what}; usage: {}", usage(name))))?
                .parse::<f64>()
                .map_err(|_| {
                    Failure::usage(format!("{what} must be a number; usage: {}", usage(name)))
                })
        };
        match name {
            "sleep" => {
                thread::sleep(Duration::from_secs_f64(number(1, "SECONDS")?.max(0.0)));
                Ok(json!({ "ok": true }))
            }
            "launch" => {
                let socket = self.socket.clone();
                let wait = words.iter().any(|w| w == "--wait" || w == "--wait-ready");
                let reply = launch::launch(&socket, &words[1..]).map_err(|reply| Failure {
                    status: if reply["kind"] == "usage" {
                        USAGE
                    } else {
                        UNREACHABLE
                    },
                    reply,
                })?;
                if !wait {
                    return Ok(reply);
                }
                let mut ready = self.wait_ready(300.0)?;
                ready["launch"] = reply;
                Ok(ready)
            }
            "interact-entity" => {
                let id = match words {
                    [_, id] if id.parse::<i64>().is_ok() => id,
                    _ => return Err(Failure::usage(format!("usage: {}", usage(name)))),
                };
                self.game(&format!("look-at entity {id}"))?;
                self.game("mouse right click")?;
                self.game("wait 2")?;
                let mut screen = self.game("screen")?;
                screen["ok"] = json!(true);
                Ok(screen)
            }
            "hold" => {
                let (key, seconds) = match words {
                    [_, key, _] => (key.clone(), number(2, "SECONDS")?),
                    _ => return Err(Failure::usage(format!("usage: {}", usage(name)))),
                };
                self.game(&format!("key {key} down"))?;
                thread::sleep(Duration::from_secs_f64(seconds.max(0.0)));
                self.game(&format!("key {key} up"))
            }
            "logs" => logs(&self.home, &words[1..]),
            "wait-ready" => {
                let limit = if words.len() > 1 {
                    number(1, "SECONDS")?
                } else {
                    120.0
                };
                self.wait_ready(limit)
            }
            "help" if words.get(1).is_some_and(|w| w == "--game") => self.game("help"),
            _ => {
                let mut words = words.to_vec();
                if let Some(cmd) = commands::find(name) {
                    if cmd.local {
                        return Err(Failure::usage(format!(
                            "{name} runs only as horizon-ctl {name}, not inside a script"
                        )));
                    }
                    if let Some(wire) = cmd.wire {
                        words.splice(0..1, wire.split(' ').map(str::to_owned));
                    }
                }
                absolute_paths(&mut words);
                self.game(&words.join(" "))
            }
        }
    }

    fn wait_ready(&mut self, limit: f64) -> Result<Value, Failure> {
        let deadline = Instant::now() + Duration::from_secs_f64(limit.max(0.0));
        loop {
            let status = self.game("status")?;
            // On a server (single player's server thread too), also the
            // level around the player.
            if !status["player"].is_null()
                && status["pending_chunks"] == 0
                && status["server"]["loaded"] != false
            {
                return Ok(status);
            }
            if Instant::now() > deadline {
                return Err(Failure {
                    reply: json!({ "error": format!("not ready after {limit} seconds"), "status": status }),
                    status: GAME_ERROR,
                });
            }
            thread::sleep(Duration::from_millis(250));
        }
    }
}

/// `logs [--lines N] [--crash]` for the game in `home`: the reply's `text` is the log's last
/// lines or the report.
fn logs(home: &Path, words: &[String]) -> Result<Value, Failure> {
    use horizon_client::logging;
    let mut lines = 100;
    let mut crash = false;
    let mut it = words.iter();
    while let Some(word) = it.next() {
        match word.as_str() {
            "--crash" => crash = true,
            "--lines" => {
                lines = it
                    .next()
                    .and_then(|n| n.parse::<usize>().ok())
                    .filter(|&n| n > 0)
                    .ok_or_else(|| Failure::usage("--lines takes a number above 0"))?;
            }
            other => {
                return Err(Failure::usage(format!(
                    "unknown argument {other}; usage: {}",
                    usage("logs")
                )));
            }
        }
    }
    let missing = |what: String| Failure {
        reply: json!({ "error": what }),
        status: GAME_ERROR,
    };
    if crash {
        let path = logging::newest_crash_report(home).ok_or_else(|| {
            missing(format!(
                "no crash report in {}",
                logging::crash_reports_dir(home).display()
            ))
        })?;
        let text = horizon_platform::fs::read_to_string(&path)
            .map_err(|e| missing(format!("{}: {e}", path.display())))?;
        return Ok(json!({ "path": path, "text": text }));
    }
    let path = logging::logs_dir(home).join("latest.log");
    let tail =
        logging::tail(&path, lines).map_err(|e| missing(format!("{}: {e}", path.display())))?;
    Ok(json!({ "path": path, "text": tail.join("\n") + "\n" }))
}

fn usage(name: &str) -> String {
    commands::find(name).map_or_else(|| name.to_owned(), |c| format!("horizon-ctl {}", c.usage))
}

/// A control payload channel id's bytes at most, and the bytes one frame may
/// carry: the transport's bounds (`game/java_payloads.rs`). Checked here too so
/// a typo fails as a usage error before the socket, and so a huge argument is
/// refused before it is hex-encoded.
const MAX_PAYLOAD_CHANNEL: usize = 256;
const MAX_PAYLOAD_BYTES: usize = 1 << 20;

/// `payload send CHANNEL HEX|utf8:TEXT client|server` as the game's command
/// line. The payload always travels as hex, so a `utf8:` text with spaces
/// stays one argument.
fn payload_line(words: &[String]) -> Result<String, Failure> {
    let usage = "usage: horizon-ctl payload send CHANNEL HEX|utf8:TEXT client|server";
    let [_, action, channel, value, role] = words else {
        return Err(Failure::usage(usage));
    };
    if action != "send" {
        return Err(Failure::usage(usage));
    }
    if channel.is_empty() || channel.len() > MAX_PAYLOAD_CHANNEL {
        return Err(Failure::usage(format!(
            "CHANNEL must be 1..{MAX_PAYLOAD_CHANNEL} bytes"
        )));
    }
    if role != "client" && role != "server" {
        return Err(Failure::usage("ROLE must be client or server"));
    }
    let bytes = if let Some(text) = value.strip_prefix("utf8:") {
        text.as_bytes().to_vec()
    } else {
        let hex = value.strip_prefix("hex:").unwrap_or(value);
        if !hex.len().is_multiple_of(2) {
            return Err(Failure::usage("HEX must have an even number of digits"));
        }
        let mut bytes = Vec::with_capacity(hex.len() / 2);
        for pair in hex.as_bytes().as_chunks::<2>().0 {
            let high = (pair[0] as char).to_digit(16);
            let low = (pair[1] as char).to_digit(16);
            let Some((high, low)) = high.zip(low) else {
                return Err(Failure::usage("HEX contains an invalid digit"));
            };
            bytes.push(((high << 4) | low) as u8);
        }
        bytes
    };
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return Err(Failure::usage(format!(
            "payload is {} bytes (max {MAX_PAYLOAD_BYTES})",
            bytes.len()
        )));
    }
    let encoded = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!("payload send {channel} {encoded} {role}"))
}

/// `payload send ...`: validate and normalize the arguments, then hand the
/// frame to the game (the LAN host or its integrated server sends it).
fn payload_command(session: &mut Session, words: &[String]) -> Result<Value, Failure> {
    session.game(&payload_line(words)?)
}

/// Paths are written by the game process: make them absolute here.
fn absolute_paths(words: &mut [String]) {
    let Ok(cwd) = horizon_platform::env::current_dir() else {
        return;
    };
    match words.first().map(String::as_str) {
        Some("screenshot") => {
            if let Some(path) = words.get_mut(1) {
                *path = cwd.join(&*path).display().to_string();
            }
        }
        // `repro-export [NAME] [PATH]`: the second positional is the zip.
        Some("repro-export") => {
            if let Some(path) = words.get_mut(2)
                && (path.contains('/') || path.ends_with(".zip"))
            {
                *path = cwd.join(&*path).display().to_string();
            }
        }
        _ => {}
    }
}

/// Split a script line like a shell: quotes keep spaces, backslash escapes.
pub fn split_line(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => word.push(chars.next().unwrap_or('\\')),
            (Some(_), c) => word.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                word.push(chars.next().unwrap_or('\\'));
                in_word = true;
            }
            (None, '#') if !in_word => break,
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            (None, c) => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return Err("unterminated quote".into());
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

fn print(value: &Value, pretty: bool) {
    let text = if pretty {
        serde_json::to_string_pretty(value).unwrap_or_default()
    } else {
        value.to_string()
    };
    println!("{text}");
}

/// Run a script; the exit status of the first failure (or the last one with --keep-going).
fn script(session: &mut Session, reader: impl BufRead, keep_going: bool, pretty: bool) -> u8 {
    let mut status = 0;
    for (n, line) in reader.lines().enumerate() {
        let Ok(line) = line else { break };
        let words = match split_line(&line) {
            Ok(w) if w.is_empty() => continue,
            Ok(w) => w,
            Err(e) => {
                print(
                    &json!({ "error": e, "kind": "usage", "line": n + 1 }),
                    pretty,
                );
                status = USAGE;
                if keep_going {
                    continue;
                }
                break;
            }
        };
        match session.run(&words) {
            Ok(reply) => print(&reply, pretty),
            Err(mut f) => {
                f.reply["line"] = json!(n + 1);
                print(&f.reply, pretty);
                status = f.status;
                if !keep_going || f.status == UNREACHABLE {
                    break;
                }
            }
        }
    }
    status
}

fn main() -> ExitCode {
    let mut args: Vec<String> = horizon_platform::env::args().skip(1).collect();
    let mut socket = horizon_client::control::default_path();
    let mut timeout = None;
    let mut pretty = false;
    let mut api: Option<String> = None;
    // Global options before the command.
    loop {
        match args.first().map(String::as_str) {
            Some("--socket") if args.len() >= 2 => {
                socket = PathBuf::from(args.remove(1));
                args.remove(0);
            }
            Some("--timeout") if args.len() >= 2 => {
                let Ok(s) = args[1].parse::<f64>() else {
                    print(
                        &json!({ "error": "--timeout takes seconds", "kind": "usage" }),
                        false,
                    );
                    return ExitCode::from(USAGE);
                };
                timeout = Some(Duration::from_secs_f64(s.max(0.001)));
                args.drain(0..2);
            }
            Some("--api") if args.len() >= 2 => {
                api = Some(args[1].clone());
                args.drain(0..2);
            }
            Some("--pretty") => {
                pretty = true;
                args.remove(0);
            }
            _ => break,
        }
    }
    if let Some(line) = &api {
        let valid = !line.is_empty()
            && line.split('.').count() <= 3
            && line
                .split('.')
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
        if !valid {
            print(
                &json!({ "error": "--api takes a protocol line such as 0.1", "kind": "usage" }),
                false,
            );
            return ExitCode::from(USAGE);
        }
    }
    let mut session = Session::new(socket, timeout);
    session.api = api;
    let name = args.first().cloned();
    let status = match name.as_deref() {
        None | Some("-h" | "--help") => {
            print!("{}", commands::overview());
            0
        }
        Some("help") if args.get(1).is_some_and(|a| a == "--markdown") => {
            print!("{}", commands::markdown());
            0
        }
        Some("help") if args.get(1).is_none_or(|a| a != "--game") => match args.get(1) {
            None => {
                print!("{}", commands::overview());
                0
            }
            Some(c) => match commands::find(c) {
                Some(cmd) => {
                    print!("{}", commands::detail(cmd));
                    0
                }
                None => {
                    print(
                        &json!({ "error": format!("unknown command {c}; horizon-ctl help lists them, horizon-ctl help --game lists the game's own"), "kind": "usage" }),
                        pretty,
                    );
                    USAGE
                }
            },
        },
        Some(c) if args.len() == 2 && matches!(args[1].as_str(), "--help" | "-h") => {
            match commands::find(c) {
                Some(cmd) => {
                    print!("{}", commands::detail(cmd));
                    0
                }
                None => {
                    print(
                        &json!({ "error": format!("no help for {c}; horizon-ctl help --game lists the game's own commands"), "kind": "usage" }),
                        pretty,
                    );
                    USAGE
                }
            }
        }
        Some("mcp") if session.allowed().is_err() => {
            eprintln!("horizon-ctl mcp: {}", horizon_core::no_ai::REFUSAL);
            DENIED
        }
        Some("mcp") => match mcp::serve(&mut session) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("horizon-ctl mcp: {e}");
                UNREACHABLE
            }
        },
        // The original form: stdin, keep going after errors.
        Some("-") => script(&mut session, std::io::stdin().lock(), true, pretty),
        Some("script") => {
            let keep_going = args.iter().any(|a| a == "--keep-going");
            match args.iter().skip(1).find(|a| !a.starts_with("--")) {
                Some(file) if file != "-" => match horizon_platform::fs::File::open(file) {
                    Ok(f) => script(&mut session, BufReader::new(f), keep_going, pretty),
                    Err(e) => {
                        print(
                            &json!({ "error": format!("{file}: {e}"), "kind": "usage" }),
                            pretty,
                        );
                        USAGE
                    }
                },
                _ => script(&mut session, std::io::stdin().lock(), keep_going, pretty),
            }
        }
        // Text for people; scripts and MCP get the JSON reply.
        Some("logs") => match session.run(&args) {
            Ok(reply) => {
                print!("{}", reply["text"].as_str().unwrap_or_default());
                0
            }
            Err(f) => {
                print(&f.reply, pretty);
                f.status
            }
        },
        Some(_) => match session.run(&args) {
            Ok(reply) => {
                print(&reply, pretty);
                0
            }
            Err(f) => {
                print(&f.reply, pretty);
                f.status
            }
        },
    };
    ExitCode::from(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_reads_the_end_of_latest_log_and_the_newest_crash_report() {
        let home = horizon_platform::env::temp_dir().join(format!("horizon-ctl-logs-{}", std::process::id()));
        let _ = horizon_platform::fs::remove_dir_all(&home);
        let words = |s: &str| -> Vec<String> { s.split_whitespace().map(String::from).collect() };
        let failure = logs(&home, &words("")).unwrap_err();
        assert!(
            failure.reply["error"]
                .as_str()
                .unwrap()
                .contains("latest.log")
        );
        horizon_platform::fs::create_dir_all(home.join("logs")).unwrap();
        let text: String = (1..=150)
            .map(|i| format!("[10:00:00] [Render thread/INFO]: line {i}\n"))
            .collect();
        horizon_platform::fs::write(home.join("logs/latest.log"), text).unwrap();
        let reply = logs(&home, &words("")).unwrap();
        let tail = reply["text"].as_str().unwrap();
        assert_eq!(tail.lines().count(), 100);
        assert!(tail.starts_with("[10:00:00] [Render thread/INFO]: line 51\n"));
        assert!(tail.ends_with("line 150\n"));
        let reply = logs(&home, &words("--lines 2")).unwrap();
        assert_eq!(
            reply["text"],
            "[10:00:00] [Render thread/INFO]: line 149\n[10:00:00] [Render thread/INFO]: line 150\n"
        );
        assert_eq!(logs(&home, &words("--lines 0")).unwrap_err().status, USAGE);
        assert_eq!(logs(&home, &words("--tail")).unwrap_err().status, USAGE);
        assert!(logs(&home, &words("--crash")).is_err());
        horizon_platform::fs::create_dir_all(home.join("crash-reports")).unwrap();
        horizon_platform::fs::write(
            home.join("crash-reports/crash-2026-10-13_10.00.00-client.txt"),
            "old",
        )
        .unwrap();
        horizon_platform::fs::write(
            home.join("crash-reports/crash-2026-10-14_10.00.00-client.txt"),
            "new",
        )
        .unwrap();
        let reply = logs(&home, &words("--crash")).unwrap();
        assert_eq!(reply["text"], "new");
        let _ = horizon_platform::fs::remove_dir_all(&home);
    }

    #[test]
    fn script_lines_split_like_a_shell() {
        assert_eq!(
            split_line("goto 1 2 3 --near 2").unwrap(),
            ["goto", "1", "2", "3", "--near", "2"]
        );
        assert_eq!(
            split_line("click-button \"Create New World\"").unwrap(),
            ["click-button", "Create New World"]
        );
        assert_eq!(
            split_line("type 'it''s'  # comment").unwrap(),
            ["type", "its"]
        );
        assert_eq!(split_line("chat a\\ b").unwrap(), ["chat", "a b"]);
        assert_eq!(split_line("chat issue#4").unwrap(), ["chat", "issue#4"]);
        assert!(split_line("   # only a comment").unwrap().is_empty());
        assert!(split_line("type \"open").is_err());
    }

    /// A fake game on a Unix socket: answers every line with `reply`, records the lines it got.
    #[cfg(unix)]
    fn fake_game(
        name: &str,
        reply: &'static str,
    ) -> (PathBuf, horizon_platform::thread::JoinHandle<Vec<String>>) {
        let path = horizon_platform::env::temp_dir().join(format!("hctl-{name}-{}.sock", std::process::id()));
        let _ = horizon_platform::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let handle = horizon_platform::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut lines = Vec::new();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut out = stream;
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                lines.push(line.trim_end().to_owned());
                line.clear();
                writeln!(out, "{reply}").unwrap();
            }
            lines
        });
        (path, handle)
    }

    #[cfg(unix)]
    fn api_session(path: &Path, api: &str) -> Session {
        let mut s = Session::new(path.to_owned(), Some(Duration::from_secs(5)));
        s.home = horizon_platform::env::temp_dir().join("horizon-ctl-api-no-home");
        s.api = Some(api.to_owned());
        s
    }

    #[test]
    fn api_line_matches_whole_parts() {
        assert!(on_line("0.1.0", "0.1"));
        assert!(on_line("0.1.4", "0.1"));
        assert!(on_line("0.1.0", "0.1.0"));
        assert!(!on_line("0.1.0", "0.1.1"));
        assert!(!on_line("0.2.0", "0.1"));
        assert!(!on_line("0.10.0", "0.1"));
        assert!(!on_line("1.0.0", "0"));
        assert!(!on_line("0.1", "0.1.0"));
    }

    #[cfg(unix)]
    #[test]
    fn api_accepts_a_game_on_the_line() {
        let (path, game) = fake_game(
            "match",
            r#"{"protocol":"0.1.0","release":"x","features":[]}"#,
        );
        let mut s = api_session(&path, "0.1");
        let reply = s.game("hello").unwrap();
        assert_eq!(reply["protocol"], "0.1.0");
        drop(s);
        // hello for the check, then the caller's own line.
        assert_eq!(game.join().unwrap(), ["hello", "hello"]);
        let _ = horizon_platform::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn api_refuses_a_game_on_another_line() {
        let (path, game) = fake_game(
            "mismatch",
            r#"{"protocol":"0.2.0","release":"x","features":[]}"#,
        );
        let mut s = api_session(&path, "0.1");
        let f = s.game("state").unwrap_err();
        assert_eq!(f.status, API_MISMATCH);
        assert_eq!(f.reply["kind"], "api");
        let message = f.reply["error"].as_str().unwrap();
        assert!(
            message.contains("0.2.0") && message.contains("0.1"),
            "{message}"
        );
        drop(s);
        // The refused command never reached the game.
        assert_eq!(game.join().unwrap(), ["hello"]);
        let _ = horizon_platform::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn api_refuses_a_game_with_no_protocol_line() {
        let (path, game) = fake_game("old", r#"{"error":"unknown command hello"}"#);
        let mut s = api_session(&path, "0.1");
        let f = s.game("state").unwrap_err();
        assert_eq!(f.status, API_MISMATCH);
        assert!(
            f.reply["error"]
                .as_str()
                .unwrap()
                .contains("predates hello")
        );
        drop(s);
        assert_eq!(game.join().unwrap(), ["hello"]);
        let _ = horizon_platform::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn without_api_the_game_is_not_asked_hello() {
        let (path, game) = fake_game("noapi", r#"{"ok":true}"#);
        let mut s = Session::new(path.clone(), Some(Duration::from_secs(5)));
        s.game("state").unwrap();
        drop(s);
        assert_eq!(game.join().unwrap(), ["state"]);
        let _ = horizon_platform::fs::remove_file(path);
    }

    #[test]
    fn unreachable_games_fail_with_status_3() {
        let home =
            horizon_platform::env::temp_dir().join(format!("horizon-ctl-unreachable-{}", std::process::id()));
        horizon_platform::fs::create_dir_all(&home).unwrap();
        let mut s = Session::new(home.join("nobody.sock"), None);
        s.home = home.clone();
        let f = s.run(&["state".to_owned()]).unwrap_err();
        assert_eq!(f.status, UNREACHABLE);
        assert_eq!(f.reply["kind"], "connection");
        let f = s.run(&["sleep".to_owned(), "x".to_owned()]).unwrap_err();
        assert_eq!(f.status, USAGE);
        horizon_platform::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn no_ai_refuses_every_command() {
        let home = horizon_platform::env::temp_dir().join(format!("horizon-ctl-no-ai-{}", std::process::id()));
        let _ = horizon_platform::fs::remove_dir_all(&home);
        let mut s = Session::new(horizon_platform::env::temp_dir().join("horizon-ctl-nobody.sock"), None);
        s.home = home.clone();
        assert_eq!(
            s.run(&["state".to_owned()]).unwrap_err().status,
            UNREACHABLE
        );
        horizon_core::no_ai::set(&home, true).unwrap();
        for words in [vec!["state"], vec!["logs"], vec!["launch", "--world", "A"]] {
            let words: Vec<String> = words.into_iter().map(str::to_owned).collect();
            let f = s.run(&words).unwrap_err();
            assert_eq!(f.status, DENIED, "{words:?}");
            assert_eq!(f.reply["kind"], "denied");
            assert_eq!(f.reply["error"], horizon_core::no_ai::REFUSAL);
        }
        let _ = horizon_platform::fs::remove_dir_all(&home);
    }

    #[test]
    fn screenshot_paths_become_absolute() {
        let mut words = vec!["screenshot".to_owned(), "a.png".to_owned()];
        absolute_paths(&mut words);
        assert!(Path::new(&words[1]).is_absolute());
    }

    #[test]
    fn payload_lines_normalize_the_payload_and_check_its_bounds() {
        let words = |s: &str| -> Vec<String> { s.split_whitespace().map(str::to_owned).collect() };
        assert_eq!(
            payload_line(&words("payload send demo:down utf8:hello client")).unwrap(),
            "payload send demo:down 68656c6c6f client"
        );
        assert_eq!(
            payload_line(&words("payload send demo:up hex:00ff server")).unwrap(),
            "payload send demo:up 00ff server"
        );
        // Without `hex:` the digits are still hex, and case is normalized.
        assert_eq!(
            payload_line(&words("payload send demo:x 0A0b client")).unwrap(),
            "payload send demo:x 0a0b client"
        );
        // A quoted text keeps its spaces: it is hex on the wire.
        assert_eq!(
            payload_line(&[
                "payload".to_owned(),
                "send".to_owned(),
                "demo:x".to_owned(),
                "utf8:hi there".to_owned(),
                "server".to_owned(),
            ])
            .unwrap(),
            "payload send demo:x 6869207468657265 server"
        );

        // The channel and the payload bounds are the transport's: 256 bytes
        // and 1 MiB are in, one byte more is out.
        let at_limit = "x".repeat(256);
        assert!(payload_line(&words(&format!("payload send {at_limit} 00 client"))).is_ok());
        let over = "x".repeat(257);
        assert_eq!(
            payload_line(&words(&format!("payload send {over} 00 client")))
                .unwrap_err()
                .status,
            USAGE
        );
        let one_mib = "00".repeat(1 << 20);
        assert!(payload_line(&words(&format!("payload send demo:x {one_mib} client"))).is_ok());
        let too_big = "00".repeat((1 << 20) + 1);
        assert_eq!(
            payload_line(&words(&format!("payload send demo:x {too_big} client")))
                .unwrap_err()
                .status,
            USAGE
        );

        for bad in [
            "payload send demo:x 0 client",
            "payload send demo:x gg client",
            "payload send demo:x 00 either",
            "payload take demo:x 00 client",
            "payload send demo:x 00",
        ] {
            assert_eq!(
                payload_line(&words(bad)).unwrap_err().status,
                USAGE,
                "{bad}"
            );
        }
    }
}
