# jayos-bluetooth

The Bluetooth manager behind the [JayOS](https://github.com/Jadocee/jayos)
shell: one daemon that talks to BlueZ and obexd over D-Bus -- adapters and
devices, pairing and connecting, a pairing agent that asks its client every
question BlueZ asks, files sent and received -- and speaks JSON lines to its
clients; and the WirePlumber and BlueZ settings a headset's audio depends on.
Built for JayOS, usable without it.

It moved here from JayOS on 2026-11-12, by the owner's call "Move all
Bluetooth related stuff (except UI/QML) into jayos-bluetooth from JayOS". The
page, the prompts, the popup and the bar's pill that draw it stayed there.
Nothing here has met a real adapter: what runs, runs against a mock BlueZ and
a mock obexd, and `docs/BLUETOOTH.md` says which claims stand on what.

## What is here

| | |
|---|---|
| `src/` | The daemon and its clients. `protocol`, `model`, `prompts`, `transfers`, `files`, `discovery` and `throttle` are pure and carry the unit tests; `agent`, `bus`, `portal` and `daemon` are the D-Bus half; `control` and `client` are the socket. `support/` is two files of JayOS's `jayos-core`, copied; its `mod.rs` says why a copy. |
| `src/bin/jayos-bluetooth.rs` | `daemon`, `watch`, `status` and `send <device> <file>...`. |
| `tests/daemon.rs` | The real binary against a mock BlueZ and a mock obexd, each on a `dbus-daemon` of its own -- never the machine's buses. |
| `share/wireplumber/` | Two WirePlumber 0.5 drop-ins for Bluetooth audio, installed to `/usr/share/wireplumber/wireplumber.conf.d/`. Each file says what it is for. |
| `share/bluetooth/main.conf` | BlueZ's settings, installed to `/usr/share/jayos-bluetooth/main.conf`; JayOS's installer puts it over `/etc/bluetooth/main.conf`, which bluez owns. |
| `contract/` | What JayOS tests its side against: the verbs, the protocol's ops, the packaging steps and every installed path. See `contract/README.md`. |
| `packaging/steps.sh` | Every build flag, proof and installed file, once, for both recipes. |
| `packaging/arch/PKGBUILD` | Builds and installs this checkout. |
| `tools/` | `check.sh`, everything checked in one run; `contract.sh`; and the release scripts. |
| `docs/` | `BLUETOOTH.md`, how the daemon works and what each claim stands on; `RELEASES.md`, how a release is made. |

## Installing

On Arch, from a checkout:

    cd packaging/arch && makepkg -si

That installs `/usr/bin/jayos-bluetooth`, the two drop-ins, `main.conf` and the
contract (`contract/files.txt` is the whole list). JayOS carries a recipe of
its own, `pkgbuilds/jayos-bluetooth/PKGBUILD` there, which builds a release of
this repository pinned by tag and by commit.

Without JayOS, `jayos-bluetooth daemon` runs the manager with its owner on
stdin and stdout; `jayos-bluetooth watch` is a client of the socket it opens
at `$XDG_RUNTIME_DIR/quickshell/bluetooth/control.sock`.

## Checking

    tools/check.sh

Runs a syntax check over every script, the contract against the code,
`cargo build`, `cargo test --locked` and `cargo fmt --check`, and every suite
in `tests/shell/`. Exit 0 is clean, 1 is a failure, 2 is "could not run" (no
cargo, no git) -- which is not the same answer as clean and is not reported as
one.

## Linking

The Rust is linked with [mold](https://github.com/rui314/mold), by JayOS's
owner's call of 2026-11-13. `packaging/steps.sh` appends `-C
link-arg=-fuse-ld=mold` to `RUSTFLAGS` for `build` and `check`, after
makepkg's own and once, and refuses both before cargo where `ld.mold` is not
found; both recipes name `mold` among their makedepends. `RUSTFLAGS` and not
a cargo config, because makepkg sets `RUSTFLAGS`, which cargo takes in place
of every config's flags. `.cargo/config.toml` gives cargo run in this tree
the same flag, and `tools/check.sh` links with Rust's own LLD where mold is
not installed, and counts that as not exercised.
`tests/shell/test-packaging.sh` holds each of these.

Run on 2026-11-13 under rustc 1.97.0 and mold 2.30.0: build in 1 min 21 s and
check in 23 s, `jayos-bluetooth` carrying `mold 2.30.0` in its `.comment`
section, and `tools/check.sh` found nothing to fix. Not run: Arch's
makepkg.conf and Arch's mold 3.0.0.

## Releases

A release is a signed tag, `vMAJOR.MINOR.PATCH`, on `main`, whose `VERSION`
says its version; `tools/release.sh` makes it, in the owner's checkout, and
the release workflow checks and publishes it. JayOS builds a release and
nothing else. `docs/RELEASES.md` has how, and the owner's steps before the
first one.

## Licence

MIT; see `LICENSE`.
