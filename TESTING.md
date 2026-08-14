# Super-Herdr release test matrix

This matrix distinguishes automated coverage from manual platform qualification.
Passing unit tests does not imply that an unchecked desktop environment has been
qualified for release.

## Required automated checks

Run with no more than four build jobs:

```sh
cargo fmt --check
cargo test -j 4
cargo clippy -j 4 -- -D warnings
```

Automated coverage must include qualified multi-host IDs, target failure
isolation, atomic configuration writes, session discovery, terminal control and
observe routing, mouse encoding, multipage selection, clipboard size/integrity
checks, persisted UI selection, and sidebar overflow with offset-aware mouse
targets. Linux release jobs must reject binaries whose highest required GLIBC
symbol version exceeds 2.28.

## Manual desktop matrix

| Frontend | Remote path | Text copy/paste | PNG paste | Mouse/select/scroll | Host/session refresh |
| --- | --- | --- | --- | --- | --- |
| macOS terminal | OpenSSH alias | Required | Required | Required | Required |
| Linux Wayland | OpenSSH alias | Required | Required | Required | Required |
| Linux X11 with `xclip` | OpenSSH alias | Required | Required | Required | Required |
| Linux X11 with `xsel` only | OpenSSH alias | Required | Not supported | Required | Required |
| Nested SSH or Herdr | terminal-mediated copy | Copy request only; local paste | Not supported | Required | Required |

For every applicable row, verify:

1. `super-herdr clipboard check` reports capabilities without reading payloads.
2. `super-herdr probe` isolates an unreachable target while live targets remain usable.
3. `Ctrl+B` opens Herdr-action mode, its supported chords affect only the
   selected qualified session, and `Ctrl+]` remains Super-Herdr's prefix.
4. Normal clicks reach mouse-aware programs; dragging selects locally.
5. Edge dragging continues multipage selection without further pointer movement.
6. Selection excludes sidebar, borders, and terminal padding.
7. Text paste honors bracketed paste; PNG upload verifies size and SHA-256.
8. Adding, editing, or removing a target refreshes the TUI without restarting Herdr.
9. Starting or stopping a Herdr session is reflected by registry refresh.
10. The agent navigator filters and jumps across at least two hosts.
11. With more workspace rows than fit onscreen, sidebar wheel scrolling reaches
    every row, clicks select the displayed item, and keyboard selection plus
    terminal resizing keep the selected workspace visible.

Record the operating-system version, terminal emulator, display protocol, Herdr
version/protocol, and Super-Herdr commit for each qualification run. Never put
real hostnames, addresses, usernames, credentials, terminal contents, or
clipboard payloads in committed test records.
