# Super-Herdr

[![CI and Release](https://github.com/mikro-design/super-herdr/actions/workflows/release.yml/badge.svg)](https://github.com/mikro-design/super-herdr/actions/workflows/release.yml)
[![Latest release](https://img.shields.io/github/v/release/mikro-design/super-herdr)](https://github.com/mikro-design/super-herdr/releases/latest)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

Super-Herdr puts persistent Herdr sessions from all your machines in one terminal
UI. Pair a phone when you need to read output, answer a prompt, or send a file
away from your desk.

- One TUI for local and SSH-connected Herdr sessions
- Phone access through an explicit device-login flow
- Attention tracking across hosts, sessions, workspaces, and agents
- Verified file transfer for images, PDFs, Office documents, and other files

Herdr continues to own every shell, agent, workspace, and terminal history.
Super-Herdr never automatically starts, stops, restarts, or takes over a Herdr
session.

> **Public preview:** A paired browser can control a shell running as your user.
> Read the [security model](SECURITY.md) before exposing it outside your own
> devices.

## Get started

Run Super-Herdr on the macOS or Linux machine whose display, keyboard, and
clipboard you use.

### 1. Install

With Homebrew on macOS or Linux:

```sh
brew install mikro-design/tap/super-herdr
```

Debian and Ubuntu packages are published for amd64 and arm64:

```sh
version=0.7.20
arch=amd64 # or arm64
package="super-herdr_${version}-1_${arch}.deb"
curl -fLO "https://github.com/mikro-design/super-herdr/releases/download/v${version}/${package}"
sudo dpkg -i "./${package}"
```

Other Linux users can install a prebuilt archive:

```sh
tag=v0.7.20
target=x86_64-unknown-linux-gnu # or aarch64-unknown-linux-gnu
archive="super-herdr-${tag}-${target}.tar.gz"
curl -fLO "https://github.com/mikro-design/super-herdr/releases/download/${tag}/${archive}"
tar xzf "${archive}"
sudo install -m 0755 "super-herdr-${tag}-${target}/super-herdr" /usr/local/bin/
```

All macOS and Linux packages, checksums, and build attestations are on the
[releases page](https://github.com/mikro-design/super-herdr/releases/latest).

### 2. Add a Herdr machine

For a remote machine, start with an SSH alias that already works:

```sh
ssh -o BatchMode=yes development-host true
super-herdr target add development --ssh development-host --discover-sessions
```

Replace `development-host` with your OpenSSH host or alias. To use Herdr running
on this machine instead:

```sh
super-herdr target add desktop --local --discover-sessions
```

Super-Herdr discovers sessions that are already running. It does not create or
restart them.

### 3. Check and open

```sh
super-herdr probe
super-herdr
```

`probe` reports each target independently, so one unreachable machine does not
hide the others. Running `super-herdr` without a subcommand opens the TUI.

## Pair a phone

The default route uses `super-herdr.key-value.co`, so the phone needs no
Tailscale client, shared Wi-Fi, or inbound route to your computer.

1. In the TUI, press `Ctrl+]`, then uppercase `P`.
2. Scan the QR or open <https://super-herdr.key-value.co> on the phone.
3. Type the eight-character code shown separately in the TUI. The QR and URL do
   not contain the code.
4. Check that the six-digit number matches on both screens, then approve it in
   the trusted TUI with `y`.
5. Open a pane and tap **Take control & type** before using the keyboard or file
   picker.

The short code only requests pairing; it never creates a device credential by
itself. List or revoke paired devices with `super-herdr device --help`.

## Everyday controls

Normal keyboard input goes to the selected pane. Super-Herdr keeps that exact
qualified pane selected when servers connect or disconnect, so background churn
cannot redirect the next byte you type.

| Input | Action |
| --- | --- |
| `Ctrl+]`, then `j` / `k` | Next / previous pane |
| `Ctrl+]`, then `n` / `p` | Next / previous tab |
| `Ctrl+]`, then `Space` | Search navigation and actions |
| `Ctrl+]`, then `a` | Agents needing attention |
| `Ctrl+]`, then `e` | Attention history |
| `Ctrl+]`, then `h` | Add, edit, or remove a target |
| `Ctrl+]`, then `v` | Paste desktop clipboard text |
| `Ctrl+]`, then `i` | Upload a copied file and paste its verified path |
| `Ctrl+]`, then `f` | Send a file by path |
| `Ctrl+]`, then uppercase `P` | Pair a browser |
| `Ctrl+]`, then `q` | Quit Super-Herdr |

Common Herdr layout chords also work: use `Ctrl+B` followed by `h/j/k/l` to
move between panes, `c` to create a tab, `v` or `-` to split, `z` to zoom, and
`?` for help. Right-click a session, workspace, tab, or pane for actions scoped
to that exact target and session. Actions from enabled Herdr plugins appear in
the same right-click menus and searchable action palette. In the browser, open
a pane to get the actions it supports as compact **Tools** buttons. Super-Herdr
shows whether the plugin command is running, finished, or failed. If the action
opens a picker, overlay, board, or another pane, an **Open** button appears;
your current terminal never changes until you tap it.

The browser opens on an agent inbox rather than the full hierarchy: your
agents grouped into **Needs you**, **Working**, and **Recent**, each card
naming the host and Herdr session it belongs to. Tap one to open that exact
pane. Chips filter by state, host, and agent kind, and **All targets and
panes** is always one tap away when you want the hierarchy instead.

Turn on **Alerts** in the browser to be told when an agent wants you, under the
same filters, coalescing and rate limit as the desktop notifications — opt in
with `notifications.devices` and grant the browser's own permission. An alert
carries a label and a status word, never terminal contents, and tapping it
opens that exact agent or says it has gone. It reaches a device while the page
is running; waking a closed browser needs Web Push and is not built yet.

Pin an agent to keep it at the top, or mute or snooze one to move it out of the
way for a while — in the browser on the card itself, in the TUI from the same
right-click menus and action palette. These are Super-Herdr's own view of your
inbox: nothing about a mark reaches the host or changes a Herdr session.

A card whose host has disconnected, or whose agent has ended, is shown and not
offered — Super-Herdr resolves the live pane again before it routes anything,
so a reconnect can never redirect your next keystroke.

`super-herdr plugins list` shows what is installed on every host and what
differs — missing plugins, version drift, and the same version built from
different commits. Plugins are matched by where they were installed from
rather than by the id a host gave them, so two hosts that happen to use the
same name are not mistaken for one plugin. `plugins lock --from HOST` writes
that host's set pinned to resolved commits, and `plugins plan --lock FILE`
prints the `herdr plugin install` commands that would close the gap without
running any of them.

Run `super-herdr doctor` when something does not appear and you cannot tell
which layer is at fault. It checks the configuration, every target
independently, the daemon socket, the browser route, pairing, clipboard tools,
notifications and transfer dependencies; reports the command to run for
anything broken without running it; and redacts host names, destinations,
paths and URLs so the output can be pasted into an issue. `--json` gives the
same metadata for a support bundle.

## What is included

- Concurrent discovery and supervision across local and SSH targets
- A browser agent inbox: qualified per-agent cards, compact filters, and one
  tap to the exact pane that needs you
- Per-agent pins, mutes, and snoozes that order your own inbox and never touch
  a Herdr session
- Opt-in paired-device alerts carrying bounded metadata, with per-agent
  needs-you-only, mute, and snooze modes
- A live, clickable TUI with tabs, split panes, search, and agent attention
- Qualified plugin actions in the TUI and browser, with sanitized run status
  and explicit routing to any newly opened pane
- Explicit control leases: background panes and newly opened browser panes are
  read-only until control is granted
- Desktop selection, clipboard paste, drag-and-drop, and verified uploads
- Browser file fetch from a target: size, source and digest shown before any
  byte moves, verified on arrival, with text and image previews
- A bounded remote file picker over directories you configure, with literal or
  opt-in pattern search that never leaves those roots
- Desktop save from a target and host-to-host transfer that never touches this
  machine, both from the same right-click menus
- `super-herdr doctor`: one read-only pass over every layer, with redacted
  output and the command to run for anything broken
- Cross-host plugin inventory, drift, a pinned lockfile, and a printed
  install plan that runs nothing
- Configurable browser quick replies, terminal keys, a soft-keyboard dock, and
  a file picker
- Digest-verified local-to-host, host-to-local, and host-to-host file transfer
- Bounded reconnects and per-target failure isolation

Files are transferred as bytes, checked for size and SHA-256 digest, and pasted
into the terminal only as a shell-quoted path. Terminal contents, clipboard
payloads, credentials, and SSH material are never logged.

## Requirements

- A compatible Herdr client on each target. Herdr 0.8.2 / protocol 20 is the
  currently tested combination.
- OpenSSH for remote targets. Existing SSH configuration, keys, host checking,
  and proxy settings remain authoritative.
- `sha256sum` on targets that receive files.
- For desktop clipboard integration on Linux: `wl-clipboard` on Wayland, or
  `xclip` / `xsel` on X11. macOS uses its built-in clipboard tools.

A Rust toolchain is needed only when building from source.

## Configuration and help

Configuration lives at `~/.config/super-herdr/config.toml` by default. Use
`--config PATH` or `SUPER_HERDR_CONFIG` to choose another file. The CLI writes
configuration atomically with private permissions and leaves SSH credentials in
OpenSSH.

```sh
super-herdr --help
super-herdr target --help
super-herdr target list
super-herdr clipboard check
super-herdr notifications check
```

See [config.example.toml](config.example.toml) for manual TOML configuration.
The browser route can use the hosted bridge, a private address, Tailscale Serve,
or an operator-managed proxy. See the [architecture](ARCHITECTURE.md) for those
trust boundaries and the [bridge guide](crates/bridge/README.md) for self-hosting.

## Security summary

- Browser pairing requires an eight-character code and a separate matching
  six-digit approval in the trusted TUI.
- The public relay carries bounded opaque traffic, but TLS terminates there; it
  is a trusted relay, not end-to-end encryption.
- Control must be requested explicitly, and lease changes are visible.
- IDs are qualified by target and session before they are used.
- Network, clipboard, and transfer operations are bounded by time or size.

Please report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

## Project documentation

- [Architecture and protocol boundaries](ARCHITECTURE.md)
- [Security policy](SECURITY.md)
- [Roadmap and current limitations](ROADMAP.md)
- [Community-informed product backlog](PRODUCT_BACKLOG.md)
- [Release test matrix](TESTING.md)
- [Changelog](CHANGELOG.md)
- [Contributing](CONTRIBUTING.md)
- [Packaging and releases](packaging/README.md)

## Build from source

```sh
git clone https://github.com/mikro-design/super-herdr.git
cd super-herdr
cargo build --release --locked --jobs 4
./target/release/super-herdr --help
```

Development and pull-request checks are documented in
[CONTRIBUTING.md](CONTRIBUTING.md). Build concurrency is capped at four jobs.

## License

Copyright (c) 2026 Mikro Design.

Super-Herdr is dual licensed under [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at your option. It contains no Herdr source and
integrates through Herdr's documented public interfaces.
