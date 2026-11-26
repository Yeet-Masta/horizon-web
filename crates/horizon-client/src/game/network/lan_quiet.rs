//! Quiet commands (`horizon_protocol::quiet`): a Horizon client's mod sends a
//! batch of command lines in one payload, and the server runs each exactly as
//! that player typing it would (`guest_command`'s checks, the player's own
//! permission level, at the time it runs) with its output suppressed, as
//! vanilla's functions run (`CommandSourceStack.withSuppressedOutput`): no
//! feedback and no failure in anyone's chat. A batch waits in a queue and runs
//! [`PER_TICK`] lines a player a tick, so a mod can't flood a server. Each
//! payload counts as one command for the spam throttler (`detect_rate_spam`),
//! and finished batches reach the server's log as at most one line a second
//! for each player.
use super::{Game, lan_actions::GuestCommand};
use horizon_protocol::quiet;
use std::collections::{HashMap, VecDeque};

/// Lines a player may have waiting at most; a batch past it loses the rest.
pub(super) const PENDING: usize = 4096;
/// Lines run a player a tick at most.
pub(super) const PER_TICK: usize = 64;

/// A batch waiting to run.
#[derive(Default)]
pub(super) struct QuietBatch {
    conn: u32,
    mod_id: String,
    lines: VecDeque<String>,
    ran: usize,
    failed: usize,
    /// Lines past [`PENDING`], never run.
    dropped: usize,
}

impl Game<'_> {
    /// A `horizon:quiet_commands` payload from `conn`: queued (or logged as
    /// refused when it breaks the payload's caps).
    pub(super) fn guest_quiet_commands(&mut self, conn: u32, data: &[u8]) {
        let Some(host) = self.lan.as_mut() else {
            return;
        };
        let Some(name) = host.hub.guest(conn).map(|g| g.name.clone()) else {
            return;
        };
        let batch = match quiet::decode(data) {
            Ok(b) => b,
            Err(e) => {
                // At most one a second: a client sending refused payloads by the hundred a tick
                // must not fill the log.
                static LAST: horizon_std::sync::Mutex<Option<horizon_std::time::Instant>> =
                    horizon_std::sync::Mutex::new(None);
                let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
                if last.is_none_or(|t| t.elapsed() >= horizon_std::time::Duration::from_secs(1)) {
                    *last = Some(horizon_std::time::Instant::now());
                    super::dedicated::log(
                        "WARN",
                        &format!("{name} sent quiet commands that were refused: {e}"),
                    );
                }
                return;
            }
        };
        // Nothing to run, nothing to log: an empty batch would only be a summary line.
        if batch.lines.is_empty() {
            return;
        }
        // A payload counts as one command, as a chat command does: the lines of one payload
        // (at most `quiet::MAX_LINES`) are a mod's one burst, but payloads in a stream are
        // spam, and a guest who is not an operator is kicked as for chat commands.
        self.detect_rate_spam(conn, true);
        let Some(host) = self.lan.as_mut() else {
            return;
        };
        let waiting: usize = host
            .quiet
            .iter()
            .filter(|b| b.conn == conn)
            .map(|b| b.lines.len())
            .sum();
        let room = PENDING.saturating_sub(waiting);
        let dropped = batch.lines.len().saturating_sub(room);
        // No room at all: counted on the player's last waiting batch, not a new (logged) one.
        if room == 0
            && let Some(last) = host.quiet.iter_mut().rev().find(|b| b.conn == conn)
        {
            last.dropped += dropped;
            return;
        }
        host.quiet.push_back(QuietBatch {
            conn,
            mod_id: batch.mod_id,
            lines: batch.lines.into_iter().take(room).collect(),
            ran: 0,
            failed: 0,
            dropped,
        });
    }

    /// The waiting batches, [`PER_TICK`] lines a player this tick, in order.
    pub(super) fn run_quiet_commands(&mut self) {
        let mut left: HashMap<u32, usize> = HashMap::new();
        let mut i = 0;
        loop {
            let Some(host) = self.lan.as_mut() else {
                return;
            };
            let Some(batch) = host.quiet.get_mut(i) else {
                return;
            };
            let conn = batch.conn;
            let budget = left.entry(conn).or_insert(PER_TICK);
            let gone = host.hub.guest(conn).is_none();
            if gone || batch.lines.is_empty() {
                let batch = host.quiet.remove(i).expect("got it above");
                if !gone {
                    let name = host
                        .hub
                        .guest(conn)
                        .map_or(String::new(), |g| g.name.clone());
                    let log = &mut host.play.entry(conn).or_default().quiet_log;
                    if let Some(line) = log.finish(horizon_std::time::Instant::now(), &name, &batch)
                    {
                        super::dedicated::log("INFO", &line);
                    }
                }
                continue;
            }
            if *budget == 0 {
                i += 1;
                continue;
            }
            *budget -= 1;
            let line = batch.lines.pop_front().expect("not empty");
            let ok = self.run_quiet_line(conn, &line);
            if let Some(batch) = self.lan.as_mut().and_then(|h| h.quiet.get_mut(i)) {
                batch.ran += 1;
                batch.failed += usize::from(!ok);
            }
        }
    }

    /// One line as `guest_command` would run it for `conn`, quietly. False
    /// when it is refused or fails. A command that carries a chat message
    /// (`say`, `msg`...) is refused: it would need the player's signature.
    fn run_quiet_line(&mut self, conn: u32, line: &str) -> bool {
        let allowed = self.options.dedicated.is_some() || self.allow_commands();
        let Some(host) = self.lan.as_ref() else {
            return false;
        };
        let integrated = host.hub.config.integrated;
        if !integrated && (!host.settings.guest_commands || !allowed) {
            return false;
        }
        let Some((permission, uuid, name)) = host
            .hub
            .guest(conn)
            .map(|g| (g.permission, g.uuid, g.name.clone()))
        else {
            return false;
        };
        let command = line.strip_prefix('/').unwrap_or(line);
        if !self.signable_arguments(permission, command).is_empty() {
            return false;
        }
        // The mods hear it as they hear a typed command; the batch is quiet,
        // so their replies go nowhere.
        let mut canceled = false;
        self.native_player_command(uuid, &name, command, &mut canceled, &mut Vec::new());
        if canceled {
            return false;
        }
        self.run_guest_command(GuestCommand {
            conn,
            command: command.to_owned(),
            permission,
            integrated,
            quiet: true,
        })
    }
}

/// A player's finished batches, folded into the log lines the server writes.
#[derive(Default)]
pub(super) struct QuietLog {
    last: Option<horizon_std::time::Instant>,
    batches: usize,
    /// The folded batches' counts, under the first one's mod.
    folded: QuietBatch,
    /// Another mod's batch was folded into the same line.
    several_mods: bool,
}

impl QuietLog {
    /// The log line is written synchronously on the game thread, so a player's batches are
    /// folded into one line a second.
    const EVERY: horizon_std::time::Duration = horizon_std::time::Duration::from_secs(1);

    /// Adds a finished batch; the line to log when the last one is a second old, else None
    /// (the batch is counted in the next line).
    fn finish(
        &mut self,
        now: horizon_std::time::Instant,
        name: &str,
        b: &QuietBatch,
    ) -> Option<String> {
        let f = &mut self.folded;
        if self.batches == 0 {
            f.mod_id.clone_from(&b.mod_id);
        } else if f.mod_id != b.mod_id {
            self.several_mods = true;
        }
        self.batches += 1;
        f.ran += b.ran;
        f.failed += b.failed;
        f.dropped += b.dropped;
        if self
            .last
            .is_some_and(|t| now.duration_since(t) < Self::EVERY)
        {
            return None;
        }
        self.last = Some(now);
        let line = self.summary(name);
        *self = QuietLog {
            last: self.last,
            ..QuietLog::default()
        };
        Some(line)
    }

    /// The folded batches' line in the server's log.
    fn summary(&self, name: &str) -> String {
        let f = &self.folded;
        let mut line = if self.several_mods {
            log_line(name, "several mods", f.ran, f.failed, f.dropped)
        } else {
            summary(name, f)
        };
        if self.batches > 1 {
            line += &format!(" ({} batches)", self.batches);
        }
        line
    }
}

/// The batch's line in the server's log.
fn summary(name: &str, b: &QuietBatch) -> String {
    log_line(
        name,
        &format!("mod {}", b.mod_id),
        b.ran,
        b.failed,
        b.dropped,
    )
}

fn log_line(name: &str, source: &str, ran: usize, failed: usize, dropped: usize) -> String {
    let mut line = format!("{name} ran {ran} quiet command(s) from {source}: {failed} failed");
    if dropped > 0 {
        line += &format!(", {dropped} dropped (more than {PENDING} waiting)");
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(ran: usize, failed: usize, dropped: usize) -> QuietBatch {
        QuietBatch {
            conn: 1,
            mod_id: "orbital".into(),
            lines: VecDeque::new(),
            ran,
            failed,
            dropped,
        }
    }

    #[test]
    fn the_log_line_counts_the_batch() {
        let now = horizon_std::time::Instant::now();
        assert_eq!(
            QuietLog::default()
                .finish(now, "Alex", &batch(12, 3, 0))
                .unwrap(),
            "Alex ran 12 quiet command(s) from mod orbital: 3 failed"
        );
        let line = QuietLog::default()
            .finish(now, "Alex", &batch(12, 3, 5))
            .unwrap();
        assert!(line.ends_with(", 5 dropped (more than 4096 waiting)"));
    }

    /// A player finishing a batch every tick writes one line a second, and the next line
    /// carries what the quiet ones counted.
    #[test]
    fn batches_in_one_second_make_one_log_line() {
        let start = horizon_std::time::Instant::now();
        let mut log = QuietLog::default();
        let mut lines = Vec::new();
        for tick in 0..50u32 {
            let now = start + horizon_std::time::Duration::from_millis(50 * u64::from(tick));
            lines.extend(log.finish(now, "Alex", &batch(1, 0, 0)));
        }
        assert_eq!(lines.len(), 3, "{lines:?}");
        let later = start + horizon_std::time::Duration::from_secs(10);
        let line = log.finish(later, "Alex", &batch(1, 1, 0)).unwrap();
        assert_eq!(
            line,
            "Alex ran 10 quiet command(s) from mod orbital: 1 failed (10 batches)"
        );
    }

    /// A line folded from two mods' batches credits neither alone.
    #[test]
    fn batches_of_two_mods_make_a_line_from_several_mods() {
        let start = horizon_std::time::Instant::now();
        let mut log = QuietLog::default();
        assert!(log.finish(start, "Alex", &batch(1, 0, 0)).is_some());
        let other = QuietBatch {
            mod_id: "minimap".into(),
            ..batch(2, 0, 0)
        };
        assert!(log.finish(start, "Alex", &other).is_none());
        let later = start + horizon_std::time::Duration::from_secs(2);
        assert_eq!(
            log.finish(later, "Alex", &batch(1, 0, 0)).unwrap(),
            "Alex ran 3 quiet command(s) from several mods: 0 failed (2 batches)"
        );
    }

    /// Each payload is one command for the spam throttler, as a chat command is, however many
    /// lines it holds.
    #[test]
    #[ignore = "daily"]
    fn a_payload_counts_as_one_command_for_the_spam_throttler() {
        crate::game::native_guest_turn_tests::with_guests(|game| {
            let lines: Vec<String> = (0..64).map(|_| "say hi".into()).collect();
            let payloads = quiet::encode("orbital", &lines);
            assert_eq!(payloads.len(), 1);
            for n in 1..=3 {
                game.guest_quiet_commands(12, &payloads[0]);
                assert_eq!(game.lan.as_ref().unwrap().play[&12].command_spam, 20 * n);
            }
        });
    }
}
