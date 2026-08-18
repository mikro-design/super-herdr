# Packaging

Package definitions are generated, not maintained by hand: a formula or PKGBUILD
must name the exact archives a release published and their checksums, and a
hand-edited copy drifts silently. `scripts/render-packaging.sh` renders both from
a release's own `SHA256SUMS`, and the release workflow runs it and attaches the
results to the GitHub release.

## Publishing a release to the package managers

1. Tag a release (`git tag vX.Y.Z && git push origin vX.Y.Z`) and wait for the
   `CI and Release` workflow.
2. Download `super-herdr.rb` and `PKGBUILD` from the finished release.
3. Homebrew: commit `super-herdr.rb` to `mikro-design/homebrew-tap` as
   `Formula/super-herdr.rb`. Users then install with
   `brew install mikro-design/tap/super-herdr`.
4. AUR: commit `PKGBUILD` to the `super-herdr-bin` AUR repository, regenerate
   `.SRCINFO` with `makepkg --printsrcinfo > .SRCINFO`, and push.

Neither step is automated, because pushing to a tap or to the AUR needs
credentials that this repository deliberately does not hold.

## Rendering locally

```sh
scripts/render-packaging.sh 0.2.1 SHA256SUMS mikro-design/super-herdr ./out
```

## Debian packages

`.deb` packages for amd64 and arm64 are built in the release workflow with
`cargo deb --no-build`, reusing the same binaries that go into the archives, so a
`.deb` and its matching `.tar.gz` contain byte-identical executables. Debian
metadata lives in `[package.metadata.deb]` in `Cargo.toml`.
