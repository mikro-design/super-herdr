# Super-Herdr

Super-Herdr is a desktop TUI for working with persistent Herdr sessions on
multiple machines. The frontend owns federation, rendering, navigation, and the
desktop clipboard. Each Herdr server continues to own its shells, agents,
workspaces, and terminal history.

Super-Herdr integrates only through Herdr's documented CLI and socket interfaces.
It never takes over, stops, starts, or restarts a Herdr session.

## What works

- Concurrent discovery and supervision of running Herdr sessions on several hosts.
- Failure isolation and bounded reconnect backoff per target.
- Qualified identities in the form `target/session/server-local-id`.
- A live TUI with host/session/workspace navigation, tabs, and split panes.
- Writable control for the selected pane and read-only observation for background
  panes.
- Keyboard and mouse routing through Herdr's public terminal-session interface.
- Local text selection, multipage edge-drag selection, and desktop clipboard copy.
- Atomic multiline paste from the terminal or explicit desktop clipboard into a
  selected remote pane.
- Verified clipboard file upload to the selected host with path-only injection,
  for PNG, JPEG, WebP, GIF, TIFF, PDF, and SVG.
- Copying files in a file manager and pasting them into a pane. A clipboard
  holds references rather than bytes for copied files, so the references are
  followed, each file is read from local disk and sent under its own name, and
  the verified remote paths are pasted in the order they were copied — the same
  path an image takes. A file that does not arrive is named rather than passed
  over.
- Verified transfer of arbitrary content to a target through the daemon, relayed
  as it arrives rather than held, under a name the caller chooses, bounded by
  `transfers.max_bytes` and unstaged on the host if any check fails.
- Verified read-back of a file from a target to the client that asked, paced by
  credit the client grants so a slow reader bounds what is in flight rather than
  filling the daemon. The host computes the digest, the client checks it, and
  the daemon computes nothing.
- Verified movement of a file from one target to another without it passing
  through this machine, with a control lease required on both ends and each
  host's own digest compared before the copy is kept.
- Transfers that survive a lost connection: an interrupted one keeps what
  reached the host, a sender returns with the token it was issued, and the
  offset it continues from is the host's own count rather than anyone's
  bookkeeping. What is kept is bounded by a ten-minute clock and by a limit on
  how many unfinished transfers a host may hold at once.
- Event-driven updates when a documented Herdr socket is configured, with polling
  fallback.
- Atomic persistence and restoration of the last explicitly selected qualified
  pane.
- Command-line and in-TUI target management backed by one atomic TOML store.
- Live configuration and running-session discovery refresh without restarting
  Herdr.
- A global agent navigator with attention and active-work filters.
- A persistent lower-sidebar attention feed for blocked/waiting agents and recent
  transitions across all live hosts, with click-to-jump routing.
- A durable metadata-only attention history with unread counts, transition
  deduplication, and qualified jump-and-mark-read behavior.
- Opt-in native desktop notifications for selected attention transitions, with
  startup-history suppression, coalescing, and rate limiting. Where the desktop
  can report a click, the notification jumps straight to its qualified pane.
- Live workspace moves inside one Herdr session: every tab is rebuilt in the
  destination workspace with its splits and ratios, and no process restarts.
- Explicit workspace recreation on another session or host, rebuilding the tab
  and split structure with new shells while the source keeps running.
- A fuzzy-searchable action palette for navigation and qualified workspace,
  tab, and pane lifecycle operations.
- Right-click session, workspace, tab, and pane menus backed by those same
  qualified lifecycle actions and close confirmations.
- Pairing a device by scanning: with `--web-url`, the TUI draws the pairing code
  as a QR a phone can read. The code travels in the URL fragment, which is never
  sent to a server, so it stays out of request lines and out of any proxy in
  front of the daemon. Without that flag the code is shown to be typed, because
  a daemon binding loopback cannot know the address a phone would use and a
  guess would scan perfectly and reach nothing.
- Rendered pane observation in the browser client, bounded per viewer: a client
  acknowledges each update it paints, and one that falls behind is sent the
  screen as it stands rather than a backlog.

## Recommended topology

Run Super-Herdr on the machine where the display and clipboard live. Connect from
that frontend to every Herdr host over SSH:

```text
macOS or Linux desktop
  Super-Herdr TUI and clipboard broker
    ├── SSH → development host → persistent Herdr sessions
    ├── SSH → build host → persistent Herdr sessions
    └── local → optional desktop Herdr sessions
```

This keeps remote shells persistent while giving Super-Herdr direct, acknowledged
access to the desktop clipboard. Running Super-Herdr itself through SSH still
supports best-effort OSC 52 copy, but the remote process cannot directly read the
client machine's clipboard.

## Requirements

- Rust toolchain with Cargo.
- Herdr client compatible with each target server. Herdr 0.8.0/protocol 19 is the
  currently tested version.
- OpenSSH for remote targets. Normal SSH config, aliases, keys, host-key checking,
  and proxy settings remain authoritative.
- One native clipboard toolset on the frontend:

  - macOS: `pbcopy`, `pbpaste`, and `osascript` from the operating system.
  - Linux Wayland: `wl-copy` and `wl-paste` from `wl-clipboard`.
  - Linux X11: `xclip`, or `xsel` for text-only copy and paste.

- Native notification delivery when enabled:

  - macOS: `osascript` from the operating system.
  - Linux desktop: `notify-send` from the desktop notification tools.

- `sha256sum` on an SSH target when transferring files or clipboard media to it.

Build concurrency is capped at four jobs in `.cargo/config.toml` and in the
provided Makefile targets.

## Install

Upgrade from 0.7.0 if you served the browser client through a reverse proxy.
0.7.0 exempted loopback from pairing, on the reasoning that anyone who can reach
loopback can already read the daemon's socket. A proxy that terminates TLS on a
network and forwards to loopback — `tailscale serve` is one — breaks that
reasoning without changing anything the daemon looked at: every visitor arrived
from 127.0.0.1, so nobody was asked for a code. 0.7.1 reads the forwarding
headers a relayed request carries, so the question the daemon answers is whether
a request was made locally rather than whether it appears to come from nearby.

If you ran 0.7.0's browser client that way, assume every device that could reach
that proxy had the access a paired device has. `device remove` does not undo it:
those visitors were never asked to pair and hold no token to revoke, which is
also why devices that simply worked before will ask for a code after upgrading.
A daemon reached over an SSH forward of its Unix socket, or bound to loopback
with nothing in front of it, was never exposed.

Upgrade from 0.7.1 as well if any target relies on session discovery for its
Herdr API socket — a target with `discover_sessions` and no `socket` line.
0.7.0 and 0.7.1 decided whether an atomic paste was possible by reading the
configuration file rather than what discovery resolved, so those targets were
told they had no socket and a multiline paste into a pane without bracketed
paste was refused. 0.7.2 asks the resolved sessions, which is where a
discovered socket actually is.

### Package managers

```sh
# macOS and Linux, Homebrew tap
brew install mikro-design/tap/super-herdr

# Debian and Ubuntu. The version is part of the asset name, so there is no
# stable "latest" URL; set these to a published release and your architecture.
version=0.7.11
arch=amd64 # or arm64
package="super-herdr_${version}-1_${arch}.deb"
curl -fLO "https://github.com/mikro-design/super-herdr/releases/download/v${version}/${package}"
sudo dpkg -i "${package}"
```

### Prebuilt binaries

Each tagged release publishes stripped binaries, Debian packages, a `SHA256SUMS`
manifest with its Sigstore bundle, and build provenance attestations on the
[releases page](https://github.com/mikro-design/super-herdr/releases). Pick the
archive matching your platform:

- `aarch64-apple-darwin` — macOS Apple Silicon
- `x86_64-apple-darwin` — macOS Intel
- `x86_64-unknown-linux-gnu` — Linux x86_64
- `aarch64-unknown-linux-gnu` — Linux ARM64

Linux release binaries are built with `cross` and CI rejects artifacts requiring
newer than GLIBC 2.28.

```sh
# Set these to an available release and one of the targets listed above.
tag=v0.7.11
target=aarch64-apple-darwin
archive="super-herdr-${tag}-${target}.tar.gz"
release_url="https://github.com/mikro-design/super-herdr/releases/download/${tag}"

curl -fLO "${release_url}/${archive}"
curl -fLO "${release_url}/SHA256SUMS"
grep " ${archive}$" SHA256SUMS > "${archive}.sha256"
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum -c "${archive}.sha256"
else
  shasum -a 256 -c "${archive}.sha256"
fi
tar xzf "${archive}"
install -d ~/.cargo/bin
install -m 0755 "super-herdr-${tag}-${target}/super-herdr" ~/.cargo/bin/
super-herdr --version
```

Install into any directory on your `PATH`; `~/.cargo/bin` matches what
`make macos`/`make linux` use below.

### Verifying a release

Every published file is covered twice: by a signed checksum manifest and by a
build provenance attestation. Both are keyless—the signature is bound to the
release workflow's own identity, so there is no key to trust or rotate.

```sh
# Provenance: this file was produced by this repository's release workflow.
gh attestation verify "${archive}" --repo mikro-design/super-herdr

# Or verify the signed manifest, then check any file against it.
curl -fLO "${release_url}/SHA256SUMS"
curl -fLO "${release_url}/SHA256SUMS.sigstore.json"
cosign verify-blob \
  --bundle SHA256SUMS.sigstore.json \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp \
    '^https://github\.com/mikro-design/super-herdr/\.github/workflows/release\.yml@refs/tags/v' \
  SHA256SUMS
```

`gh attestation verify` needs GitHub CLI 2.49 or newer; an older `gh` does not
recognize the subcommand and prints its own help instead of an error. The second
path needs [cosign](https://github.com/sigstore/cosign) installed and no `gh` at
all.

A checksum alone only proves a file is intact; the signature and the attestation
are what tie it to this repository's workflow rather than to whoever served the
download.

Pull requests and manual workflow runs execute the quality gates and build all
four archives without publishing a release. To publish, push a `v<version>` tag
whose version exactly matches `Cargo.toml`; for example, package version `0.7.11`
must be tagged `v0.7.11`.

### macOS

```sh
git pull
make macos
super-herdr target add development --ssh development-host --discover-sessions
super-herdr target list
super-herdr clipboard check
super-herdr probe
super-herdr
```

Replace `development-host` with an alias that already works with ordinary
OpenSSH. `make macos` installs the binary and preserves any existing
configuration. The first `target add` creates a private configuration file.

### Linux desktop

Install `wl-clipboard` for Wayland or `xclip`/`xsel` for X11, then run:

```sh
git pull
make linux
super-herdr target add development --ssh development-host --discover-sessions
super-herdr target list
super-herdr clipboard check
super-herdr probe
super-herdr
```

`make linux` preserves an existing configuration. As on macOS, replace the
example SSH alias with one defined on the desktop where Super-Herdr runs.

### Development checkout

```sh
cp config.example.toml config.local.toml
cargo run -j 4 -- --config config.local.toml check
cargo run -j 4 -- --config config.local.toml probe
cargo run -j 4 -- --config config.local.toml tui
```

`config.local.toml` is ignored by Git.

## Configuration

The default configuration path is
`~/.config/super-herdr/config.toml`. It can be overridden with either
`--config PATH` or `SUPER_HERDR_CONFIG`.

### Add a machine from the command line

First verify that the desktop can reach the machine using its normal SSH alias:

```sh
ssh -o BatchMode=yes development-host true
```

Then add it and discover every Herdr session that is already running there:

```sh
super-herdr target add development \
  --ssh development-host \
  --discover-sessions

super-herdr target list
super-herdr probe
```

`development` is Super-Herdr's stable label for the machine;
`development-host` is the OpenSSH destination. Super-Herdr stores the alias, not
an SSH password or private key. SSH authentication, host keys, jump hosts, and
network routing remain in OpenSSH configuration.

To monitor just one named Herdr session instead of discovering all running
sessions:

```sh
super-herdr target add build --ssh build-host --session toolchains
```

To add Herdr running on the same desktop:

```sh
super-herdr target add desktop --local --discover-sessions
```

The optional advanced overrides are `--socket /absolute/path/to/herdr.sock` and
repeatable `--herdr-bin COMMAND`. Normally neither is necessary: session
discovery supplies the socket path and `herdr` is resolved on the target's
command search path.

`target add` validates the complete TOML configuration and replaces it
atomically with mode `0600`; when appending to an existing file it retains the
existing text and comments. It only changes Super-Herdr configuration. It does
not connect to, create, start, stop, or restart a Herdr session. Run `probe`
afterward to test connectivity.

Edit an existing target without changing its Herdr processes:

```sh
super-herdr target edit development --ssh replacement-host
super-herdr target edit build --single-session --session toolchains
super-herdr target edit desktop --local --discover-sessions
```

`--rename` changes the federation name, which changes every qualified ID that
uses it. `--clear-session`, `--clear-socket`, and `--default-herdr-bin` drop an
override and restore the default rather than replacing it with another value.

Remove a target from Super-Herdr only:

```sh
super-herdr target remove build --yes
```

Removal requires `--yes`, refuses to remove the final configured target, and
does not stop or restart anything on the target machine.

JSON is not the configuration format. TOML remains the durable, human-editable
source of truth. JSON output is available only where it helps automation, such
as `probe --json`.

### Edit TOML directly

The equivalent hand-written configuration is:

```toml
[transport]
ssh_bin = "ssh"
batch_mode = true
connect_timeout_seconds = 10
command_timeout_seconds = 20

[notifications]
enabled = false
needs_attention = true
completed = true
disappeared = true
working = false
status_changed = false
minimum_interval_seconds = 5
command_timeout_seconds = 5

[transfers]
max_bytes = 1073741824

[[targets]]
name = "development"
ssh = "development-host"
discover_sessions = true

[[targets]]
name = "build"
ssh = "build-host"
discover_sessions = true
```

Here, `development-host` and `build-host` are example entries in
`~/.ssh/config`; replace them with aliases defined for your own machines.

Target fields:

- `name`: stable federation name. It forms part of every qualified ID.
- `ssh`: OpenSSH destination or alias. Omit it for a local target.
- `discover_sessions`: run documented `herdr session list --json` at startup and
  every ten seconds, adding every running session from this host. Stopped
  sessions are never started.
- `session`: session used directly, or retained as the fallback if discovery
  fails.
- `socket`: optional absolute Herdr socket path on the target. It enables
  documented `events.subscribe` updates. SSH targets forward this Unix socket
  through OpenSSH.
- `herdr_bins`: compatible Herdr client candidates in preference order. Only a
  protocol mismatch advances to the next candidate. The default is `herdr` from
  the target's command search path; configure an absolute path only when needed.

Each discovered session is supervised independently. A failed target does not
freeze or tear down other targets.

**Drop a file onto the terminal and it is sent** to the selected pane. Dragging a
file onto a terminal does not copy it and does not paste it: the terminal types
the path in, which is why a bridge that only read the clipboard never saw the
file. A drop and a paste arrive identically, so what the text names is what
tells them apart — every word an absolute path to a readable file makes it a
drop, and anything else is pasted as text, as before. It is all or nothing: a
selection containing one file that is not there is pasted rather than half sent.

`Ctrl+]` then `f` does the same thing for a path you would rather type. Either
way the path is read the way a shell reads it: backslash escapes, single or
double quotes, a trailing space, and a leading `~` all name the file they look
like.

The browser client is served by default, on port 8790, bound to this machine's
own private address — the LAN one, or the `100.x` a mesh like Tailscale hands
out — which it works out for itself. Nothing has to be configured for a pairing
code to be scannable: `Ctrl+]` then `P` draws a QR pointing at an address the
phone in your hand can reach. The listener refuses a public address, and answers
nothing at all to a device that has not been paired.

The `[web]` table changes that when it needs changing:

```toml
[web]
port = 8790                     # 0 serves no browser client at all
address = "192.168.1.42"        # override what the machine worked out
url = "https://host.ts.net"     # where a phone reaches it, if that differs
```

`url` is only needed when the address a phone uses is not the address this
process binds, which is what a proxy terminating TLS elsewhere does — with
`tailscale serve` in front, the host, port and scheme all differ and cannot be
derived.

A pairing code may name any address the daemon is willing to bind, over plain
HTTP included. Pairing sends the code to that host over that connection whether
it was scanned or typed, so refusing to encode a URL the browser is about to use
anyway removed the QR without protecting the code. A public address over HTTP is
still refused. The `daemon` subcommand's `--web`, `--web-address` and
`--web-url` flags override the table for one run.

`transfers.max_bytes` is the largest transfer this daemon will accept, and it
defaults to one gibibyte. It bounds the target host's disk rather than
Super-Herdr's memory: a transfer is relayed onto the host as it arrives rather
than held whole, so the ceiling is a statement about the machine being written
to. It is separate from the frontend's own 32 MiB clipboard limit, because a
screenshot and a file are not the same question.

Notifications are disabled by default. When enabled, the event switches select
which transitions may reach the desktop. `minimum_interval_seconds` bounds the
delivery rate after a short coalescing window, and `command_timeout_seconds`
bounds the native notification process. Notifications contain only an agent
label, workspace label, qualified target/session, and transition kind. Terminal
and clipboard contents are never included.

## Commands

```sh
super-herdr target add NAME --ssh SSH_ALIAS --discover-sessions
super-herdr target edit NAME --ssh NEW_SSH_ALIAS
super-herdr target remove NAME --yes
super-herdr target list
super-herdr check
super-herdr probe
super-herdr probe --json
super-herdr probe --json --snapshots
super-herdr probe --timeout SECONDS
super-herdr clipboard check
super-herdr clipboard check --wait SECONDS
super-herdr notifications check
super-herdr notifications enable
super-herdr notifications test
super-herdr notifications disable
super-herdr tui
```

Running `super-herdr` without a subcommand opens the TUI.

- `check` parses the configuration and reports its targets.
- `target add` atomically adds a local or SSH host to the TOML configuration.
- `target edit` changes one named target block and validates the complete result.
- `target remove` removes only Super-Herdr configuration and requires `--yes`.
- `target list` shows configured hosts without contacting them.
- `probe` queries configured sessions concurrently and reports failures per
  target. On an interactive terminal, `OK` is green and `FAIL` is red; redirected
  output, JSON output, and terminals with `NO_COLOR` set remain uncolored.
  `--timeout` overrides the configured command timeout for one run, and
  `--snapshots` adds full server snapshots to `--json` output.
- `clipboard check` reports the active copy, text-paste, and media-paste paths,
  which flavors the clipboard is currently offering, any file the clipboard is
  pointing at, and which flavors this build can upload. When no file is found it
  also reports what each file reader answered, so a reader that failed is not
  mistaken for a clipboard that holds no file. It does not read or print
  clipboard payloads; flavor names and reader complaints are metadata, and are
  stripped of control characters and bounded before they are shown.
- `clipboard check --wait SECONDS` watches for up to that long so the file can be
  copied *after* the command starts. Use it whenever getting the command into the
  terminal would itself overwrite the clipboard being measured — pasting the
  command replaces the copied file, and the check then truthfully reports the
  text that replaced it.
- `notifications check` reports the configured filters and whether native
  delivery is available without sending anything.
- `notifications enable` and `notifications disable` atomically update only the
  notification setting. A running TUI reloads it without reconnecting terminal
  routes or restarting Herdr.
- `notifications test` sends one synthetic metadata-only notification and
  requires notifications to be enabled.
- `tui` opens the federated terminal UI.

## TUI controls

Normal keyboard input goes to the selected Herdr pane. Super-Herdr uses
`Ctrl+]` as its federation prefix and reserves `Ctrl+B` for Herdr-compatible
workspace actions:

| Input | Action |
| --- | --- |
| `Ctrl+]`, then `j` / `k` | Select next / previous qualified pane |
| `Ctrl+]`, then `p` / `n` | Select previous / next tab |
| `Ctrl+]`, then `1`–`9` | Select a numbered workspace |
| `Ctrl+]`, then `Space` | Search navigation and resource actions |
| `Ctrl+]`, then `d` | Close the selected Herdr workspace after qualified host/session confirmation |
| `Ctrl+]`, then `a` | Open the global agent navigator |
| `Ctrl+]`, then `e` | Open persistent agent-transition and unread history |
| `Ctrl+]`, then `h` | Open the target manager |
| `Ctrl+]`, then `v` | Paste desktop clipboard text into the selected pane |
| `Ctrl+]`, then `i` | Upload the clipboard's file and paste its verified target path |
| `Ctrl+]`, then `f` | Send a file by typing its path |
| `Ctrl+]`, then `q` | Quit Super-Herdr |
| `Ctrl+]` twice | Send a literal `Ctrl+]` to the selected pane |
| `Escape` after `Ctrl+]` | Cancel the Super-Herdr prefix |
| Right-click session/workspace/tab/pane | Open its qualified action menu |

Workspace close is deliberately confirmed with its Super-Herdr host name,
Herdr session, display label, and server-local workspace ID. It closes only that
workspace and its tabs and panes; Super-Herdr never stops or restarts the Herdr
session.

The action palette searches both action names and qualified target/session
scopes. Type to filter, use `Down`/`Tab`/`Ctrl+N` and `Up`/`Ctrl+P` to navigate,
press `Enter` to run the selected action, and press `Escape` to close it. It can
jump to any live session, workspace, tab, pane, or agent; create or rename
workspaces; create, rename, or close tabs; and split, zoom, or close the selected
pane. Workspace, tab, and pane closure always requires confirmation. Lifecycle
operations use the documented Herdr CLI and affect only the qualified session
shown in the palette.

Herdr's public terminal-session interface is a raw pane stream; it does not
expose Herdr's own TUI key dispatcher. Super-Herdr therefore maps supported
`Ctrl+B` chords to the equivalent documented Herdr CLI operations instead of
sending the control byte into the shell or agent:

| Herdr input | Action in the selected qualified session |
| --- | --- |
| `Ctrl+B`, then `h` / `j` / `k` / `l` | Select the neighboring pane using Herdr's layout |
| `Ctrl+B`, then `p` / `n` | Select previous / next tab |
| `Ctrl+B`, then `1`–`9` | Select a numbered tab |
| `Ctrl+B`, then `c` | Create and focus a tab through `herdr tab create` |
| `Ctrl+B`, then `v` / `-` | Split right / down through `herdr pane split` |
| `Ctrl+B`, then `z` | Toggle pane zoom through `herdr pane zoom` |
| `Ctrl+B`, then `?` | Show supported Herdr actions |
| `Escape` after `Ctrl+B` | Cancel the Herdr prefix |

Unsupported or custom Herdr TUI chords are rejected with a status message; they
are never leaked into the running pane. Protocol 19 has no public operation for
dispatching an arbitrary key through Herdr's client-side keymap.

The sidebar, tabs, and visible split panes are clickable. Scroll the mouse wheel
over the sidebar to reach hosts and workspaces beyond the current viewport. A
keyboard or mouse selection automatically brings its workspace back into view.
A click changes only Super-Herdr's local selection; it does not change another
Herdr client's global focus.

The target manager supports both keyboard and mouse operation. Click a machine
and the displayed Add, Edit, Remove, or Close button, or use `j`/`k`, `a`, `e`,
`d`, and `q`. In the form, click a field or use `Tab`; `Space` toggles
running-session discovery, `Ctrl+T` or **Test & discover** checks the machine,
and `Enter` or **Save** writes a valid form. Validation is shown before any file
write, including duplicate names and unsafe SSH/session values. A blank SSH
field means the Herdr installation is local to the desktop.

The connection test runs asynchronously with the configured timeout and invokes
only the documented `herdr session list --json` command. It never starts, stops,
or restarts Herdr and never displays raw SSH diagnostics. Running sessions found
by the test appear in the form. Clicking one—or focusing the list with `Tab`,
selecting it with the arrow keys, and pressing `Enter`—switches the target to
that exact session and its reported socket. Leave discovery enabled to follow
all running sessions instead. Advanced socket and client-path overrides remain
available through `target add` and `target edit`.

The sidebar reserves its upper half for host/session/workspace navigation and its
lower half for attention. The lower feed shows agents waiting now followed by
newest-first transition history; it scrolls independently, and every agent/event
row retains its qualified host/session/pane for click-to-jump routing. `Ctrl+]`,
then `a` opens the full navigator on the `attention` filter; use `j`/`k` to
select, `f` to cycle between `attention`, `active`, and `all`, and `Enter` to jump
to the agent's pane.

Agent status changes are recorded as payload-free attention events. `Ctrl+]`,
then `e` opens the newest-first history: use `j`/`k` to select, `Enter` to jump
and mark that qualified pane's events read, `r` to mark everything read, and `c`
to clear events already read. Repeated snapshots do not create duplicate events,
and a disconnected target does not falsely mark its agents as disappeared.

Native attention notifications are opt-in. Enable and verify them on the desktop
where Super-Herdr runs:

```sh
super-herdr notifications check
super-herdr notifications enable
super-herdr notifications test
```

The TUI never replays persisted attention history as notifications at startup.
New matching transitions are deduplicated, briefly coalesced, and rate-limited.
Delivery failures are isolated from target supervision and terminal routing. A
Super-Herdr process running through SSH or nested inside Herdr reports native
delivery as unavailable; keep it on the desktop for this feature. Disable it at
any time with `super-herdr notifications disable`.

A delivered notification offers one `Jump to pane` action. Clicking it selects
the qualified pane the notification described, exactly as clicking that row in
the attention feed would; a pane that has closed since delivery marks its events
read and says so instead of moving the selection. The pane identifier only
routes the click and never appears in the notification text.

Click reporting depends on the desktop, and `notifications check` reports which
you have:

- Linux with libnotify 0.8 or newer, whose `notify-send` offers `--action` and
  `--wait`, and a notification daemon that supports actions: supported.
- Linux with libnotify 0.7.x: notifications work, clicking does nothing. The
  older `notify-send` rejects unknown options, so the flags are used only when
  it advertises them.
- macOS: notifications work, clicking does nothing. `osascript` can display a
  notification but cannot report that it was clicked.

Waiting for a click never delays the next notification: delivery completes when
the desktop accepts the notification, and the wait is bounded by how long the
notification can stay on screen.

A workspace can be moved into another workspace of the same Herdr session, from
the action palette or the workspace context menu (`Move workspace "a" into
"b"`). Super-Herdr reads each source tab's split tree from Herdr and replays it
with documented pane moves, so panes keep their processes, scrollback, and
agents; only their identifiers are re-qualified by Herdr. The whole sequence
runs as one action: if a step fails, the tabs already moved stay in the
destination and the rest stay in the source, and nothing is closed. The context
menu is not scrollable, so it lists at most eight destinations in workspace
order; the action palette lists every one. A move destination in another session
or on another host is never offered—Herdr's protocol 19 has no cross-session
transfer.

Crossing a session boundary is offered separately as `Recreate workspace "a" on
build/toolchains (new shells)`, listing every other live session including those
on other hosts. Recreation reads the source workspace, creates a workspace on
the destination, and rebuilds each tab's split structure, ratios, pane labels,
and recorded working directories there. It is not a move:

- Every pane is a new shell. Processes, scrollback, and agent state stay in the
  source workspace, which keeps running and is never closed. Close it yourself
  with the usual confirmation once you are satisfied with the result.
- Pane commands and environment variables are deliberately not replayed, so
  recreation never runs a program or carries a secret onto another machine.
- A working directory that does not exist on the destination host makes Herdr
  reject that tab.
- Recreation refuses a workspace that would start more than 64 panes. If a tab
  fails part way through, the report names the destination workspace and how
  many tabs it holds so you can close it there and retry.

Right-clicking a session, workspace, tab, or pane opens a pointer-anchored menu.
Use the mouse, `j`/`k`, or the arrow keys to choose an action and `Enter` to run
it. Workspace, tab, and pane closure still goes through the same qualified
confirmation used by the action palette; no context action stops or restarts a
Herdr session. Right-click is reserved for this qualified Super-Herdr menu and is
not forwarded to the program inside the selected pane.

Mouse behavior inside the selected terminal:

- A normal left-button drag selects terminal text locally, including in
  mouse-aware applications.
- Holding a selection at the top or bottom edge continuously scrolls Herdr's host
  scrollback and extends the highlight across pages.
- Releasing copies the entire selection and retains the highlight until the next
  click or key.
- A press and release without movement is forwarded as an application click.
- Wheel gestures use Herdr's documented `terminal.scroll` routing, allowing Herdr
  to choose application mouse reporting, alternate-screen scrolling, or host
  scrollback.

If another Herdr client owns the control lease, the selected pane remains visible
as `[read-only]`. Super-Herdr retries a normal control lease automatically and
never uses `--takeover`.

## Clipboard behavior

| Frontend environment | Copy selection | Normal terminal paste | Explicit text paste (`Ctrl+] v`) | File upload (`Ctrl+] i`) |
| --- | --- | --- | --- | --- |
| macOS desktop | Native `pbcopy` | Atomic | Native `pbpaste`, atomic | Native clipboard via `osascript` |
| Linux Wayland desktop | Native `wl-copy` | Atomic | Native `wl-paste`, atomic | `wl-paste` with `image/png` |
| Linux X11 with `xclip` | Native | Atomic | Native, atomic | `xclip` with `image/png` |
| Linux X11 with only `xsel` | Native text | Atomic | Native text, atomic | Unavailable |
| Super-Herdr launched through SSH or inside Herdr | OSC 52 request | Atomic, using the local terminal's paste | Unavailable | Unavailable |

Selection copy excludes Super-Herdr borders, sidebar content, and right-side
padding. Native delivery reports `copied N characters to system clipboard`. OSC
52 reports only that a copy was requested because the remote process receives no
clipboard acknowledgement.

Super-Herdr enables bracketed paste in its outer terminal and buffers every
normal terminal paste, including multiline content, as one bounded event. Both
that path and explicit `Ctrl+] v` delivery use Herdr's documented
`pane.send_input` socket API. Herdr therefore applies the pane runtime's actual
bracketed-paste state and receives one semantic input request instead of one
submission per newline. For SSH targets, the private Herdr Unix socket is
forwarded through OpenSSH; clipboard text is never placed in SSH or Herdr process
arguments.

Text paste:

- is limited to 1 MiB;
- is sent as one semantic request and honors Herdr's authoritative
  bracketed-paste mode;
- rejects a payload containing a bracketed-paste terminator; and
- requires a writable selected pane.

Running-session discovery supplies the documented socket path automatically. If
a target has neither discovery nor an explicit socket, Super-Herdr permits the
existing raw fallback for a single line but refuses multiline input rather than
silently splitting it into several messages.

Explicit file upload:

- covers PNG, JPEG, WebP, GIF, TIFF, PDF, and SVG. Super-Herdr asks the desktop
  which flavors the clipboard is offering and takes the first it supports,
  preferring PNG so a screenshot behaves exactly as it always has;
- is limited to 32 MiB;
- writes to a private temporary directory on the selected host;
- verifies remote byte count and SHA-256 digest;
- removes the uploaded file again if either check fails, so a refused
  transfer leaves nothing on the host; and
- sends only the verified target path to the pane.

The transfer itself is format-agnostic: it moves bytes, verifies them, and
injects a path. A payload too large to hold in memory is streamed instead,
hashed on the way past so the digest attests to exactly the bytes that were
sent, with the declared length enforced in both directions: a source that ends
early is refused as truncated, and one that runs long is cut off rather than
allowed to write unbounded data onto the host. Each refusal names the check that
failed, and removes whatever reached the host. A type the table does not carry
is uploaded with no extension at all rather than refused, because a name from
outside would be untrusted text in a remote command. A format is recognized by byte patterns at fixed offsets, so
WebP is identified by its container tag rather than by the `RIFF` prefix it
shares with AVI and WAV, and formats with more than one valid header, such as
GIF and TIFF, carry every form. A flavor with no dependable signature, such as
SVG, is carried on the digest alone rather than refused. The uploaded file's
extension always comes from Super-Herdr's own table and never from the
clipboard, so no untrusted text reaches the remote command.

Clipboard payloads and terminal contents are never logged.

## Session discovery, events, and persistence

With `discover_sessions = true`, Super-Herdr invokes the documented
`herdr session list --json` command at startup and refreshes the registry every
ten seconds. Hosts are queried concurrently. Each running session becomes an
independently qualified target; additions and disappearances replace only
Super-Herdr's per-session supervisors. Discovery failures retain the configured
fallback session and remain isolated to that host.

The same refresh loop detects atomic changes to the TOML configuration,
including edits made by another terminal. The TUI remains responsive while SSH
discovery runs. No refresh starts, stops, or restarts a Herdr session.

When `socket` is configured, resource, layout, and agent-status events request an
immediate authoritative snapshot. If subscription setup or the stream fails, the
target continues using its five-second polling fallback. The sidebar reports
`evt`, `poll`, or `poll!`.

The last explicitly selected qualified pane is persisted atomically at:

```text
$XDG_STATE_HOME/super-herdr/ui-state.json
```

or, when `XDG_STATE_HOME` is unset:

```text
~/.local/state/super-herdr/ui-state.json
```

The bounded attention index is persisted separately at
`$XDG_STATE_HOME/super-herdr/attention-state.json`, or
`~/.local/state/super-herdr/attention-state.json` when `XDG_STATE_HOME` is
unset. It contains only qualified pane identity, agent/workspace labels, status,
transition kind, timestamp, and unread state. It never contains terminal output,
clipboard payloads, SSH material, or complete server snapshots.

On startup, restoration waits for the corresponding target/session to reconnect
and succeeds only if that exact qualified pane is live. Saved state never starts,
focuses, stops, or restarts anything in Herdr. Terminal content, snapshots,
clipboard data, SSH material, and control leases are not persisted.

## Safety and boundaries

- Super-Herdr never automatically stops, starts, restarts, or takes over a Herdr
  session.
- Herdr server-local IDs are always qualified with target and session before use
  in federation state.
- Network and clipboard operations are bounded by time and size limits.
- SSH host-key verification and user configuration remain in force.
- Target failures are isolated.
- Clipboard payloads, terminal contents, credentials, and SSH diagnostics are not
  logged.
- Integration remains independent of Herdr internals and uses documented public
  interfaces.

## Verification

Before handing off changes, run:

```sh
cargo fmt --check
cargo test -j 4
cargo clippy -j 4 --all-targets -- -D warnings

# Platform-gated code needs its own target to be seen at all.
rustup target add x86_64-apple-darwin
cargo clippy -j 4 --target x86_64-apple-darwin --all-targets -- -D warnings
```

## Current limitations and next slices

- Advanced socket/client-path editing remains CLI-driven; the target manager
  preserves existing overrides and fills the documented socket automatically
  when a discovered session is selected.
- Notification click-to-jump needs a desktop that reports actions: libnotify 0.8
  or newer on Linux. On macOS, and on Linux with libnotify 0.7.x, notifications
  remain one-way; use the lower attention feed or `Ctrl+] e` to jump.
- A workspace still cannot be *moved* between Herdr sessions. Each session is a
  separate server process owning its panes, and protocol 19 exposes no
  cross-session transfer; `workspace.move` only reorders workspaces inside one
  session. Recreation is the supported substitute and restarts every process.
  A live cross-session move needs a Herdr-side transfer of the panes themselves.
- A copied file is read whole before it is sent, so one is capped at 32 MiB even
  though the daemon's own ceiling is larger. Streaming from disk is what would
  close that gap.
- WebP and SVG have no classic macOS pasteboard class, so on macOS they report as
  unsupported rather than being requested under a code that cannot exist.
- Remote-to-local clipboard messages emitted by a program inside a Herdr pane are
  not available through Herdr 0.8's documented terminal-session stream. Local
  Super-Herdr text selection remains the supported remote-to-local copy path.
- Optional server-side focus actions and richer capability/host-key diagnostics
  remain planned.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the domain model and invariants,
[TESTING.md](TESTING.md) for the release test matrix and its qualification
records, and [ROADMAP.md](ROADMAP.md) for dependency-ordered follow-up work.
`scripts/qualify-desktop.sh` runs the machine-decidable part of that matrix on
whichever desktop you invoke it from.

## Licensing

Copyright (c) 2026 Mikro Design.

Super-Herdr is dual licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. This is the customary Rust dual license: the MIT terms are the
simplest to comply with, and Apache-2.0 adds an explicit patent grant.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in Super-Herdr by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.

This license covers Super-Herdr only. Super-Herdr interoperates through Herdr's
public interfaces and contains no Herdr source; copying or modifying Herdr source
remains a separate licensing and maintenance decision.
