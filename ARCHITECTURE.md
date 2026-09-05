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

Plugin actions require Herdr protocol 20 or newer. Their registries are fetched
per target with bounded socket requests, cached for one minute, invalidated by a
target connection generation change, and never allowed to stall another target.

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
Herdr server. Once any pane is selected, federation reconciliation preserves
that exact qualified identity even if its target enters backoff or an
authoritative snapshot removes the pane. Only an explicit navigation action may
replace it. This is an input-safety invariant: availability churn must never
retarget a pending or subsequent keystroke to a different shell.

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

Enabled Herdr plugin actions use that routing model too. The daemon discovers
them with `plugin.action.list`, removes their command arrays, qualifies each
plugin/action pair with target and session, and exposes the remaining title,
description, and supported contexts to the frontend. Invocation uses
`plugin.action.invoke` only after the daemon hydrates the chosen resource through
documented `workspace.get`, `tab.get`, `pane.get`, and `pane.list` calls. That
prevents Herdr's own local focus from replacing the pane selected in
Super-Herdr. Selection-only actions are not exposed, and terminal selection text
is never carried as incidental plugin context.

The browser asks for the same sanitized registry and renders the actions that
support its currently viewed pane. An invocation returns only a qualified
plugin log ID and `running`, `succeeded`, or `failed`; polling
`plugin.log.list` is reduced to those fields before it crosses the daemon
boundary, so command arguments, stdout, stderr, and plugin error payloads are
discarded in the daemon and never reach a client. Federation updates reveal
panes created after the action.
The browser offers those panes as explicit links and never changes the pane
being viewed or controlled automatically.

Moving a workspace is a multi-request action inside one session. Super-Herdr
reads each source tab's split tree through the documented layout export, reduces
it to one anchor pane plus the ordered splits that rebuild it, and replays that
plan as documented pane moves into the destination workspace. Herdr moves live
panes, so processes, scrollback, and agent authority survive; because a pane
identifier encodes its workspace, each later split targets the identifier the
previous move returned. The whole sequence runs in one task behind the same
single-action guard and reports one result, and a partial failure leaves already
moved tabs in the destination and the remainder in the source without closing
anything. A session is a separate server process that owns its panes, and the
documented API has no cross-session transfer, so destinations are restricted to
the source session and cross-session recreation is not silently substituted for
a move.

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

The daemon itself is not published to the public Internet. It binds loopback
and reaches the hosted bridge with an outbound connection; explicit private,
WireGuard, Tailscale, and operator-proxy routes remain opt-outs. Each device is
paired out of band—the TUI presents a one-time code typed at the fixed bridge
site. A correctly typed code produces a fresh browser-generated six-digit
comparison number on the trusted TUI; only an explicit matching approval creates
a revocable per-device token in the same atomic TOML store that already holds
targets. This is device pairing for one operator's own clients, not a shared
identity or authorization system.

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

The same ownership argument produces the agent card projection. The federation
hierarchy answers where a pane is; it does not answer which agent is waiting,
and on a phone that is the only question with room on the screen. The daemon
therefore builds one inbox — sections for needs-attention, working, and recent,
in one order — rather than letting each client derive its own. Two clients
deriving sections independently would disagree the moment their snapshots
differed by a refresh, and a person moving between the TUI and a phone would be
reading two different inboxes of the same federation.

A card is keyed by a qualified agent identity: the agent session Herdr reports
where it reports one, and otherwise the pane the agent occupies, in both cases
carried with its target and Herdr session. Two hosts that both name a pane
`w1:p1` produce two cards that never collapse. The session is preferred because
it survives the agent moving to another pane, which a pane id cannot express —
a move would otherwise read as one agent dying and another being born, taking
the card's place in the queue and any pin on it along with it. It is optional
in Herdr's schema and absent in practice at protocol 19, so nothing depends on
having one, and the two forms are tagged rather than merged so that a session
value which happened to equal a pane id cannot name the same card as a
different agent. A reference missing any of its four fields, or carrying the
separator the key is built from, is treated as no reference at all: a partial
key is one two different sessions could share.

Because the identity is not the route, resolution asks the current snapshot
which agent answers to a key rather than taking the key apart. Two agents
answering to one identity is therefore a case that can happen and is refused —
both cards stay visible, because a person should be able to see that it
happened, and neither is offered, because the daemon cannot say which pane an
action would mean. Ordering is by section entry
rather than by first sighting, so the agent blocked longest is at the top, and
an unrelated target connecting, disconnecting, or refreshing does not renumber
anything: the projection republishes only when the cards actually differ.

A card is not a route. It records what was true when it was built, and the live
pane is derived again from the current federation before anything is sent —
refusing when the target is not live, the agent is gone, its pane is missing, or
a snapshot disagrees with itself. That is what lets a card one refresh out of
date be rendered safely, and it is why a disconnect never changes which pane
receives the next byte. A disappeared agent stays visible as bounded history
with no pane at all, so it can be read and never typed into. Cards carry labels,
a status word, a phase, and timestamps; terminal contents are not summarized,
indexed, or persisted, and the projection itself is derived rather than stored,
because the federation state and attention index it reads already survive a
restart on their own terms.

Pins, mutes and snoozes are Super-Herdr's own opinions about that inbox, and
deliberately not Herdr's. Pinning an agent does not rename, move, or focus the
pane it runs in; muting one does not stop it working or change what its target
reports. Nothing in a mark crosses back to a host, so a mark can never be the
reason a session behaves differently — which is why the TUI files them among
the Herdr actions a person reaches for while excluding them from the actions
that mutate Herdr.

They are keyed by the same qualified agent identity the cards use, so pinning
an agent on one host cannot silence a same-named agent on another. A pin is the
one reorder a person actually asked for and so is allowed to win over section
entry; a mute or a snooze moves a card out of the way without denying what it
is, because an agent that is still blocked is still blocked and a card claiming
otherwise is one a person could act on wrongly. Coming back from either is
re-entering the queue rather than reclaiming a former place in it.

A snooze is stored as a deadline the daemon computed from a duration the client
asked for. A client never names a moment: a phone with a wrong clock would
otherwise be able to silence an agent until next year, and the daemon has no
way to tell that from a deliberate request. Expiry is checked on the projection
path rather than by a timer, so it surfaces within one federation refresh
without another clock in the process to keep correct. The file is bounded in
every direction — how many agents may be marked, how large it may be, how far a
snooze may reach — because it is written by a request from a paired device.

One-tap replies on a paired device are configuration, not interpretation. The
distinction is forced by what Herdr documents: its API reports an agent's
status and whether it is ready for input, and nothing anywhere in its request
or event schemas describes the options a blocked agent is offering. There is
therefore no structured prompt metadata to render a semantic Yes, No, Approve
or numbered-choice button from, and the only way to produce one would be to
read the terminal and guess. Super-Herdr does not: a button that types `y`
because the screen looked like a yes/no prompt is a keystroke sent on the
strength of a pattern match, and the person holding the phone cannot see the
match that produced it.

So the replies are what somebody wrote down in their own configuration. The
daemon sends the list in the handshake, because it is constant for the process
and a client needs it before it can draw controls, and a client renders exactly
that list — an empty one draws no buttons rather than falling back to a guess.
Each reply carries its own text, whether Enter follows it, and whether it wants
confirming; with no structured metadata to learn that a response is
irreversible from, the person who wrote the reply is the one who declares it,
and confirming is a second tap on the button rather than a dialog a phone
dismisses by reflex. Control characters are refused in a reply's text, so a
configuration cannot smuggle an escape sequence into a pane: Enter, Escape, Tab
and Ctrl-C are separate, explicitly labelled keys.

A reply becomes terminal input like any other, which means it needs the pane's
control lease and is refused by the daemon without one. An armed confirmation
belongs to the moment it was armed in, so losing the lease disarms it.

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
protocol number for exactly this reason. The browser reads and displays it; the
Rust client still validates the protocol and discards the version. That client
should surface the version when it attaches to a daemon it does not host, so the
machine in the middle is identifiable rather than assumed current.

A browser cannot open a Unix socket, so the daemon can also serve the same
protocol over two ordinary HTTP requests: its messages arrive on a server-sent
event stream, and a client's messages are posted back. That needs no framing,
no masking, and no handshake beyond HTTP, which is why it is a small
hand-written server rather than a web stack. Behind both requests is one
ordinary in-process attachment, so a browser is a client of the same daemon in
the same way the frontend is, through the same handshake, receiving the same
vocabulary rather than a translation of it. When the browser holds a pane's
control lease, its line submissions, terminal-key buttons, and verified file
chunks use the same posts. Browser command posts enter a bounded queue before
the daemon attachment, so target backpressure reaches a file sender instead of
turning a slow route into unbounded memory. A future client offering continuous
per-keystroke interaction would have to justify a socket upgrade against the
extra transport and authentication surface.

The phone client gives the observed terminal the remaining dynamic viewport
height and uses scrolling for panes wider than the screen. It never reduces
terminal text below a readable minimum merely to fit all columns. Control input
and explicit terminal keys stay in the bottom control area above the soft
keyboard. Federation state is grouped as collapsed targets and sessions, and
agent attention is one bounded, deduplicated, actionable list rather than a
second navigation tree. A control holder may pick files up to 32 MiB; the page
hashes the bytes it chunks, the daemon verifies the target's size and SHA-256
receipt, and only then does the page paste the returned shell-quoted path.

A stream and the posts that steer it are separate requests, joined by an
identifier the browser generates. Without that join, a subscription posted by
one request would be made on a connection another request is reading — which
works until a second tab is open. The identifier is only ever a map key, so it
keeps the characters that can be one and nothing else.

A paired device normally reaches that listener through the fixed public bridge.
The daemon binds loopback and opens the connection outward, so NAT, lack of a
shared LAN, and lack of a Tailscale client on the phone do not become product
requirements. Each daemon run creates a random public route and a separate
registration secret. The secret is memory-only, travels in the connector's
authorization header rather than the browser URL, and is redacted from Debug.
The relay multiplexes raw HTTP connections as bounded opaque chunks; it never
parses daemon messages, and route count, viewers per route, frame size, and
queues all have hard ceilings. Losing one connector closes only its browser
connections and never stops the daemon or a Herdr session.

The bridge is trusted infrastructure: HTTPS/WSS protects both network legs, but
TLS terminates there, so this is not end-to-end encryption against the bridge
operator. The service and its proxy must never log request bodies, response
bodies, authorization headers, terminal content, clipboard payloads, or pairing
material. A future untrusted relay would require browser-to-daemon authenticated
encryption and a way to make the served browser code itself trustworthy; opaque
forwarding alone does not make that claim.

The public relay is built as the separate `super-herdr-bridge` workspace
package. The end-user `super-herdr` binary contains only the outbound connector
and cannot be switched into a public listener. Separating the deployment unit
keeps the relay's public attack surface and operational lifecycle out of the
Homebrew and Debian client interface while sharing the narrow tunnel contract
at compile time.

Pairing still starts from a client that is already trusted: it asks the daemon
for a short code, the code appears on a screen someone already has, and the
daemon publishes that code over its authenticated connector for at most five
minutes. A browser opens the fixed bridge page and types the code into eight
single-character boxes there; paste distributes a complete code across them.
The
bridge uses it only to select the owning route and forwards the request. The
browser generates a six-digit comparison number with Web Crypto, and the daemon
forwards the name and number to state-subscribed trusted clients. It atomically
spends the short code but issues no token until a trusted client approves that
exact pending request; rejection, timeout, or loss of the browser creates no
credential. Codes are never put in a URL. Concurrent daemons have independent
routes and pending codes; an accidental code collision is ambiguous and routes
neither person. The bridge also bounds pairing submissions per Cloudflare
source, but this is flood control rather than the authority check: even a valid
short code grants only a comparison prompt.

A device name must remain unique because it is the human-facing revocation
identity. The daemon checks a submitted name only after verifying the live
code, so the public endpoint cannot enumerate paired names. A collision returns
`409 Conflict` before the code is consumed or an approval is requested; the
bridge preserves that rendezvous and the browser can rename and retry the same
code.

The fixed bridge URL is reserved. An old configuration may spell it explicitly
as `web.url`; resolution still treats that exact address as the hosted outbound
bridge. Treating it as a generic operator proxy would render the correct QR but
open no connector, leaving every correctly entered code absent from the public
registry.

On Linux, the outbound connector explicitly installs Rustls's ring crypto
provider before spawning its TLS task. The bridge crate is linked into a larger
desktop binary, so provider inference from the final feature graph is not a
stable startup rule; a failure there must not become an unnoticed
background-task panic while the pairing screen continues to advertise an
unpublished code. macOS uses its native TLS stack instead.
The successful token cookie is scoped to the random route, so devices for two
daemons in one browser do not overwrite or cross-send one another. The daemon
stores only the token's digest, so
the configuration file is not itself a set of credentials — a copy of it hands
nobody a working device — and revoking one is deleting a line, which takes
effect at that device's next request rather than at the next restart. A code is
spent by a match rather than by an attempt, because a wrong entry is far more
often a typo than an attack; it survives a few of those and is discarded after
that, so a flood cannot keep one alive.

When the hosted bridge is disabled, an existing Tailscale Serve configuration
is a useful exception to the rule
that a bind cannot reveal its outside URL: its read-only JSON status explicitly
pairs an HTTPS host and port with a loopback proxy target. When exactly one root
route matches Super-Herdr's preferred port on either side, the daemon binds the
target and advertises the HTTPS side. It never creates or changes Serve state,
and it does not adopt a public Funnel route without an explicit URL. Ambiguity
falls back to the direct private-address listener rather than guessing which
unrelated local service belongs to Super-Herdr.

A token authenticates a device. It does not encrypt anything, and the daemon
does not pretend otherwise: it binds loopback for the TLS bridge, or a private
and mesh address for a direct route, and refuses a public bind. There is no
loopback exemption for the browser protocol. The root page and session status
remain reachable so a new device can pair, but event streams and commands
always require the HttpOnly device cookie. This keeps a proxy, another local
process, and a directly opened loopback browser on the same authorization path.

The page is held in the binary and prefixes requests with the random bridge
route when it was loaded through one; direct listeners remain rooted at `/`.
It groups the federation as target, session, workspace, and pane, opens a pane's
viewer on selection, and keeps its keyboard disabled until that paired client
owns the exclusive pane lease. It also shows the daemon's version,
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

### Two representations of a pane

A pane subscription names what it wants. `frames` is the default and carries
encoded ANSI untouched, which is what a client owning a VT parser wants and what
the TUI uses. `screen` carries a rendered screen instead, for a client that
cannot carry an emulator: vendoring one into the browser page would mean a
megabyte of third-party JavaScript inside a signed binary, in the component most
exposed to a network. The browser's explicit control input does not change that
rendering boundary: it sends bytes only after obtaining the lease and still
receives a server-rendered screen.

This does not weaken the rule that encoded ANSI reaches a renderer without a
lossy intermediate model. That rule forbids a *double* parse — a middle that
decodes and re-encodes and hands a renderer something degraded. A rendered
screen is one parse moved to whichever end can afford it, delivered directly,
and it reaches the same ceiling a client-side `vt100` would, because the TUI
already reduces to a `vt100` screen. Both representations are served from a
single parse of a single frame, so a TUI and a browser can watch one pane at
once. The emulator is created with the first screen subscriber and dropped with
the last, so a pane nobody renders costs nothing.

A full repaint or a resize rebuilds the emulator rather than feeding it, which
is the rule the TUI already follows. A frame that leaves the visible screen
unchanged sends nothing and does not advance the sequence: a client that missed
nothing should not be told that it is behind.

### Backpressure on rendered updates

A frame subscriber needs no flow control, because the socket provides it — a
client that stops draining stops the daemon reading. A rendered update is queued
into an unbounded outbox instead, so nothing in the transport pushes back, and a
viewer on a slow link watching a busy pane would accumulate one message per
frame in the daemon's memory.

So the depth is bounded here. A screen subscriber may hold a small number of
updates, the daemon decides how many, and a viewer acknowledges an update once
it has painted it. What is held is the sequences themselves rather than a count
of them, so one acknowledgement settles the update it names and every earlier
one still outstanding: a viewer that paints four and reports only the newest is
telling the truth about all four, and a viewer that drops an acknowledgement is
repaired by the next rather than losing a slot for ever. Counting would have
made both permanent, and would have made the protocol depend on a client
acknowledging exactly once per update without anywhere saying so.

An acknowledgement must name an update that subscription was actually sent.
That is membership in the outstanding queue, not merely a sequence below the
newest one: the queue has gaps, because a viewer at its limit is skipped and
what it missed was never outstanding, and without membership a client could
clear those gaps by naming updates it never received. It reports progress rather
than granting anything, for the same reason a resume token is not authority and
a download's credit is a request rather than an instruction.

What a viewer that fell behind is sent when it catches up is the screen as it
stands, not the backlog it missed — which is both fewer bytes and the thing it
actually wants. Frames cannot do this, because every byte matters to a parser
consuming them statefully. A snapshot can, because it is self-contained and
carries the sequence that tells a client what it skipped. That asymmetry is the
argument for the second representation, and it bounds the cost of a viewer
however far behind it falls.

## File bridge

Moving a file between the machine a person is holding, the daemon host, and the
target host cannot be an ad hoc multi-hop copy: that copy is unverified and
bypasses every boundary the rest of this document establishes. The implemented
bridge instead generalizes the verified upload the clipboard broker already
performs: stream bytes into a private per-target staging directory, read back a
byte count and SHA-256 receipt, verify it against the sender's own digest, and
use the resulting path only after it verifies. The protocol carries all three
directions; the remaining work is to expose them consistently in each client.

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

The name must remain one printable path component because it is line-framed into
the staging stream; separators, controls, traversal-like double dots, and names
beyond the component limit are refused rather than repaired. Spaces, quotes,
shell punctuation, and Unicode are otherwise preserved. They remain inert when
the verified path reaches a terminal because each client single-quotes it as one
shell word, escaping an embedded quote by closing, escaping, and reopening the
quoted word. Where no name is given the flavor supplies one, which is the
clipboard's case: a screenshot has no name to keep.

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

Target to target is the composition of the other two, and needed almost nothing
of its own. The host with the file describes and sends it; the host receiving it
stages and hashes what it stored; the daemon holds both connections at once, so
the reader is one host's SSH output and the writer is the other's SSH input and
a single chunk is in this process at a time. Backpressure is end to end without
anything arranging it, and no credit is needed because nothing is queued for
anybody. Both digests are the hosts' own and the daemon compares them, which is
the only role the middle has ever had here.

Two rules change because the situation does. An interruption is discarded rather
than kept: the file still exists where it started, so a second attempt costs a
re-read rather than something nobody can reproduce — the opposite of a client's
upload, where the bytes lived on a device that may be gone. And a move answers
to the control lease on both panes, because holding one on the destination is
not permission to read somebody else's host, and holding one on the source is
not permission to write to theirs.

Reading a file back off a target inverts two things and keeps everything else.
The client names the path, where an upload never does; that is not a widening of
what it may reach, because a client holding the pane's control lease can already
type `cat` into a shell on that host and read the same bytes through the
terminal. This is the same reach through a channel that can say what it moved,
and it answers to the same lease for exactly that reason.

The digest inverts with the direction: the host computes it and the client
checks it, where an upload has the client attest and the host store. The daemon
computes nothing either way. What does change is when the digest is available.
An uploading client hashes while it sends, so its digest attests to the bytes
that went out; a portable shell cannot tee a stream through a hash, so a host
hashes in its own pass and sends the result ahead of the file. A file modified
between those passes therefore fails at the client — a false refusal rather than
a false acceptance, which is the direction an unavoidable weakness has to point.

Flow control also inverts, and this is the part that needed inventing rather
than mirroring. An upload is backpressured by the socket: the daemon stops
reading and the client stops writing. Going the other way the daemon is the
sender and the queue to a client is unbounded, so nothing in the transport can
push back — a browser on a slow link would have a gigabyte waiting for it in the
daemon's memory. So the protocol carries it: a client grants credit in chunks
and the daemon sends no more than it has been given, reading nothing from the
host while nobody is ready for it.

A grant is a request rather than an instruction, and the daemon clamps it. That
half is not optional and is easy to leave out: credit says what a client is
ready for, and a client is not a reliable witness about itself — a window
computed from a file's size rather than from a buffer is an ordinary mistake
rather than an attack. Since the queue to a client is unbounded, an unclamped
grant is the whole file in the daemon's memory, which is the outcome credit
exists to prevent. A client may pull as often as it likes at whatever rate the
link sustains; how much is held while it does is the daemon's decision, exactly
as it is in the other direction. Peak memory is a window rather than a file
either way, which is the same promise reached by different means.

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
