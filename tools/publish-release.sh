#!/usr/bin/env bash
#
# The release workflow's half of a release: check the tag it was handed, and
# publish the release's page.
#
#   tools/publish-release.sh verify  <tag>            0 when the tag is a release
#   tools/publish-release.sh notes   <tag>            the release's notes, on stdout
#   tools/publish-release.sh publish <tag> <notes>    the GitHub release, through gh
#
# Run by .github/workflows/release.yml, in a checkout of the tag with every
# branch fetched; tools/release.sh made the tag. Exit status: 0 done, 1
# refused with the reason, 2 could not run.
#
# VERIFY asks what tools/release.sh promised, of what reached GitHub rather
# than what was meant to: the tag is vMAJOR.MINOR.PATCH, annotated, signed by
# a key origin/main's .github/allowed_signers lists -- main's, never the
# tag's own tree's, so a tag cannot bring the key that signs it -- on main,
# and VERSION at the tag says its version. A tag pushed by hand, or by a key
# nobody listed, gets no release page, and JayOS's tools/package-release.sh jayos-bluetooth
# asks the same of it before it pins one.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/release.sh
source "$ROOT/tools/lib/release.sh" || { echo "cannot read tools/lib/release.sh" >&2; exit 2; }
cd "$ROOT" || exit 2

refuse() { printf 'refused: %s\n' "$*" >&2; exit 1; }
cannot() { printf 'cannot: %s\n' "$*" >&2; exit 2; }

verify() { # verify <tag>
    local tag="$1" signers version
    release_tag_ok "$tag" || refuse "$tag is not a release tag, vMAJOR.MINOR.PATCH"
    git rev-parse --verify --quiet "refs/remotes/origin/main" >/dev/null \
        || cannot "there is no origin/main here to check the tag against; fetch every branch"
    signers=$(mktemp "${TMPDIR:-/tmp}/jayos-bluetooth-signers.XXXXXX") || cannot "no temporary file"
    git show refs/remotes/origin/main:.github/allowed_signers >"$signers" 2>/dev/null \
        || { rm -f "$signers"; refuse "origin/main has no .github/allowed_signers"; }
    if [[ -z "$(release_signers_keys "$signers")" ]]; then
        rm -f "$signers"
        refuse "origin/main's .github/allowed_signers holds no key, so no tag can be a release"
    fi
    if ! release_verify_tag "$tag" "$signers"; then
        rm -f "$signers"
        refuse "$tag is not an annotated tag signed by a key origin/main's .github/allowed_signers lists"
    fi
    rm -f "$signers"
    git merge-base --is-ancestor "$tag^{commit}" refs/remotes/origin/main 2>/dev/null \
        || refuse "$tag names $(git rev-parse --short "$tag^{commit}"), which is not on main"
    version=$(git show "$tag:VERSION" 2>/dev/null | head -n 1)
    [[ "$version" == "${tag#v}" ]] || refuse "VERSION at $tag says '${version}', not ${tag#v}"
    printf '%s is a release: signed by a listed key, on main, VERSION %s, commit %s\n' \
        "$tag" "$version" "$(git rev-parse "$tag^{commit}")"
}

# is_release <tag> <signers file>: verify's questions, answered quietly --
# for the notes, which count from the release before this one, and must not
# count from a tag that is not one: an unsigned tag pushed by hand, a tag off
# main, a tag whose VERSION says otherwise.
is_release() {
    release_tag_ok "$1" || return 1
    release_verify_tag "$1" "$2" || return 1
    git merge-base --is-ancestor "$1^{commit}" refs/remotes/origin/main 2>/dev/null || return 1
    [[ "$(git show "$1:VERSION" 2>/dev/null | head -n 1)" == "${1#v}" ]]
}

notes() { # notes <tag>
    local tag="$1" prev range signers t
    release_tag_ok "$tag" || refuse "$tag is not a release tag"
    git rev-parse --verify --quiet "refs/tags/$tag" >/dev/null || cannot "no tag $tag here"
    signers=$(mktemp "${TMPDIR:-/tmp}/jayos-bluetooth-signers.XXXXXX") || cannot "no temporary file"
    git show refs/remotes/origin/main:.github/allowed_signers >"$signers" 2>/dev/null || : >"$signers"
    prev=$(git tag --list 'v*' | while IFS= read -r t; do is_release "$t" "$signers" && echo "$t"; done \
        | release_before "$tag")
    rm -f "$signers"
    printf 'jayos-bluetooth %s\n\n' "${tag#v}"
    printf 'Commit `%s`.\n\n' "$(git rev-parse "$tag^{commit}")"
    if [[ -n "$prev" ]]; then
        printf 'Since %s:\n\n' "$prev"
        range="$prev..$tag"
    else
        printf 'The first release.\n\n'
        range="$tag"
    fi
    git log --no-merges --format='- %s' "$range" 2>/dev/null | head -n 200
    printf '\nJayOS builds a release from its pkgbuilds/jayos-bluetooth recipe, which pins it\n'
    printf 'by tag and commit; its tools/package-release.sh jayos-bluetooth pins this one.\n'
}

publish() { # publish <tag> <notes file>
    local tag="$1" file="$2"
    [[ -s "$file" ]] || cannot "no notes at $file"
    command -v gh >/dev/null 2>&1 || cannot "gh is not installed"
    gh release create "$tag" --verify-tag --title "jayos-bluetooth ${tag#v}" --notes-file "$file" \
        || cannot "gh release create $tag failed"
}

case "${1:-}" in
    verify)  [[ -n "${2:-}" ]] || cannot "usage: publish-release.sh verify <tag>"; verify "$2" ;;
    notes)   [[ -n "${2:-}" ]] || cannot "usage: publish-release.sh notes <tag>"; notes "$2" ;;
    publish) [[ -n "${2:-}" && -n "${3:-}" ]] || cannot "usage: publish-release.sh publish <tag> <notes>"
             publish "$2" "$3" ;;
    *) echo "usage: $(basename "$0") verify <tag> | notes <tag> | publish <tag> <notes>" >&2; exit 2 ;;
esac
