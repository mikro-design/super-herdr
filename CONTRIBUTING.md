# Contributing to Super-Herdr

Focused bug fixes, tests, documentation improvements, and narrowly scoped
features are welcome. Open an issue before starting a large change so its
security and Herdr compatibility boundaries can be agreed first.

## Project boundaries

- Integrate with Herdr through documented CLI and socket interfaces. Do not
  copy or depend on Herdr internals.
- Treat every Herdr identifier as server-local and qualify it with target and
  session before using it in federation state.
- Never automatically stop, restart, or take over a user's Herdr session.
- Isolate target failures and bound network operations with timeouts.
- Never log terminal contents, clipboard payloads, secrets, device tokens, or
  SSH material.
- Keep build and test concurrency at four jobs or fewer.

## Development setup

```sh
cp config.example.toml config.local.toml
cargo run -j 4 -- --config config.local.toml check
```

`config.local.toml` is ignored. Keep real hostnames, usernames, socket paths,
credentials, and pairing material out of commits, tests, screenshots, and issue
reports.

## Required checks

Run these before opening a pull request:

```sh
cargo fmt --check
node tools/check-docs.mjs
cargo test --locked -j 4
cargo clippy --locked -j 4 --all-targets -- -D warnings
rustup target add x86_64-apple-darwin
cargo clippy --locked -j 4 --target x86_64-apple-darwin --all-targets -- -D warnings
node tools/page-harness.mjs src/daemon/app.html
node tools/page-harness.mjs src/daemon/app.html /r/0123456789abcdef0123456789abcdef/
node tools/bridge-page-harness.mjs crates/bridge/src/bridge.html
```

Changes involving a desktop, browser, SSH login shell, or real process boundary
also need the applicable manual checks in [TESTING.md](TESTING.md). Automated
tests do not qualify those environments.

## Pull requests

Keep commits reviewable and explain the user-visible behavior, security impact,
and verification performed. Do not include generated build output. A pull
request grants no permission to alter or stop a reviewer's running Herdr
sessions.

Contributions are accepted under the repository's dual MIT OR Apache-2.0
license, as described in [README.md](README.md#license).
