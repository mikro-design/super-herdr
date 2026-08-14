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
observe routing, mouse encoding, multipage selection, atomic bracketed-paste
decoding, one-request semantic pane input, clipboard size/integrity checks,
persisted UI selection, and independently scrolling split-sidebar viewports with
offset-aware mouse targets. Attention coverage must include qualified transition
deduplication, disconnect isolation, unread handling, atomic metadata-only
persistence, and click-to-jump rows in the lower sidebar. Context-menu coverage
must prove that actions retain their exact qualified session/workspace/tab/pane
identity and that destructive actions enter the shared confirmation path.
Linux release jobs must reject binaries whose highest required GLIBC symbol
version exceeds 2.28.

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
2. `super-herdr probe` isolates an unreachable target while live targets remain
   usable; interactive `OK`/`FAIL` labels are green/red, while redirected,
   `--json`, and `NO_COLOR=1` output contains no ANSI escapes.
3. `Ctrl+B` opens Herdr-action mode, its supported chords affect only the
   selected qualified session, and `Ctrl+]` remains Super-Herdr's prefix.
4. Normal clicks reach mouse-aware programs; dragging selects locally.
5. Edge dragging continues multipage selection without further pointer movement.
6. Selection excludes sidebar, borders, and terminal padding.
7. Paste a long multiline prompt with the terminal's normal paste command and
   again with `Ctrl+] v`; each must appear as one editable paste and must not be
   submitted as multiple messages. PNG upload must verify size and SHA-256.
8. Adding, editing, or removing a target refreshes the TUI without restarting Herdr.
9. Starting or stopping a Herdr session is reflected by registry refresh.
10. The agent navigator filters and jumps across at least two hosts.
11. With more workspace rows than fit onscreen, sidebar wheel scrolling reaches
    every row, clicks select the displayed item, and keyboard selection plus
    terminal resizing keep the selected workspace visible.
12. The action palette fuzzy-searches across at least two qualified sessions;
    create and rename affect only the displayed session, and workspace, tab, and
    pane closure require confirmation showing the exact qualified resource.
13. Blocked, waiting, and input-ready agents from every live host appear in the
    lower attention pane; its waiting and history rows scroll independently from
    the upper navigation pane, clicking a qualified row jumps to its pane, and
    `Ctrl+] a` opens the navigator on the attention filter.
14. Change an agent from working to waiting and verify exactly one unread event;
    restart Super-Herdr, open `Ctrl+] e`, jump to the qualified pane, and verify
    that the event remains read after another restart. Disconnecting a target
    must not create an agent-disappeared event.
15. Right-click a session, workspace, tab, and pane across at least two targets.
    Verify each menu is anchored to the resource under the pointer,
    non-destructive actions affect only that qualified resource, and every close
    action displays the exact qualified resource in its confirmation before
    execution.

Record the operating-system version, terminal emulator, display protocol, Herdr
version/protocol, and Super-Herdr commit for each qualification run. Never put
real hostnames, addresses, usernames, credentials, terminal contents, or
clipboard payloads in committed test records.
