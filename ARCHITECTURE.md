# Super-Herdr architecture

## Goal

Present multiple persistent Herdr servers as one desktop application without
moving remote shells or agents to the desktop and without losing local clipboard
and image-paste integration. The same federation should also be reachable from a
tablet or phone without moving that authority onto the device, and files should
cross between device, daemon host, and target through one verified path rather
than an ad hoc copy; see "Daemon and remote clients" and "File bridge".

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

A notification may carry one action back. Delivery and activation are separate
phases: delivery completes when the desktop accepts the notification, so a burst
is never gated on a person reading one, while a bounded wait watches for the
click. The queued delivery object carries the qualified pane only as routing
data—it is never rendered into the notification text—and an activation is
accepted only when the desktop reports the one offered action identifier. A
clicked pane is re-checked against live state before the selection moves,
because the identifier is only as fresh as the notification that carried it; a
pane that has since closed marks its events read instead. Whether the desktop
can report a click at all is probed once from the notification tool's own
advertised options, since an older tool rejects unknown options outright and
would otherwise lose notifications entirely.

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

## Daemon and remote clients

This boundary is designed, not implemented. It is recorded here so the split is
planned rather than discovered.

The federation authority has to run where SSH configuration, host keys, and an
always-on network already live. A tablet or phone is none of those things: it
suspends background work, it should not carry credentials for every target, and
it cannot hold one supervisor per qualified session across a day of sleep. So
the reach-it-from-anywhere problem is not solved by porting the frontend. The
binary splits instead: a headless daemon keeps every responsibility described
above, and clients render. The existing TUI becomes one client. That boundary
already exists inside the process—`FederationStore` publishes state through a
watch channel, terminal routes publish encoded frames, and `ResourceAction`
values carry qualified intent—so a remote client replaces a function call with a
socket at exactly those three points and is granted nothing else.

```text
always-on host
  super-herdr daemon: federation authority
       |-- SSH --> development host: Herdr session work
       |-- SSH --> build host: Herdr session toolchains
       `-- one authenticated protocol over a private network
              |-- super-herdr TUI, local or remote
              `-- tablet and phone clients
```

The protocol carries four streams. Federation state is a snapshot followed by
deltas and drives all navigation. Attention events are already bounded and
payload-free. Terminal frames flow only for panes a client has explicitly
subscribed to; a phone subscribes to one. Upstream, a client sends typed actions
and pane input. Frames stay encoded ANSI until that client's renderer, matching
the rule the local frontend already follows, so no lossy intermediate screen
model is introduced by the hop. Per-client queues are bounded and coalesced, and
a client that stops reading—a backgrounded phone—loses frames rather than
growing a queue, because the next frame is authoritative anyway.

Identity and authority do not change. A client never resolves a target; it sends
qualified IDs and the daemon routes them exactly as the local frontend does. A
client holds no SSH material, no Herdr socket path, and no configuration
authority. A lost or compromised device therefore reaches a host only through
the daemon's own policy, and revoking it is a daemon-side operation rather than
a key rotation on every target.

Several attached clients make control arbitration a real concept for the first
time. "The selected pane owns keyboard input" is a frontend-local rule; with a
phone and a desktop attached at once, the per-pane control lease becomes
daemon-owned and exclusive. A second client observes that pane and may request a
takeover that the current holder is told about. Two keyboards are never silently
multiplexed into one PTY.

The daemon holds one route per pane rather than one per client, so two people
watching the same pane cost the target one observe stream. That sharing is what
makes arbitration necessary, and it fixes the rules: a client asking for control
that another holds is given observation rather than a refusal, an explicit
takeover downgrades the previous holder and tells it so, and a holder that
disconnects frees the lease without handing it to an observer that never asked
for one. Input, scroll, and resize are refused for a client without the lease
and reported back, because silently dropping a keystroke leaves a person typing
into a pane that never answers. A route opened only to observe cannot be
promoted in place, so a takeover reopens it with an input channel.

Pane size follows the control lease. Only the lease holder may resize, so an
observer on a phone cannot reshape a pane someone is working in, and a lease
that moves to a client of a different size resizes the pane with it. A route is
retired when an authoritative snapshot from a live target no longer contains its
pane; a target in backoff proves nothing about its panes and never retires one,
matching the rule attention tracking already uses to avoid inventing
disappearances during a reconnect.

A client asks for operations, not intents. The frontend's `ResourceAction`
values are requests to open a prompt or a confirmation: renaming means *ask for
a label*, closing means *ask whether they meant it*. Asking is the client's job,
so the wire carries a resolved `Operation` instead — a rename that already holds
its new label, a close that names only something the person has agreed to lose.
A daemon receiving intents would have to grow a UI or guess. Every operation
names exactly one session, and the server-local identifier is extracted at the
final transport step, so a multi-request move and a single documented command
travel the same qualified path.

The daemon refreshes the durable configuration on the same bounded schedule the
frontend uses, and one refresh runs at a time so a slow discovery on an
unreachable host cannot queue up behind itself. A refresh rebuilds supervisors
but never starts, stops, or restarts Herdr, and it retires only the routes whose
target actually changed: a route is an SSH child already talking to a Herdr
server, so re-reading a file must not cost somebody the terminal they are
working in. A changed transport invalidates every route because the command that
opened them would now be built differently. A configuration that fails to parse
leaves the running federation exactly as it was.

The daemon listens on a Unix socket with owner-only permissions and is not
published to a network. A client on another machine reaches it the way
Super-Herdr already reaches a Herdr socket: forwarded over OpenSSH. That keeps
the security posture unchanged for now — OpenSSH remains the authority, and
there is no second credential store — and leaves device pairing and a network
transport as one deliberate decision rather than something that arrives by
accident with the first remote client. A socket that refuses a connection is a
leftover from a process that did not clean up and is replaced; one that accepts
a connection belongs to a running daemon and is never evicted.

Exposure is deliberately delegated. The daemon binds to loopback or a private
interface and is not published to the public internet; reachability from
elsewhere is a WireGuard or Tailscale concern rather than a transport
Super-Herdr reimplements. Each device is paired out of band—the TUI presents a
pairing code—and receives a revocable per-device token recorded in the same
atomic TOML store that already holds targets, bound to a TLS connection. This is
device pairing for one operator's own clients. It is deliberately not the shared
identity, authorization, or audit system the roadmap defers, and it must not
grow into one by accident.

The frontend is a client of that daemon and hosts one inside itself, so a
single-machine install remains one command with no socket and no service to
operate. It attaches over an in-memory pipe rather than a socket, but takes the
same code path: one implementation of framing, handshake, leases, and every rule
behind them, so nothing can hide in the mode nobody runs on their own machine.
What stays in the frontend is what a renderer owns — the VT parser, the screen
model, selection, and the desktop clipboard — and what leaves is everything that
talks to a Herdr server. A frontend no longer builds a Herdr command line; it
names a resolved operation and the daemon chooses the client, resolves the
target, and extracts the server-local identifier at the last step.

Two rules moved with that split. A refused control stream falls back to
observation rather than closing the pane, which was a frontend rule and is now
the daemon's, so it holds for every attached client at once. And the frontend
distinguishes what it asked for from what it was granted: comparing the two
would otherwise make a downgraded lease look like a stale subscription and
resubscribe it forever.

The durable attention index stays with the frontend for now, because exactly one
process may own it — two would write the same file with independently numbered
events — and moving it is one step with native notification delivery rather than
two half-steps. A daemon hosted inside a frontend therefore does not derive
attention; a standalone one does.

A protocol version says what a daemon can speak, not which fixes it carries.
That distinction is invisible while the frontend hosts its own daemon, and
becomes load-bearing the moment they are separate machines: behaviour that runs
on the daemon host — an upload that leaves nothing behind when it fails
verification, say — is a property of the binary installed there, and no client
can observe it. The handshake already carries the daemon's version alongside its
protocol number for exactly this reason, but nothing yet reads it. A client
should learn the version and be able to say what it is, so the machine in the
middle is identifiable rather than assumed current.

The first remote client is a web client the daemon itself serves, which covers
tablet and phone from one codebase and can be saved to a home screen. Native
clients can follow on the same protocol without changing it. Push delivery for
attention events is a further sink alongside the existing native desktop command
worker and inherits its filters, deduplication, coalescing, and rate limits; the
qualified pane travels as routing data and is never rendered into notification
text, exactly as the desktop path already guarantees. Because a single-machine
install should not require operating a service, the TUI retains an in-process
mode that runs the daemon's responsibilities inside the frontend process.

## File bridge

Moving a file between the machine a person is holding, the daemon host, and the
target host is currently unsupported, and once the daemon is not the machine in
front of the person, an ad hoc multi-hop copy becomes the default. That copy is
unverified and it bypasses every boundary the rest of this document establishes.
The bridge instead generalizes the verified upload the clipboard broker already
performs: stream bytes into a private per-target staging directory, read back a
byte count and SHA-256 receipt, verify it against the sender's own digest, and
use the resulting path only after it verifies.

The generalization is narrow. Content is arbitrary rather than PNG, the name is
supplied by the caller, the ceiling is configurable and separate from the image
ceiling, and a transfer is chunked and resumable so a large file survives a
reconnect. Bytes stream through every hop rather than being buffered whole, so
peak memory does not track file size at either end.

Verification is what makes the relay hop safe to not buffer, so the framing
around it carries the weight. The digest is always computed at the source and
never by the middle. It travels in the offer when the bytes are already in
memory, and in a trailer when they are streamed: hashing while sending attests
to the bytes that actually went out, where a digest computed in an earlier pass
attests only to what the source held during that pass, and a file that changes
in between fails a correct transfer against a stale promise.

Four rules follow, and none of them are optional. A declared length is enforced
rather than believed: an offer above the ceiling is refused before any bytes
move, and the relay stops at the declared length, because a lying length is a
disk-fill on the target host that no digest would catch. The end of a stream is
an explicit frame carrying the digest, never end-of-input, so a dropped
connection cannot be mistaken for a finished transfer. A transfer that ends
without a digest is refused. And a refusal leaves nothing behind — a staged file
from a refused transfer is a verified-looking artifact reached by another route,
and a path injected into a pane cannot be told apart from one that passed. A
refusal names which check failed, because only a missing trailer is likely to be
a dropped connection worth retrying.

Three directions share that one path. A client uploads through the daemon to a
target. A target's file is pulled by the daemon and handed to the client. A file
also moves target to target without touching the device at all, because only the
daemon holds live connections to both—the direction the current desktop-bound
design cannot express, and the one that removes the laptop from the middle of a
build-host-to-development-host copy.

Where the bytes land is a mutation question, so the default is conservative. A
transfer completes into a private staging directory and the verified path is
injected into the pane; the bridge does not write into a working directory the
person did not name. Delivering into a pane's working directory is a separate,
explicitly chosen action. A transferred file is data: it is never executed, its
mode is not carried across hosts, and no command is inferred from its name or
content—the same reasoning that drops pane commands and environment during
cross-session recreation.

The clipboard divides along the same line rather than moving as a unit. Reading
one is a desktop-session capability: it shells out to the compositor or window
system the person is sitting in front of, and already refuses to run over SSH or
nested inside Herdr rather than pretending there is a clipboard there. That half
stays with the client permanently. Uploading needs a route to the host, which is
what a daemon has and a device does not, so that half moves. The client
therefore enumerates the flavors it can see, names one, and states the size and
digest of what it holds; the daemon resolves the extension, moves the bytes, and
verifies what landed against what was promised. A flavor the table does not know
is the same case as a file from a device with no flavor at all, so one path
serves both.

A caller-supplied name is sanitized to a single path component before it reaches
a remote shell, and no path is interpolated unquoted. Payload bytes never enter
logs, command arguments, terminal frames, or persistent state; only sizes,
digests, and outcomes appear in diagnostics. Staging is per target and bounded,
and an abandoned transfer is cleaned up rather than retained until the process
exits.

Delivery initiated from inside a pane—a person typing a command to send the file
they are looking at to whichever client they have attached—has no documented
channel. Herdr's `terminal session` stream carries frames, closure, input,
resize, scroll, and release, with nothing client-bound a host-side helper could
write into. Until such an envelope exists, transfers are initiated from the
client, and Super-Herdr does not invent a side channel by scraping the pane's
own output for markers.

## Security

- OpenSSH configuration and host-key verification remain authoritative.
- Batch mode is the default so a dead target cannot freeze the whole UI on a
  password prompt.
- Remote command arguments are POSIX-shell quoted; SSH destinations beginning with
  an option or containing whitespace/control characters are rejected.
- Credentials, clipboard payloads, and terminal contents are not written to logs.
- A target failure is isolated and bounded by connect and command timeouts.
- The daemon is never published to a public network; remote reachability is
  delegated to an existing private network, and each paired device holds a
  revocable per-device token rather than any credential for a target.
- A remote client receives rendered state and sends qualified IDs. It never
  receives SSH material, Herdr socket paths, or configuration authority.
- Transferred file bytes are size-bounded, verified by byte count and SHA-256,
  never executed, and never written to logs or command arguments.

## Source boundary

The implementation uses Herdr's documented Socket API and CLI reference:

- https://herdr.dev/docs/socket-api/
- https://herdr.dev/docs/cli-reference/

Super-Herdr should not copy Herdr internals unless the project intentionally
accepts the licensing and maintenance coupling: https://github.com/ogulcancelik/herdr
