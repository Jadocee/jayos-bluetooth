//! OBEX transfers in both directions, as the `transfers` array shows them.
//!
//! The rate is computed here, over a sliding window, because obexd reports
//! progress at two different cadences: its client (sending) emits
//! `Transferred` from a one-second timer, its server (receiving) on every
//! chunk written (obexd/client/transfer.c and obexd/src/obex.c). Overskride
//! took the difference between consecutive signals and called it KB/s, which
//! was right for the first and wrong by an order of magnitude for the second
//! (overskride 0.6.5, src/obex/obex.rs:39-123; read, not run).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::model::{Props, Val};

/// How much history the rate is averaged over.
pub const RATE_WINDOW_MS: i64 = 3_000;

/// How long a finished transfer stays listed, so the person sees how it
/// ended without having to clear it.
pub const KEEP_FINISHED: Duration = Duration::from_secs(60);

/// Bytes per second over roughly the last three seconds.
#[derive(Debug, Default, Clone)]
pub struct Rate {
    samples: VecDeque<(i64, u64)>,
}

impl Rate {
    /// Record that `bytes` had arrived by `t_ms`. The oldest sample kept is
    /// the newest one at or before the window's start, so the span measured
    /// is never shorter than the window once there is that much history.
    pub fn push(&mut self, t_ms: i64, bytes: u64) {
        if let Some(&(_, last)) = self.samples.back() {
            if bytes < last {
                // A count that went backwards is a restarted transfer, and
                // the history describes a different one.
                self.samples.clear();
            }
        }
        self.samples.push_back((t_ms, bytes));
        while self.samples.len() > 2 && self.samples[1].0 <= t_ms - RATE_WINDOW_MS {
            self.samples.pop_front();
        }
    }

    pub fn bytes_per_second(&self) -> Option<u64> {
        let (&(t0, b0), &(t1, b1)) = (self.samples.front()?, self.samples.back()?);
        if t1 <= t0 {
            return None;
        }
        Some((b1 - b0) * 1000 / (t1 - t0) as u64)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    In,
    Out,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::In => "in",
            Direction::Out => "out",
        }
    }
}

/// How a transfer ended, reported once, when it first does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Complete,
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct Transfer {
    pub id: String,
    pub direction: Direction,
    pub device: String,
    pub address: String,
    pub device_name: String,
    pub file: String,
    pub path: String,
    pub size: Option<u64>,
    pub transferred: u64,
    pub status: String,
    pub started: i64,
    pub finished: Option<i64>,
    pub error: String,
    /// The `send` batch an outgoing transfer belongs to.
    pub batch: Option<u64>,
    /// This daemon has asked obexd to cancel it. obexd answers a Cancel by
    /// setting Status to `error`, and that signal can reach the core before
    /// the Cancel's own reply does; while this is set, it means cancelled.
    pub cancelling: bool,
    rate: Rate,
    finished_at: Option<Instant>,
}

/// What a new transfer starts from.
#[derive(Debug, Clone, Default)]
pub struct Start {
    pub id: String,
    pub device: String,
    pub address: String,
    pub device_name: String,
    pub file: String,
    pub path: String,
    pub size: Option<u64>,
}

impl Transfer {
    pub fn new(direction: Direction, start: Start, batch: Option<u64>, now_ms: i64) -> Transfer {
        Transfer {
            id: start.id,
            direction,
            device: start.device,
            address: start.address,
            device_name: start.device_name,
            file: start.file,
            path: start.path,
            size: start.size,
            transferred: 0,
            status: "queued".into(),
            started: now_ms,
            finished: None,
            error: String::new(),
            batch,
            cancelling: false,
            rate: Rate::default(),
            finished_at: None,
        }
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.status.as_str(), "complete" | "error" | "cancelled")
    }

    fn finish(&mut self, status: &str, now_ms: i64, now: Instant) {
        self.status = status.to_string();
        self.finished = Some(now_ms);
        self.finished_at = Some(now);
    }

    /// Apply obexd's Transfer1 properties. Returns the outcome the first time
    /// the transfer reaches an end; once finished, later signals change
    /// nothing -- in particular the `error` obexd reports after a cancel this
    /// daemon asked for does not turn `cancelled` into `error`.
    pub fn apply(&mut self, props: &Props, now_ms: i64, now: Instant) -> Option<Outcome> {
        if self.is_finished() {
            return None;
        }
        if let Some(Val::Int(n)) = props.get("Size") {
            self.size = Some((*n).max(0) as u64);
        }
        if let Some(Val::Int(n)) = props.get("Transferred") {
            self.transferred = (*n).max(0) as u64;
            self.rate.push(now_ms, self.transferred);
        }
        let Some(Val::Str(status)) = props.get("Status") else { return None };
        match status.as_str() {
            "complete" => {
                if let Some(size) = self.size {
                    self.transferred = self.transferred.max(size);
                }
                self.finish("complete", now_ms, now);
                Some(Outcome::Complete)
            }
            "error" if self.cancelling => {
                self.finish("cancelled", now_ms, now);
                Some(Outcome::Cancelled)
            }
            "error" => {
                self.error = "the transfer did not complete".into();
                self.finish("error", now_ms, now);
                Some(Outcome::Failed(self.error.clone()))
            }
            "queued" | "active" | "suspended" => {
                self.status = status.clone();
                None
            }
            // A status obexd does not document is shown as it came rather than
            // guessed at; it is not an end.
            other => {
                self.status = other.to_string();
                None
            }
        }
    }

    /// The daemon's own cancel succeeded.
    pub fn cancel(&mut self, now_ms: i64, now: Instant) -> Option<Outcome> {
        if self.is_finished() {
            return None;
        }
        self.finish("cancelled", now_ms, now);
        Some(Outcome::Cancelled)
    }

    /// Ended from outside -- obexd went away mid-transfer.
    pub fn fail(&mut self, message: &str, now_ms: i64, now: Instant) -> Option<Outcome> {
        if self.is_finished() {
            return None;
        }
        self.error = message.to_string();
        self.finish("error", now_ms, now);
        Some(Outcome::Failed(message.to_string()))
    }

    pub fn json(&self) -> Value {
        let rate = if self.status == "active" { self.rate.bytes_per_second() } else { None };
        json!({
            "id": self.id,
            "direction": self.direction.as_str(),
            "device": self.device,
            "address": self.address,
            "deviceName": self.device_name,
            "file": self.file,
            "path": self.path,
            "size": self.size,
            "transferred": self.transferred,
            "status": self.status,
            "rate": rate,
            "started": self.started,
            "finished": self.finished,
            "error": self.error,
        })
    }
}

#[derive(Debug, Default)]
pub struct Transfers {
    list: Vec<Transfer>,
}

impl Transfers {
    /// Add a transfer, replacing one with the same id: obexd numbers transfer
    /// objects per session, so an id seen again after a restart of obexd is a
    /// new transfer, not the old one resumed.
    pub fn insert(&mut self, t: Transfer) {
        self.list.retain(|x| x.id != t.id);
        self.list.push(t);
    }

    pub fn get(&self, id: &str) -> Option<&Transfer> {
        self.list.iter().find(|t| t.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Transfer> {
        self.list.iter_mut().find(|t| t.id == id)
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Transfer> {
        self.list.iter_mut()
    }

    /// Remove a finished transfer. A running one is refused: hiding it would
    /// not stop it.
    pub fn dismiss(&mut self, id: &str) -> Result<(), (&'static str, &'static str)> {
        let Some(i) = self.list.iter().position(|t| t.id == id) else {
            return Err(("no-such-transfer", "no transfer has that id"));
        };
        if !self.list[i].is_finished() {
            return Err(("bad-request", "the transfer is still running; cancel it first"));
        }
        self.list.remove(i);
        Ok(())
    }

    /// Drop transfers that finished long enough ago. Returns whether any went.
    pub fn expire(&mut self, now: Instant) -> bool {
        let before = self.list.len();
        self.list.retain(|t| t.finished_at.is_none_or(|f| now < f + KEEP_FINISHED));
        self.list.len() != before
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.list.iter().filter_map(|t| t.finished_at).map(|f| f + KEEP_FINISHED).min()
    }

    /// Where unfinished incoming transfers are writing, so a second push of
    /// the same name in the same second is not handed the same path.
    pub fn reserved_paths(&self) -> Vec<String> {
        self.list
            .iter()
            .filter(|t| t.direction == Direction::In && !t.is_finished())
            .map(|t| t.path.clone())
            .collect()
    }

    pub fn json(&self) -> Vec<Value> {
        self.list.iter().map(Transfer::json).collect()
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(pairs: &[(&str, Val)]) -> Props {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    fn transfer() -> Transfer {
        Transfer::new(
            Direction::Out,
            Start { id: "/t1".into(), file: "a.pdf".into(), size: Some(3000), ..Start::default() },
            Some(1),
            0,
        )
    }

    #[test]
    fn the_rate_is_bytes_per_second_over_the_window_not_per_signal() {
        let mut r = Rate::default();
        assert_eq!(r.bytes_per_second(), None);
        r.push(0, 0);
        assert_eq!(r.bytes_per_second(), None, "one sample is not a rate");
        // obexd's server: a signal every 10 ms, 1000 bytes each = 100 kB/s.
        for i in 1..=500 {
            r.push(i * 10, (i * 1000) as u64);
        }
        assert_eq!(r.bytes_per_second(), Some(100_000));
        // obexd's client: once a second.
        let mut r = Rate::default();
        r.push(0, 0);
        r.push(1000, 50_000);
        r.push(2000, 100_000);
        assert_eq!(r.bytes_per_second(), Some(50_000));
    }

    #[test]
    fn the_window_forgets_what_is_older_than_three_seconds() {
        let mut r = Rate::default();
        r.push(0, 0);
        r.push(1000, 1_000_000); // a fast first second
        for t in (2000..=10_000).step_by(1000) {
            r.push(t, 1_000_000 + (t as u64 - 1000) * 10);
        }
        // Only the slow tail is in the window now: 10 bytes per millisecond.
        assert_eq!(r.bytes_per_second(), Some(10_000));
        assert!(r.samples.len() <= 5, "{:?}", r.samples);
    }

    #[test]
    fn a_count_that_goes_backwards_starts_the_history_again() {
        let mut r = Rate::default();
        r.push(0, 0);
        r.push(1000, 5000);
        r.push(2000, 10);
        assert_eq!(r.bytes_per_second(), None);
    }

    #[test]
    fn progress_and_completion_are_applied_and_reported_once() {
        let t0 = Instant::now();
        let mut t = transfer();
        assert_eq!(t.apply(&props(&[("Status", Val::Str("active".into()))]), 0, t0), None);
        assert_eq!(t.apply(&props(&[("Transferred", Val::Int(1000))]), 1000, t0), None);
        assert_eq!(t.apply(&props(&[("Transferred", Val::Int(2000))]), 2000, t0), None);
        assert_eq!(t.json()["rate"], 1000);
        assert_eq!(t.json()["status"], "active");
        // A Status-only signal does not reset the count (overskride's bar went to 0).
        assert_eq!(
            t.apply(&props(&[("Status", Val::Str("complete".into()))]), 2500, t0),
            Some(Outcome::Complete)
        );
        assert_eq!(t.transferred, 3000);
        assert_eq!(t.json()["finished"], 2500);
        assert_eq!(t.json()["rate"], Value::Null);
        assert_eq!(t.apply(&props(&[("Status", Val::Str("error".into()))]), 2600, t0), None);
        assert_eq!(t.status, "complete");
    }

    #[test]
    fn a_cancel_the_daemon_made_is_not_overwritten_by_obexds_error() {
        let t0 = Instant::now();
        let mut t = transfer();
        assert_eq!(t.cancel(5, t0), Some(Outcome::Cancelled));
        assert_eq!(t.apply(&props(&[("Status", Val::Str("error".into()))]), 6, t0), None);
        assert_eq!(t.status, "cancelled");
        assert_eq!(t.cancel(7, t0), None);
    }

    #[test]
    fn obexds_error_after_a_cancel_this_daemon_asked_for_is_the_cancel() {
        let mut t = transfer();
        t.cancelling = true;
        let out = t.apply(&props(&[("Status", Val::Str("error".into()))]), 1, Instant::now());
        assert_eq!(out, Some(Outcome::Cancelled));
        assert_eq!(t.status, "cancelled");
        assert_eq!(t.error, "");
    }

    #[test]
    fn an_error_is_reported_with_a_message() {
        let mut t = transfer();
        let out = t.apply(&props(&[("Status", Val::Str("error".into()))]), 1, Instant::now());
        assert!(matches!(out, Some(Outcome::Failed(_))));
        assert_eq!(t.json()["status"], "error");
        assert_ne!(t.json()["error"], "");
    }

    #[test]
    fn every_transfer_key_is_present() {
        let v = transfer().json();
        for k in [
            "id",
            "direction",
            "device",
            "address",
            "deviceName",
            "file",
            "path",
            "size",
            "transferred",
            "status",
            "rate",
            "started",
            "finished",
            "error",
        ] {
            assert!(v.get(k).is_some(), "{k}");
        }
        assert_eq!(v.as_object().unwrap().len(), 14);
        assert_eq!(v["status"], "queued");
        assert_eq!(v["finished"], Value::Null);
    }

    #[test]
    fn finished_transfers_stay_a_minute_or_until_dismissed() {
        let t0 = Instant::now();
        let mut all = Transfers::default();
        let mut a = transfer();
        a.apply(&props(&[("Status", Val::Str("complete".into()))]), 0, t0);
        all.insert(a);
        let mut b = transfer();
        b.id = "/t2".into();
        all.insert(b);
        assert_eq!(
            all.dismiss("/t2"),
            Err(("bad-request", "the transfer is still running; cancel it first"))
        );
        assert_eq!(all.dismiss("/nope").unwrap_err().0, "no-such-transfer");
        assert_eq!(all.next_deadline(), Some(t0 + KEEP_FINISHED));
        assert!(!all.expire(t0 + Duration::from_secs(59)));
        assert!(all.expire(t0 + KEEP_FINISHED));
        assert_eq!(all.len(), 1);
        all.get_mut("/t2").unwrap().cancel(0, t0);
        assert_eq!(all.dismiss("/t2"), Ok(()));
        assert!(all.is_empty());
    }

    #[test]
    fn only_unfinished_incoming_paths_are_reserved() {
        let t0 = Instant::now();
        let mut all = Transfers::default();
        let inbound = |id: &str, path: &str| {
            Transfer::new(
                Direction::In,
                Start { id: id.into(), path: path.into(), ..Start::default() },
                None,
                0,
            )
        };
        all.insert(inbound("/a", "/d/a.txt"));
        let mut done = inbound("/b", "/d/b.txt");
        done.apply(&props(&[("Status", Val::Str("complete".into()))]), 0, t0);
        all.insert(done);
        all.insert(transfer());
        assert_eq!(all.reserved_paths(), vec!["/d/a.txt".to_string()]);
    }
}
