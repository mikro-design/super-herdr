# Packaging

Package definitions are generated, not maintained by hand: a formula must name
the exact archives a release published and their checksums, and a hand-edited
copy drifts silently. `scripts/render-packaging.sh` renders the Homebrew formula
from a release's own `SHA256SUMS`, the release workflow runs it and attaches the
result to the GitHub release, and `scripts/verify-packaging.sh` proves the
rendered checksums match before the release exists.

## Publishing a release to Homebrew

1. Tag a release (`git tag vX.Y.Z && git push origin vX.Y.Z`) and wait for the
   `CI and Release` workflow.
2. Download `super-herdr.rb` from the finished release.
3. Commit it to
   [`mikro-design/homebrew-tap`](https://github.com/mikro-design/homebrew-tap)
   as `Formula/super-herdr.rb`, unchanged. Users then install with
   `brew install mikro-design/tap/super-herdr`.

This is not automated, because pushing to the tap needs credentials that this
repository deliberately does not hold.

## Publishing a release to the APT repository

`apt install super-herdr` is served by
[`mikro-design/apt`](https://github.com/mikro-design/apt), a second repository
published with GitHub Pages. It is separate from this one so that released
binaries never enter this repository's history, and so that publishing and
source do not share push permissions.

Unlike a Homebrew formula, an APT repository is **signed**. `apt` does not
verify packages; it verifies an index, and trusts the packages that index names
by checksum. The index is therefore the whole security boundary, and the
private key that signs it is deliberately not held by this repository's CI —
the same reason the Homebrew step above is manual. A leaked signing key is worse
than leaked push access: it signs an index served from anywhere, by anyone, and
there is nothing to revoke short of getting a new key onto every machine that
installed the old one.

1. Tag a release and wait for the `CI and Release` workflow, as above.
2. From a checkout of this repository, on the machine holding the signing key:

   ```sh
   scripts/publish-apt-repo.sh 0.7.23 ../apt "${SUPER_HERDR_APT_KEY}"
   ```

   That downloads the release's `.deb` packages, checks them against the
   release's own `SHA256SUMS`, copies them into the pool, prunes to the last ten
   releases, renders the index, signs it, and verifies the result.
3. Commit and push the `apt` checkout. The script prints the exact commands and
   deliberately does not run them: publishing is the irreversible step.

The three scripts are separate because only one of them needs the key.
`render-apt-repo.sh` is reproducible and runs anywhere; `sign-apt-repo.sh` runs
where the key is; `verify-apt-repo.sh` reads only published bytes and re-checks
every signature, index checksum and package checksum the way a client would.

### The signing key

Generate it once, on the machine that will keep it:

```sh
gpg --quick-generate-key "Super-Herdr <mikrodesign@proton.me>" ed25519 sign 2y
```

An expiry means a forgotten key stops being trusted rather than living forever;
it can be extended at any time with `gpg --quick-set-expire`. Keep the
revocation certificate GnuPG writes into `openpgp-revocs.d` somewhere other than
the machine holding the key.

The public half is exported into the repository as `super-herdr.gpg` on every
publish, which is the file the README tells users to install.

## No Arch package

There is deliberately no AUR package. Publishing to the AUR authenticates only
by an SSH key registered to an AUR account, so it cannot run from CI without
parking a private key in secrets, and it would need a manual push on every
release. Homebrew covers macOS and Linux and the Debian packages cover
Debian and Ubuntu; Arch users take a prebuilt archive.

## Rendering locally

```sh
scripts/render-packaging.sh 0.2.1 SHA256SUMS mikro-design/super-herdr ./out
```

## Debian packages

`.deb` packages for amd64 and arm64 are built in the release workflow with
`cargo deb --no-build`, reusing the same binaries that go into the archives, so a
`.deb` and its matching `.tar.gz` contain byte-identical executables. Debian
metadata lives in `[package.metadata.deb]` in `Cargo.toml`.
