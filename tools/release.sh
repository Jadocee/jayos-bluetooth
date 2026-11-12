#!/usr/bin/env bash
#
# Release jayos-bluetooth: VERSION's version, as a signed tag on main, pushed.
#
#   tools/release.sh            dry run: the version, the commit, and every refusal
#   tools/release.sh --apply    the signed tag vVERSION, made, checked and pushed
#
# Run by the owner, in their checkout, on an up-to-date main, after VERSION
# says the version to release. docs/RELEASES.md is the account.
#
# Exit status: 0 tagged, or would tag; 1 refused, with the reason; 2 could
# not run -- a bad flag, no checkout, an origin that did not answer.
#
# WHAT A RELEASE IS FOR. JayOS's recipe, pkgbuilds/jayos-bluetooth/PKGBUILD in
# that repository, builds the release it pins -- this tag, and the commit it
# names -- and JayOS's suites hold its Bluetooth page, its installer's BlueZ
# step and its doctor to that release's contract/ and share/, vendored by its
# `tools/package-release.sh jayos-bluetooth`. The daemon came here from JayOS
# on 2026-11-12, by the owner's call "Move all Bluetooth related stuff (except
# UI/QML) into jayos-bluetooth", released from the first, by the owner's call
# of the same day, "Add releases to jayos-bluetooth and jayos-sony-xm5": a
# recipe that built whatever main held at the moment of the build would let
# two machines a day apart hold two packages under one version, which is why
# jayos-audio's recipe stopped doing so the same morning.
#
# A DRY RUN BY DEFAULT, like JayOS's tools/release.sh and its installers: it
# says the version and every refusal it would make, and changes nothing but
# the remote-tracking refs `git fetch` updates, which it needs to know the
# releases that exist rather than the ones this clone has seen.
#
# THE OWNER SIGNS, HERE. The release workflow (.github/workflows/release.yml)
# holds no key: a workflow that could tag would be one that could sign as the
# owner. It checks what it is handed and publishes the release's page.
#
# WHAT IT REFUSES, each before anything is tagged:
#
#   - a branch other than main, or a main that is not origin/main: what is
#     released is the commit everybody can fetch, nothing unpushed and
#     nothing behind
#   - changes to tracked files: what is tagged is the commit, and a tree that
#     differs from it is a tree nobody will get
#   - a VERSION that is not MAJOR.MINOR.PATCH, one already released, or one
#     not newer than the newest release
#   - a commit that already carries a release
#   - a commit whose tools/check.sh does not pass, run in a worktree of the
#     commit itself, so that nothing untracked here stands in for it: there
#     is no CI on this repository, and this is the gate a release stands on
#     -- the daemon's tests against a mock BlueZ among it
#   - signing that is not SSH, or no signing key: .github/allowed_signers is
#     ssh-keygen's format, which is what the workflow and JayOS check with
#   - an allowed_signers on origin/main with no key in it
#
# AND ONE AFTER THE TAG IS MADE: the tag is verified against origin/main's
# .github/allowed_signers, as the workflow will verify it, before it is
# pushed. A tag that fails is deleted here and never reaches GitHub.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/release.sh
source "$ROOT/tools/lib/release.sh" || { echo "cannot read tools/lib/release.sh" >&2; exit 2; }

APPLY=0
while (( $# )); do
    case "$1" in
        --apply) APPLY=1 ;;
        -h|--help) sed -n '3,9p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument: $1 (see --help)" >&2; exit 2 ;;
    esac
    shift
done

say()    { printf '  %s\n' "$*"; }
refuse() { printf '  refused: %s\n' "$1" >&2; shift; local l; for l in "$@"; do printf '           %s\n' "$l" >&2; done; exit 1; }
cannot() { printf '  cannot: %s\n' "$*" >&2; exit 2; }

command -v git >/dev/null 2>&1 || cannot "git is not installed"
git -C "$ROOT" rev-parse --git-dir >/dev/null 2>&1 || cannot "$ROOT is not a git checkout"
cd "$ROOT" || cannot "cannot enter $ROOT"

# ── what there is ────────────────────────────────────────────────────────────
# --tags as well as main: a release made from another clone has to be known,
# or two clones could each tag one version at two commits.
git fetch --quiet --tags origin main 2>/dev/null \
    || cannot "git fetch from origin failed, so the releases that exist cannot be known"

BRANCH=$(git symbolic-ref --quiet --short HEAD 2>/dev/null || true)
[[ "$BRANCH" == main ]] || refuse "this is ${BRANCH:-a detached HEAD}, not main" \
    "a release is a commit of main, the branch JayOS's recipe names"

# Tracked files only. An untracked file changes nothing that is tagged, and a
# checkout always has some -- rust/target, a build.
DIRTY=$(git status --porcelain --untracked-files=no)
[[ -z "$DIRTY" ]] || refuse "tracked files have changes; what is tagged is the commit, not this tree" \
    "$(head -3 <<<"$DIRTY")"

HEAD_SHA=$(git rev-parse HEAD)
ORIGIN_SHA=$(git rev-parse --verify --quiet refs/remotes/origin/main || true)
[[ -n "$ORIGIN_SHA" ]] || cannot "origin has no main branch to compare with"
[[ "$HEAD_SHA" == "$ORIGIN_SHA" ]] || refuse "main is not origin/main" \
    "this clone is at ${HEAD_SHA:0:12}, origin/main at ${ORIGIN_SHA:0:12}: pull, or push first"

VERSION=$(head -n 1 VERSION 2>/dev/null)
release_version_ok "$VERSION" || refuse "VERSION says '${VERSION}', not MAJOR.MINOR.PATCH"
TAG="v$VERSION"

# The releases are origin's tags, not this clone's: a tag made here and never
# pushed -- a test, a mistake -- is no release, and counted it would hold
# every later version back behind it. Each tag's commit is its peeled ^{}
# line where it has one -- an annotated tag's own line names the tag object,
# not the commit -- and its own line where it is a lightweight tag.
ORIGIN_TAGS=$(git ls-remote --tags origin 2>/dev/null) \
    || cannot "git ls-remote could not list origin's tags"
TAG_COMMITS=$(awk '{ n = $2; sub("^refs/tags/", "", n)
                     if (n ~ /\^\{\}$/) { sub(/\^\{\}$/, "", n); peeled[n] = $1 }
                     else own[n] = $1 }
                   END { for (n in own) print (n in peeled ? peeled[n] : own[n]), n }' <<<"$ORIGIN_TAGS")
RELEASED=$(awk '{ print $2 }' <<<"$TAG_COMMITS")
ALREADY=$(awk -v h="$HEAD_SHA" '$1 == h { print $2 }' <<<"$TAG_COMMITS" \
    | while IFS= read -r t; do release_tag_ok "$t" && echo "$t"; done)
[[ -z "$ALREADY" ]] || refuse "${HEAD_SHA:0:12} is already released, as $(tr '\n' ' ' <<<"$ALREADY")"
grep -qxF -- "$TAG" <<<"$RELEASED" \
    && refuse "$TAG is released already" "raise VERSION for a new release; a release is never moved"
git rev-parse --verify --quiet "refs/tags/$TAG" >/dev/null \
    && refuse "$TAG exists in this clone and not on origin, at $(git rev-parse --short "$TAG^{commit}")" \
        "it is no release; delete it (git tag -d $TAG) and run this again"
LATEST=$(release_latest <<<"$RELEASED")
if [[ -n "$LATEST" ]] && [[ "$(release_cmp "$VERSION" "${LATEST#v}")" != 1 ]]; then
    refuse "VERSION $VERSION is not newer than the newest release, $LATEST"
fi
say "main at ${HEAD_SHA:0:12}, origin/main the same, nothing changed"
say "the newest release: ${LATEST:-none yet}; VERSION says $VERSION, so the tag is $TAG"

# ── the signature's prerequisites ────────────────────────────────────────────
# Asked before the check, which takes minutes: a refusal that needs no build
# should not wait for one.
FORMAT=$(git config --get gpg.format || true)
SAID_FORMAT=${FORMAT:-openpgp}
[[ "$FORMAT" == ssh ]] || refuse "tags here are signed with $SAID_FORMAT, not ssh" \
    ".github/allowed_signers is ssh-keygen's format" "git config gpg.format ssh"
SIGNING_KEY=$(git config --get user.signingkey || true)
[[ -n "$SIGNING_KEY" ]] || refuse "no user.signingkey is set, so git tag -s has no key to sign with" \
    "git config user.signingkey <your public key's file>"

WORKTREE=""
SIGNERS=$(mktemp "${TMPDIR:-/tmp}/jayos-bluetooth-release-signers.XXXXXX") || cannot "no temporary file"
cleanup() {
    rm -f "$SIGNERS"
    [[ -n "$WORKTREE" ]] && git worktree remove --force "$WORKTREE" >/dev/null 2>&1
    return 0
}
trap cleanup EXIT
# Origin's main, not this checkout's: the file the workflow reads, and a key
# added here but not pushed is one the workflow does not know.
git show refs/remotes/origin/main:.github/allowed_signers >"$SIGNERS" 2>/dev/null \
    || refuse "origin/main has no .github/allowed_signers"
[[ -n "$(release_signers_keys "$SIGNERS")" ]] \
    || refuse ".github/allowed_signers on origin/main holds no key yet" \
        "every release is refused until the public half of your signing key is in it" \
        "(docs/RELEASES.md, \"The owner's steps\")"

# ── the check, over the commit ───────────────────────────────────────────────
WORKTREE=$(mktemp -d "${TMPDIR:-/tmp}/jayos-bluetooth-release-tree.XXXXXX") || cannot "no temporary directory"
rmdir "$WORKTREE"
git worktree add --quiet --detach "$WORKTREE" "$HEAD_SHA" 2>/dev/null \
    || { WORKTREE=""; cannot "git worktree add could not check out ${HEAD_SHA:0:12} to run its check"; }
say "running tools/check.sh over ${HEAD_SHA:0:12}, in a worktree of it"
( cd "$WORKTREE" && bash tools/check.sh ) >"$WORKTREE.check.log" 2>&1
CHECK=$?
if (( CHECK != 0 )); then
    tail -n 12 "$WORKTREE.check.log" | sed 's/^/           /' >&2
    rm -f "$WORKTREE.check.log"
    refuse "tools/check.sh over ${HEAD_SHA:0:12} exited $CHECK, not 0" \
        "a release stands on its check passing; the lines above are its end"
fi
say "$(tail -n 1 "$WORKTREE.check.log")"
rm -f "$WORKTREE.check.log"

MESSAGE="jayos-bluetooth $VERSION"
if (( ! APPLY )); then
    say "would run: git tag -s $TAG ${HEAD_SHA:0:12}, signed with ssh"
    say "would check it against origin/main's .github/allowed_signers, as the workflow will"
    say "would run: git push origin refs/tags/$TAG"
    say "nothing changed; --apply makes the tag"
    exit 0
fi

# ── the tag ──────────────────────────────────────────────────────────────────
git tag -s -m "$MESSAGE" "$TAG" "$HEAD_SHA" \
    || refuse "git tag -s $TAG did not make a signed tag" "git says why, above"
if ! release_verify_tag "$TAG" "$SIGNERS"; then
    git tag -d "$TAG" >/dev/null 2>&1
    refuse "$TAG is not signed by a key origin/main's .github/allowed_signers lists" \
        "the workflow and JayOS would refuse it, so it was deleted here and nothing was pushed" \
        "add this key's public half to .github/allowed_signers on main, or sign with the listed one"
fi
say "$TAG made, and signed by a key .github/allowed_signers lists"
if ! git push --quiet origin "refs/tags/$TAG"; then
    git tag -d "$TAG" >/dev/null 2>&1
    refuse "git push of $TAG failed; the tag was deleted here, so a retry starts clean"
fi
say "$TAG pushed; the release workflow checks it again and publishes its page"
say "then, in JayOS: tools/package-release.sh jayos-bluetooth $VERSION --apply pins it"
exit 0
