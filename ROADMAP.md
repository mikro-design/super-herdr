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
- Dual MIT/Apache-2.0 licensing, so the project can accept outside contributions.
- Signed releases: a keyless-signed checksum manifest, build provenance
  attestations, Debian packages, and a generated Homebrew formula.
- First signed release published as v0.3.1, with the formula live in
  `mikro-design/homebrew-tap` so `brew install mikro-design/tap/super-herdr`
  works on macOS and Linux.
- Live in-session workspace moves that replay an exported split tree with
  documented pane moves and restart no process.
- Explicit cross-session workspace recreation with sanitized layouts, a bounded
  pane count, and a read-only source.
- A versioned client protocol and a headless `super-herdr daemon` that serves
  federation state, shared per-pane terminal routes, resolved Herdr operations,
  and the durable attention index over an owner-only Unix socket, with an
  exclusive per-pane control lease and an explicit takeover.
- Live configuration and session-discovery refresh inside the daemon, which
  rebuilds supervisors without restarting Herdr and retires only the routes
  whose target actually changed.
- The TUI as a client of that daemon, hosting one in-process so a
  single-machine install still runs as one command and binds nothing, and
  reaching Herdr only through resolved operations and shared pane routes.

## Next

1. Execute and record the full desktop matrix in `TESTING.md`. The
   machine-decidable checks are automated as `scripts/qualify-desktop.sh` and
   recorded for a nested run; the macOS, Wayland, and X11 rows, and every item
   needing a pointer or a notification click, are still unrecorded.
2. Move the durable attention index and native notification delivery into the
   daemon. The frontend still owns both, so a hosted daemon is told not to
   derive attention; until this lands, an attached phone cannot learn that an
   agent is waiting unless the desktop frontend is running.
3. Give a remote client a path for atomic multiline paste and clipboard media
   upload. Both still run from the frontend against the target's own Herdr API
   socket, which works only while the frontend and the daemon share a machine.
4. Remove the daemon's socket on a signalled shutdown. A stale socket is
   already replaced on the next start, so this costs a leftover file rather
   than a failed restart, but the daemon should not rely on that.
5. Add device pairing: a pairing code presented by the TUI, a revocable
   per-device token in the existing atomic TOML store, and a network transport.
   Until then a remote client reaches the daemon's Unix socket over OpenSSH
   forwarding, which is why this can be decided deliberately rather than in a
   hurry.
6. Generalize the clipboard broker's verified upload into a file bridge —
   arbitrary content, caller-supplied name, chunked and resumable, streamed at
   every hop — covering client-to-target, target-to-client, and target-to-target
   transfers.
7. Ship the first remote client as a web client the daemon serves, covering
   tablet and phone from one codebase. Navigation, the attention feed, and
   read-only pane observation come before input.
8. Extend attention delivery with push notifications to paired devices, as a
   further sink under the existing filters, coalescing, and rate limits.

## Blocked on Herdr

- Pane-initiated file delivery, where a person inside a pane sends the file they
  are looking at to whichever client they have attached. The documented
  `terminal session` stream carries frames, closure, input, resize, scroll, and
  release, with no client-bound envelope a host-side helper could write into.
  Transfers are initiated from the client until such an envelope exists;
  scraping pane output for markers is not an acceptable substitute.
- Moving a workspace between sessions on one host with its processes intact.
  Each session is its own server process, and protocol 19 has no cross-session
  transfer. A true move needs a Herdr-side transfer that hands live PTYs to the
  destination server, the way `server.live_handoff` already does across an
  upgrade. Recreation ships as the client-side substitute and is presented as
  such; it restarts every process and drops scrollback, so it does not close the
  gap.

## Later

- Native tablet and phone clients on the same daemon protocol, for hardware
  keyboard handling and filesystem integration through an iOS File Provider or
  an Android DocumentsProvider.
- Provider boundaries for optional non-Herdr backends, without depending on
  Herdr internals.
- Read-only shared observation and collaboration controls.
- Enterprise identity, authorization, or audit integrations through established
  access systems rather than a new credential store.

Team sharing, authorization, and auditing remain deferred until the
single-operator terminal and clipboard experience is qualified on every
supported desktop path. Device pairing above is not that work: it grants one
operator's own devices access to that operator's own daemon and adds no shared
or delegated authority.
