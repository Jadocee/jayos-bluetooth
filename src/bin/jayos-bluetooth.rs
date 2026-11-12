//! Bluetooth for the shell: devices, pairing prompts and file transfer, from BlueZ and obexd.
//!
//!     jayos-bluetooth daemon                     the manager; its owner is on stdin/stdout
//!     jayos-bluetooth watch                      stdin to the daemon, its events to stdout
//!     jayos-bluetooth status                     the current state, once
//!     jayos-bluetooth send <device> <file>...    Object Push; <device> is an address or path
//!
//! WHAT IT SPEAKS
//!
//! One JSON object per line in each direction. Commands carry an `op` and an
//! `id` the reply echoes; events are `state`, `reply`, `notice` and `fatal`:
//!
//! ```text
//! {"id":"7","op":"connect","device":"/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF"}
//! {"event":"reply","id":"7","op":"connect","ok":false,
//!  "error":"org.bluez.Error.Failed","message":"br-connection-page-timeout"}
//! ```
//!
//! A `state` carries every adapter, device, transfer and open prompt, with
//! every key present on every object, and goes to a client the moment it
//! connects and then at most every 400 ms while anything changes.
//!
//! WHO RUNS IT
//!
//! The shell starts `daemon` and owns it through its stdin: when the shell
//! goes, stdin closes and the daemon exits 0, so it can never be orphaned. The
//! settings page runs `watch`, which talks to the daemon over
//! `$XDG_RUNTIME_DIR/quickshell/bluetooth/control.sock` (0600 in a 0700
//! directory). A second `daemon` waits up to five seconds for the first to
//! exit, because a shell reload starts the new one before the old one is
//! gone; if the first is still there, it prints `{"event":"fatal",...}` and
//! exits 1.
//!
//! `watch`, `status` and `send` exit 2 with `{"ok":false,"error":"..."}` when
//! no daemon answers. `send` exits 0 when every file arrived and 1 otherwise.
//! An unknown verb is `emit::unknown_command`.

use std::ffi::OsString;
use std::process::ExitCode;

use jayos_bluetooth::support::emit;

/// The verbs, in the order the doc block lists them.
const VERBS: &[&str] = &["daemon", "watch", "status", "send"];

fn main() -> ExitCode {
    // args_os, not Args: `send`'s file names are paths, and a lossy or
    // flag-parsed copy of `--weird name.txt` no longer names the file.
    let argv: Vec<OsString> = std::env::args_os().collect();
    let program = argv
        .first()
        .and_then(|p| std::path::Path::new(p).file_name().map(|f| f.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "jayos-bluetooth".into());
    let verb = argv.get(1).map(|v| v.to_string_lossy().into_owned()).unwrap_or_default();
    let code = match verb.as_str() {
        "daemon" => jayos_bluetooth::daemon::run(),
        "watch" => jayos_bluetooth::client::watch(),
        "status" => jayos_bluetooth::client::status(),
        "send" => {
            if argv.len() < 4 {
                eprintln!("{program}: usage: {program} send <device> <file>...");
                let _ = emit::json(&serde_json::json!({
                    "ok": false,
                    "error": format!("usage: {program} send <device> <file>..."),
                }));
                emit::REFUSED
            } else {
                jayos_bluetooth::client::send(&argv[2].to_string_lossy(), &argv[3..])
            }
        }
        other => return emit::unknown_command(&program, other, VERBS),
    };
    ExitCode::from(code)
}
