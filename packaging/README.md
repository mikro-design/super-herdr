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
