# Project instructions

- Keep Super-Herdr independent of Herdr internals; integrate through documented
  CLI/socket interfaces until an explicit licensing decision is made.
- Treat all Herdr IDs as server-local and qualify them with target and session.
- Never stop or restart a user's Herdr session automatically.
- Isolate failures per target and put timeouts around network operations.
- Do not log clipboard payloads, terminal contents, secrets, or SSH material.
- Preserve the four-job build limit and do not run broad stress/test workloads
  without explicit approval.
- Run `cargo fmt --check`, `cargo test`, and `cargo clippy --all-targets --
  -D warnings` before handing off code changes, and lint the macOS target too
  (`cargo clippy --target x86_64-apple-darwin --all-targets -- -D warnings`).
  Platform-gated code is invisible to a host-only lint, and test code is
  invisible without `--all-targets`; CI enforces both.
- Do not commit or push unless the user asks.
