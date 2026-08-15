# Super-Herdr roadmap

Work is ordered by dependency and product risk.

## Completed foundation

- Persistent multi-host federation with qualified target/session IDs.
- Live TUI terminal observation and selected-pane control routing.
- Atomic terminal/desktop multiline paste and verified PNG upload bridge.
- Atomic UI selection persistence.
- CLI target add/list/edit/remove backed by atomic TOML writes.
- Mouse- and keyboard-driven target manager with live validation, asynchronous
  connection testing, and selectable discovered sessions.
- Concurrent live configuration and running-session registry refresh.
- Global agent navigator with all/attention/active filters.
- Persistent lower-sidebar attention feed with current waiting agents, recent
  transitions, independent scrolling, and qualified click-to-jump rows.
- Durable payload-free attention history with unread transition tracking and
  deduplication across restarts.
- Fuzzy-searchable action palette with qualified workspace, tab, and pane
  lifecycle operations.
- Qualified mouse context menus for sessions, workspaces, tabs, and panes, backed
  by the shared resource-action and close-confirmation paths.

## Next

1. Execute and record the full desktop matrix in `TESTING.md`.
2. Add opt-in native desktop delivery for metadata-only attention events.
3. Add signed release artifacts and package-manager installation.
4. Select an explicit project license before inviting external contributions.

## Later

- Provider boundaries for optional non-Herdr backends, without depending on
  Herdr internals.
- Read-only shared observation and collaboration controls.
- Enterprise identity, authorization, or audit integrations through established
  access systems rather than a new credential store.

Team sharing, authorization, and auditing remain deferred until the
single-operator terminal and clipboard experience is qualified on every
supported desktop path.
