# Super-Herdr

Super-Herdr is a multi-host client for Herdr. One desktop process owns the UI,
clipboard, and image-paste bridge while independent Herdr servers keep shells and
agents alive on their respective machines.

The project deliberately starts outside Herdr and uses its public CLI/socket API.
That gives us a small compatibility boundary and avoids modifying the server before
we have measured the federation model.

## Current runnable slice

The current slice loads several targets, validates them, queries their session
snapshots concurrently, and reports failures independently. It also has a
live TUI backed by independent per-target supervisors. Every resource is
identified as `target/session/server-local-id`; two hosts may therefore both have
`w1:p1` without colliding.

```sh
cp config.example.toml config.local.toml
cargo run -- --config config.local.toml check
cargo run -- --config config.local.toml probe
cargo run -- --config config.local.toml probe --json
cargo run -- --config config.local.toml
cargo run -- --config config.local.toml tui
```

With an installed configuration at `~/.config/super-herdr/config.toml`, running
`super-herdr` with no subcommand opens the TUI. The explicit `tui` subcommand is
retained for scripts and diagnostics.

For native desktop clipboard integration, run the frontend process on the desktop
and connect outward to every Herdr host. On macOS, start from
`config.macos.example.toml`; selections then use local `pbcopy`, while ws01 and
Kamrui continue to own all persistent Herdr sessions. Running the frontend through
plain SSH can only request OSC 52 from the local terminal emulator and cannot
guarantee that the desktop clipboard accepts it.

After cloning on macOS, one command installs the binary and creates the default
configuration without replacing an existing configuration:

```sh
make macos
```

The TUI discovers Herdr sessions independently on each configured host through
`herdr session list --json`. Each running session is shown as a qualified
`host/session` entry; discovery never starts a stopped session. A failed discovery is isolated
to that host and retains its configured fallback session.

The TUI keeps a host/session/workspace sidebar and tab strip around the active tab. It
uses Herdr's public layout snapshots to reproduce split-pane geometry, opens
read-only observer streams for background panes, and routes input to the selected
pane through the public control stream. Press `Ctrl+]`, then `j`/`k` to switch
qualified panes, `p`/`n` to switch tabs, `1`–`9` to select a numbered workspace,
or `q` to quit; press `Ctrl+]` twice to send a literal prefix byte. `Ctrl+B`
passes through to Herdr unchanged. Navigation stays local without changing
another Herdr client's server-global focus. Super-Herdr
never passes `--takeover` and never stops or restarts a Herdr session. If another
Herdr client owns control, the selected pane stays live in `[read-only]` mode and
Super-Herdr retries control automatically.

The host/session/workspace sidebar, tabs, and visible split panes are clickable.
Sidebar clicks use hit rectangles captured from the rendered layout and activate
the originally pressed row on release, so the title row and asynchronous state
updates cannot shift the target underneath a click. Terminal-frame bursts are
coalesced to a 60 Hz render cadence with input handled first.
An ordinary left-button drag inside the selected terminal selects text and remains
clamped to that pane even when the inner application requested mouse reporting.
A press and release without movement is forwarded as an application click. Other
mouse buttons are forwarded normally. Wheel gestures use
Herdr's documented `terminal.scroll` command, letting the server choose application
mouse reporting, alternate-screen scrolling, or host scrollback. Releasing a text
selection copies trimmed text through the native clipboard when available and OSC
52 otherwise, without including Super-Herdr's borders, sidebar, or right padding.
Native delivery reports that text was copied. OSC 52 reports only that a terminal
clipboard copy was requested, because the remote process receives no acknowledgement.
The finalized highlight remains visible until the next click or key. When running
inside Herdr, OSC 52 is always preferred so the outer Herdr client bridges the copy
to the user's desktop instead of writing to a display-local clipboard on the host.
Read-only panes also select locally because they cannot forward mouse input.
Selection uses reverse-video rather than color-only styling, so it remains visible
when `NO_COLOR` is set.

`config.local.toml` is ignored by Git. A target without `ssh` is executed locally;
a target with `ssh = "alias"` is reached through the user's normal OpenSSH config.
Each target has an ordered `herdr_bins` candidate list. Super-Herdr advances only
when a client reports a protocol mismatch, keeping mixed-version servers usable
without restarting their sessions.

An optional absolute `socket` path enables documented `events.subscribe`
updates. For SSH targets it is the remote socket path, forwarded through
OpenSSH. Resource, layout, and per-pane agent-status events trigger immediate
authoritative snapshots. If the subscription fails, that target keeps its
five-second polling fallback; the sidebar shows `evt`, `poll`, or `poll!`.
Set `discover_sessions = true` on a target to replace its configured fallback
session/socket with all sessions reported by that host at startup.

Build concurrency is capped at four jobs in `.cargo/config.toml`.

## Planned slices

1. Refresh host session registries while the TUI is running and persist UI selection.
2. Add optional server-side focus actions for mouse navigation.
3. Add local-to-remote clipboard mediation, then upload clipboard images to the selected
   host and inject only the remote path.
4. Add host-key diagnostics, richer capability negotiation, and fake SSH/Herdr
   process fixtures.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the design boundaries and invariants.

## Licensing note

No project license has been selected yet. Interoperation is through Herdr's public
interfaces. Copying or modifying Herdr source requires a separate licensing and
maintenance decision.
