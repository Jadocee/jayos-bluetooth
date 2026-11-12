//! The desktop portal's file chooser, for "Send files…".
//!
//! The portal answers `OpenFile` at once with a request handle and delivers
//! the choice later as a `Response` signal on that handle. The handle's path
//! is predictable from the caller's unique name and a token the caller picks
//! (the portal's own documentation of `handle_token`), and the subscription is
//! made on that path BEFORE the call: a person who picks quickly can otherwise
//! answer before a subscription made on the returned handle exists.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use zbus::blocking::{Connection, MessageIterator};
use zbus::message::Type as MsgType;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::MatchRule;

use crate::bus::{call, BusError};
use crate::files::file_uri_to_path;

pub const PORTAL: &str = "org.freedesktop.portal.Desktop";
pub const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";

/// How long a file chooser may stay open before the daemon stops waiting.
/// Long, because a person may wander off; bounded, because a portal that
/// crashed never answers and the request would otherwise wait for ever.
pub const PICK_LIMIT: Duration = Duration::from_secs(3600);

/// `/org/freedesktop/portal/desktop/request/<sender>/<token>`, where sender is
/// the unique name without its ':' and with '.' as '_'.
pub fn request_path(unique_name: &str, token: &str) -> String {
    let sender = unique_name.trim_start_matches(':').replace('.', "_");
    format!("{PORTAL_PATH}/request/{sender}/{token}")
}

/// What the person chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Picked {
    Files(Vec<PathBuf>),
    Cancelled,
}

/// The portal is missing, or has no file chooser behind it.
pub fn is_unavailable(e: &BusError) -> bool {
    e.is_absent()
        || matches!(
            e.name.as_str(),
            "org.freedesktop.DBus.Error.UnknownMethod"
                | "org.freedesktop.DBus.Error.UnknownInterface"
                | "org.freedesktop.DBus.Error.UnknownObject"
        )
}

/// The response's `uris`, as local paths; anything that is not a local file
/// is dropped rather than sent.
pub fn uris_to_paths(results: &HashMap<String, OwnedValue>) -> Vec<PathBuf> {
    let uris = results
        .get("uris")
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| Vec::<String>::try_from(v).ok())
        .unwrap_or_default();
    uris.iter().filter_map(|u| file_uri_to_path(u)).collect()
}

/// Open the chooser and wait for the answer. Runs on an action thread.
pub fn pick(conn: &Connection, title: &str, token: &str) -> Result<Picked, BusError> {
    let unique = conn.unique_name().map(|n| n.as_str().to_string()).ok_or_else(|| {
        BusError::new("org.freedesktop.DBus.Error.Failed", "no unique name on the session bus")
    })?;
    let handle = request_path(&unique, token);
    // The sender is named because the handle is guessable and anybody on the
    // session bus may emit a Response on it: the files in a forged one would
    // be sent as if chosen. The bus applies the name to broadcasts; it cannot
    // to a signal addressed to this connection, and neither can zbus locally
    // (match_rule/mod.rs, `matches`), which is what the check on the
    // portal's unique name below is for. libportal and GTK subscribe the
    // same way, and GLib's CVE-2024-34397 was this mistake.
    let subscribe = |path: &str| -> Result<MessageIterator, BusError> {
        let rule = MatchRule::builder()
            .msg_type(MsgType::Signal)
            .sender(PORTAL)
            .and_then(|b| b.interface("org.freedesktop.portal.Request"))
            .and_then(|b| b.member("Response"))
            .and_then(|b| b.path(path.to_string()))
            .map_err(BusError::from)?
            .build();
        Ok(MessageIterator::for_match_rule(rule, conn, Some(8))?)
    };
    let mut iter = subscribe(&handle)?;

    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(token));
    options.insert("multiple", Value::from(true));
    options.insert("modal", Value::from(true));
    let reply = call(
        conn,
        PORTAL,
        PORTAL_PATH,
        "org.freedesktop.portal.FileChooser",
        "OpenFile",
        &("", title, options),
    )?;
    let returned: OwnedObjectPath = reply.body().deserialize()?;
    if returned.as_str() != handle {
        // A portal older than handle_token returns a handle of its own; the
        // late subscription is the best that can be done with one.
        iter = subscribe(returned.as_str())?;
    }
    // The connection that answered OpenFile is the one that owns the request
    // and emits its Response.
    let portal = reply.header().sender().map(|s| s.as_str().to_string()).ok_or_else(|| {
        BusError::new("org.freedesktop.DBus.Error.Failed", "the portal's reply has no sender")
    })?;

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for msg in iter.flatten() {
            if msg.header().sender().map(|s| s.as_str()) != Some(portal.as_str()) {
                continue;
            }
            let body = msg.body();
            if let Ok(r) = body.deserialize::<(u32, HashMap<String, OwnedValue>)>() {
                let _ = tx.send(r);
                return;
            }
        }
    });
    let (code, results) = rx.recv_timeout(PICK_LIMIT).map_err(|_| {
        BusError::new("org.freedesktop.DBus.Error.NoReply", "the file chooser did not answer")
    })?;
    match code {
        0 => Ok(Picked::Files(uris_to_paths(&results))),
        1 => Ok(Picked::Cancelled),
        _ => Err(BusError::new("org.freedesktop.portal.Error.Failed", "the file chooser failed")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handle_path_is_predicted_from_the_unique_name() {
        assert_eq!(
            request_path(":1.42", "qs_bluetooth_7"),
            "/org/freedesktop/portal/desktop/request/1_42/qs_bluetooth_7"
        );
    }

    #[test]
    fn only_local_file_uris_become_paths() {
        let mut r = HashMap::new();
        r.insert(
            "uris".to_string(),
            OwnedValue::try_from(Value::from(vec![
                "file:///home/u/a%20b.txt",
                "https://x/y",
                "file:///c",
            ]))
            .unwrap(),
        );
        assert_eq!(uris_to_paths(&r), vec![PathBuf::from("/home/u/a b.txt"), PathBuf::from("/c")]);
        assert!(uris_to_paths(&HashMap::new()).is_empty());
    }

    #[test]
    fn a_missing_portal_is_unavailable_and_a_refusal_is_not() {
        assert!(is_unavailable(&BusError::new("org.freedesktop.DBus.Error.ServiceUnknown", "")));
        assert!(is_unavailable(&BusError::new("org.freedesktop.DBus.Error.UnknownMethod", "")));
        assert!(!is_unavailable(&BusError::new("org.freedesktop.portal.Error.Failed", "")));
    }
}
