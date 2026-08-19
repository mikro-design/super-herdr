# Packaging

Package definitions are generated, not maintained by hand: a formula or PKGBUILD
must name the exact archives a release published and their checksums, and a
hand-edited copy drifts silently. `scripts/render-packaging.sh` renders the formula,
the PKGBUILD, and the matching `.SRCINFO` from a release's own `SHA256SUMS`, and
the release workflow runs it and attaches the results to the GitHub release.

## Publishing a release to the package managers

1. Tag a release (`git tag vX.Y.Z && git push origin vX.Y.Z`) and wait for the
   `CI and Release` workflow.
2. Download `super-herdr.rb`, `PKGBUILD`, and `super-herdr-bin.SRCINFO` from the
   finished release.
3. Homebrew: commit `super-herdr.rb` to
   [`mikro-design/homebrew-tap`](https://github.com/mikro-design/homebrew-tap)
   as `Formula/super-herdr.rb`, unchanged. Users then install with
   `brew install mikro-design/tap/super-herdr`.
4. AUR: commit `PKGBUILD` and `super-herdr-bin.SRCINFO` (renamed to `.SRCINFO`)
   to the `super-herdr-bin` AUR repository and push:

   ```sh
   git clone ssh://aur@aur.archlinux.org/super-herdr-bin.git
   cp PKGBUILD super-herdr-bin/PKGBUILD
   cp super-herdr-bin.SRCINFO super-herdr-bin/.SRCINFO
   cd super-herdr-bin && git add -A && git commit -m "Update to X.Y.Z" && git push
   ```

Neither step is automated, because pushing to a tap or to the AUR needs
credentials that this repository deliberately does not hold. The AUR push in
particular needs an AUR account whose SSH key is registered there.

## Why `.SRCINFO` is rendered rather than generated

The AUR validates a push against `.SRCINFO`, not against `PKGBUILD`, so a stale
`.SRCINFO` publishes the wrong checksums. `makepkg` cannot run on the Ubuntu
release runner, so the script renders the same projection from the same values
that produce the `PKGBUILD`. On a machine that has `makepkg`, the rendering can
be confirmed against the real thing:

```sh
cd out && makepkg --printsrcinfo | diff - super-herdr-bin.SRCINFO
```

The rendered file drops the leading dot because the release workflow attaches
`release-files/*`, and that glob skips hidden files.

## Rendering locally

```sh
scripts/render-packaging.sh 0.2.1 SHA256SUMS mikro-design/super-herdr ./out
```

## Debian packages

`.deb` packages for amd64 and arm64 are built in the release workflow with
`cargo deb --no-build`, reusing the same binaries that go into the archives, so a
`.deb` and its matching `.tar.gz` contain byte-identical executables. Debian
metadata lives in `[package.metadata.deb]` in `Cargo.toml`.
