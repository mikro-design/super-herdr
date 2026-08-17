# Super-Herdr architecture

## Goal

Present multiple persistent Herdr servers as one desktop application without
moving remote shells or agents to the desktop and without losing local clipboard
and image-paste integration.

## Process boundary

```text
desktop
  super-herdr UI + clipboard broker
       |-- SSH --> development host: Herdr session work
       |-- SSH --> build host: Herdr session toolchains
       `-- local --> optional desktop Herdr sessions
```

Super-Herdr is the federation authority. Herdr remains the authority for state
inside one server/session. The initial transport invokes the public `herdr api`
and `herdr terminal session` commands over OpenSSH; a later transport can bridge
the newline-delimited socket protocol without changing the domain model.

## Identity invariant

Herdr identifiers are only unique inside a session. All state and commands use:

```text
QualifiedId = target-name / session-name / server-local-id
```

Raw IDs must never be used as federation map keys. Routing first resolves the
target, then sends the untouched raw ID to that target's Herdr server.

## Compatibility

Each target specifies an ordered `herdr_bins` candidate list. A protocol mismatch
advances to the next client without restarting the server. Super-Herdr records
server version and protocol from each snapshot, negotiates features per target,
and degrades one target rather than the entire UI. It must never restart a session
to repair a mismatch automatically because stopping a Herdr server may exit its
pane processes.

Snapshot parsing treats optional fields as capabilities. Event and terminal
support will be enabled only after the target advertises or successfully probes
the corresponding operation.

The live state loop reconciles snapshots independently for each target, retains
the last known state during bounded reconnect backoff, and assigns a new
connection generation after reconnect. A configured API socket enables one
long-lived documented `events.subscribe` stream per target; events trigger an
immediate authoritative snapshot while the five-second polling deadline remains
the liveness and resynchronization boundary. The stream is replaced only after
failure or a pane-set change, avoiding retained-event replay loops. SSH targets
use OpenSSH Unix-socket forwarding. The TUI keeps terminal screen models only in
memory for currently visible panes.

Before starting per-session supervisors, a host configured for discovery invokes
the documented `herdr session list --json` command. Each returned session becomes
an independently qualified target and uses the reported public socket path. Host
registries are refreshed concurrently every ten seconds. When the qualified
session set changes, Super-Herdr replaces its own supervisors and terminal
routes; it never starts, stops, or restarts Herdr. A stopped session is omitted.
Discovery failure falls back to the configured session and remains isolated to
that host.

The frontend watches the durable TOML configuration through the same bounded
refresh path. CLI and TUI target management use one atomic file store. A refresh
may reconnect Super-Herdr routes but cannot mutate server-owned workspaces,
sessions, or processes. Notification-only configuration changes update the
delivery queue in place and do not rebuild supervisors or terminal routes.

The target-manager form validates a prospective full configuration before save.
Its connection test is an asynchronous, timeout-bounded invocation of the same
documented session registry used during refresh. Results are correlated to the
form request and discarded after any intervening edit. A successful response can
populate a selectable list of running sessions; choosing one records its name and
reported socket in Super-Herdr configuration only. Raw command diagnostics and
SSH material never enter UI state or logs.

The desktop persists versioned UI intent and a separate bounded attention index.
UI intent currently contains the last explicitly selected qualified pane. The
attention index contains only qualified pane identity, agent/workspace labels,
normalized status, transition kind, timestamp, unread state, and the last agent
observation needed for deduplication. Writes use a private state directory and
atomic file replacement. Terminal contents, clipboard payloads, SSH material,
connection leases, and server snapshots are never persisted. Restoration waits
through target reconnects and succeeds only when the exact
target/session/local-ID tuple is live; stale IDs are ignored without mutating the
Herdr server.

Agent attention is derived only from authoritative normalized snapshots. A
documented Herdr event causes an immediate snapshot, so status transitions appear
without waiting for the polling deadline. The first observation establishes a
baseline; later phase changes create one unread event. Repeated snapshots are
deduplicated. An agent is recorded as disappeared only when its target remains
live and an authoritative snapshot removes it; target disconnects never create
false disappearance events. Selecting the qualified pane marks its events read.
The sidebar dedicates its lower half to the independently scrollable attention
feed: agents waiting now precede newest-first transition history. Each actionable
row retains its qualified pane identity for click-to-jump routing. The full
attention center remains available for filtering, unread management, and history
cleanup.

Native desktop delivery is an opt-in consumer of new attention events. Its
cursor starts at the newest persisted event, so startup never replays historical
notifications. Event-kind filters run before a bounded metadata queue; matching
events are deduplicated, briefly coalesced, and rate-limited before an isolated
native command worker receives them. Notification text is constructed only from
the transition kind, qualified target/session, and bounded agent/workspace
labels. Status text, terminal contents, clipboard payloads, SSH material, and raw
diagnostics never enter the delivery object. Native commands have a separate
timeout, and delivery failure changes only a local TUI diagnostic—it cannot stop
target supervision, reconnect a route, or mutate Herdr.

## Terminal data plane

One selected pane owns keyboard input. Observe streams may remain open for visible
background panes; the selected pane uses the control stream. Terminal frames stay
as encoded ANSI payloads until the frontend renderer, avoiding lossy intermediate
screen models. On disconnect, input is disabled until the control lease is
re-established.

The current frontend uses Ratatui with an independent VT parser. It renders a
host/session/workspace sidebar, tab strip, and the visible split panes from Herdr's public
layout rectangles. `Ctrl+]` is the federation prefix for switching qualified
panes. Because the documented terminal-session API is a raw pane stream rather
than Herdr's client-side key dispatcher, `Ctrl+B` enters a Herdr-action mode in
Super-Herdr. Read-only navigation is resolved against the normalized layout;
supported mutations invoke the equivalent documented Herdr CLI operation for
the qualified target and session. Unknown chords are rejected instead of being
sent to the pane process. The selected pane gets a control stream without
`--takeover`; other visible
panes get observer streams. If the control stream is refused or closes, the
selected pane immediately falls back to observation and periodically retries a
normal control lease without interrupting the Herdr server.

Closing a workspace is a qualified federation action: `Ctrl+] d` captures the
selected target, session, and server-local workspace ID, presents all of that
scope for confirmation, and only then invokes the documented
`herdr workspace close` operation. The action never stops or restarts a Herdr
session, and a workspace-close failure remains isolated to its target.

The action palette is backed by typed `ResourceAction` values rather than raw
command strings. An action retains its qualified target/session/resource until
execution; only the final transport step extracts the server-local ID for the
documented Herdr CLI. Search, keyboard shortcuts, and mouse context menus share
one routing model. A right-click captures the exact qualified session,
workspace, tab, or pane under the pointer; it never reconstructs identity from a
display label. Destructive workspace, tab, and pane actions must pass through
the same qualified confirmation path.

Moving a workspace is a multi-request action inside one session. Super-Herdr
reads each source tab's split tree through the documented layout export, reduces
it to one anchor pane plus the ordered splits that rebuild it, and replays that
plan as documented pane moves into the destination workspace. Herdr moves live
panes, so processes, scrollback, and agent authority survive; because a pane
identifier encodes its workspace, each later split targets the identifier the
previous move returned. The whole sequence runs in one task behind the same
single-action guard and reports one result, and a partial failure leaves already
moved tabs in the destination and the remainder in the source without closing
anything. A session is a separate server process that owns its panes, and
protocol 19 has no cross-session transfer, so destinations are restricted to the
source session and cross-session recreation is not silently substituted for a
move.

Crossing a session boundary is a different operation with different guarantees,
and the two are never conflated. Recreation reads the source workspace's tabs and
layouts, reduces each exported tree to structure, ratios, labels, and working
directories, and applies those on the destination session's own socket after
creating a workspace there. Server-local identifiers, pane commands, and pane
environment are dropped rather than forwarded: an identifier is meaningless on
another server, and replaying a command or an environment would run a program or
carry a secret onto another machine. The number of panes one recreation may
start is bounded. The source is only read—never closed—so a failed recreation
costs a partially built destination workspace that the report names, and never
the running work.

The private API socket answers one request per connection. A multi-request action
therefore opens a connection per request while holding the SSH forwarding child
for its whole sequence, so a remote move pays for one tunnel rather than one per
pane.

The outer TUI captures SGR mouse input and translates terminal coordinates back
to the selected pane. A left press is held until the gesture is resolved: release
without movement becomes an application click, while movement becomes local text
selection. Other mouse button reporting honors the mode and encoding tracked from
that pane's ANSI stream. Wheel gestures use the public `terminal.scroll`
controller command so the Herdr server owns application/alternate-screen/host-
scrollback routing. A click on another visible split changes only Super-Herdr's
local selection and consumes the full gesture. Host/session/workspace rows and
tabs are also locally clickable. Right-click is reserved for Super-Herdr's
qualified resource menu and is not forwarded to the selected terminal. When
mouse reporting is disabled, left-button
drags select text and remain clamped to the originating terminal surface.
Observer-only routes select locally because they have no input channel.
The outer capture requests button-motion (`1002`) rather than all-motion (`1003`),
so hover traffic cannot starve clicks and drag updates when Super-Herdr is nested
inside another terminal multiplexer.
Sidebar hit rectangles are derived from the rendered block's inner area and stored
with that frame. A press owns its resolved item until release, matching Herdr's
interaction model instead of recomputing a row from potentially newer state.
The sidebar is split equally between two independently scrollable viewports. The
upper viewport contains every qualified host/session/workspace row; the lower
contains agents waiting now and recent attention transitions. Wheel input changes
only the offset of the viewport under the pointer. Selecting a pane resumes
automatic tracking of the corresponding workspace. Hit rectangles are derived
after applying each offset so clipped rows cannot receive clicks.
Rendering is capped at 60 Hz, terminal-frame events are drained in bounded batches,
and input is prioritized so busy panes cannot starve navigation.

## Clipboard and images

Text selected with a left-button drag is extracted from only the inner VT cells,
with line-end padding removed, and copied through a native desktop clipboard tool
when available or OSC 52 otherwise. Payloads are bounded and never logged.
The supported reliable topology runs this broker on the desktop: macOS uses local
`pbcopy`, while terminal servers remain on their target hosts. A broker launched
through plain SSH has no acknowledged desktop clipboard channel; OSC 52 is a
best-effort request whose acceptance is controlled by the local terminal emulator.
Finalized selections remain rendered until the next click or key. `HERDR_ENV=1`
forces OSC 52 so a nested Super-Herdr uses the outer Herdr client's clipboard
forwarding rather than a host-local graphical clipboard.
Selection is rendered with the color-independent reverse-video modifier so a
`NO_COLOR` environment cannot make the marked range invisible.
Local-to-remote text has two entry paths. Super-Herdr enables bracketed paste in
the outer terminal and buffers a normal terminal paste as one bounded event; the
explicit desktop-broker action reads from `pbpaste`, `wl-paste`, `xclip`, or
`xsel`. Both paths apply a 1 MiB input limit and deliver one documented
`pane.send_input` request through the target session's private Herdr API socket.
Herdr owns the authoritative runtime input state, adds bracketed-paste markers
when needed, and writes the complete input atomically. SSH targets use a bounded
OpenSSH Unix-socket forward. Clipboard text is never placed in command arguments,
logs, persistent state, or terminal frames. When no documented socket is known,
Super-Herdr refuses multiline input rather than forwarding newline-delimited raw
input that could become multiple messages. The explicit desktop broker remains
unavailable when Super-Herdr itself runs through SSH because the remote process
cannot directly read the client's clipboard; normal paste from the local terminal
remains supported in that topology. PNG image bridging reads explicitly from the
desktop clipboard, uploads at most 32 MiB to a private per-target temporary
directory, verifies the remote byte count and SHA-256 digest, and injects only the
resulting path through the selected terminal route. File-list clipboard upload
remains a later extension of the same broker. Remote agents never need desktop
clipboard access.

Herdr 0.8's documented `terminal session` stream currently exposes terminal
frames, closure, input, resize, scroll, and release, but not the server-to-client
clipboard message used by the native Herdr client. Remote-to-local clipboard
forwarding therefore needs an added public terminal-session envelope (preferred)
or a separately reviewed documented transport. Super-Herdr must not scrape or
depend on Herdr's private client protocol to bypass that boundary.

## Security

- OpenSSH configuration and host-key verification remain authoritative.
- Batch mode is the default so a dead target cannot freeze the whole UI on a
  password prompt.
- Remote command arguments are POSIX-shell quoted; SSH destinations beginning with
  an option or containing whitespace/control characters are rejected.
- Credentials, clipboard payloads, and terminal contents are not written to logs.
- A target failure is isolated and bounded by connect and command timeouts.

## Source boundary

The implementation uses Herdr's documented Socket API and CLI reference:

- https://herdr.dev/docs/socket-api/
- https://herdr.dev/docs/cli-reference/

Super-Herdr should not copy Herdr internals unless the project intentionally
accepts the licensing and maintenance coupling: https://github.com/ogulcancelik/herdr
