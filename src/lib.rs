//! The Bluetooth manager behind the settings page and the pairing prompts.
//!
//! One daemon talks to BlueZ on the system bus and to obexd on the session
//! bus, holds everything both report in one model, and speaks JSON lines to
//! its clients: the shell that started it, on stdin and stdout, and anything
//! else -- the settings page, a terminal -- on a private socket. One process
//! that sees everything is one source of truth for the page and the prompts;
//! Quickshell's own Bluetooth module has no agent, no OBEX, and reports every
//! failure only to its log.
//!
//! The modules split along what can be tested without a bus. `protocol`,
//! `model`, `prompts`, `transfers`, `files`, `discovery` and `throttle` are
//! pure and carry the unit tests; `agent`, `bus`, `portal` and `daemon` are
//! the D-Bus half, held by `tests/daemon.rs` against a mock BlueZ and a mock
//! obexd on private buses; `control` and `client` are the socket.

pub mod agent;
pub mod bus;
pub mod client;
pub mod control;
pub mod daemon;
pub mod discovery;
pub mod files;
pub mod model;
pub mod portal;
pub mod prompts;
pub mod protocol;
pub mod support;
pub mod throttle;
pub mod transfers;

/// Milliseconds since the Unix epoch, the unit every timestamp in `state` uses
/// because that is what JavaScript's `Date` takes.
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}
