//! Where everything lives.
//!
//! The same layout Paths.qml and paths.sh describe, resolved the same way and
//! in the same order. Three implementations of one layout is two too many, but
//! the shell is QML, the keybinds are bash, and these are Rust -- so what
//! matters is that all three agree, and this file is written to be checked
//! against the other two rather than to be clever.
//!
//! Every value is overridable from the environment. That is not decoration: it
//! is how a second copy of the tree can be run side by side without touching
//! the installed one, which is the only safe way to try a change to the shell.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// `$HOME`, or `/` if the environment has no home -- which happens under a
/// systemd unit with an empty environment, and is worth not panicking over.
pub fn home() -> PathBuf {
    env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn env_dir(key: &str) -> Option<PathBuf> {
    match env::var_os(key) {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => None,
    }
}

/// `$XDG_CONFIG_HOME`, or `~/.config`.
pub fn config_root() -> PathBuf {
    env_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home().join(".config"))
}

/// `$XDG_RUNTIME_DIR`, or the nearest private thing to it.
///
/// The fallback was `/tmp`, which is what paths.sh used: world-writable and
/// shared, so a name under it could be made first by another account, and what
/// the helpers read back -- a PID, a socket, a state file the shell trusts --
/// would be theirs to choose. A helper that refuses to run without a runtime
/// directory is still worse than one that works, so there is still a fallback,
/// in the order paths.sh resolves it: logind's `/run/user/<euid>` when it is a
/// real directory and ours, which is the directory the session is using; and
/// with no session at all, `~/.local/state/quickshell-runtime`, made 0700.
/// That last outlives a reboot, which [`Kind::Run`] says a runtime directory
/// must not, and is the price of never being `/tmp`. paths.sh has the rest.
/// `QS_LOGIND_RUNTIME` stands in for `/run/user/<euid>`, as it does there.
pub fn runtime_root() -> PathBuf {
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    let logind =
        env_dir("QS_LOGIND_RUNTIME").unwrap_or_else(|| PathBuf::from(format!("/run/user/{euid}")));
    let root = choose_runtime_root(env_dir("XDG_RUNTIME_DIR"), &logind, euid, &home());
    if root == stand_in_runtime(&home()) {
        // Made here rather than by each caller, because each caller makes
        // only its own subdirectory, with the default mode.
        if fs::create_dir_all(&root).is_ok() {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&root, fs::Permissions::from_mode(0o700));
        }
    }
    root
}

fn stand_in_runtime(home: &Path) -> PathBuf {
    home.join(".local/state/quickshell-runtime")
}

/// The decision alone, so it can be tested without a session.
fn choose_runtime_root(xdg: Option<PathBuf>, logind: &Path, euid: u32, home: &Path) -> PathBuf {
    if let Some(dir) = xdg {
        return dir;
    }
    use std::os::unix::fs::MetadataExt;
    // symlink_metadata, so a link at that name is not followed to whatever it
    // points at: paths.sh refuses one with `! -L`.
    match fs::symlink_metadata(logind) {
        Ok(meta) if meta.is_dir() && meta.uid() == euid => logind.to_path_buf(),
        _ => stand_in_runtime(home),
    }
}

/// The QML tree.
///
/// `QS_SHELL_DIR` wins. Otherwise the running shell publishes its own resolved
/// root to `shell_root`, which is how a tree installed somewhere unusual is
/// found by a script started from a Hyprland keybind -- those do not inherit
/// the shell's environment.
///
/// The published path is only trusted if it still holds a `Shell.qml`. The file
/// outlives the process that wrote it, so a stale path from a tree that has
/// since moved would otherwise beat the default and send every helper at a
/// directory that is not there any more.
pub fn shell_dir() -> PathBuf {
    if let Some(dir) = env_dir("QS_SHELL_DIR") {
        return dir;
    }

    let published = runtime_root().join("quickshell/shell_root");
    if let Ok(text) = fs::read_to_string(&published) {
        let candidate = PathBuf::from(text.trim());
        if candidate.join("Shell.qml").is_file() {
            return candidate;
        }
    }

    config_root().join("quickshell")
}

/// The scripts both the shell and the Hyprland config call.
pub fn scripts_dir() -> PathBuf {
    env_dir("QS_SCRIPTS_DIR").unwrap_or_else(|| config_root().join("common/scripts"))
}

/// The Hyprland configuration.
pub fn hypr_dir() -> PathBuf {
    env_dir("QS_HYPR_DIR").unwrap_or_else(|| config_root().join("hypr"))
}

/// `settings.json`, which the shell reads and several helpers write.
pub fn settings_path() -> PathBuf {
    shell_dir().join("settings.json")
}

/// `colors.conf`, written by matugen and read by the Hyprland config.
pub fn colors_path() -> PathBuf {
    hypr_dir().join("colors.conf")
}

fn base(kind: Kind) -> PathBuf {
    match kind {
        Kind::Cache => home().join(".cache/quickshell"),
        Kind::State => home().join(".local/state/quickshell"),
        Kind::Run => runtime_root().join("quickshell"),
        Kind::Log => runtime_root().join("quickshell/logs"),
    }
}

/// Which of the four per-widget directories is wanted.
///
/// The distinction is not bookkeeping. Cache may be deleted at any time and
/// everything still works; state must survive a reboot; run must NOT survive a
/// reboot, because a stale pid file in it is how two daemons end up running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Cache,
    State,
    Run,
    Log,
}

impl Kind {
    fn env_prefix(self) -> &'static str {
        match self {
            Kind::Cache => "QS_CACHE_",
            Kind::State => "QS_STATE_",
            Kind::Run => "QS_RUN_",
            Kind::Log => "QS_LOG_",
        }
    }
}

/// A widget's directory of the given kind, created if it does not exist.
///
/// Created rather than merely named: every caller wanted it to exist, and the
/// three that forgot to say so wrote into a directory that was not there and
/// reported an IO error for what was really a missing mkdir.
///
/// The environment override is `QS_<KIND>_<WIDGET>` with the widget name
/// uppercased, matching Paths.qml's `_resolve`.
pub fn widget_dir(kind: Kind, widget: &str) -> PathBuf {
    let key = format!("{}{}", kind.env_prefix(), widget.to_uppercase());
    let dir = env_dir(&key).unwrap_or_else(|| base(kind).join(widget));
    let _ = fs::create_dir_all(&dir);
    dir
}

/// A widget's cache directory.
pub fn cache_dir(widget: &str) -> PathBuf {
    widget_dir(Kind::Cache, widget)
}

/// A widget's state directory.
pub fn state_dir(widget: &str) -> PathBuf {
    widget_dir(Kind::State, widget)
}

/// A widget's runtime directory.
pub fn run_dir(widget: &str) -> PathBuf {
    widget_dir(Kind::Run, widget)
}

/// A widget's log directory.
pub fn log_dir(widget: &str) -> PathBuf {
    widget_dir(Kind::Log, widget)
}

/// A path under the scripts directory.
pub fn script(rel: impl AsRef<Path>) -> PathBuf {
    scripts_dir().join(rel)
}

/// A path under the QML tree.
pub fn shell_file(rel: impl AsRef<Path>) -> PathBuf {
    shell_dir().join(rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Environment variables are process-global, so these run in one test to
    // keep them from racing each other under the default parallel runner.
    #[test]
    fn layout_follows_the_environment() {
        // SAFETY: single-threaded test, and nothing else in this process reads
        // these variables concurrently.
        unsafe {
            env::set_var("HOME", "/home/tester");
            env::remove_var("XDG_CONFIG_HOME");
            env::remove_var("QS_SHELL_DIR");
            env::remove_var("QS_SCRIPTS_DIR");
            env::remove_var("XDG_RUNTIME_DIR");
        }
        assert_eq!(config_root(), PathBuf::from("/home/tester/.config"));
        assert_eq!(scripts_dir(), PathBuf::from("/home/tester/.config/common/scripts"));
        // runtime_root() itself is not called here: with no runtime directory
        // it makes one under HOME. choose_runtime_root has its own tests below.
        unsafe { env::set_var("XDG_RUNTIME_DIR", "/run/user/4242") };
        assert_eq!(runtime_root(), PathBuf::from("/run/user/4242"));

        unsafe { env::set_var("XDG_CONFIG_HOME", "/elsewhere/config") };
        assert_eq!(config_root(), PathBuf::from("/elsewhere/config"));
        assert_eq!(hypr_dir(), PathBuf::from("/elsewhere/config/hypr"));

        // An empty value is not an override. Systemd units routinely export a
        // variable as "" and treating that as a path resolves everything to
        // the current directory.
        unsafe { env::set_var("XDG_CONFIG_HOME", "") };
        assert_eq!(config_root(), PathBuf::from("/home/tester/.config"));

        unsafe { env::set_var("QS_SHELL_DIR", "/opt/shell") };
        assert_eq!(shell_dir(), PathBuf::from("/opt/shell"));
        assert_eq!(settings_path(), PathBuf::from("/opt/shell/settings.json"));
    }

    // ── the runtime directory with no XDG_RUNTIME_DIR ────────────────────────

    fn scratch(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("qs-core-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn euid() -> u32 {
        // SAFETY: as in runtime_root.
        unsafe { libc::geteuid() }
    }

    #[test]
    fn the_variable_wins_whatever_else_exists() {
        let dir = scratch("variable");
        let chosen =
            choose_runtime_root(Some(PathBuf::from("/run/user/7")), &dir, euid(), Path::new("/h"));
        assert_eq!(chosen, PathBuf::from("/run/user/7"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn logind_directory_of_our_own_is_next() {
        let dir = scratch("logind");
        assert_eq!(choose_runtime_root(None, &dir, euid(), Path::new("/h")), dir);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn logind_directory_of_someone_else_is_passed_over() {
        let dir = scratch("other");
        let other = euid().wrapping_add(1);
        assert_eq!(
            choose_runtime_root(None, &dir, other, Path::new("/h")),
            PathBuf::from("/h/.local/state/quickshell-runtime")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_link_at_the_logind_name_is_not_followed() {
        let dir = scratch("link");
        let link = dir.join("user");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert_eq!(
            choose_runtime_root(None, &link, euid(), Path::new("/h")),
            PathBuf::from("/h/.local/state/quickshell-runtime")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn with_no_session_at_all_the_stand_in_and_never_tmp() {
        let missing = PathBuf::from("/nonexistent/jayos-core/run/user");
        let chosen = choose_runtime_root(None, &missing, euid(), Path::new("/h"));
        assert_eq!(chosen, PathBuf::from("/h/.local/state/quickshell-runtime"));
        assert!(!chosen.starts_with("/tmp"));
    }
}
