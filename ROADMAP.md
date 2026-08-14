# Super-Herdr roadmap

Work is ordered by dependency and product risk.

## Completed foundation

- Persistent multi-host federation with qualified target/session IDs.
- Live TUI terminal observation and selected-pane control routing.
- Desktop text clipboard and verified PNG upload bridge.
- Atomic UI selection persistence.
- CLI target add/list/edit/remove backed by atomic TOML writes.
- In-TUI target add/edit/remove manager.
- Concurrent live configuration and running-session registry refresh.
- Global agent navigator with all/attention/active filters.
- Persistent cross-host waiting-agent list with qualified click-to-jump rows.
- Fuzzy-searchable action palette with qualified workspace, tab, and pane
  lifecycle operations.

## Next

1. Add mouse context menus backed by the shared resource-action layer.
2. Add unread activity tracking and a durable, payload-free notification index.
3. Add mouse activation and richer validation to target-manager forms.
4. Execute and record the full desktop matrix in `TESTING.md`.
5. Add signed release artifacts and package-manager installation.
6. Select an explicit project license before inviting external contributions.

## Later

- Provider boundaries for optional non-Herdr backends, without depending on
  Herdr internals.
- Read-only shared observation and collaboration controls.
- Enterprise identity, authorization, or audit integrations through established
  access systems rather than a new credential store.

Team sharing, authorization, and auditing remain deferred until the
single-operator terminal and clipboard experience is qualified on every
supported desktop path.
