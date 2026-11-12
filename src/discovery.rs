//! Who wants each adapter scanning.
//!
//! Discovery is per client and per adapter, reference-counted: the first
//! client to want it starts it, the last to stop wanting it -- by saying so or
//! by disconnecting -- stops it. Overskride scans for as long as its window
//! is open (overskride 0.6.5, src/bluetooth/device.rs:292-470), which keeps
//! the radio busy whether or not anybody is looking at the list.
//!
//! BlueZ keeps one discovery session per D-Bus connection, so the daemon is a
//! single client to it whatever its own clients do; this count is what turns
//! many interested pages into one StartDiscovery and one StopDiscovery.

use std::collections::{BTreeMap, BTreeSet};

/// What the bus should be told after a change of interest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Start(String),
    Stop(String),
    Nothing,
}

#[derive(Debug, Default)]
pub struct Interest {
    by_adapter: BTreeMap<String, BTreeSet<u64>>,
}

impl Interest {
    pub fn set(&mut self, client: u64, adapter: &str, on: bool) -> Change {
        if on {
            let set = self.by_adapter.entry(adapter.to_string()).or_default();
            let was_empty = set.is_empty();
            set.insert(client);
            if was_empty {
                Change::Start(adapter.to_string())
            } else {
                Change::Nothing
            }
        } else {
            let Some(set) = self.by_adapter.get_mut(adapter) else { return Change::Nothing };
            if !set.remove(&client) {
                return Change::Nothing;
            }
            if set.is_empty() {
                self.by_adapter.remove(adapter);
                Change::Stop(adapter.to_string())
            } else {
                Change::Nothing
            }
        }
    }

    /// A client went away: every adapter it alone was keeping awake.
    pub fn drop_client(&mut self, client: u64) -> Vec<String> {
        let mut stop = Vec::new();
        self.by_adapter.retain(|adapter, set| {
            if set.remove(&client) && set.is_empty() {
                stop.push(adapter.clone());
                return false;
            }
            !set.is_empty()
        });
        stop
    }

    pub fn wanted(&self, adapter: &str) -> bool {
        self.by_adapter.get(adapter).is_some_and(|s| !s.is_empty())
    }

    /// Adapters somebody wants scanning, for restarting discovery after
    /// BlueZ or the adapter comes back.
    pub fn adapters(&self) -> Vec<String> {
        self.by_adapter.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "/org/bluez/hci0";
    const B: &str = "/org/bluez/hci1";

    #[test]
    fn the_first_client_starts_and_the_last_stops() {
        let mut i = Interest::default();
        assert_eq!(i.set(1, A, true), Change::Start(A.into()));
        assert_eq!(i.set(2, A, true), Change::Nothing);
        assert_eq!(i.set(1, A, true), Change::Nothing, "asking twice is one interest");
        assert_eq!(i.set(1, A, false), Change::Nothing);
        assert!(i.wanted(A));
        assert_eq!(i.set(2, A, false), Change::Stop(A.into()));
        assert!(!i.wanted(A));
        assert_eq!(i.set(2, A, false), Change::Nothing, "stopping twice stops once");
    }

    #[test]
    fn adapters_are_counted_separately() {
        let mut i = Interest::default();
        assert_eq!(i.set(1, A, true), Change::Start(A.into()));
        assert_eq!(i.set(1, B, true), Change::Start(B.into()));
        assert_eq!(i.set(1, A, false), Change::Stop(A.into()));
        assert_eq!(i.adapters(), vec![B.to_string()]);
    }

    #[test]
    fn a_client_that_disconnects_releases_only_what_it_alone_held() {
        let mut i = Interest::default();
        i.set(1, A, true);
        i.set(2, A, true);
        i.set(2, B, true);
        assert_eq!(i.drop_client(2), vec![B.to_string()]);
        assert!(i.wanted(A));
        assert_eq!(i.drop_client(1), vec![A.to_string()]);
        assert_eq!(i.drop_client(1), Vec::<String>::new());
        assert!(i.adapters().is_empty());
    }
}
