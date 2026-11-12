//! The socket clients: `watch`, `status` and `send`.
//!
//! Each prints JSON on stdout and nothing else, the convention every helper
//! here follows (crate::support::emit). "No daemon answered" is exit 2 with an
//! envelope -- it ran nothing, which is what 2 means across this tree -- so
//! the settings page can tell "the daemon is not running" from "the daemon
//! said no".

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::control;

/// How long `status` and `send` wait for the first `state`. The daemon sends
/// it on connect, so anything this slow is a wedged daemon.
const ANSWER: Duration = Duration::from_secs(5);

fn envelope(error: &str) {
    let _ = crate::support::emit::json(&json!({"ok": false, "error": error}));
}

fn no_daemon(e: &std::io::Error) -> u8 {
    eprintln!("jayos-bluetooth: no daemon: {e}");
    envelope("the jayos-bluetooth daemon is not running");
    2
}

fn connect() -> Result<UnixStream, u8> {
    control::connect(&control::run_dir()).map_err(|e| no_daemon(&e))
}

fn print_line(line: &str) -> bool {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    writeln!(lock, "{line}").and_then(|()| lock.flush()).is_ok()
}

/// Read until the first `state` event, within `ANSWER`.
fn first_state(reader: &mut BufReader<UnixStream>) -> Option<Value> {
    reader.get_ref().set_read_timeout(Some(ANSWER)).ok()?;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let v: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v["event"] == "state" {
            reader.get_ref().set_read_timeout(None).ok()?;
            return Some(v);
        }
    }
}

/// `jayos-bluetooth watch`: stdin lines to the daemon, every event to stdout.
pub fn watch() -> u8 {
    let stream = match connect() {
        Ok(s) => s,
        Err(code) => return code,
    };
    let Ok(mut to_daemon) = stream.try_clone() else {
        envelope("the socket could not be shared");
        return 1;
    };
    std::thread::spawn(move || {
        // On EOF this stops forwarding and leaves the socket open: shutting
        // the write half down would read as a disconnect, and the daemon
        // would drop this client's scan interest while it is still watching.
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { return };
            if writeln!(to_daemon, "{line}").is_err() {
                return;
            }
        }
    });
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        if !print_line(&line) {
            // Nobody is reading what this prints.
            return 0;
        }
    }
    envelope("the jayos-bluetooth daemon closed the connection");
    1
}

/// `jayos-bluetooth status`: the current state, once.
pub fn status() -> u8 {
    let stream = match connect() {
        Ok(s) => s,
        Err(code) => return code,
    };
    match first_state(&mut BufReader::new(stream)) {
        Some(v) => {
            print_line(&v.to_string());
            0
        }
        None => {
            envelope("the jayos-bluetooth daemon did not answer");
            2
        }
    }
}

/// `<device>` as an object path: a path is taken as given, an address is
/// looked up in `state`.
pub fn resolve_device(state: &Value, device: &str) -> Option<String> {
    if device.starts_with('/') {
        return Some(device.to_string());
    }
    state["devices"].as_array()?.iter().find_map(|d| {
        let a = d["address"].as_str()?;
        a.eq_ignore_ascii_case(device).then(|| d["path"].as_str().map(str::to_string)).flatten()
    })
}

/// File arguments as absolute UTF-8 paths, or the first that cannot be one.
/// The daemon needs absolute paths because its working directory is not the
/// caller's, and D-Bus strings are UTF-8, so a name that is not cannot be
/// handed to obexd at all.
pub fn absolute_files(files: &[OsString]) -> Result<Vec<String>, String> {
    files
        .iter()
        .map(|f| {
            let p = std::path::absolute(Path::new(f)).unwrap_or_else(|_| PathBuf::from(f));
            p.to_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{} is not a UTF-8 name", p.display()))
        })
        .collect()
}

/// `jayos-bluetooth send <device> <file>...`: send, print each transfer as it
/// changes, end with the daemon's reply. Exit 0 when every file arrived.
pub fn send(device: &str, files: &[OsString]) -> u8 {
    let files = match absolute_files(files) {
        Ok(f) => f,
        Err(e) => {
            let _ = crate::support::emit::json(
                &json!({"ok": false, "error": "file-unreadable", "message": e}),
            );
            return 1;
        }
    };
    let stream = match connect() {
        Ok(s) => s,
        Err(code) => return code,
    };
    let Ok(mut to_daemon) = stream.try_clone() else {
        envelope("the socket could not be shared");
        return 1;
    };
    let mut reader = BufReader::new(stream);
    let Some(state) = first_state(&mut reader) else {
        envelope("the jayos-bluetooth daemon did not answer");
        return 2;
    };
    let Some(path) = resolve_device(&state, device) else {
        let _ = crate::support::emit::json(&json!({"ok": false, "error": "no-such-device",
            "message": format!("no device with address {device}")}));
        return 1;
    };
    let id = format!("send-{}", std::process::id());
    let cmd = json!({"id": id, "op": "send", "device": path, "files": files});
    // Transfers this call started: outgoing, to this device, of one of these
    // files, and begun after the request. The page may be sending too, so the
    // device alone is not enough. The daemon's clock is this machine's.
    let since = crate::now_ms() - 1;
    if writeln!(to_daemon, "{cmd}").is_err() {
        envelope("the jayos-bluetooth daemon closed the connection");
        return 1;
    }
    let mut printed: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if v["event"] == "state" {
            for t in v["transfers"].as_array().into_iter().flatten() {
                let ours = t["direction"] == "out"
                    && t["device"] == path.as_str()
                    && t["path"].as_str().is_some_and(|p| files.iter().any(|f| f == p))
                    && t["started"].as_i64().is_some_and(|s| s >= since);
                let Some(tid) = t["id"].as_str() else { continue };
                let text = t.to_string();
                if ours && printed.get(tid) != Some(&text) {
                    print_line(&text);
                    printed.insert(tid.to_string(), text);
                }
            }
        } else if v["event"] == "reply" && v["id"] == id.as_str() {
            print_line(&line);
            return if v["ok"] == true { 0 } else { 1 };
        }
    }
    envelope("the jayos-bluetooth daemon closed the connection");
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_is_named_by_path_or_by_address() {
        let state = json!({"devices": [{"path": "/org/bluez/hci0/dev_AA", "address": "AA:BB:CC:DD:EE:FF"}]});
        assert_eq!(
            resolve_device(&state, "aa:bb:cc:dd:ee:ff").as_deref(),
            Some("/org/bluez/hci0/dev_AA")
        );
        assert_eq!(
            resolve_device(&state, "/org/bluez/hci1/dev_X").as_deref(),
            Some("/org/bluez/hci1/dev_X")
        );
        assert_eq!(resolve_device(&state, "00:00:00:00:00:00"), None);
        assert_eq!(resolve_device(&json!({}), "AA:BB:CC:DD:EE:FF"), None);
    }

    #[test]
    fn files_are_made_absolute_and_must_be_utf8() {
        use std::os::unix::ffi::OsStringExt;
        let cwd = std::env::current_dir().unwrap();
        let got = absolute_files(&[OsString::from("a.txt"), OsString::from("/b")]).unwrap();
        assert_eq!(got, vec![cwd.join("a.txt").to_string_lossy().into_owned(), "/b".to_string()]);
        assert!(absolute_files(&[OsString::from_vec(b"/x\xff".to_vec())]).is_err());
    }
}
