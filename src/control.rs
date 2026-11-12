//! The control socket: where it lives, who may hold it, who may connect.
//!
//! THE LOCK IS NOT THE SOCKET. A socket file left by a daemon that was killed
//! looks exactly like one a live daemon is listening on, and "unlink it and
//! bind again" is how two daemons come to be running -- two pairing agents
//! racing for one prompt. `daemon.lock`, held with `flock` for the life of the
//! process, is the mutex; only its holder may conclude that a socket is
//! debris. This is `rust/crates/jayos-video/src/control.rs`'s arrangement, and
//! its comments carry the history of each step; the code is repeated rather
//! than shared because a crate dependency for one function would link
//! jayos-video's dependencies into this one. The one departure is that a second
//! daemon waits a little for the lock (`LOCK_GRACE`) instead of none, because
//! this daemon's owner is the shell, and a reload overlaps two of them.

use std::ffi::CString;
use std::fs;
use std::io::ErrorKind;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const SOCKET: &str = "control.sock";
pub const LOCK: &str = "daemon.lock";

/// How long a daemon waits for another to let go of the lock before it
/// reports that one is running.
///
/// A shell reload starts the new generation's daemon while the old one can
/// still be alive: Quickshell kills the old Process's child from the Process
/// destructor (v0.3.1 src/io/process.cpp:31-38, read, not run), and nothing
/// orders that destruction before the new Process starts. With no wait, the
/// new daemon lost that race whenever it came first, said `fatal`, and the
/// shell answered no pairing request until its restart timer came round. A
/// killed process lets go of its locks as it is torn down, far inside five
/// seconds; five is still short enough that a daemon which really is
/// somebody else's is reported while someone is looking.
/// `config/quickshell/services/BluetoothService.qml` waits ten seconds after
/// a `fatal`, a figure chosen to be past this one.
pub const LOCK_GRACE: Duration = Duration::from_secs(5);

/// How often the lock is tried again within the grace: a tenth of a second is
/// too short a gap for anyone to notice in a reload's handover, and a second
/// daemon that waits out the whole grace makes fifty attempts, not a busy loop.
const LOCK_POLL: Duration = Duration::from_millis(100);

/// `$XDG_RUNTIME_DIR/quickshell/bluetooth`.
pub fn run_dir() -> PathBuf {
    crate::support::paths::run_dir("bluetooth")
}

/// Create `path` as a private directory, or make an existing one private.
/// A symlink is refused rather than followed, and so is a directory another
/// user owns: chmodding either would hand this daemon's socket to whoever
/// put it there. jayos-video's copy explains the errno pair below.
pub fn ensure_private_dir(path: &Path) -> std::io::Result<fs::File> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    let dir = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(dir) => dir,
        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) => {
            return Err(std::io::Error::new(
                ErrorKind::PermissionDenied,
                format!("{} is a symbolic link or not a directory", path.display()),
            ));
        }
        Err(e) => return Err(e),
    };
    // SAFETY: getuid has no preconditions and cannot fail.
    let ours = unsafe { libc::getuid() };
    let owner = dir.metadata()?.uid();
    if owner != ours {
        return Err(std::io::Error::new(
            ErrorKind::PermissionDenied,
            format!("{} belongs to uid {owner}, not {ours}", path.display()),
        ));
    }
    dir.set_permissions(fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

fn c_name(name: &str) -> std::io::Result<CString> {
    CString::new(name)
        .map_err(|_| std::io::Error::new(ErrorKind::InvalidInput, "nul in a file name"))
}

/// The right to be the daemon, and the listening socket that comes with it.
pub struct Bound {
    pub listener: UnixListener,
    _lock: fs::File,
    dir: fs::File,
    name: CString,
    ino: libc::ino_t,
}

impl Bound {
    /// Become the daemon in `run`, waiting up to `grace` for another daemon to
    /// let go of the lock. An error of kind `AddrInUse` means another daemon
    /// still held it when `grace` ran out. The daemon passes `LOCK_GRACE`; the
    /// parameter exists so the tests can wait for less.
    ///
    /// Every operation after `ensure_private_dir` goes through the directory
    /// descriptor it returned (`openat`, `fstatat`, `unlinkat`, `fchmodat`,
    /// and a bind through `/proc/self/fd`), so swapping the directory after
    /// the check cannot redirect any of them.
    pub fn take_in(run: &Path, grace: Duration) -> std::io::Result<Bound> {
        let dir = ensure_private_dir(run)?;
        let dir_fd = dir.as_raw_fd();

        let c_lock = c_name(LOCK)?;
        // SAFETY: dir_fd is a valid directory descriptor; c_lock outlives the call.
        let lock_fd = unsafe {
            libc::openat(
                dir_fd,
                c_lock.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_CLOEXEC,
                0o600,
            )
        };
        if lock_fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: openat returned a descriptor this function now owns.
        let lock = unsafe { fs::File::from_raw_fd(lock_fd) };
        // LOCK_NB, tried again until the deadline, rather than a blocking
        // flock: a blocking flock cannot be given a deadline without a signal
        // to interrupt it, and a second daemon that waited for ever would sit
        // behind a live first one with the shell hearing nothing. Every
        // failure is waited on, not only EWOULDBLOCK, and ends as AddrInUse,
        // because the caller carries on without a socket for any other error,
        // and a daemon without the lock is the two-agent case this file exists
        // to prevent. The bind stays below, after the lock: only the holder
        // may decide that a socket is debris.
        let deadline = Instant::now() + grace;
        // SAFETY: flock's only precondition is a valid descriptor.
        while unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(std::io::Error::new(
                    ErrorKind::AddrInUse,
                    "another jayos-bluetooth daemon is running",
                ));
            }
            std::thread::sleep(left.min(LOCK_POLL));
        }

        // Holding the lock, a leftover socket is certainly debris -- but only
        // a socket is removed; anything else at the name stops the daemon.
        let c_socket = c_name(SOCKET)?;
        // SAFETY: an all-zero stat is a valid value to be overwritten.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: dir_fd valid, c_socket outlives the call, stat is live.
        let rc = unsafe {
            libc::fstatat(dir_fd, c_socket.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW)
        };
        if rc == 0 {
            if (stat.st_mode & libc::S_IFMT) != libc::S_IFSOCK {
                return Err(std::io::Error::new(
                    ErrorKind::AlreadyExists,
                    format!("{} exists and is not a socket", run.join(SOCKET).display()),
                ));
            }
            // SAFETY: as above.
            unsafe { libc::unlinkat(dir_fd, c_socket.as_ptr(), 0) };
        } else {
            let e = std::io::Error::last_os_error();
            if e.kind() != ErrorKind::NotFound {
                return Err(e);
            }
        }

        let listener = UnixListener::bind(format!("/proc/self/fd/{dir_fd}/{SOCKET}"))?;
        // After the bind, because bind applies the umask and there is no
        // create-with-mode for a socket; the 0700 directory already keeps
        // everyone else out in the meantime.
        // SAFETY: dir_fd valid, c_socket outlives the call.
        if unsafe { libc::fchmodat(dir_fd, c_socket.as_ptr(), 0o600, 0) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: as for `stat` above.
        let mut made: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: dir_fd valid, c_socket outlives the call, made is live.
        if unsafe { libc::fstatat(dir_fd, c_socket.as_ptr(), &mut made, libc::AT_SYMLINK_NOFOLLOW) }
            != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Bound { listener, _lock: lock, dir, name: c_socket, ino: made.st_ino })
    }
}

impl Drop for Bound {
    /// Unlink the socket only if it is still the one the bind made.
    fn drop(&mut self) {
        let dir_fd = self.dir.as_raw_fd();
        // SAFETY: an all-zero stat is a valid value to be overwritten.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: dir_fd is owned by self; the name outlives the call.
        let rc = unsafe {
            libc::fstatat(dir_fd, self.name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW)
        };
        if rc == 0 && (stat.st_mode & libc::S_IFMT) == libc::S_IFSOCK && stat.st_ino == self.ino {
            // SAFETY: as above.
            unsafe { libc::unlinkat(dir_fd, self.name.as_ptr(), 0) };
        }
    }
}

/// The uid on the other end of a connection.
///
/// The 0700 directory already keeps other users out; this is the second
/// lock, for a directory whose mode somebody loosened, because the socket
/// answers pairing prompts and a prompt answered by another account is a
/// device paired without the owner's consent.
pub fn peer_uid(stream: &UnixStream) -> Option<u32> {
    // SAFETY: an all-zero ucred is a valid value to be overwritten.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the descriptor is a live socket; cred and len are live and sized.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (rc == 0).then_some(cred.uid)
}

/// Connect to the daemon in `run`.
pub fn connect(run: &Path) -> std::io::Result<UnixStream> {
    UnixStream::connect(run.join(SOCKET))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No grace: the tests that are not about the wait should not pay for it.
    const NOW: Duration = Duration::ZERO;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("qs-bluetooth-control-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn one_daemon_holds_the_socket_and_a_second_is_refused() {
        let run = scratch("lock");
        let first = Bound::take_in(&run, NOW).unwrap();
        let mode = |p: &Path| fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&run), 0o700);
        assert_eq!(mode(&run.join(SOCKET)), 0o600);
        let err = Bound::take_in(&run, NOW).err().expect("a second daemon is refused");
        assert_eq!(err.kind(), ErrorKind::AddrInUse);
        let client = connect(&run).unwrap();
        // SAFETY: getuid cannot fail.
        assert_eq!(peer_uid(&client), Some(unsafe { libc::getuid() }));
        drop(first);
        assert!(!run.join(SOCKET).exists(), "the socket goes with the daemon");
        let _ = fs::remove_dir_all(&run);
    }

    #[test]
    fn a_lock_let_go_within_the_grace_is_taken() {
        let run = scratch("handover");
        let first = Bound::take_in(&run, NOW).unwrap();
        let hold = Duration::from_millis(300);
        let started = Instant::now();
        let first_goes = std::thread::spawn(move || {
            std::thread::sleep(hold);
            drop(first);
        });
        let second = Bound::take_in(&run, LOCK_GRACE).expect("taken once the first lets go");
        assert!(started.elapsed() >= hold, "taken while the first still held it");
        first_goes.join().unwrap();
        assert!(connect(&run).is_ok(), "the second daemon's socket answers");
        drop(second);
        let _ = fs::remove_dir_all(&run);
    }

    #[test]
    fn the_grace_runs_out_and_the_second_is_refused() {
        let run = scratch("grace");
        let _first = Bound::take_in(&run, NOW).unwrap();
        let grace = Duration::from_millis(300);
        let started = Instant::now();
        let err = Bound::take_in(&run, grace).err().expect("refused once the grace is spent");
        let waited = started.elapsed();
        assert_eq!(err.kind(), ErrorKind::AddrInUse);
        assert!(waited >= grace, "gave up after {waited:?}, inside a {grace:?} grace");
        assert!(waited < LOCK_GRACE, "waited {waited:?}: the grace passed in was not the one used");
        assert!(connect(&run).is_ok(), "the first daemon's socket is untouched");
        let _ = fs::remove_dir_all(&run);
    }

    #[test]
    fn a_stale_socket_is_replaced_and_anything_else_is_not() {
        let run = scratch("stale");
        fs::create_dir_all(&run).unwrap();
        // A socket with no listener: what a killed daemon leaves.
        drop(UnixListener::bind(run.join(SOCKET)).unwrap());
        assert!(run.join(SOCKET).exists());
        let b = Bound::take_in(&run, NOW).unwrap();
        assert!(connect(&run).is_ok());
        drop(b);
        fs::write(run.join(SOCKET), b"not a socket").unwrap();
        let err = Bound::take_in(&run, NOW).err().expect("a regular file is not debris");
        assert_eq!(err.kind(), ErrorKind::AlreadyExists);
        assert_eq!(fs::read(run.join(SOCKET)).unwrap(), b"not a socket");
        let _ = fs::remove_dir_all(&run);
    }

    #[test]
    fn a_symlinked_runtime_directory_is_refused() {
        let root = scratch("link");
        fs::create_dir_all(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        let err = Bound::take_in(&root.join("link"), NOW).err().expect("refused");
        assert_eq!(err.kind(), ErrorKind::PermissionDenied);
        let _ = fs::remove_dir_all(&root);
    }
}
