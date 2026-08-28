## Summary

Describe the user-visible change and why it is needed.

## Safety and compatibility

- [ ] Uses only documented Herdr CLI/socket interfaces.
- [ ] Keeps Herdr IDs qualified by target and session.
- [ ] Does not automatically stop or restart a Herdr session.
- [ ] Does not log terminal contents, clipboard payloads, credentials, tokens, or SSH material.
- [ ] Isolates target failures and bounds network operations.

## Verification

- [ ] `cargo fmt --check`
- [ ] `cargo test --locked -j 4`
- [ ] Host and macOS-target Clippy with `--all-targets -- -D warnings`
- [ ] Browser harnesses when browser or bridge behavior changed
- [ ] Applicable manual checks from `TESTING.md`

List the checks actually run and any environment-dependent checks that remain.
