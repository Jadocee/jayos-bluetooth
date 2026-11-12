#!/usr/bin/env bash
#
# Everything this repository checks, in one run.
#
#   tools/check.sh
#
# Exit 0: everything that ran passed; the cases a suite ran past without
# exercising -- its counted skips, `  skip ...` lines -- are named in the
# summary, and the last line says how many. 1: something failed. 2: nothing
# failed but a whole step could not run -- no cargo, no git, no dbus-daemon --
# which is a different answer from "clean" and is reported as one. A check
# that examined nothing must not be counted as a check that found nothing,
# and a run that skipped a case must not end on the line a full run prints.
#
# The same three statuses as jayos's suites, because jayos's installer reads
# its own suites' answers the same way, and as jayos-audio's tools/check.sh,
# whose shape this is.
#
# What runs: every script's syntax, the contract against the code, the crate
# built, its tests -- 76 in its modules and 27 in tests/daemon.rs, which start
# the binary against a mock BlueZ and obexd on dbus-daemons of their own --
# rustfmt's verdict, and then every tests/shell/test-*.sh, test-manager.sh
# over the debug build just made rather than whatever release build an
# earlier run left. tools/release.sh runs this over a worktree of the commit
# it is asked to tag, and the release workflow runs it at the tag.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 2

FAILED=()
SKIPPED=()
PARTIAL=()

# Each step's output, kept for the skip count below while it streams.
LOG=$(mktemp "${TMPDIR:-/tmp}/jayos-bluetooth-check.XXXXXX") || {
    echo "could not make a scratch file; nothing was examined"
    exit 2
}
trap 'rm -f "$LOG"' EXIT

step() { printf '\n\033[1m%s\033[0m\n' "$1"; }

# run <name> <command...>: 0 passes, 2 is "could not run", anything else fails.
#
# A 0 is not the whole answer. A suite that runs past a case the machine
# cannot ask -- test-manager.sh's private bus where dbus-daemon is missing --
# says so with a counted `  skip ...` line while exiting 0, because a machine
# without dbus-daemon is not a machine with findings. Looking at the status
# alone, this would end on "ok, nothing to fix", byte for byte what a run that
# asked everything prints. So the skips of a suite that passed are collected
# and named in the summary, and the status is left alone -- jayos's
# install.sh does the same in its skips_within(), for the same reason. The
# skips of a suite that failed or could not run are not: FAILED and "not
# exercised" already say more. `^  skip ` with its trailing space, the shape
# every suite's skip() prints.
run() {
    local name="$1" status line
    shift
    "$@" | tee "$LOG"
    status=${PIPESTATUS[0]}
    case $status in
        0)  while IFS= read -r line; do
                PARTIAL+=("$name: ${line#  skip }")
            done < <(grep -E '^  skip ' "$LOG")
            ;;
        2) SKIPPED+=("$name") ;;
        *) FAILED+=("$name") ;;
    esac
}

# The scripts git knows of, tracked or not yet added, or every script in the
# tree outside a git checkout -- where `git ls-files` lists nothing, and a
# syntax step over nothing would report clean having examined nothing.
#
# Untracked as well as tracked, minus what .gitignore names (target/, and
# makepkg's src/ and pkg/): tracked alone skipped every file not yet added,
# so a new script with a syntax error passed this step -- jayos-audio's
# check.sh found that in its own tree. A tracked file deleted from the work
# tree is listed too, and is skipped below rather than reported as a syntax
# error.
scripts() {
    # "true", not merely exit 0: inside a .git directory it exits 0 and
    # prints "false", and ls-files there lists nothing.
    if [[ "$(git rev-parse --is-inside-work-tree 2>/dev/null)" == true ]]; then
        git ls-files --cached --others --exclude-standard '*.sh' 'packaging/arch/PKGBUILD'
    else
        find . \( -path ./target -o -path ./packaging/arch/src -o -path ./packaging/arch/pkg \) -prune -o -type f \
            \( -name '*.sh' -o -path ./packaging/arch/PKGBUILD \) -print
    fi
}

step "shell syntax"
syntax() {
    local f bad=0 n=0
    while IFS= read -r f; do
        [[ -f "$f" ]] || continue
        n=$((n + 1))
        bash -n "$f" || { echo "  syntax error in $f"; bad=1; }
    done < <(scripts)
    if (( n == 0 )); then
        echo "  no scripts found; this step examined nothing"
        return 2
    fi
    echo "  $n files parse"
    return "$bad"
}
run "shell syntax" syntax

step "contract"
run "contract" bash tools/contract.sh

# The crate: the build first, so that test-manager.sh below has a program to
# run -- `cargo test` builds the test harnesses and, for tests/daemon.rs, the
# binary, but this makes the dependency explicit -- and the suite is pointed
# at that build, the debug one, rather than left to find a release binary
# some earlier run left, which may predate the source. --locked, as
# packaging/steps.sh fetches: a lock this would have to change is a defect,
# not a refresh. rustfmt's check where rustfmt answers; where it does not, a
# counted skip, since a machine with cargo and no rustfmt is not one with
# findings.
step "the crate: build, tests and format"
rust_checks() {
    # Under the tree whatever the building user set globally, so the debug
    # build below is where JAYOS_BLUETOOTH_BIN says it is.
    export CARGO_TARGET_DIR="$ROOT/target"
    if ! command -v cargo >/dev/null 2>&1; then
        echo "  no cargo, so the crate was not built or tested"
        return 2
    fi
    cargo build --locked || return 1
    cargo test --locked || return 1
    if cargo fmt --version >/dev/null 2>&1; then
        cargo fmt --check || return 1
        echo "  the crate is formatted as rustfmt.toml says"
    else
        echo "  skip rustfmt not exercised: cargo fmt is not installed"
    fi
}
run "the crate" rust_checks
[[ -x target/debug/jayos-bluetooth ]] \
    && export JAYOS_BLUETOOTH_BIN="$ROOT/target/debug/jayos-bluetooth"

# nullglob, so a tree with no suites runs none rather than one named by the
# unexpanded pattern -- which failed as "No such file or directory" -- and the
# count says so: no suites is "examined nothing", not "nothing failed".
shopt -s nullglob
suites=(tests/shell/test-*.sh)
shopt -u nullglob
if (( ${#suites[@]} == 0 )); then
    echo "  no suites under tests/shell; nothing was examined there"
    SKIPPED+=("tests/shell")
fi
for suite in ${suites[@]+"${suites[@]}"}; do
    step "$suite"
    run "$suite" bash "$suite"
done

echo
if (( ${#FAILED[@]} )); then
    printf 'FAILED: %s\n' "${FAILED[@]}"
fi
if (( ${#SKIPPED[@]} )); then
    printf 'not exercised: %s\n' "${SKIPPED[@]}"
fi
if (( ${#PARTIAL[@]} )); then
    echo "ran, but not every case:"
    printf '  %s\n' "${PARTIAL[@]}"
fi
if (( ${#FAILED[@]} )); then
    exit 1
elif (( ${#SKIPPED[@]} )); then
    exit 2
fi
if (( ${#PARTIAL[@]} )); then
    echo "ok, nothing to fix; ${#PARTIAL[@]} case(s) not exercised, listed above"
else
    echo "ok, nothing to fix"
fi
