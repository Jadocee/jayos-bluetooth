//! How often `state` is sent.
//!
//! Leading edge, then at most one per interval. During discovery BlueZ sends
//! an RSSI change per device per advertisement and obexd's server sends
//! progress per chunk received; a snapshot each time would be hundreds of
//! lines a second for a page that redraws at most a few times. The first
//! change goes out at once, so a toggle feels immediate; changes inside the
//! interval are folded into one snapshot at its end, so the last one is never
//! lost.

use std::time::{Duration, Instant};

pub const INTERVAL: Duration = Duration::from_millis(400);

#[derive(Debug, Default)]
pub struct Throttle {
    last: Option<Instant>,
    pending: bool,
}

impl Throttle {
    /// Something changed. True when the snapshot should go out now.
    pub fn request(&mut self, now: Instant) -> bool {
        match self.last {
            Some(last) if now < last + INTERVAL => {
                self.pending = true;
                false
            }
            _ => {
                self.sent(now);
                true
            }
        }
    }

    /// A snapshot went out regardless -- a prompt appeared or went, which
    /// must not wait -- and covers whatever was pending.
    pub fn sent(&mut self, now: Instant) {
        self.last = Some(now);
        self.pending = false;
    }

    /// When the folded snapshot is due, if one is.
    pub fn deadline(&self) -> Option<Instant> {
        match (self.pending, self.last) {
            (true, Some(last)) => Some(last + INTERVAL),
            _ => None,
        }
    }

    /// True when the folded snapshot is due now.
    pub fn due(&mut self, now: Instant) -> bool {
        match self.deadline() {
            Some(d) if now >= d => {
                self.sent(now);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_change_goes_at_once_and_the_rest_are_folded() {
        let t0 = Instant::now();
        let mut t = Throttle::default();
        assert!(t.request(t0));
        assert_eq!(t.deadline(), None);
        assert!(!t.request(t0 + Duration::from_millis(10)));
        assert!(!t.request(t0 + Duration::from_millis(200)));
        assert_eq!(t.deadline(), Some(t0 + INTERVAL));
        assert!(!t.due(t0 + Duration::from_millis(399)));
        assert!(t.due(t0 + INTERVAL));
        assert!(!t.due(t0 + INTERVAL), "one snapshot for the fold");
        assert_eq!(t.deadline(), None);
    }

    #[test]
    fn a_quiet_interval_lets_the_next_change_through_at_once() {
        let t0 = Instant::now();
        let mut t = Throttle::default();
        assert!(t.request(t0));
        assert!(t.request(t0 + INTERVAL));
        assert!(!t.request(t0 + INTERVAL + Duration::from_millis(1)));
    }

    #[test]
    fn a_forced_snapshot_covers_what_was_pending() {
        let t0 = Instant::now();
        let mut t = Throttle::default();
        t.request(t0);
        t.request(t0 + Duration::from_millis(100));
        t.sent(t0 + Duration::from_millis(150));
        assert_eq!(t.deadline(), None);
        assert!(
            !t.request(t0 + Duration::from_millis(200)),
            "the interval restarts from the forced one"
        );
        assert_eq!(t.deadline(), Some(t0 + Duration::from_millis(150) + INTERVAL));
    }
}
