#!/usr/bin/env bash
#
# The build, check and install steps of the jayos-bluetooth package.
#
#   steps.sh fetch               cargo fetch, locked, for this host only
#   steps.sh build               the release build of jayos-bluetooth
#   steps.sh check               what it links, the crate's tests, and
#                                tests/shell/test-manager.sh over the build
#   steps.sh package <pkgdir>    install everything under <pkgdir>
#
# Every step works on the tree this script sits in. Exit 0 when the step did
# what it says, 1 when it did not, with a line on stderr saying why.
#
# WHY A SCRIPT, AND NOT THE STEPS WRITTEN INTO A RECIPE
#
# Two recipes build this package. packaging/arch/PKGBUILD builds the checkout
# it sits in; JayOS's pkgbuilds/jayos-bluetooth/PKGBUILD clones the release it
# pins -- a signed tag, and the commit that tag named when it was pinned
# (docs/RELEASES.md) -- and builds that. Two recipes that each spelled out the
# cargo flags and the file list would drift, and the drift that matters is
# silent: a drop-in installed by one recipe and not the other is a headset
# that stutters on one machine and not on the next, with nothing failing. So
# both call this, and the only thing either recipe owns is where its sources
# come from. jayos-audio's packaging/steps.sh is the model, for the same reason.
#
# The verbs and their arguments are the interface JayOS's recipe calls, and
# contract/steps.txt lists them; renaming one breaks JayOS's build. What
# `package` installs is contract/files.txt, which JayOS reads to know where the
# drop-ins and BlueZ's main.conf are, and tests/shell/test-packaging.sh holds
# this verb to that list exactly, both ways.
#
# NO TREE ARGUMENT, unlike jayos-audio's steps, whose every verb takes the
# plugin's clone: this package has one source, the tree this file is in, so
# `steps.sh build` from either recipe means the same thing.

set -uo pipefail

ROOT="$(cd "$(dirname "$(realpath "${BASH_SOURCE[0]}")")/.." && pwd)"
BIN=jayos-bluetooth

die() { echo "steps.sh: $*" >&2; exit 1; }

usage() {
    echo "usage: steps.sh fetch | build | check | package <pkgdir>" >&2
    exit 1
}

# The toolchain name is for rustup, which is what JayOS's toolchain stage
# installs; a plain rust package ignores it. CARGO_TARGET_DIR under the tree
# even when the building user set one globally, so check and package find the
# binary where they look and a `makepkg -C` cleans it.
cargo_env() {
    export RUSTUP_TOOLCHAIN=stable
    export CARGO_TARGET_DIR="$ROOT/target"
    [[ -f "$ROOT/Cargo.toml" && -f "$ROOT/Cargo.lock" ]] \
        || die "no Cargo.toml and Cargo.lock in $ROOT, so nothing can be built"
}

step_fetch() {
    local host
    cargo_env
    # The lockfile is the pin for everything the release's commit does not
    # cover: --locked refuses to resolve anything Cargo.lock does not already
    # name, and the host target keeps the fetch to what this machine builds,
    # so build and check can run --frozen where makepkg gives them no network.
    host=$(rustc -vV 2>/dev/null | sed -n 's/^host: //p')
    [[ -n "$host" ]] || die "rustc did not say which target this host is; is a toolchain selected? (rustup default stable)"
    ( cd "$ROOT" && cargo fetch --locked --target "$host" ) || die "cargo fetch failed"
}

step_build() {
    cargo_env
    # Cargo.toml's release profile, JayOS's settings when the crate lived
    # there: fat LTO, one codegen unit and panic = "abort", for a program whose
    # owner, the shell, restarts it when it ends.
    ( cd "$ROOT" && cargo build --frozen --release --bin "$BIN" ) || die "cargo build failed"
}

# What the binary needs at run time is what depends= installs, and nothing
# more: glibc's libc, libm and loader, and gcc-libs' libgcc_s for the
# unwinder std still links with panic = "abort". zbus is pure Rust, so no
# libdbus -- the reason it was chosen (Cargo.toml) -- and a NEEDED entry for
# one would be a dependency that arrived with a crate update unannounced. A
# program that links a library depends= does not install runs on the build
# machine and nowhere else. readelf is binutils', which base-devel brings and
# makepkg requires.
links_only_depends() { # links_only_depends <binary>
    local needed outside
    command -v readelf >/dev/null 2>&1 || die "readelf is not installed (binutils), so what $BIN links cannot be read"
    needed=$(readelf -d "$1" | sed -n 's/.*(NEEDED).*\[\(.*\)\]$/\1/p') || true
    if [[ -z "$needed" ]]; then
        echo "readelf read no NEEDED entry from ${1##*/}" >&2
        return 1
    fi
    outside=$(grep -vxE 'libc\.so\.6|libm\.so\.6|libgcc_s\.so\.1|ld-linux-x86-64\.so\.2|ld-linux-aarch64\.so\.1' <<<"$needed") || true
    if [[ -n "$outside" ]]; then
        echo "${1##*/} links what depends= does not install: ${outside//$'\n'/ }" >&2
        return 1
    fi
}

step_check() {
    local bin="$ROOT/target/release/$BIN" suite="$ROOT/tests/shell/test-manager.sh" status
    cargo_env
    [[ -x "$bin" ]] || die "no $bin; build first"
    links_only_depends "$bin" || exit 1
    # The dev profile's tests, not the release build's: tests ignore
    # panic = "abort", and fat LTO over a test harness would double the step
    # for nothing it tests. tests/daemon.rs starts the real binary against a
    # mock BlueZ and obexd, each on a dbus-daemon of its own -- never the
    # machine's buses, so a build on a machine with a headset connected
    # touches nothing of it.
    ( cd "$ROOT" && cargo test --frozen ) || die "the crate's tests failed"
    # And the release binary itself, run the way the shell runs it. Its 2 is
    # a failure here: the suite saying it examined nothing, over a binary that
    # was just built.
    [[ -f "$suite" ]] || die "no $suite to run the built binary against"
    JAYOS_BLUETOOTH_BIN="$bin" bash "$suite"
    status=$?
    (( status == 0 )) || die "tests/shell/test-manager.sh exited $status against the built $BIN"
}

step_package() {
    local pkgdir share f
    [[ -n "${1:-}" ]] || usage
    pkgdir=$1
    share="$pkgdir/usr/share"

    # /usr/bin itself: JayOS's shell runs it by that path (its paths.sh's
    # QS_PKG_BIN, and Paths.qml's pkgBinDir), as it runs jayos-audio's.
    install -Dm755 "$ROOT/target/release/$BIN" -t "$pkgdir/usr/bin/" \
        || die "could not install $BIN; build first"

    # WirePlumber's drop-ins, in its own package directory: read before /etc
    # and ~/.config, so that a person's file of the same name masks this one
    # and a file at a later name wins (each file says what it is for).
    while IFS= read -r f; do
        install -Dm644 "$ROOT/share/wireplumber/$f" "$share/wireplumber/$f" \
            || die "could not install share/wireplumber/$f"
    done < <(cd "$ROOT/share/wireplumber" && find . -type f -name '*.conf' | sed 's|^\./||' | LC_ALL=C sort)

    # BlueZ's main.conf, beside the contract and not at /etc/bluetooth: bluez
    # owns that path, and two packages cannot. JayOS's install.d/60-system.sh
    # installs this copy over it as root, keeping the original once, since
    # bluetoothd reads <configdir>/main.conf and nothing else (BlueZ's
    # src/main.c, load_config, read).
    install -Dm644 "$ROOT/share/bluetooth/main.conf" "$share/jayos-bluetooth/main.conf" \
        || die "could not install share/bluetooth/main.conf"

    # The contract, for a machine that wants to compare what it runs against
    # what a desktop built on this vendored.
    install -d "$share/jayos-bluetooth/contract" || die "could not make the contract directory"
    install -m644 "$ROOT"/contract/* "$share/jayos-bluetooth/contract/" || die "could not install the contract"

    install -Dm644 "$ROOT/LICENSE" -t "$share/licenses/jayos-bluetooth/" || die "could not install the licence"
}

case "${1:-}" in
    fetch)   step_fetch ;;
    build)   step_build ;;
    check)   step_check ;;
    package) step_package "${2:-}" ;;
    *)       usage ;;
esac
