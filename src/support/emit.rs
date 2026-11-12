//! Printing the answer.
//!
//! Every one of these programs is called by QML in the same way: run it,
//! parse stdout as JSON, ignore stderr. That makes two rules absolute.
//!
//! STDOUT IS ONLY EVER JSON. A progress line, a warning, a stray debug print --
//! any of them make `JSON.parse` throw, and the widget shows nothing with no
//! indication why. Diagnostics go to stderr.
//!
//! A FAILURE STILL PRINTS VALID JSON. The Python scripts mostly got this right
//! by accident, with a bare `print("[]")` in an except block. Getting it wrong
//! looks identical to getting it right until something fails: the widget is
//! empty either way, but one of them can say so.
//!
//! So the convention is an envelope. A list endpoint returns its list; an
//! endpoint that can fail returns `{"ok": false, "error": "..."}` and the QML
//! reads `.error`. Which one each endpoint uses is fixed by its existing
//! callers, not chosen here -- these functions just make each easy to do.

use serde::Serialize;
use std::io::{self, Write};
use std::process::ExitCode;

/// Print a value as JSON on stdout.
///
/// A broken pipe is not an error worth reporting: it means the caller stopped
/// reading, which QML does routinely when a widget closes mid-fetch.
pub fn json<T: Serialize>(value: &T) -> ExitCode {
    let text = match serde_json::to_string(value) {
        Ok(t) => t,
        Err(e) => {
            // Serialising our own output failed, which is a bug rather than a
            // condition. Still emit something parseable so the caller sees an
            // error rather than a parse failure.
            eprintln!("error: could not serialise output: {e}");
            println!("{{\"ok\":false,\"error\":\"internal serialisation failure\"}}");
            return ExitCode::FAILURE;
        }
    };

    let stdout = io::stdout();
    let mut lock = stdout.lock();
    match writeln!(lock, "{text}").and_then(|()| lock.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: could not write output: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Print an empty JSON array and succeed.
///
/// The "there is nothing to show, and that is not a failure" answer. An
/// unplugged device, an empty clipboard, a service that is not running.
pub fn empty_list() -> ExitCode {
    json(&Vec::<u8>::new())
}

/// Print a failure envelope on stdout and the reason on stderr.
///
/// Both, deliberately. The widget needs structured JSON it can render; a person
/// running the helper in a terminal needs a line they can read.
pub fn failure(message: impl std::fmt::Display) -> ExitCode {
    let text = message.to_string();
    eprintln!("error: {text}");
    let _ = json(&serde_json::json!({ "ok": false, "error": text }));
    ExitCode::FAILURE
}

/// Print a success envelope, for actions that do something rather than report.
pub fn success() -> ExitCode {
    json(&serde_json::json!({ "ok": true }))
}

/// Print a success envelope carrying a message.
pub fn success_with(message: impl std::fmt::Display) -> ExitCode {
    json(&serde_json::json!({ "ok": true, "message": message.to_string() }))
}

/// Report an unrecognised subcommand consistently.
///
/// Named verbs rather than a generic "usage": these are called from QML with a
/// verb chosen at a call site, and the useful message when one is wrong is the
/// list of ones that are right.
///
/// EXIT 2, NOT 1 -- and for exactly the argument [`unknown_flag`] makes below.
///
/// This returned `ExitCode::FAILURE`. So one policy -- "this program cannot
/// parse what it was given, so it will not run" -- had two statuses depending
/// on which half of the argument list held the typo: `qs-bluetooth-devices
/// --lst` answered 2 through [`unknown_flag`] and `qs-bluetooth-devices lst`
/// answered 1 through here (the network popup's Bluetooth helper then, since
/// replaced by jayos-bluetooth), from the same parse of the same argv, and a
/// wrapper could not tell the second from a real failure. `failure` returns 1
/// to mean "I ran, and the answer is that it failed"; an unknown verb ran
/// nothing and has no answer, which is the 2 that jayos-check, install.sh,
/// update.sh, bootstrap.sh and [`unknown_flag`] all already spell.
///
/// The envelope on stdout stays. Two call sites choose it deliberately --
/// jayos-wallpaper-search and jayos-steam-library both note that a caller reading
/// stdout should find an error rather than a parse failure -- and both say
/// only that the status is non-zero.
pub fn unknown_command(program: &str, given: &str, known: &[&str]) -> ExitCode {
    eprintln!("{program}: unknown command: {given}");
    eprintln!("{program}: expected one of: {}", known.join(", "));
    let _ = json(&serde_json::json!({
        "ok": false,
        "error": format!("unknown command: {given}"),
        "expected": known,
    }));
    ExitCode::from(REFUSED)
}

/// The exit status a helper answers with when it refuses an argument it does
/// not recognise -- a flag or a verb alike.
///
/// Named rather than written into [`unknown_flag`] as a literal, because
/// several other places have to agree with it: [`unknown_command`],
/// `jayos-localsend`, whose hand-rolled parser reports through [`unknown_flag`]
/// but whose sibling usage errors pick their own status, and the three helpers
/// that never had an `Args` at all. The argument for 2 is on [`unknown_flag`].
pub const REFUSED: u8 = 2;

/// Report an unrecognised option consistently, and refuse to run.
///
/// The same shape as [`unknown_command`], for the same reason: a flag that was
/// not recognised is a question the program cannot answer, and answering a
/// DIFFERENT question is the failure being avoided. `jayos-calendar-sync --chek`
/// fetched the calendar; `jayos-aur audit --dry-run` audited for real.
///
/// Spelled with the dashes on both lines because that is what was typed and
/// what should be typed instead; the caller declares its flags without them.
///
/// EXIT 2, NOT 1
///
/// This used to hand back `ExitCode::FAILURE`, and the four helpers that
/// hand-roll their parsers -- `jayos-controllers`, `jayos-steam-downloads`,
/// `jayos-focus-stats`, and `jayos-localsend port-free` -- all answered 2 for the
/// same refusal, so one policy had two statuses depending on which parser the
/// helper happened to use. 2 is the one that survives, because 1 is already
/// spoken for: [`failure`] returns it to mean "I ran, and the answer is that
/// it failed", and `jayos-localsend port-free` returns it to mean "the port is
/// taken". A refused flag is neither -- nothing was attempted, so there is no
/// answer to report, which is exactly what 2 means everywhere else in this
/// tree (see the `report` header in jayos-check for the same 0/1/2 contract).
/// It is also what argparse returned for a usage error, which is what the
/// shell callers of the hand-rolled four were written against.
///
/// Reached through `Args::reject_unknown` in jayos's `jayos-core` `cli.rs`, which decides whether
/// this program refuses at all. Not every helper does -- see the decision
/// table in `cli.rs`.
pub fn unknown_flag(program: &str, unknown: &[String], known: &[&str]) -> ExitCode {
    let given = with_dashes(unknown.iter().map(String::as_str));
    eprintln!("{program}: unknown option: {given}");
    eprintln!("{program}: {}", expected(known));
    // `expected` carries the dashes here where `unknown_command`'s carries bare
    // verbs, because in both cases it is the argv token to type instead -- and
    // for an option that token includes them.
    let dashed: Vec<String> = known.iter().map(|n| format!("--{n}")).collect();
    let _ = json(&serde_json::json!({
        "ok": false,
        "error": format!("unknown option: {given}"),
        "expected": dashed,
    }));
    ExitCode::from(REFUSED)
}

/// Say an option was not recognised, and let the program carry on.
///
/// For a helper a QML `Process` polls with a flag in the call site: stderr is
/// the only channel a widget does not consume, so this is the one place the
/// typo can be said WITHOUT changing the shape of stdout. Quickshell logs it,
/// which is where someone debugging the widget is already looking.
///
/// Nothing on stdout, deliberately. See the module header.
pub fn note_unknown_flag(program: &str, unknown: &[String], known: &[&str]) {
    let given = with_dashes(unknown.iter().map(String::as_str));
    eprintln!("{program}: unknown option: {given} (ignored); {}", expected(known));
}

fn with_dashes<'a>(names: impl Iterator<Item = &'a str>) -> String {
    names.map(|n| format!("--{n}")).collect::<Vec<_>>().join(", ")
}

/// The second line of both messages.
///
/// An empty list says so rather than printing "expected one of:" with nothing
/// after the colon, which reads as the message having been truncated.
///
/// "here" rather than "this program", because several helpers declare their
/// flags per verb: `jayos-aur audit` takes none while `jayos-aur upgrade` takes
/// `--force`, and telling someone the program has no options would be false.
fn expected(known: &[&str]) -> String {
    if known.is_empty() {
        return "no options are accepted here".to_string();
    }
    format!("expected one of: {}", with_dashes(known.iter().copied()))
}
