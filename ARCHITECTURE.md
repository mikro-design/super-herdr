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
a connection belongs to a running daemon and is never evicted. A daemon asked to
stop removes its own socket, so a path found on disk means a process that died
rather than one that exited — the replacement rule stays as the answer to a
crash rather than as ordinary behaviour. SIGHUP is not handled: it conventionally
means reload, the configuration already refreshes on its own schedule, and
exiting on it would surprise anyone who closed a terminal.

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

The daemon owns the durable attention index, so an agent that starts waiting is
recorded whether or not anyone is watching. Exactly one process may own it —
two would number their events independently and write the same file over each
other — so a client does not derive attention at all. It holds a mirror, seeded
with the history when it subscribes and extended by each event as it arrives.

Read state is part of that history rather than a local view of it. Marking a
pane read is a request, and the daemon answers every change by republishing the
whole bounded index instead of a description of what changed: a change touches
many events at once, and a client reproducing it locally would be guessing at
the authority's result. A client still edits its mirror immediately so a badge
clears in the frame the person acted in, but that edit is optimistic — the
republished history overrides it, so a request that never lands is corrected
rather than silently believed.

Delivering a notification is a separate question from owning the index, and it
divides the way the clipboard does. Native desktop delivery is a desktop-session
capability: it belongs to the client, because a daemon on another machine
notifying its own desktop reaches nobody. Push delivery to a paired device is
the opposite — it needs a process that is awake when no frontend is, which is
what the daemon is for. Both are sinks on the same event stream, and the filters,
deduplication, coalescing, and rate limits that decide what is worth delivering
sit with the index rather than being reimplemented per sink.

A protocol version says what a daemon can speak, not which fixes it carries.
That distinction is invisible while the frontend hosts its own daemon, and
becomes load-bearing the moment they are separate machines: behaviour that runs
on the daemon host — an upload that leaves nothing behind when it fails
verification, say — is a property of the binary installed there, and no client
can observe it. The handshake already carries the daemon's version alongside its
protocol number for exactly this reason, but nothing yet reads it. A client
should learn the version and be able to say what it is, so the machine in the
middle is identifiable rather than assumed current.

A browser cannot open a Unix socket, so the daemon can also serve the same
protocol over two ordinary HTTP requests: its messages arrive on a server-sent
event stream, and a client's messages are posted back. That needs no framing,
no masking, and no handshake beyond HTTP, which is why it is a small
hand-written server rather than a web stack. Behind both requests is one
ordinary in-process attachment, so a browser is a client of the same daemon in
the same way the frontend is, through the same handshake, receiving the same
vocabulary rather than a translation of it. When the client needs to type into a
pane, the latency of a post per keystroke will argue for a socket upgrade; while
it only observes, it does not.

A stream and the posts that steer it are separate requests, joined by an
identifier the browser generates. Without that join, a subscription posted by
one request would be made on a connection another request is reading — which
works until a second tab is open. The identifier is only ever a map key, so it
keeps the characters that can be one and nothing else.

A paired device may reach that listener directly; anything else must forward
the port. Pairing starts from a client that is already trusted: it asks the
daemon for a short code, the code appears on a screen someone already has, and a
browser exchanges it for a token. The daemon stores only the token's digest, so
the configuration file is not itself a set of credentials — a copy of it hands
nobody a working device — and revoking one is deleting a line, which takes
effect at that device's next request rather than at the next restart. A code is
spent by a match rather than by an attempt, because a wrong entry is far more
often a typo than an attack; it survives a few of those and is discarded after
that, so a flood cannot keep one alive.

A token authenticates a device. It does not encrypt anything, and the daemon
does not pretend otherwise: it binds loopback or a private and mesh address and
refuses a public one. On a mesh like WireGuard or Tailscale the network already
provides confidentiality; on the open internet it would not, and the way in from
there stays a forwarded port, which is an explicit act rather than a flag
somebody set once. A genuinely local request is never asked for a token, since
anyone who can reach loopback can already read the daemon's socket — but that
is a claim about a process, not about an address. A proxy terminating TLS on a
network and forwarding to loopback makes every visitor look local while being
nothing of the kind, so a request carrying forwarding headers is asked like
anything else arriving over a network. A client can of course set those headers
itself, which costs it the exemption rather than gaining anything.

The page is held in the binary and refers to nothing it is not served, because a
forwarded port has no route to anywhere else. It shows the daemon's version,
which the handshake has always carried: the page ships inside that binary, so
the client a device loads is whichever version is installed on the daemon host,
and a browser is exactly where a stale daemon would otherwise be invisible.

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

A caller-supplied name was once ruled out here, and what changed is worth
recording, because the objection was correct at the time. The name was
interpolated into the staging script, where anything from the wire would have
been text in a remote command guarded only by a sanitizer that has to stay
right forever. It now arrives as the first line of the same stream that carries
the payload: data to that script rather than part of it, so nothing from the
wire is ever parsed as shell. The script refuses a separator itself, which is
the half that still holds if the daemon's check is ever widened.

The character class stays narrow regardless, for a reason that outlives the
quoting. The staged path is pasted into a pane, so a name carrying a space, a
quote, a semicolon or a `$` would be a command somebody's shell runs — inert as
text is the requirement, not merely inert as an argument. Letters, digits, dots,
dashes and underscores qualify, and nothing else does. A name that does not is
refused rather than repaired, since a silently renamed file tells its sender it
got what it asked for. Where no name is given the flavor supplies one, which is
the clipboard's case: a screenshot has no name to keep.

Both sides stage the same way — a private directory per transfer, with the file
inside it under its own name. Local and remote then have one shape, one cleanup,
and one rule about what may be removed, rather than a resemblance that has to be
maintained.

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

Backpressure is what makes the middle hop a relay rather than a rename. Chunks
are forwarded by the connection that receives them rather than by the loop that
serves every client: a queue that fills then stops that connection being read,
which stops its client writing, which is the only signal that reaches a sender
at all. A shared loop could not wait on one client's target without stalling
every other client, and a queue deep enough never to fill would be the buffer
this exists to remove. The cost is that a connection moving a large file is not
reading its own other messages meanwhile — a consequence of one ordered stream
per connection, paid by the transfer's own client.

One transfer may take more than one attempt. A connection that drops
mid-transfer is not a refusal — nobody decided anything — and discarding a
gigabyte because a laptop closed is a cost with nothing on the other side of it.
So an interruption keeps what arrived, and a sender that comes back with the
token it was issued continues from there.

That reads like a contradiction of the rule above, and it is worth being precise
rather than carving out an exception. What "a refusal leaves nothing behind"
protects is that a partial file must never be mistaken for a complete one, and
the rule stated exactly is that *nothing partial is ever named, reported, or
acted on*: no path is reported, what a sender is told is a byte count rather
than a location, and nothing reaches a pane until a digest verifies. That holds
before this change and after it. Emptiness was the means and not the rule, and
an exception would only invite the next reader to ask whether their case is
inside it.

The distinction that decides what is kept is that an interruption is the
*absence* of a decision. A withdrawal is discarded, because the sender asked. A
sender that attests to a transfer it did not deliver is refused and discarded,
because it said it was finished and it was not. Only a stream that stops without
saying anything is kept, bounded by a clock and by a count — a clock alone
bounds how long one sender may occupy a host, while a count is what bounds how
many of them may at once.

Retention also ends with the process. The token that names a retained transfer
lives in the daemon's memory, so a transfer outliving the daemon is unresumable
by construction: a stopping daemon gives the host its bytes back rather than
leaving partial files that nothing can name and nothing will collect. A crash
still leaves them, which is a crashed process holding temporary files and the
one case this cannot reach.

Where the next byte belongs is asked of the host rather than remembered. An
attempt that died mid-chunk left a length nobody predicted, and a daemon that
resumed from its own bookkeeping would corrupt a file silently. The token names
the transfer; the file decides the offset. A resume answers to the same control
lease a new transfer does, so returning with a token is not on its own authority
to write to a host.

Verification changes shape once a transfer can be assembled from several
attempts. The digest a sender attests to covers the whole content, and it is
compared against the digest the host computed over the file it stored — the
middle computes nothing. That is stronger than the per-attempt comparison it
replaces, and it is the only comparison that can span attempts at all. It also
means a file that changed between attempts fails rather than resuming into a
mixture of two versions: the earlier bytes came from an earlier read, and the
attestation covers what the source holds now.

Refusing early is free and refusing late is not, which decides where each check
lives. A length above the ceiling, and a pane whose target is not configured,
are both settled before a byte moves. The trailer cannot be: it attests to bytes
that have already reached the host by the time it arrives, so failing it means
removing a file that exists there. That is the same unstaging an abandoned
transfer needs — a client that vanishes mid-transfer is a stream that stopped
short — and it is why the ceiling now bounds what may be written onto a target's
disk rather than what the daemon can hold.

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

That framing is not hypothetical: it is what a clipboard upload already uses.
A client offers a type and a length, sends the payload in chunks that fit inside
the message bound, and ends with a digest over the bytes it sent. Nothing
reaches the target host until that digest verifies, so an abandoned transfer and
a refused one have the same result — the daemon holds the payload in memory and
discards it, rather than leaving a file nothing checked. Atomic multiline paste
travels the same way but needs no framing at all: it is one documented request
through the session's socket, which is why it was the half that had no local
capability behind it.

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
