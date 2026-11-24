//! The event stream (`GET /api/v1/events`, docs/api/server.md): every log line, the supervisor's phase, and
//! the game's typed events (players joining, leaving and kicked, chat, deaths), numbered in one sequence. A
//! client that asks for `text/event-stream` gets server-sent events as they happen, and resumes after a
//! reconnection from `Last-Event-ID`; any other gets the events after `since` as JSON.

use crate::{
    api::{Phase, Shared},
    engine,
    http::{Request, Response},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::Write,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

/// The events kept for a client that reconnects or polls.
const KEPT: usize = 2000;
/// How often the game's events are fetched.
const POLL: Duration = Duration::from_millis(250);
/// A comment this often keeps proxies from closing a quiet stream, and finds clients that left.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// The events, numbered from 1 since the supervisor started.
#[derive(Default)]
pub struct Events {
    kept: VecDeque<Value>,
    last: u64,
}

impl Events {
    /// The events numbered above `since` whose type `wanted` takes, and the newest number.
    pub(crate) fn after(&self, since: u64, wanted: &dyn Fn(&str) -> bool) -> (u64, Vec<Value>) {
        let skip = self
            .kept
            .len()
            .saturating_sub((self.last.saturating_sub(since)) as usize);
        let events = self
            .kept
            .iter()
            .skip(skip)
            .filter(|e| wanted(e["type"].as_str().unwrap_or_default()))
            .cloned()
            .collect();
        (self.last, events)
    }
}

impl Shared {
    /// Adds an event of type `kind` and wakes the streams.
    pub fn emit(&self, kind: &str, mut event: Value) {
        let mut events = self.events.lock().unwrap();
        events.last += 1;
        event["id"] = events.last.into();
        event["type"] = kind.into();
        if events.kept.len() == KEPT {
            events.kept.pop_front();
        }
        events.kept.push_back(event);
        drop(events);
        self.event_added.notify_all();
    }

    /// A log line, for `logs` and as a `log` event.
    pub fn log_line(&self, line: String) {
        self.emit("log", json!({ "line": line }));
        self.log.lock().unwrap().push(line);
    }
}

/// Fetches the game's typed events (`server-events` on its control endpoint) while it starts or runs.
pub fn poll_game(shared: Arc<Shared>) {
    let _ = thread::Builder::new().name("events".into()).spawn(move || {
        let mut poller = GamePoller::default();
        loop {
            thread::sleep(POLL);
            poller.poll_once(&shared, engine::REPLY_TIMEOUT);
        }
    });
}

#[derive(Default)]
struct GameEventCursor {
    pid: Option<u32>,
    since: u64,
    skip_startup_history: bool,
}

impl GameEventCursor {
    /// Returns true for a new engine process, whose event sequence starts over at one.
    fn select_pid(&mut self, pid: u32) -> bool {
        if self.pid == Some(pid) {
            return false;
        }
        self.pid = Some(pid);
        self.since = 0;
        self.skip_startup_history = true;
        true
    }
}

#[derive(Default)]
struct GamePoller {
    control: Option<(u32, engine::Control)>,
    cursor: GameEventCursor,
}

impl GamePoller {
    /// One poll pass. Keeping the cursor separate from the socket lets a timeout reconnect to the
    /// same engine without replaying events it already forwarded.
    fn poll_once(&mut self, shared: &Arc<Shared>, reply_timeout: Duration) {
        let (pid, pollable) = {
            let state = shared.state.lock().unwrap();
            (
                state.engine_pid,
                matches!(state.phase, Phase::Starting | Phase::Running),
            )
        };
        let Some(pid) = pid else {
            self.control = None;
            self.cursor = GameEventCursor::default();
            return;
        };
        if !pollable {
            self.control = None;
            return;
        }

        // Only a different process resets the cursor. A dropped connection to the same process
        // must retain it, or a slow startup can replay every event already delivered.
        if self.cursor.select_pid(pid) {
            self.control = None;
        }
        if self
            .control
            .as_ref()
            .is_none_or(|(connected_pid, _)| *connected_pid != pid)
        {
            self.control = engine::Control::connect(&shared.dir)
                .ok()
                .map(|control| (pid, control));
        }
        let Some((_, game)) = self.control.as_mut() else {
            return;
        };
        if game.set_reply_timeout(reply_timeout).is_err() {
            self.control = None;
            return;
        }
        let Ok(reply) = game.ask(&format!("server-events {}", self.cursor.since)) else {
            self.control = None;
            return;
        };

        // Keep this set across connection/auth failures. The first successful reply for a new
        // process is the only one whose pre-management save history should be suppressed.
        let skip_startup_history = self.cursor.skip_startup_history;
        self.cursor.skip_startup_history = false;
        for event in reply["events"].as_array().into_iter().flatten() {
            let mut event = event.clone();
            self.cursor.since = self
                .cursor
                .since
                .max(event["seq"].as_u64().unwrap_or(self.cursor.since));
            let kind = event["type"].as_str().unwrap_or("game").to_owned();
            if let Some(fields) = event.as_object_mut() {
                fields.remove("seq");
            }
            if !(skip_startup_history && matches!(kind.as_str(), "saving" | "saved")) {
                crate::management::game_event(shared, &kind, &event);
            }
            shared.emit(&kind, event);
        }
    }
}

/// `GET /api/v1/events[?since=N][&types=join,chat,...]`.
pub fn route(shared: &Arc<Shared>, request: &Request) -> Response {
    let types: Vec<String> = request
        .param("types")
        .map(|t| t.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    let wanted = move |kind: &str| types.is_empty() || types.iter().any(|t| t == kind);
    let since = request
        .param("since")
        .or_else(|| request.header("last-event-id"))
        .and_then(|s| s.parse().ok());
    let streamed = request
        .header("accept")
        .is_some_and(|a| a.contains("text/event-stream"));
    if !streamed {
        let (last, events) = shared
            .events
            .lock()
            .unwrap()
            .after(since.unwrap_or(0), &wanted);
        return Response::json(200, &json!({ "next": last, "events": events }));
    }
    // Without a number, the stream starts with the request: what happens while the reply's headers go out
    // is sent.
    let mut cursor = since.unwrap_or_else(|| shared.events.lock().unwrap().last);
    let shared = shared.clone();
    let mut response = Response::text(200, "text/event-stream", "");
    response.stream = Some(Box::new(move |mut stream| {
        // A client that stops reading would otherwise hold this thread in a write for good.
        let _ = stream.set_write_timeout(Some(KEEP_ALIVE * 2));
        let mut quiet_since = Instant::now();
        loop {
            let events = {
                let events = shared.events.lock().unwrap();
                let events = if events.last == cursor {
                    shared
                        .event_added
                        .wait_timeout(events, KEEP_ALIVE)
                        .unwrap()
                        .0
                } else {
                    events
                };
                let (last, after) = events.after(cursor, &wanted);
                cursor = last;
                after
            };
            let mut text = String::new();
            for event in &events {
                text += &format!(
                    "id: {}\nevent: {}\ndata: {event}\n\n",
                    event["id"],
                    event["type"].as_str().unwrap_or_default()
                );
            }
            if text.is_empty() && quiet_since.elapsed() >= KEEP_ALIVE {
                text = ": keep-alive\n\n".into();
            }
            if !text.is_empty() {
                quiet_since = Instant::now();
                if stream
                    .write_all(text.as_bytes())
                    .and_then(|()| stream.flush())
                    .is_err()
                {
                    return;
                }
            }
        }
    }));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        net::{TcpListener, TcpStream},
        sync::Mutex,
    };

    fn event_reply(response: Response) -> Value {
        assert!(response.stream.is_none());
        json!({
            "status": response.status,
            "content_type": response.content_type,
            "headers": response.headers,
            "body": serde_json::from_str::<Value>(&response.body).unwrap(),
        })
    }

    fn accept_control_client(listener: &TcpListener) -> (BufReader<TcpStream>, TcpStream) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let (stream, _) = loop {
            match listener.accept() {
                Ok(accepted) => break accepted,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("timed out accepting control client: {error}"),
            }
        };
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut auth = String::new();
        reader.read_line(&mut auth).unwrap();
        assert_eq!(auth.trim_end(), "auth t");
        let mut writer = stream;
        writeln!(writer, "{{\"ok\":true}}").unwrap();
        (reader, writer)
    }

    fn read_since(reader: &mut BufReader<TcpStream>) -> u64 {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        line.trim_end()
            .strip_prefix("server-events ")
            .unwrap()
            .parse()
            .unwrap()
    }

    fn write_events(writer: &mut TcpStream, events: &[Value]) -> std::io::Result<()> {
        writeln!(
            writer,
            "{}",
            json!({
                "next": events.last().and_then(|event| event["seq"].as_u64()).unwrap_or(0),
                "events": events,
            })
        )
    }

    #[test]
    fn reconnect_after_control_timeout_resumes_the_event_cursor() {
        let temp = std::env::temp_dir().join(format!(
            "horizon-server-events-reconnect-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&temp).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let path = horizon_server_launch::control_path(&temp);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            json!({
                "port": listener.local_addr().unwrap().port(),
                "token": "t",
            })
            .to_string(),
        )
        .unwrap();

        let all_events = vec![
            json!({ "seq": 1, "type": "world_upgrade_started" }),
            json!({ "seq": 2, "type": "world_upgrade_progress", "progress": 0.5 }),
            json!({ "seq": 3, "type": "world_upgrade_finished" }),
        ];
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen_requests = requests.clone();
        let server_events = all_events.clone();
        let server = thread::spawn(move || {
            {
                let (mut reader, mut writer) = accept_control_client(&listener);
                let first_since = read_since(&mut reader);
                seen_requests.lock().unwrap().push(first_since);
                write_events(&mut writer, &server_events[..2]).unwrap();

                let timed_out_since = read_since(&mut reader);
                seen_requests.lock().unwrap().push(timed_out_since);
                // Simulate a long startup operation outlasting the control reply timeout.
                thread::sleep(Duration::from_millis(180));
                let _ = write_events(&mut writer, &server_events);
            }

            let (mut reader, mut writer) = accept_control_client(&listener);
            let resumed_since = read_since(&mut reader);
            seen_requests.lock().unwrap().push(resumed_since);
            let pending: Vec<_> = server_events
                .iter()
                .filter(|event| event["seq"].as_u64().unwrap() > resumed_since)
                .cloned()
                .collect();
            write_events(&mut writer, &pending).unwrap();
        });

        let shared = Shared::with_install(&temp, None);
        {
            let mut state = shared.state.lock().unwrap();
            state.phase = Phase::Starting;
            state.engine_pid = Some(std::process::id());
        }

        let mut poller = GamePoller::default();
        poller.poll_once(&shared, Duration::from_secs(1));
        poller.poll_once(&shared, Duration::from_millis(40));
        poller.poll_once(&shared, Duration::from_secs(1));
        drop(poller);
        server.join().unwrap();

        assert_eq!(*requests.lock().unwrap(), [0, 2, 2]);
        let all = |_: &str| true;
        let (last, events) = shared.events.lock().unwrap().after(0, &all);
        assert_eq!(last, 3);
        assert_eq!(
            events
                .iter()
                .map(|event| event["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "world_upgrade_started",
                "world_upgrade_progress",
                "world_upgrade_finished",
            ]
        );
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn events_after_a_number_by_type() {
        let mut events = Events::default();
        for i in 0..KEPT + 5 {
            events.last += 1;
            if events.kept.len() == KEPT {
                events.kept.pop_front();
            }
            let kind = if i % 2 == 0 { "log" } else { "chat" };
            events
                .kept
                .push_back(json!({ "id": events.last, "type": kind }));
        }
        let all = |_: &str| true;
        let (last, after) = events.after(events.last - 3, &all);
        assert_eq!(last, (KEPT + 5) as u64);
        assert_eq!(
            after
                .iter()
                .map(|e| e["id"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [last - 2, last - 1, last]
        );
        // Asked from before the oldest kept: everything kept.
        assert_eq!(events.after(0, &all).1.len(), KEPT);
        let chat = |t: &str| t == "chat";
        assert!(
            events
                .after(last - 10, &chat)
                .1
                .iter()
                .all(|e| e["type"] == "chat")
        );
        assert!(events.after(last, &all).1.is_empty());
    }

    #[test]
    #[ignore = "daily"]
    fn a_stream_sends_what_happens() {
        use std::io::{BufRead, BufReader};
        let dir =
            std::env::temp_dir().join(format!("horizon-server-events-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shared = Shared::with_install(&dir, None);
        shared.log_line("before".into());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let served = shared.clone();
        crate::http::serve(listener, move |request| route(&served, request)).unwrap();
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        write!(
            stream,
            "GET /api/v1/events?types=join HTTP/1.1\r\nAccept: text/event-stream\r\n\r\n"
        )
        .unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        while line != "\r\n" {
            line.clear();
            reader.read_line(&mut line).unwrap();
        }
        shared.log_line("not a join".into());
        shared.emit("join", json!({ "name": "Kim0" }));
        let mut got = Vec::new();
        for _ in 0..3 {
            line.clear();
            reader.read_line(&mut line).unwrap();
            got.push(line.trim_end().to_owned());
        }
        assert_eq!(got[0], "id: 3");
        assert_eq!(got[1], "event: join");
        assert!(
            got[2].starts_with("data: {") && got[2].contains("\"name\":\"Kim0\""),
            "{got:?}"
        );
        // Polled as JSON: everything after the first.
        let polled = route(
            &shared,
            &Request {
                method: "GET".into(),
                path: "/api/v1/events".into(),
                query: "since=1".into(),
                headers: Vec::new(),
                body: Vec::new(),
            },
        );
        let polled: Value = serde_json::from_str(&polled.body).unwrap();
        assert_eq!(polled["next"], 3);
        assert_eq!(polled["events"][0]["line"], "not a join");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[ignore = "daily"]
    fn event_replies_match_api_v1_goldens() {
        use std::io::Read;

        let golden: Value =
            serde_json::from_str(include_str!("../testdata/api-v1-events.json")).unwrap();
        let shared = Shared::with_install(std::path::Path::new("unused"), None);
        shared.log_line("ready".into());
        shared.emit("join", json!({ "name": "Alex" }));
        shared.emit("chat", json!({ "name": "Alex", "message": "hi" }));
        let poll = |query: &str, headers: Vec<(String, String)>| {
            event_reply(route(
                &shared,
                &Request {
                    method: "GET".into(),
                    path: "/api/v1/events".into(),
                    query: query.into(),
                    headers,
                    body: Vec::new(),
                },
            ))
        };
        assert_eq!(poll("", Vec::new()), golden["all"]);
        assert_eq!(poll("since=2", Vec::new()), golden["since"]);
        assert_eq!(poll("types=join,leave", Vec::new()), golden["types"]);
        assert_eq!(
            poll("", vec![("Last-Event-ID".into(), "1".into())]),
            golden["last_event_id"]
        );
        assert_eq!(poll("since=not-a-number", Vec::new()), golden["bad_since"]);
        assert_eq!(poll("types=unknown", Vec::new()), golden["unknown_type"]);

        let mut response = route(
            &shared,
            &Request {
                method: "GET".into(),
                path: "/api/v1/events".into(),
                query: "types=join,chat".into(),
                headers: vec![
                    ("Accept".into(), "text/event-stream".into()),
                    ("Last-Event-ID".into(), "1".into()),
                ],
                body: Vec::new(),
            },
        );
        assert_eq!(response.status, golden["stream"]["status"]);
        assert_eq!(response.content_type, golden["stream"]["content_type"]);
        assert_eq!(response.headers, Vec::<(&str, String)>::new());
        let expected = golden["stream"]["body"].as_str().unwrap().as_bytes();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let control = server.try_clone().unwrap();
        let write = response.stream.take().unwrap();
        let writer = std::thread::spawn(move || write(&server));
        let mut body = vec![0; expected.len()];
        client.read_exact(&mut body).unwrap();
        assert_eq!(body, expected);
        control.shutdown(std::net::Shutdown::Both).unwrap();
        shared.emit("join", json!({ "name": "Wake" }));
        writer.join().unwrap();
    }
}
