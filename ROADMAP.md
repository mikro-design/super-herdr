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
- A daemon-owned durable attention index, mirrored by each client from the
  history it is sent on subscribe, with read state as a request the daemon
  answers by republishing the authoritative history.
- Signalled shutdown that removes the daemon's socket, so a path left on disk
  means a process that died rather than one that stopped.
- Atomic multiline paste and clipboard media upload through the daemon, with the
  upload offered as a declared length, chunked payload, and digest trailer that
  is verified before anything reaches the target host.
- A browser client the daemon serves over loopback HTTP, carrying the protocol
  on a server-sent event stream with commands posted back, showing the
  federation and the attention feed on a phone or tablet.
- Device pairing: a code requested from the terminal client, exchanged by a
  browser for a token whose digest alone is stored, revocable with immediate
  effect, and a listener that refuses any address a token could not safely
  authenticate over.

## Next

1. Execute and record the full desktop matrix in `TESTING.md`. The
   machine-decidable checks are automated as `scripts/qualify-desktop.sh` and
   recorded for a nested run; the macOS, Wayland, and X11 rows, and every item
   needing a pointer or a notification click, are still unrecorded.
2. Generalize the clipboard broker's verified upload into a file bridge —
   arbitrary content, caller-supplied name, chunked and resumable, streamed at
   every hop — covering client-to-target, target-to-client, and target-to-target
   transfers. Client-to-target is done for content that fits one attempt: the
   daemon relays a transfer as it arrives rather than holding it, with
   backpressure reaching the sending client, the trailer checked against the
   bytes actually relayed, and a refused or abandoned transfer unstaged on the
   host. Content is arbitrary, the caller may name the file, the ceiling is
   `transfers.max_bytes` rather than the clipboard's, and a transfer survives a
   reconnect: an interruption keeps what arrived, a sender returns with the
   token it was issued, and the offset comes from the host's own count. What
   Target-to-client is done too: a client names a file on the pane's host, the
   host describes it and sends it, credit in the protocol bounds what is in
   flight because the queue to a client cannot push back, and the client is the
   one that verifies. What remains is target-to-target without the device in
   the middle.
3. Add read-only pane observation to the browser client. Navigation and the
   attention feed are served; rendering a terminal needs a VT implementation in
   the page, which means vendoring one — the first third-party code the client
   would carry, and worth deciding deliberately rather than reaching for.
   Typing into a pane needs the socket upgrade a post-per-keystroke would
   deserve.
4. Add push delivery of attention events to paired devices, as a further sink
   under the existing filters, coalescing, and rate limits. Native desktop
   delivery is not moving with it: notifying a desktop is a desktop-session
   capability and stays with the client, for the same reason reading a
   clipboard does. Push is the half that needs a process awake when no
   frontend is.

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
