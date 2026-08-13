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
- Explicit desktop-clipboard text paste into a selected remote pane.
- Verified PNG clipboard upload to the selected host with path-only injection.
- Event-driven updates when a documented Herdr socket is configured, with polling
  fallback.
- Atomic persistence and restoration of the last explicitly selected qualified
  pane.
- Command-line and in-TUI target management backed by one atomic TOML store.
- Live configuration and running-session discovery refresh without restarting
  Herdr.
- A global agent navigator with attention and active-work filters.

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

- `sha256sum` on an SSH target when using verified PNG upload.

Build concurrency is capped at four jobs in `.cargo/config.toml` and in the
provided Makefile targets.

## Install

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
super-herdr clipboard check
super-herdr tui
```

Running `super-herdr` without a subcommand opens the TUI.

- `check` parses the configuration and reports its targets.
- `target add` atomically adds a local or SSH host to the TOML configuration.
- `target edit` changes one named target block and validates the complete result.
- `target remove` removes only Super-Herdr configuration and requires `--yes`.
- `target list` shows configured hosts without contacting them.
- `probe` queries configured sessions concurrently and reports failures per
  target.
- `clipboard check` reports the active copy, text-paste, and image-paste paths.
  It does not read or print clipboard payloads.
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
| `Ctrl+]`, then `a` | Open the global agent navigator |
| `Ctrl+]`, then `h` | Open the target manager |
| `Ctrl+]`, then `v` | Paste desktop clipboard text into the selected pane |
| `Ctrl+]`, then `i` | Upload a clipboard PNG and paste its verified target path |
| `Ctrl+]`, then `q` | Quit Super-Herdr |
| `Ctrl+]` twice | Send a literal `Ctrl+]` to the selected pane |
| `Escape` after `Ctrl+]` | Cancel the Super-Herdr prefix |

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

The sidebar, tabs, and visible split panes are clickable. A click changes only
Super-Herdr's local selection; it does not change another Herdr client's global
focus.

The target manager uses `j`/`k` to select a configured machine, `a` to add, `e`
or `Enter` to edit, and `d` to remove with confirmation. In its form, `Tab`
moves between fields, `Space` toggles running-session discovery, and `Enter`
saves. A blank SSH field means the Herdr installation is local to the desktop.
Advanced socket and client-path overrides remain available through `target add`
and `target edit`.

The agent navigator combines agents from every live target. It sorts blocked,
waiting, or input-ready agents first. Use `j`/`k` to select, `f` to cycle between
`all`, `attention`, and `active`, and `Enter` to jump to the agent's pane.

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

| Frontend environment | Copy selection | Explicit text paste (`Ctrl+] v`) | PNG upload (`Ctrl+] i`) |
| --- | --- | --- | --- |
| macOS desktop | Native `pbcopy` | Native `pbpaste` | Native clipboard via `osascript` |
| Linux Wayland desktop | Native `wl-copy` | Native `wl-paste` | `wl-paste` with `image/png` |
| Linux X11 with `xclip` | Native | Native | `xclip` with `image/png` |
| Linux X11 with only `xsel` | Native text | Native text | Unavailable |
| Super-Herdr launched through SSH or inside Herdr | OSC 52 request | Unavailable; use the local terminal's paste | Unavailable |

Selection copy excludes Super-Herdr borders, sidebar content, and right-side
padding. Native delivery reports `copied N characters to system clipboard`. OSC
52 reports only that a copy was requested because the remote process receives no
clipboard acknowledgement.

Explicit text paste:

- is limited to 1 MiB;
- honors the selected terminal's bracketed-paste mode;
- rejects a payload containing a bracketed-paste terminator; and
- requires a writable selected pane.

Explicit PNG upload:

- is limited to 32 MiB;
- writes to a private temporary directory on the selected host;
- verifies remote byte count and SHA-256 digest; and
- sends only the verified target path to the pane.

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
cargo clippy -j 4 -- -D warnings
```

## Current limitations and next slices

- The target-manager form covers the common name, SSH alias, session, and
  discovery fields. Advanced socket/client-path editing remains CLI-driven.
- The agent navigator uses Herdr's current status fields; unread activity and a
  durable notification history are not implemented.
- File-list clipboard upload is not implemented; the bridge currently accepts PNG
  images only.
- Remote-to-local clipboard messages emitted by a program inside a Herdr pane are
  not available through Herdr 0.8's documented terminal-session stream. Local
  Super-Herdr text selection remains the supported remote-to-local copy path.
- Optional server-side focus actions and richer capability/host-key diagnostics
  remain planned.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the domain model and invariants,
[TESTING.md](TESTING.md) for the release test matrix, and
[ROADMAP.md](ROADMAP.md) for dependency-ordered follow-up work.

## Licensing

No project license has been selected. Super-Herdr interoperates through Herdr's
public interfaces. Copying or modifying Herdr source requires a separate licensing
and maintenance decision.
