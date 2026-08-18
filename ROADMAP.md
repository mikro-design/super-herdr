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
- Opt-in native desktop delivery for filtered metadata-only attention events,
  with history suppression, coalescing, rate limiting, and failure isolation,
  and click-to-jump routing where the desktop reports actions.
- Fuzzy-searchable action palette with qualified workspace, tab, and pane
  lifecycle operations.
- Qualified mouse context menus for sessions, workspaces, tabs, and panes, backed
  by the shared resource-action and close-confirmation paths.
- Live in-session workspace moves that replay an exported split tree with
  documented pane moves and restart no process.
- Explicit cross-session workspace recreation with sanitized layouts, a bounded
  pane count, and a read-only source.

## Next

1. Execute and record the full desktop matrix in `TESTING.md`.
2. Add signed release artifacts and package-manager installation.
3. Select an explicit project license before inviting external contributions.

## Blocked on Herdr

- Moving a workspace between sessions on one host with its processes intact.
  Each session is its own server process, and protocol 19 has no cross-session
  transfer. A true move needs a Herdr-side transfer that hands live PTYs to the
  destination server, the way `server.live_handoff` already does across an
  upgrade. Recreation ships as the client-side substitute and is presented as
  such; it restarts every process and drops scrollback, so it does not close the
  gap.

## Later

- Provider boundaries for optional non-Herdr backends, without depending on
  Herdr internals.
- Read-only shared observation and collaboration controls.
- Enterprise identity, authorization, or audit integrations through established
  access systems rather than a new credential store.

Team sharing, authorization, and auditing remain deferred until the
single-operator terminal and clipboard experience is qualified on every
supported desktop path.
