#!/usr/bin/env bash
#
# The Bluetooth manager's contracts, exercised through the real binary.
#
#   tests/shell/test-manager.sh
#   JAYOS_BLUETOOTH_BIN=<binary> tests/shell/test-manager.sh
#
# The binary is target/release/jayos-bluetooth unless JAYOS_BLUETOOTH_BIN names
# another: packaging/steps.sh's check names the build it has just made. Moved
# here from jayos's tools/test-bluetooth-manager.sh with the daemon on
# 2026-11-12; jayos's installer ran it over its own build until then.
#
# WHY THIS EXISTS, NEXT TO THE RUST TESTS
#
# `cargo test` holds the daemon to a mock BlueZ and a mock obexd, but the
# shell does not
# talk to a test harness: it runs `jayos-bluetooth daemon` and reads stdout, and
# the settings page runs `jayos-bluetooth watch` and reads ITS stdout. The page
# takes any end of `watch` as the daemon gone and shows the envelope's
# `error` as the reason; only exit 127, or a watch that never started, reads
# as "not installed". It never looks for exit 2. That status is for the
# callers that do -- a script running `status`, a file manager's action
# running `send` -- which must tell "no daemon answered" from `send`'s 1, a
# file that did not arrive. A daemon that printed a state missing a key would
# leave the page reading `undefined`, an envelope with no `error` would leave
# it no reason to give, and a client that answered a missing daemon with 1
# would tell such a script that a file failed when none was offered -- and
# only the installed binary, run the way the shell runs it, shows any of
# them. So this drives the release build: with no buses at all, and on a
# private bus with no BlueZ behind it.
#
# WHAT IT CANNOT CHECK: anything that needs bluetoothd or obexd. Pairing,
# connecting and file transfer are `cargo test`'s (tests/daemon.rs), against
# mocks, and against a real adapter nobody's; they are named at the end rather
# than skipped silently.

set -uo pipefail

SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${JAYOS_BLUETOOTH_BIN:-$SRC/target/release/jayos-bluetooth}"
MAIN="$SRC/src/bin/jayos-bluetooth.rs"

# Exit 2 at the TOP, before any assertion: nothing was examined, so nothing
# may be reported clean. tools/check.sh reports a 2 as "not exercised", and
# packaging/steps.sh's check as a failure, since it has just built one.
if [[ ! -x "$BIN" ]]; then
    echo "  test-manager not exercised: $BIN is not built: run cargo build --release --locked" >&2
    exit 2
fi

PASS=0
FAIL=0
SKIP=0
pass() { PASS=$((PASS + 1)); printf '  \033[32mok\033[0m   %s\n' "$1"; }
bad()  { FAIL=$((FAIL + 1)); printf '  \033[31mfail\033[0m %s\n' "$1"; }
# A case that could not RUN: counted, two spaces and no colour, so it matches
# the `^  skip ` tools/check.sh and jayos's skips_within() both grep for.
skip() { printf '  skip %s\n' "$1"; SKIP=$((SKIP + 1)); }
check() {
    if [[ "$2" == "$3" ]]; then pass "$1"; else
        bad "$1"
        printf '         expected: %s\n' "$2"
        printf '         actual:   %s\n' "$3"
    fi
}

WORK="$(mktemp -d "${TMPDIR:-/tmp}/jayos-bluetooth-test-manager.XXXXXX")"
# The private bus's socket lives in a directory of its own with a short name:
# a unix socket path is limited to 108 bytes, and TMPDIR need not be short.
SOCKDIR=""
BUS_PID=""
DAEMON_PID=""
cleanup() {
    [[ -n "$DAEMON_PID" ]] && kill "$DAEMON_PID" 2>/dev/null
    [[ -n "$BUS_PID" ]] && kill "$BUS_PID" 2>/dev/null
    [[ -n "$SOCKDIR" ]] && rm -rf "$SOCKDIR"
    rm -rf "$WORK"
}
trap cleanup EXIT

# Every run below gets its own runtime directory and buses that do not exist,
# so nothing here can reach the machine's own bluetoothd or a running daemon.
NOBUS="unix:path=/nonexistent/jayos-bluetooth-test-manager"
# BT_TIMEOUT, when set, bounds the run: `watch` outlives its stdin by design.
run_bt() { # run_bt <runtime dir> <args>...
    local run="$1"; shift
    env -u QS_RUN_BLUETOOTH -u XDG_DOWNLOAD_DIR XDG_RUNTIME_DIR="$run" HOME="$WORK/home" \
        DBUS_SYSTEM_BUS_ADDRESS="${SYSTEM_BUS:-$NOBUS}" DBUS_SESSION_BUS_ADDRESS="${SESSION_BUS:-$NOBUS}" \
        ${BT_TIMEOUT:+timeout "$BT_TIMEOUT"} "$BIN" "$@"
}

# The keys every state carries. A missing one is `undefined` in a QML
# binding, and the page would draw it.
STATE_KEYS='["adapters","agent","agentError","bluez","devices","event","files","obex","obexError","prompts","receiveDir","transfers","version"]'

# ── a stale build answers for code that is no longer there ──────────────────
#
# The sources the binary is built from, and not tests/: cargo does not relink
# for a change to an integration test, so counting one would leave the binary
# "older" for ever.
NEWER=$(find "$SRC/src" "$SRC/Cargo.toml" \
    -newer "$BIN" \( -name '*.rs' -o -name Cargo.toml \) -print -quit 2>/dev/null)
HAVE_JQ=1
command -v jq >/dev/null 2>&1 || HAVE_JQ=0
WHY=""
if [[ -n "$NEWER" ]]; then
    WHY="${BIN#"$SRC"/} is older than ${NEWER#"$SRC"/}"
elif (( ! HAVE_JQ )); then
    WHY="jq is not installed to read its answers"
fi

echo
echo "the command line"
if [[ -n "$WHY" ]]; then
    skip "the command line not exercised: $WHY"
else
    OUT=$(run_bt "$WORK/run-verb" --definitely-not-a-verb 2>/dev/null); STATUS=$?
    check "an unknown verb is an argument error, exit 2" "2" "$STATUS"
    check "and says so in an envelope" "false" "$(jq -r '.ok' <<< "$OUT" 2>/dev/null)"
    OUT=$(run_bt "$WORK/run-verb" 2>/dev/null); STATUS=$?
    check "no verb at all is the same refusal" "2" "$STATUS"

    # The verbs the //! block documents are the verbs main() accepts, read
    # from what the binary itself says it expects -- docs/HELPERS.md is
    # generated from that block, and a verb documented but refused, or
    # accepted but undocumented, is the drift this pins.
    DOC=$(sed -n '\|^//!|!q; s|^//! \{0,1\}||; p' "$MAIN" | sed -nE 's/^[[:space:]]+jayos-bluetooth ([a-z-]+).*/\1/p' | sort -u | paste -sd' ')
    EXPECTED=$(run_bt "$WORK/run-verb" --definitely-not-a-verb 2>/dev/null | jq -r '.expected[]' 2>/dev/null | sort -u | paste -sd' ')
    check "the //! block's verbs are the verbs the binary accepts" "$EXPECTED" "$DOC"
    [[ -n "$DOC" ]] && pass "and the block names some" || bad "and the block names some"

    OUT=$(run_bt "$WORK/run-verb" send AA:BB:CC:DD:EE:FF 2>/dev/null); STATUS=$?
    check "send without a file is a usage error, exit 2" "2" "$STATUS"
    check "with an envelope" "false" "$(jq -r '.ok' <<< "$OUT" 2>/dev/null)"
fi

echo
echo "with no daemon"
if [[ -n "$WHY" ]]; then
    skip "the no-daemon answers not exercised: $WHY"
else
    for verb in status watch "send AA:BB:CC:DD:EE:FF /etc/hostname"; do
        # shellcheck disable=SC2086  # the verb and its arguments, split on purpose
        OUT=$(run_bt "$WORK/run-empty" $verb 2>/dev/null </dev/null); STATUS=$?
        check "${verb%% *} with no daemon exits 2" "2" "$STATUS"
        if jq -e '.ok == false and (.error | type == "string" and length > 0)' >/dev/null 2>&1 <<< "$OUT"; then
            pass "${verb%% *} says why in a JSON envelope"
        else
            bad "${verb%% *} says why in a JSON envelope"
            printf '         actual:   %s\n' "$OUT"
        fi
    done
fi

echo
echo "the daemon with no buses"
if [[ -n "$WHY" ]]; then
    skip "the daemon without buses not exercised: $WHY"
else
    # stdin at EOF from the start: the shell gone before the daemon began.
    BT_TIMEOUT=10 run_bt "$WORK/run-nobus" daemon </dev/null >"$WORK/nobus.out" 2>/dev/null; STATUS=$?
    OUT=$(head -1 "$WORK/nobus.out")
    check "stdin at EOF ends the daemon with exit 0" "0" "$STATUS"
    check "its first line is a state" "state" "$(jq -r '.event' <<< "$OUT" 2>/dev/null)"
    check "with bluez false" "false" "$(jq -r '.bluez' <<< "$OUT" 2>/dev/null)"
    check "the agent absent" "absent" "$(jq -r '.agent' <<< "$OUT" 2>/dev/null)"
    check "obex absent" "absent" "$(jq -r '.obex' <<< "$OUT" 2>/dev/null)"
    check "and every top-level key present" "$STATE_KEYS" "$(jq -c 'keys' <<< "$OUT" 2>/dev/null)"
    check "no adapters and no devices without BlueZ" "0 0" "$(jq -r '"\(.adapters | length) \(.devices | length)"' <<< "$OUT" 2>/dev/null)"
fi

echo
echo "on a private bus with no BlueZ"
if [[ -n "$WHY" ]]; then
    skip "the daemon on a private bus not exercised: $WHY"
elif ! command -v dbus-daemon >/dev/null 2>&1; then
    skip "the daemon on a private bus not exercised: dbus-daemon is not installed"
else
    SOCKDIR="$(mktemp -d /tmp/qsbt.XXXXXX)"
    cat > "$SOCKDIR/bus.conf" <<CONF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path=$SOCKDIR/bus</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
CONF
    dbus-daemon --config-file="$SOCKDIR/bus.conf" --nofork --print-address=1 > "$SOCKDIR/addr" 2>/dev/null &
    BUS_PID=$!
    for _ in $(seq 100); do [[ -s "$SOCKDIR/addr" ]] && break; sleep 0.05; done
    ADDR=$(head -1 "$SOCKDIR/addr" 2>/dev/null)
    if [[ -z "$ADDR" ]]; then
        skip "the daemon on a private bus not exercised: dbus-daemon did not start"
    else
        # One bus in both roles: neither org.bluez nor org.bluez.obex is on it.
        export SYSTEM_BUS="$ADDR" SESSION_BUS="$ADDR"
        RUN="$WORK/run-bus"
        mkfifo "$WORK/in"
        run_bt "$RUN" daemon < "$WORK/in" > "$WORK/daemon.out" 2>"$WORK/daemon.err" &
        DAEMON_PID=$!
        exec 3> "$WORK/in"
        SOCK="$RUN/quickshell/bluetooth/control.sock"
        for _ in $(seq 100); do [[ -S "$SOCK" ]] && break; sleep 0.05; done
        if [[ -S "$SOCK" ]]; then pass "the daemon makes its control socket"; else bad "the daemon makes its control socket"; fi
        check "the socket is 0600" "600" "$(stat -c %a "$SOCK" 2>/dev/null)"
        check "inside a 0700 directory" "700" "$(stat -c %a "$RUN/quickshell/bluetooth" 2>/dev/null)"

        STATUS_OUT=$(run_bt "$RUN" status 2>/dev/null); STATUS=$?
        check "status through the socket exits 0" "0" "$STATUS"
        check "and answers with bluez false" "false" "$(jq -r '.bluez' <<< "$STATUS_OUT" 2>/dev/null)"
        check "and obex absent" "absent" "$(jq -r '.obex' <<< "$STATUS_OUT" 2>/dev/null)"
        check "the same state the owner was sent" \
            "$(head -1 "$WORK/daemon.out" | jq -cS . 2>/dev/null)" "$(jq -cS . <<< "$STATUS_OUT" 2>/dev/null)"

        printf '%s\n' '{"id":"p1","op":"power","adapter":"/org/bluez/hci0","on":true}' >&3
        REPLY=""
        for _ in $(seq 100); do
            REPLY=$(grep -m1 '"id":"p1"' "$WORK/daemon.out" 2>/dev/null)
            [[ -n "$REPLY" ]] && break
            sleep 0.05
        done
        check "a power op without BlueZ is refused" "false" "$(jq -r '.ok' <<< "$REPLY" 2>/dev/null)"
        check "as bluez-absent" "bluez-absent" "$(jq -r '.error' <<< "$REPLY" 2>/dev/null)"

        # watch forwards a command and prints the events, the page's whole
        # transport; `timeout` ends it, since watch outlives its stdin.
        WATCH_OUT=$( { printf '%s\n' '{"id":"w1","op":"snapshot"}'; sleep 1; } | BT_TIMEOUT=3 run_bt "$RUN" watch 2>/dev/null)
        check "watch's first line is the state" "state" "$(head -1 <<< "$WATCH_OUT" | jq -r '.event' 2>/dev/null)"
        check "and it carries the reply to what it forwarded" "true" \
            "$(grep '"id":"w1"' <<< "$WATCH_OUT" | head -1 | jq -r '.ok' 2>/dev/null)"

        SECOND=$(BT_TIMEOUT=10 run_bt "$RUN" daemon </dev/null 2>/dev/null); STATUS=$?
        check "a second daemon on the same runtime directory exits 1" "1" "$STATUS"
        check "after a fatal event" "another jayos-bluetooth daemon is running" "$(jq -r 'select(.event == "fatal") | .message' <<< "$SECOND" 2>/dev/null)"

        exec 3>&-
        for _ in $(seq 100); do kill -0 "$DAEMON_PID" 2>/dev/null || break; sleep 0.05; done
        wait "$DAEMON_PID" 2>/dev/null; STATUS=$?
        DAEMON_PID=""
        check "closing the owner's stdin ends the daemon with exit 0" "0" "$STATUS"
        [[ -e "$SOCK" ]] && bad "and the socket goes with it" || pass "and the socket goes with it"
    fi
fi

echo
echo "  not checked here: pairing, connecting and sending or receiving files."
echo "  cargo test runs those against a mock BlueZ and obexd (tests/daemon.rs);"
echo "  against a real adapter, nothing does -- this machine has none."

echo
# The tally names the skips, so a run that passed past a case does not end on
# the line a full run prints.
SKIPPED=""
(( SKIP > 0 )) && SKIPPED=", $SKIP not exercised"
if (( FAIL )); then
    printf '  \033[31m%d passed, %d failed%s\033[0m\n' "$PASS" "$FAIL" "$SKIPPED"
    exit 1
fi
printf '  \033[32m%d passed%s\033[0m\n' "$PASS" "$SKIPPED"
