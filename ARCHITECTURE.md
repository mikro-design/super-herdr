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
sessions, or processes.

The desktop persists only versioned UI intent, currently the last explicitly
selected qualified pane. Writes use a private state directory and atomic file
replacement. Terminal contents, clipboard payloads, SSH material, connection
leases, and server snapshots are never persisted. Restoration waits through
target reconnects and succeeds only when the exact target/session/local-ID tuple
is live; stale IDs are ignored without mutating the Herdr server.

## Terminal data plane

One selected pane owns keyboard input. Observe streams may remain open for visible
background panes; the selected pane uses the control stream. Terminal frames stay
as encoded ANSI payloads until the frontend renderer, avoiding lossy intermediate
screen models. On disconnect, input is disabled until the control lease is
re-established.

The current frontend uses Ratatui with an independent VT parser. It renders a
host/session/workspace sidebar, tab strip, and the visible split panes from Herdr's public
layout rectangles. `Ctrl+]` is the federation prefix for switching qualified
panes, leaving Herdr's `Ctrl+B` prefix untouched. The selected pane gets a control
stream without `--takeover`; other visible
panes get observer streams. If the control stream is refused or closes, the
selected pane immediately falls back to observation and periodically retries a
normal control lease without interrupting the Herdr server.

The outer TUI captures SGR mouse input and translates terminal coordinates back
to the selected pane. A left press is held until the gesture is resolved: release
without movement becomes an application click, while movement becomes local text
selection. Other mouse button reporting honors the mode and encoding tracked from
that pane's ANSI stream. Wheel gestures use the public `terminal.scroll`
controller command so the Herdr server owns application/alternate-screen/host-
scrollback routing. A click on another visible split changes only Super-Herdr's
local selection and consumes the full gesture. Host/session/workspace rows and
tabs are also locally clickable. When mouse reporting is disabled, left-button
drags select text and remain clamped to the originating terminal surface.
Observer-only routes select locally because they have no input channel.
The outer capture requests button-motion (`1002`) rather than all-motion (`1003`),
so hover traffic cannot starve clicks and drag updates when Super-Herdr is nested
inside another terminal multiplexer.
Sidebar hit rectangles are derived from the rendered block's inner area and stored
with that frame. A press owns its resolved item until release, matching Herdr's
interaction model instead of recomputing a row from potentially newer state.
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
Local-to-remote text paste is an explicit desktop-broker action. The broker reads
from `pbpaste`, `wl-paste`, `xclip`, or `xsel`, applies a 1 MiB limit, honors the
selected terminal's bracketed-paste mode, and routes only the resulting terminal
input through Herdr's public control stream. It is deliberately unavailable when
Super-Herdr itself runs through SSH because the remote process cannot directly
read the client's clipboard; the local terminal remains the paste mediator in
that topology. PNG image bridging reads explicitly from the desktop clipboard,
uploads at most 32 MiB to a private per-target temporary directory, verifies the
remote byte count and SHA-256 digest, and injects only the resulting path through
the selected terminal route. File-list clipboard upload remains a later extension
of the same broker. Remote agents never need desktop clipboard access.

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
