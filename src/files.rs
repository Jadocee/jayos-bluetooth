//! Where received files go, and what they may be called.
//!
//! The name in an incoming push is chosen by the sending device, so it is
//! treated as hostile: `../../.bashrc` must land as `bashrc` inside the
//! receive folder, never beside it. obexd splits whatever path the agent
//! returns at its last '/' and writes there (obexd/src/manager.c,
//! `agent_reply`), so the agent's answer is the only place this can be
//! enforced.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

/// The longest name kept, in bytes. Under every Linux filesystem's 255 with
/// room for the ` (12)` a duplicate gets.
pub const MAX_NAME: usize = 200;

/// Characters that reorder text on screen without being visible. Dropped with
/// the control characters because a name like "photo\u{202E}gpj.exe" shows as
/// "photoexe.jpg" in the prompt that asks whether to accept it.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// A device-chosen file name made safe to create inside the receive folder.
pub fn sanitise(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String =
        base.chars().filter(|c| !c.is_control() && !is_bidi_control(*c)).collect();
    let trim = |s: &str| s.trim_matches(|c: char| c == '.' || c.is_whitespace()).to_string();
    let mut out = trim(&cleaned);
    if out.len() > MAX_NAME {
        let mut end = MAX_NAME;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        // The cut can leave a trailing dot or space that the trim removed
        // from the original.
        out = trim(&out);
    }
    // Trimming dots already rules out "." and ".."; empty is what is left of
    // a name that was nothing else.
    if out.is_empty() {
        out = "file".into();
    }
    out
}

/// `dir/name`, or `dir/stem (1).ext`, `(2)`... when that is taken. `taken`
/// answers for the filesystem and for paths promised to transfers still in
/// flight.
pub fn unique_path(dir: &Path, name: &str, taken: impl Fn(&Path) -> bool) -> PathBuf {
    let first = dir.join(name);
    if !taken(&first) {
        return first;
    }
    // The extension starts at the last dot, unless that dot is the first
    // character -- sanitise has trimmed those, but this is public.
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let mut n = 1u32;
    loop {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// What the receive folder is resolved from, gathered by the caller so the
/// resolution can be tested without touching the process environment.
#[derive(Debug, Clone, Default)]
pub struct DirEnv {
    pub home: PathBuf,
    pub xdg_download_dir: Option<String>,
    pub xdg_config_home: Option<PathBuf>,
}

impl DirEnv {
    pub fn from_process() -> DirEnv {
        let nonempty = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        DirEnv {
            home: crate::support::paths::home(),
            xdg_download_dir: nonempty("XDG_DOWNLOAD_DIR"),
            xdg_config_home: nonempty("XDG_CONFIG_HOME").map(PathBuf::from),
        }
    }
}

/// `~/x` and a bare `~` expand to the home directory; a relative path is
/// taken from the home directory too, because the daemon's working directory
/// is wherever the shell happened to start it.
pub fn expand_home(text: &str, home: &Path) -> PathBuf {
    if text == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return home.join(rest);
    }
    let p = PathBuf::from(text);
    if p.is_absolute() {
        p
    } else {
        home.join(p)
    }
}

/// `XDG_DOWNLOAD_DIR` out of `user-dirs.dirs`, whose format is shell
/// assignments of either `"$HOME/..."` or an absolute path
/// (xdg-user-dirs' own description of the file).
pub fn download_dir_from_user_dirs(text: &str, home: &Path) -> Option<PathBuf> {
    for line in text.lines() {
        let line = line.trim();
        let Some(value) = line.strip_prefix("XDG_DOWNLOAD_DIR=") else { continue };
        let value = value.trim();
        let value = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value);
        let value = value.replace("\\\"", "\"").replace("\\\\", "\\");
        let expanded = if let Some(rest) =
            value.strip_prefix("$HOME").or_else(|| value.strip_prefix("${HOME}"))
        {
            let rest = rest.trim_start_matches('/');
            if rest.is_empty() {
                home.to_path_buf()
            } else {
                home.join(rest)
            }
        } else {
            PathBuf::from(&value)
        };
        if expanded.is_absolute() {
            return Some(expanded);
        }
    }
    None
}

/// The folder received files are saved to: the configured one, else the XDG
/// download directory, else `~/Downloads`.
pub fn receive_dir(configured: &str, env: &DirEnv) -> PathBuf {
    let configured = configured.trim();
    if !configured.is_empty() {
        return expand_home(configured, &env.home);
    }
    if let Some(d) = &env.xdg_download_dir {
        return expand_home(d, &env.home);
    }
    let config = env.xdg_config_home.clone().unwrap_or_else(|| env.home.join(".config"));
    if let Ok(text) = std::fs::read_to_string(config.join("user-dirs.dirs")) {
        if let Some(d) = download_dir_from_user_dirs(&text, &env.home) {
            return d;
        }
    }
    env.home.join("Downloads")
}

/// Make the receive folder if it is missing, private: what arrives over
/// Bluetooth is nobody else's business. An existing folder keeps its mode.
pub fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// A `file://` URI from the portal, as a path. Any other scheme, a host other
/// than this one, a malformed escape or a relative path is refused: the
/// portal only hands out local files, so anything else is not one.
pub fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = if rest.starts_with('/') { rest } else { rest.strip_prefix("localhost")? };
    if !rest.starts_with('/') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    // A decoded NUL cannot be in a path and would truncate it in obexd.
    if out.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_is_reduced_to_the_last_component() {
        assert_eq!(sanitise("../../etc/passwd"), "passwd");
        assert_eq!(sanitise("../../evil.txt"), "evil.txt");
        assert_eq!(sanitise("/etc/passwd"), "passwd");
        assert_eq!(sanitise("..\\..\\Windows\\win.ini"), "win.ini");
        assert_eq!(sanitise("a/b\\c"), "c");
    }

    #[test]
    fn dots_and_spaces_are_trimmed_and_never_name_a_directory() {
        assert_eq!(sanitise(".."), "file");
        assert_eq!(sanitise("."), "file");
        assert_eq!(sanitise("..."), "file");
        assert_eq!(sanitise("../.."), "file");
        assert_eq!(sanitise(" .bashrc "), "bashrc");
        assert_eq!(sanitise("photo.jpg."), "photo.jpg");
        assert_eq!(sanitise("  report final.pdf  "), "report final.pdf");
        assert_eq!(sanitise("a..b"), "a..b", "inner dots are a name, not a path");
    }

    #[test]
    fn empty_and_invisible_names_become_file() {
        assert_eq!(sanitise(""), "file");
        assert_eq!(sanitise("/"), "file");
        assert_eq!(sanitise("dir/"), "file");
        assert_eq!(sanitise("\u{1}\u{2}"), "file");
    }

    #[test]
    fn control_and_reordering_characters_are_dropped() {
        assert_eq!(sanitise("a\nb\tc\u{7f}.txt"), "abc.txt");
        assert_eq!(sanitise("photo\u{202E}gpj.exe"), "photogpj.exe");
        assert_eq!(sanitise("x\u{2066}y"), "xy");
    }

    #[test]
    fn long_names_are_cut_on_a_character_boundary() {
        let long = "é".repeat(300); // 600 bytes
        let s = sanitise(&long);
        assert!(s.len() <= MAX_NAME);
        assert_eq!(s.len(), MAX_NAME);
        assert!(s.chars().all(|c| c == 'é'));
        let odd = format!("a{}", "€".repeat(100)); // 1 + 300 bytes
        let s = sanitise(&odd);
        assert!(s.len() <= MAX_NAME && s.is_char_boundary(s.len()));
        assert_eq!(s.len(), 199);
        let dotted = format!("{}.{}", "a".repeat(199), "b".repeat(50));
        assert_eq!(sanitise(&dotted), "a".repeat(199), "a cut that ends in a dot is trimmed");
    }

    #[test]
    fn an_existing_name_gets_a_number_before_its_extension() {
        let dir = Path::new("/r");
        let taken = ["/r/photo.jpg", "/r/photo (1).jpg", "/r/notes", "/r/a.tar.gz"];
        let is_taken = |p: &Path| taken.iter().any(|t| Path::new(t) == p);
        assert_eq!(unique_path(dir, "new.txt", is_taken), PathBuf::from("/r/new.txt"));
        assert_eq!(unique_path(dir, "photo.jpg", is_taken), PathBuf::from("/r/photo (2).jpg"));
        assert_eq!(unique_path(dir, "notes", is_taken), PathBuf::from("/r/notes (1)"));
        assert_eq!(unique_path(dir, "a.tar.gz", is_taken), PathBuf::from("/r/a.tar (1).gz"));
    }

    #[test]
    fn unique_path_is_checked_against_the_real_filesystem_too() {
        let dir = std::env::temp_dir().join(format!("qs-bluetooth-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.txt"), b"").unwrap();
        assert_eq!(unique_path(&dir, "x.txt", |p| p.exists()), dir.join("x (1).txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn env(home: &str) -> DirEnv {
        DirEnv { home: PathBuf::from(home), ..DirEnv::default() }
    }

    #[test]
    fn a_configured_folder_wins_and_tilde_is_expanded() {
        let e = DirEnv { xdg_download_dir: Some("/dl".into()), ..env("/home/u") };
        assert_eq!(receive_dir("~/Bluetooth", &e), PathBuf::from("/home/u/Bluetooth"));
        assert_eq!(receive_dir("~", &e), PathBuf::from("/home/u"));
        assert_eq!(receive_dir("/srv/in", &e), PathBuf::from("/srv/in"));
        assert_eq!(receive_dir("In", &e), PathBuf::from("/home/u/In"));
        assert_eq!(receive_dir("  ", &e), PathBuf::from("/dl"), "blank is unset");
    }

    #[test]
    fn the_environment_comes_before_user_dirs() {
        let e = DirEnv { xdg_download_dir: Some("~/Fetched".into()), ..env("/home/u") };
        assert_eq!(receive_dir("", &e), PathBuf::from("/home/u/Fetched"));
    }

    #[test]
    fn user_dirs_is_read_with_home_expanded() {
        let home = Path::new("/home/u");
        let text = "# written by xdg-user-dirs-update\nXDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOWNLOAD_DIR=\"$HOME/Téléchargements\"\n";
        assert_eq!(
            download_dir_from_user_dirs(text, home),
            Some(PathBuf::from("/home/u/Téléchargements"))
        );
        assert_eq!(
            download_dir_from_user_dirs("XDG_DOWNLOAD_DIR=\"/data/dl\"", home),
            Some(PathBuf::from("/data/dl"))
        );
        assert_eq!(
            download_dir_from_user_dirs("XDG_DOWNLOAD_DIR=\"$HOME/\"", home),
            Some(PathBuf::from("/home/u"))
        );
        assert_eq!(
            download_dir_from_user_dirs("XDG_DOWNLOAD_DIR=\"${HOME}/x\"", home),
            Some(PathBuf::from("/home/u/x"))
        );
        assert_eq!(download_dir_from_user_dirs("XDG_DOWNLOAD_DIR=\"relative\"", home), None);
        assert_eq!(download_dir_from_user_dirs("#XDG_DOWNLOAD_DIR=\"/x\"", home), None);
    }

    #[test]
    fn user_dirs_is_found_under_xdg_config_home_and_home_downloads_is_last() {
        let root =
            std::env::temp_dir().join(format!("qs-bluetooth-userdirs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("cfg")).unwrap();
        std::fs::write(root.join("cfg/user-dirs.dirs"), "XDG_DOWNLOAD_DIR=\"$HOME/Incoming\"\n")
            .unwrap();
        let e = DirEnv { xdg_config_home: Some(root.join("cfg")), ..env("/home/u") };
        assert_eq!(receive_dir("", &e), PathBuf::from("/home/u/Incoming"));
        let e = DirEnv { xdg_config_home: Some(root.join("missing")), ..env("/home/u") };
        assert_eq!(receive_dir("", &e), PathBuf::from("/home/u/Downloads"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_new_receive_folder_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("qs-bluetooth-recv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("a/b");
        ensure_dir(&dir).unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        ensure_dir(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_uris_decode_and_nothing_else_does() {
        assert_eq!(
            file_uri_to_path("file:///home/u/My%20File.txt"),
            Some(PathBuf::from("/home/u/My File.txt"))
        );
        assert_eq!(file_uri_to_path("file://localhost/tmp/a"), Some(PathBuf::from("/tmp/a")));
        assert_eq!(
            file_uri_to_path("file:///t/%C3%A9t%C3%A9.jpg"),
            Some(PathBuf::from("/t/été.jpg"))
        );
        let raw = file_uri_to_path("file:///t/%FF").unwrap();
        assert_eq!(
            raw.as_os_str().as_encoded_bytes(),
            b"/t/\xff",
            "a name that is not UTF-8 survives"
        );
        assert_eq!(file_uri_to_path("https://example.com/a"), None);
        assert_eq!(file_uri_to_path("file://otherhost/a"), None);
        assert_eq!(file_uri_to_path("file:///a%2"), None);
        assert_eq!(file_uri_to_path("file:///a%zz"), None);
        assert_eq!(file_uri_to_path("file:///a%00b"), None);
        assert_eq!(file_uri_to_path("/plain/path"), None);
    }
}
