# Releases

A jayos-bluetooth release is a signed tag, `vMAJOR.MINOR.PATCH`, on `main`,
whose `VERSION` file says `MAJOR.MINOR.PATCH` -- and `Cargo.toml`'s version
and the checkout recipe's `pkgver` say the same, which
`tests/shell/test-packaging.sh` holds. JayOS builds a release and nothing
else: its recipe, `pkgbuilds/jayos-bluetooth/PKGBUILD` there, pins one by tag
and by commit, and its suites hold its Bluetooth page, its installer's BlueZ
step and its doctor to the same release's `contract/` and `share/`, which it
vendors. The owner's call of 2026-11-12, "Add releases to jayos-bluetooth and
jayos-sony-xm5", the day the daemon moved here: a recipe that built whatever
`main` held at the moment of the build would let two machines a day apart
hold two packages under one version, and JayOS's copy of the contract name a
commit `main` had moved past -- the reason jayos-audio's recipe stopped doing
so the same morning. The scripts here are jayos-audio's, with the name
changed, so one key and one habit sign all three.

## Making one

On `main`, up to date with origin, with `VERSION`, `Cargo.toml` and
`packaging/arch/PKGBUILD` raised to the version to release and that commit
pushed:

    tools/release.sh            # a dry run: the version, and every refusal
    tools/release.sh --apply    # the signed tag, checked and pushed

It refuses a branch other than `main`, a `main` that is not `origin/main`,
changes to tracked files, a `VERSION` that is not `MAJOR.MINOR.PATCH` or not
newer than the newest release, a commit already released, a commit whose
`tools/check.sh` does not pass -- run in a worktree of the commit itself,
since this repository has no CI and that check, the daemon's tests against a
mock BlueZ among it, is the gate a release stands on -- signing that is not
SSH, no signing key, and a `.github/allowed_signers` on `origin/main` with no
key in it. The releases it counts are origin's tags, not the clone's: a tag
made in a clone and never pushed is no release. After the tag is made it is
verified against `origin/main`'s `.github/allowed_signers`, as the workflow
will verify it, and a tag that fails is deleted before anything is pushed.

`.github/workflows/release.yml` runs on the pushed tag. It holds no key and
makes no tag: `tools/publish-release.sh verify` asks again that the tag is
annotated, signed by a key `origin/main`'s `.github/allowed_signers` lists --
`main`'s, never the tag's own tree's, so a tag cannot bring the key that signs
it -- on `main`, and that `VERSION` at the tag is its version;
`tools/check.sh` runs at the tag; `notes` writes the release's notes, the
commits since the release before it, counting only tags that verify as
releases; and `publish` makes the GitHub release with them.

Then, in JayOS, `tools/package-release.sh jayos-bluetooth <version> --apply`
pins the release: it fetches the tag, verifies it against JayOS's own
`.github/allowed_signers`, writes its version and commit into the recipe, and
vendors its `contract/` and `share/` files into
`tools/fixtures/jayos-bluetooth-contract/` there, with their sums. That is a
change to JayOS, made and committed there, and its suites say whether the
release still fits.

## The owner's steps

Once, before the first release:

1. An SSH key that signs, the same one that signs JayOS's and jayos-audio's
   releases if there is one: `git config gpg.format ssh` and
   `git config user.signingkey <the public key's file>`.
2. Its public half in `.github/allowed_signers`, one line in ssh-keygen's
   ALLOWED SIGNERS format with `namespaces="git"`, committed and merged to
   `main`. The file is empty until then, and every release is refused.
3. The same line in JayOS's `.github/allowed_signers`, which its
   `tools/package-release.sh` verifies a release against.

For each release: raise `VERSION` (and `Cargo.toml` and the checkout recipe's
`pkgver` with it), merge to `main`, then `tools/release.sh`, then
`tools/release.sh --apply`, then JayOS's
`tools/package-release.sh jayos-bluetooth <version> --apply`.

The first is 1.0.0, the version the crate had in JayOS's workspace when it
moved. Until it is made and pinned, JayOS's recipe pins none, and JayOS
cannot build this package.

## What has run, and what has only been read

RUN, on 2026-11-12, in a container that is not Arch:

- `tests/shell/test-release.sh`, 57 assertions, against scratch repositories,
  a bare origin, and two keys `ssh-keygen` made for the run: every refusal
  above, a release made, signed, verified and pushed, two more after it, a
  stranger's key refused with the tag deleted unpushed, `verify` over a
  lightweight tag, an unsigned one, a stranger's, one off `main` and one whose
  `VERSION` says otherwise, the notes counted from the release before, and
  `publish`'s call of `gh release create` against a stand-in.
- `tests/shell/test-packaging.sh`, 28 assertions: the three versions agree;
  `steps.sh package`, from a scratch copy of the tree, installs exactly
  `contract/files.txt`, byte for byte and at the modes it should; `steps.sh
  check` refuses no build, a binary that links a library `depends=` does not
  install (`/usr/bin/curl`'s libcurl, read by the real readelf), and a
  `test-manager.sh` that fails or exits 2; and the recipe's four functions,
  run in errexit shells as makepkg runs them, call the four verbs
  `contract/steps.txt` lists and nothing else.
- `packaging/steps.sh` itself, over this tree: `fetch`, `build` (the release
  build), `check` -- the real binary's NEEDED list, `libgcc_s.so.1` and
  `libc.so.6`, the crate's 105 tests and `tests/shell/test-manager.sh`'s 35
  over the release binary -- and `package` into a scratch directory, whose
  files agreed with `contract/files.txt`.
- `tools/check.sh`: exit 0, "ok, nothing to fix".
- Thirteen mutations, each restored and compared byte for byte afterwards,
  and each failed its suite: in `tools/release.sh`, the branch refusal, the
  tracked-changes refusal, a failed check let through, and a tag that does
  not verify pushed anyway; in `tools/publish-release.sh`, the on-`main`
  check; in `packaging/steps.sh`, `main.conf` installed under `/etc`, one
  drop-in left out, the NEEDED gate let through, the suite's exit 2
  tolerated, and the licence not installed; a path added to
  `contract/files.txt` that nothing installs; the recipe's `pkgver` drifting
  from `VERSION`; and a stale `contract/steps.txt`, which `tools/contract.sh`
  failed.

Read, not run: makepkg, which this container does not have, so neither recipe
has run under it; pacman installing what it makes; the workflow on GitHub's
runners; `gh release create` against GitHub; and a release of this
repository, of which none exists yet.
