//! Output and paths, for `jayos-bluetooth`.
//!
//! WHY A COPY AND NOT A DEPENDENCY
//!
//! These two files are jayos's `rust/crates/jayos-core/src/{emit,paths}.rs`
//! at jayos commit cf8c928, copied whole with their tests, when the daemon
//! moved out of that tree on 2026-11-12 by the owner's call, "Move all
//! Bluetooth related stuff (except UI/QML) into jayos-bluetooth". A git
//! dependency on jayos's workspace would make this repository's build depend
//! on the one it moved out of -- the coupling the move exists to remove -- and
//! jayos-sony-xm5 and jayos-audio made the same choice for the same reason.
//!
//! `paths` is here for `run_dir("bluetooth")` and `home()`: the socket and
//! lock have to land where the shell's Paths.qml and paths.sh look for them,
//! `$XDG_RUNTIME_DIR/quickshell/bluetooth/`, so the copy keeps jayos's
//! resolution order exactly rather than a simplified one that would agree on
//! most machines. The one edit is a doc link in `emit` that named a jayos
//! module this copy does not carry.
//!
//! A fix to one copy is a fix to one copy: a change to either file in jayos
//! is carried here by hand, diffed from the commit above.
//!
//! `paths.rs` was carried again from jayos commit 7de4e12 on 2026-11-15: its
//! layout test cleared four of the seven names the module reads, so a desktop
//! session's QS_HYPR_DIR, which paths.sh exports into every terminal, failed
//! it on the owner's machine and refused jayos-bluetooth's first release. It
//! clears all seven now, and a second test holds the list to the code.

pub mod emit;
pub mod paths;
