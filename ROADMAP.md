# Super-Herdr roadmap

Work is ordered by dependency and product risk.

The community-informed product epics that follow these qualification gates are
specified as checkable work in [PRODUCT_BACKLOG.md](PRODUCT_BACKLOG.md).

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
- Qualified Herdr 0.8.2 plugin actions in the TUI and browser, with
  metadata-only lifecycle polling and an explicit, non-focus-stealing route to
  newly opened plugin panes. Plugin commands, process output, and terminal
  selection text are not forwarded.
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
- Device pairing: a code requested from the terminal client and manually typed
  in the browser, followed by an explicit matching-number approval in the
  trusted TUI. Only then is a token issued; its digest alone is stored and it is
  revocable with immediate effect. Browser events and commands require that
  token even on loopback.
- A browser client served by both a standalone daemon and the daemon hosted by
  the normal TUI, with a default outbound reverse tunnel to the fixed public
  bridge and explicit direct/private/Tailscale routes as opt-outs.
- A bounded bridge transport and standalone `super-herdr-bridge` origin:
  random per-run routes, separately authorized daemon registration, bounded
  routes/viewers/frames/queues, path-aware browser requests, and failure
  isolation that never touches a Herdr session.
- Multi-user device-login rendezvous at the fixed bridge URL: each daemon
  publishes one expiring code, the user types it once at the bridge, collisions
  route nobody, bridge guesses are bounded per source, and the owning daemon
  verifies and spends it. A matching-number approval on a trusted TUI is still
  required before the resulting HttpOnly cookie is scoped to that daemon's
  random route.
- Rendered browser pane observation with acknowledged, bounded updates and
  current-screen catch-up instead of a backlog for a slow viewer. Navigation is
  grouped by target, session, and workspace, and selecting a pane opens its
  viewer immediately.
- Explicit browser control takeover and terminal input: a paired device begins
  as an observer, asks for the pane lease, and can then send a line, Enter, Tab,
  Escape, Ctrl-C, and shell-history arrows. Losing the lease removes its input
  surface.
- A three-direction file bridge in the daemon protocol: resumable
  client-to-target upload, credit-bounded target-to-client download, and
  target-to-target movement without routing the bytes through the device.
- Direct TUI file upload by dropping a file path onto the terminal, pasting a
  copied file, or entering a path explicitly.

## Next

Work below is acceptance-ordered. The hosted no-Tailscale route is deployed;
real-device and operational qualification remain first because deployment alone
does not prove that the browser path is reliable.

1. Operationally qualify the deployed bridge at `super-herdr.key-value.co`.
   Keep the origin on loopback, proxy WebSocket upgrades without buffering,
   retain connection/rate limits outside the binary's own hard bounds, and
   ensure proxy/platform logs contain no authorization headers, request or
   response bodies, terminal content, clipboard payloads, pairing material, or
   secrets. Record health, service-restart, connector-reconnect, and one relayed
   page-request check without recording private routes or pairing material.
2. Qualify the browser control path on a real phone **without Tailscale**.
   Record the phone OS,
   browser, network kind, Herdr version/protocol, and exact Super-Herdr commit in
   `TESTING.md`, without recording addresses, hostnames, terminal contents, or
   pairing material. The qualification is complete only when all of these have
   been observed:
   - an unconfigured installation returns
     `https://super-herdr.key-value.co`, a phone on an unrelated network scans
     the QR, the user types the displayed code once, compares the browser's
     six-digit number with the trusted TUI, approves it there, and the accepted
     browser reaches only the daemon that published it;
   - selecting a pane begins in observe mode and sends no input;
   - **Take control**, a Unicode line, Enter, Tab, Escape, Ctrl-C, and both
     history arrows reach only the qualified target/session/pane selected;
   - taking the lease from another client downgrades that client without
     closing the Herdr session or pane, and an observer cannot send input;
   - reloading or briefly disconnecting the phone restores federation and pane
     state, a slow viewer catches up to the current screen without a backlog,
     and revoking the device takes effect on its next request.
   Run `node tools/page-harness.mjs src/daemon/app.html` during the qualification
   and extend it for any browser defect found by the physical run.
3. Execute and record the full desktop matrix in `TESTING.md`. The
   machine-decidable checks are automated as `scripts/qualify-desktop.sh` and
   recorded for a nested run; the macOS, Wayland, and X11 rows, and every item
   needing a pointer or a notification click, are still unrecorded.
4. Put client surfaces around the file-bridge directions the protocol already
   carries. The TUI still needs a qualified target-to-client save flow and a
   qualified source-to-destination move flow. The browser needs upload and
   download controls appropriate to the platform. Each surface must preserve
   per-pane control leases, target/session-qualified IDs, cancellation cleanup,
   bounded transfer state, and per-target failure isolation; filenames may be
   shown, but contents and credentials must never be logged.
5. Stream TUI uploads from disk through the existing resumable protocol. File
   drop and explicit-path upload currently read the file whole and inherit the
   32 MiB clipboard ceiling; the general transfer path should instead use
   `transfers.max_bytes`, bounded chunks, and host-reported resume offsets.
6. Add push delivery of attention events to paired devices, as a further sink
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
  Each session is its own server process, and the documented API has no
  cross-session transfer. A true move needs a Herdr-side transfer that hands
  live PTYs to the destination server, the way `server.live_handoff` already
  does across an upgrade. Recreation ships as the client-side substitute and is
  presented as such; it restarts every process and drops scrollback, so it does
  not close the gap.

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
