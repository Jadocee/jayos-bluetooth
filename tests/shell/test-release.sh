#!/usr/bin/env bash
#
# Releases: tools/release.sh, the owner's signed tag, and
# tools/publish-release.sh, what the release workflow checks and publishes.
#
# RUN in scratch repositories with keys made here and thrown away: an origin,
# bare, and the owner's clone of it, holding this repository's three release
# scripts, a VERSION, an .github/allowed_signers and a tools/check.sh that
# passes or fails as a file says -- so every refusal is asked of the real
# scripts over a real git, and every signature is a real ssh-keygen's. No
# GitHub, no gh and no workflow runner is involved: publish's call of
# `gh release create` is run against a stand-in that writes down its words.
#
# Exit 0 when every case passes, 1 when any fails, 2 when it could not run
# (no git or no ssh-keygen, which git's SSH signing runs).

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
for c in git ssh-keygen; do
    command -v "$c" >/dev/null 2>&1 || { echo "test-release not exercised: no $c here" >&2; exit 2; }
done
for f in tools/release.sh tools/publish-release.sh tools/lib/release.sh .github/allowed_signers \
         .github/workflows/release.yml; do
    [[ -r "$ROOT/$f" ]] || { echo "cannot read $f" >&2; exit 2; }
done

WORK="$(mktemp -d "${TMPDIR:-/tmp}/jayos-bluetooth-release.XXXXXX")" || exit 2
trap 'rm -rf "$WORK"' EXIT

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); printf '  \033[32mok\033[0m   %s\n' "$1"; }
bad()  { FAIL=$((FAIL + 1)); printf '  \033[31mfail\033[0m %s\n' "$1"; }
check() { # check <what> <expected> <actual>
    if [[ "$2" == "$3" ]]; then pass "$1"; else
        bad "$1"; printf '         expected: %s\n' "$2"; printf '         actual:   %s\n' "$3"; fi
}
says() { if grep -qF -- "$2" <<< "$3"; then pass "$1"; else
             bad "$1"; printf '         wanted: %s\n' "$2"; printf '%s\n' "$3" | sed 's/^/         got:    /'; fi; }

# Git with nothing of this machine's: no global config, no system config, a
# HOME of its own, and a committer the scratch commits carry.
export HOME="$WORK/home" GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$WORK/home/.gitconfig"
mkdir -p "$HOME"
git config --global user.name owner
git config --global user.email owner@example.invalid
git config --global init.defaultBranch main
git config --global commit.gpgsign false
git config --global advice.detachedHead false

# Two keys: the owner's, which the signers file lists, and a stranger's.
ssh-keygen -q -t ed25519 -N '' -C owner -f "$WORK/owner" >/dev/null 2>&1 \
    && ssh-keygen -q -t ed25519 -N '' -C stranger -f "$WORK/stranger" >/dev/null 2>&1 \
    || { echo "test-release not exercised: ssh-keygen could not make a key" >&2; exit 2; }
OWNER_LINE="owner@example.invalid namespaces=\"git\" $(cut -d' ' -f1,2 "$WORK/owner.pub")"

ORIGIN="$WORK/origin.git"
CLONE="$WORK/clone"
# fresh [signers line]: an origin and the owner's clone of it, at VERSION
# 2.0.0, the signers file holding the line given (the owner's by default,
# none for ''), and tools/check.sh passing; main pushed.
fresh() {
    local line="${1-$OWNER_LINE}"
    rm -rf "$ORIGIN" "$CLONE"
    git init -q --bare "$ORIGIN"
    git init -q "$CLONE"
    mkdir -p "$CLONE/tools/lib" "$CLONE/.github"
    cp "$ROOT/tools/release.sh" "$ROOT/tools/publish-release.sh" "$CLONE/tools/"
    cp "$ROOT/tools/lib/release.sh" "$CLONE/tools/lib/"
    { grep -E '^#' "$ROOT/.github/allowed_signers"; [[ -n "$line" ]] && printf '%s\n' "$line"; } \
        > "$CLONE/.github/allowed_signers"
    printf '2.0.0\n' > "$CLONE/VERSION"
    printf '#!/usr/bin/env bash\necho "ok, nothing to fix"\nexit "$(cat %q 2>/dev/null || echo 0)"\n' \
        "$WORK/check.status" > "$CLONE/tools/check.sh"
    rm -f "$WORK/check.status"
    git -C "$CLONE" add -A && git -C "$CLONE" commit -qm "jayos-bluetooth 2.0.0"
    git -C "$CLONE" remote add origin "$ORIGIN"
    git -C "$CLONE" push -q origin main
    git -C "$CLONE" config gpg.format ssh
    git -C "$CLONE" config user.signingkey "$WORK/owner.pub"
}
release() { (cd "$CLONE" && bash tools/release.sh "$@" 2>&1); }
remote_tags() { git -C "$ORIGIN" tag --list | paste -sd' ' -; }
signers_of_main() { git -C "$CLONE" show origin/main:.github/allowed_signers > "$WORK/signers"; echo "$WORK/signers"; }

# ── the files themselves ─────────────────────────────────────────────────────
echo
echo "the signers file and the workflow"
KEYS=$(grep -vE '^[[:space:]]*(#|$)' "$ROOT/.github/allowed_signers" || true)
if [[ -z "$KEYS" ]]; then
    pass "this repository's .github/allowed_signers holds no key yet: every release is refused until the owner adds one"
else
    check "every key line in .github/allowed_signers carries namespaces=\"git\"" "" \
        "$(grep -v 'namespaces="git"' <<<"$KEYS")"
fi
WF="$ROOT/.github/workflows/release.yml"
check "the workflow runs on a v* tag alone" "1" "$(grep -cE '^[[:space:]]+tags: \["v\*"\]$' "$WF")"
check "and verifies it, runs the check, writes the notes and publishes, in that order" \
    "verify|tools/check.sh|notes|publish" \
    "$(grep -oE 'publish-release\.sh (verify|notes|publish)|tools/check\.sh' "$WF" | sed 's/^publish-release\.sh //' | paste -sd'|' -)"
check "with the checkout pinned by commit, and every branch fetched" "1|1" \
    "$(grep -cE 'uses: actions/checkout@[0-9a-f]{40} ' "$WF")|$(grep -cE '^[[:space:]]+fetch-depth: 0$' "$WF")"
check "and no secret but the job's own token" "" \
    "$(grep -oE 'secrets\.[A-Za-z_]+' "$WF" | grep -v '^secrets\.GITHUB_TOKEN$' || true)"

# ── the library ──────────────────────────────────────────────────────────────
echo
echo "tools/lib/release.sh"
# shellcheck source=../../tools/lib/release.sh
source "$ROOT/tools/lib/release.sh"
check "2.10.0 is newer than 2.9.0, by number where text says otherwise" "1|-1|0" \
    "$(release_cmp 2.10.0 2.9.0)|$(release_cmp 2.9.0 2.10.0)|$(release_cmp 3.0.0 3.0.0)"
check "the newest release among tags, other names not counted" "v2.10.0" \
    "$(printf '%s\n' v2.9.0 v2.10.0 v10.0 release-3 v2.10.0-rc1 | release_latest)"
check "and the one before a release" "v2.9.0" "$(printf '%s\n' v2.9.0 v2.10.0 v1.0.0 | release_before v2.10.0)"

# ── tools/release.sh ─────────────────────────────────────────────────────────
echo
echo "tools/release.sh"
fresh
OUT=$(release); ST=$?
check "a dry run on a clean, pushed main exits 0" 0 "$ST"
says "naming the tag VERSION gives" "would run: git tag -s v2.0.0" "$OUT"
says "having run the commit's check" "ok, nothing to fix" "$OUT"
check "and tagging nothing" "|" "$(git -C "$CLONE" tag --list | paste -sd' ' -)|$(remote_tags)"

OUT=$(release --apply); ST=$?
check "--apply exits 0 and pushes v2.0.0" "0|v2.0.0" "$ST|$(remote_tags)"
check "the tag is annotated and names main's commit" "tag|$(git -C "$CLONE" rev-parse main)" \
    "$(git -C "$CLONE" cat-file -t refs/tags/v2.0.0)|$(git -C "$CLONE" rev-parse 'v2.0.0^{commit}')"
if git -C "$CLONE" -c gpg.ssh.allowedSignersFile="$(signers_of_main)" verify-tag v2.0.0 >/dev/null 2>&1; then
    pass "and verifies against main's .github/allowed_signers"
else
    bad "and verifies against main's .github/allowed_signers"
fi
check "its message names the release" "jayos-bluetooth 2.0.0" \
    "$(git -C "$CLONE" tag -l --format='%(contents:subject)' v2.0.0)"

OUT=$(release); ST=$?
check "again, over the released commit: refused" 1 "$ST"
says "saying it is released" "is already released, as v2.0.0" "$OUT"

printf '2.0.1\n' > "$CLONE/VERSION"
git -C "$CLONE" commit -qam "jayos-bluetooth 2.0.1"
OUT=$(release); ST=$?
check "a commit main has not been pushed: refused" "1" "$ST"
says "saying origin/main is elsewhere" "main is not origin/main" "$OUT"
git -C "$CLONE" push -q origin main
printf 'stray\n' >> "$CLONE/VERSION"
OUT=$(release); ST=$?
check "a tracked file changed: refused" 1 "$ST"
says "naming it" "VERSION" "$OUT"
git -C "$CLONE" checkout -q -- VERSION
git -C "$CLONE" checkout -q -b elsewhere
OUT=$(release); ST=$?
check "a branch other than main: refused" 1 "$ST"
says "saying which" "this is elsewhere, not main" "$OUT"
git -C "$CLONE" checkout -q main

echo 1 > "$WORK/check.status"
OUT=$(release --apply); ST=$?
check "a check that fails: refused, and nothing tagged" "1|v2.0.0" "$ST|$(remote_tags)"
says "saying the check's status" "tools/check.sh over" "$OUT"
rm -f "$WORK/check.status"

git -C "$CLONE" config --unset gpg.format
OUT=$(release); ST=$?
check "signing left at openpgp: refused" 1 "$ST"
says "saying ssh is needed" "not ssh" "$OUT"
git -C "$CLONE" config gpg.format ssh
git -C "$CLONE" config --unset user.signingkey
OUT=$(release); ST=$?
check "no signing key: refused" 1 "$ST"
git -C "$CLONE" config user.signingkey "$WORK/stranger.pub"
OUT=$(release --apply); ST=$?
check "a key the signers file does not list: refused, and the tag deleted, never pushed" "1|v2.0.0|" \
    "$ST|$(remote_tags)|$(git -C "$CLONE" tag --list v2.0.1)"
says "saying why" "is not signed by a key origin/main's .github/allowed_signers lists" "$OUT"
git -C "$CLONE" config user.signingkey "$WORK/owner.pub"
OUT=$(release --apply); ST=$?
check "and with the owner's key again, 2.0.1 is released" "0|v2.0.0 v2.0.1" "$ST|$(remote_tags)"

for v in 2.0 v2.0.2 02.0.2 2.0.1 1.9.9; do
    printf '%s\n' "$v" > "$CLONE/VERSION"
    git -C "$CLONE" commit -qam "VERSION $v" && git -C "$CLONE" push -q origin main
    OUT=$(release); ST=$?
    check "VERSION $v: refused" 1 "$ST"
done
says "an older one, as not newer than the newest release" "is not newer than the newest release, v2.0.1" "$OUT"
printf '2.10.0\n' > "$CLONE/VERSION"
git -C "$CLONE" commit -qam "VERSION 2.10.0" && git -C "$CLONE" push -q origin main
OUT=$(release); ST=$?
check "2.10.0 after 2.0.1 is newer, by number" 0 "$ST"

fresh ''
OUT=$(release); ST=$?
check "a signers file with no key on main: refused" 1 "$ST"
says "saying every release waits for a key" "holds no key yet" "$OUT"

# ── tools/publish-release.sh ─────────────────────────────────────────────────
echo
echo "tools/publish-release.sh"
fresh
release --apply >/dev/null 2>&1
publish() { (cd "$CLONE" && bash tools/publish-release.sh "$@" 2>&1); }
OUT=$(publish verify v2.0.0); ST=$?
check "verify: a tag tools/release.sh made is a release" 0 "$ST"
says "naming its commit" "$(git -C "$CLONE" rev-parse 'v2.0.0^{commit}')" "$OUT"
git -C "$CLONE" tag v9.9.9
OUT=$(publish verify v9.9.9); ST=$?
check "a lightweight tag is not" 1 "$ST"
git -C "$CLONE" tag -d v9.9.9 >/dev/null
git -C "$CLONE" tag -a -m unsigned v9.9.8
OUT=$(publish verify v9.9.8); ST=$?
check "nor is an annotated tag with no signature" 1 "$ST"
git -C "$CLONE" -c user.signingkey="$WORK/stranger.pub" tag -s -m stranger v9.9.7 >/dev/null 2>&1
OUT=$(publish verify v9.9.7); ST=$?
check "nor one signed by a key the file does not list" 1 "$ST"
git -C "$CLONE" checkout -q -b side
printf '2.0.5\n' > "$CLONE/VERSION"; git -C "$CLONE" commit -qam "side"
git -C "$CLONE" tag -s -m "jayos-bluetooth 2.0.5" v2.0.5 >/dev/null 2>&1
OUT=$(publish verify v2.0.5); ST=$?
check "nor one signed by the owner but not on main" 1 "$ST"
says "saying so" "which is not on main" "$OUT"
git -C "$CLONE" checkout -q main
git -C "$CLONE" tag -s -m "wrong version" v2.0.6 >/dev/null 2>&1
OUT=$(publish verify v2.0.6); ST=$?
check "nor one whose VERSION says another version" 1 "$ST"
says "naming what VERSION says" "VERSION at v2.0.6 says '2.0.0', not 2.0.6" "$OUT"
OUT=$(publish verify release-2); ST=$?
check "nor a name that is not vMAJOR.MINOR.PATCH" 1 "$ST"

printf '2.1.0\n' > "$CLONE/VERSION"
git -C "$CLONE" commit -qam "daemon: a better default" && git -C "$CLONE" commit -q --allow-empty -m "docs: the protocol table"
git -C "$CLONE" push -q origin main
release --apply >/dev/null 2>&1
OUT=$(publish notes v2.1.0); ST=$?
check "notes: exit 0, titled with the version" "0|jayos-bluetooth 2.1.0" "$ST|$(head -1 <<<"$OUT")"
says "counting from the release before it" "Since v2.0.0:" "$OUT"
says "listing what changed since" "- daemon: a better default" "$OUT"
check "and not what came before" "0" "$(grep -c '^- jayos-bluetooth 2.0.0$' <<<"$OUT")"
OUT=$(publish notes v2.0.0)
says "the first release says it is" "The first release." "$OUT"

mkdir -p "$WORK/gh"
printf '#!/usr/bin/env bash\nprintf "%%s\\n" "$*" > %q\n' "$WORK/gh.calls" > "$WORK/gh/gh"
chmod +x "$WORK/gh/gh"
publish notes v2.1.0 > "$WORK/notes.md"
OUT=$(cd "$CLONE" && PATH="$WORK/gh:$PATH" bash tools/publish-release.sh publish v2.1.0 "$WORK/notes.md" 2>&1); ST=$?
check "publish: gh release create, for the tag that exists, with the notes" \
    "0|release create v2.1.0 --verify-tag --title jayos-bluetooth 2.1.0 --notes-file $WORK/notes.md" \
    "$ST|$(cat "$WORK/gh.calls" 2>/dev/null)"

echo
if (( FAIL )); then
    printf '  \033[31m%d passed, %d failed\033[0m\n' "$PASS" "$FAIL"
    exit 1
fi
printf '  \033[32m%d passed\033[0m\n' "$PASS"
exit 0
