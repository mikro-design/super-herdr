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
Target-manager coverage must include rendered mouse hit targets, validation
before file writes, stale asynchronous-result rejection, and exact
session/socket selection from a sanitized discovery result. CLI output coverage
must verify that redirected output has no ANSI escapes and that a downstream
consumer closing its pipe early exits cleanly without a panic.
Workspace-move coverage must prove that a nested exported layout is rebuilt
top-down, that each split targets the identifier Herdr returned for the
previously moved pane, that an unsupported layout node is rejected instead of
guessed, and that destinations outside the source session are never offered.
Recreation coverage must prove that an applied layout keeps the structure,
ratios, labels, and working directories while dropping pane identifiers,
commands, and environment, that a workspace exceeding the pane bound is refused,
that the source session is only read, and that recreation destinations are other
live sessions rather than workspaces.
Notification coverage must prove that click reporting is used only when the
desktop advertises both an action and a wait option, that only the one offered
action identifier counts as an activation, that a notification naming no pane can
never move the selection, and that the pane identifier stays out of the
notification text. Notification coverage must also prove delivery is disabled by
default, startup
history is skipped, event filters are honored, repeated events are deduplicated,
bursts are coalesced and rate-limited, and delivery objects exclude status,
terminal, and clipboard payloads. Configuration toggles must preserve comments
and existing filters, and notification-only refresh must not rebuild terminal
routes.
Linux release jobs must reject binaries whose highest required GLIBC symbol
version exceeds 2.28. Lint coverage must include test code and the macOS target,
because platform-gated code is invisible to a Linux-only lint. Release coverage must prove that every published file
appears in the signed `SHA256SUMS`, that `cosign verify-blob` accepts the
manifest for the tagged workflow identity, that `gh attestation verify` accepts
each archive and Debian package, and that the generated formula carries the
checksums of the archives that release actually published.

## Manual desktop matrix

| Frontend | Remote path | Text copy/paste | PNG paste | Mouse/select/scroll | Host/session refresh | Recorded |
| --- | --- | --- | --- | --- | --- | --- |
| macOS terminal | OpenSSH alias | Required | Required | Required | Required | No |
| Linux Wayland | OpenSSH alias | Required | Required | Required | Required | No |
| Linux X11 with `xclip` | OpenSSH alias | Required | Required | Required | Required | No |
| Linux X11 with `xsel` only | OpenSSH alias | Required | Not supported | Required | Required | No |
| Nested SSH or Herdr | terminal-mediated copy | Copy request only; local paste | Not supported | Required | Required | Command line only, 2026-08-19 |

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
8. Add, edit, and remove a target using only the target manager's mouse controls.
   Verify invalid and duplicate fields are rejected before save, **Test &
   discover** remains responsive while bounded by the configured timeout, and a
   discovered session can be selected exactly. Confirm the TUI refreshes without
   starting, stopping, or restarting Herdr and that failures expose no raw SSH
   diagnostics.
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
16. On a desktop, run `super-herdr notifications check`, enable notifications,
    send the synthetic test, and produce matching agent transitions. Verify old
    persisted history is not replayed, bursts are coalesced and rate-limited,
    notification text contains metadata only, and disabling takes effect without
    reconnecting terminal routes. Where `notifications check` reports click to
    jump as available, click a delivered notification and verify it selects the
    qualified pane, that a notification for a pane closed in the meantime
    reports it is no longer live, and that a burst is not delayed by leaving an
    earlier notification unclicked. A nested SSH or Herdr run must report native
    delivery as unavailable without affecting the TUI.

17. Build a workspace with several tabs and nested splits, run at least one
    long-running process and one agent in it, and move it into another workspace
    of the same session. Verify the destination reproduces every tab, split
    direction, and ratio, that no process restarted and scrollback survived, that
    the emptied source workspace disappears without a close confirmation, and
    that the selection follows Herdr's focus after the identifiers are
    re-qualified. Repeat over SSH and confirm one forwarding child serves the
    whole move. Verify no move destination is offered in another session or on
    another host.

18. Recreate that workspace on a second session, once on the same host and once
    on another host. Verify the destination reproduces every tab, split, and
    ratio with new shells in the recorded working directories, that no command
    from the source is re-run and no source environment appears there, that the
    source workspace still holds its running processes and scrollback, and that
    a working directory missing on the destination host reports a failure naming
    the destination workspace instead of leaving a silent gap.

Record the operating-system version, terminal emulator, display protocol, Herdr
version/protocol, and Super-Herdr commit for each qualification run. Never put
real hostnames, addresses, usernames, credentials, terminal contents, or
clipboard payloads in committed test records.

## Qualification records

`scripts/qualify-desktop.sh [config]` decides the part of the list above that a
machine can decide — items 1 and 2 in full, the delivery report in item 16, and
a bounded TUI start and quit — and prints a record block with the environment
already redacted. It sends only `Ctrl+] q`, which Super-Herdr intercepts, so no
keystroke reaches a pane and no running process is disturbed. Run it first on
each frontend; what it does not cover needs a person at that desktop, and a row
stays unrecorded until someone does it.

### 2026-08-19 — nested inside Herdr

- Host: Linux 6.8.0 x86_64
- Terminal: unknown (TERM=xterm-256color), display protocol: none
- Clipboard context: process nested inside Herdr; copy path: OSC 52 terminal
  request (not acknowledged)
- Clipboard tools present: xclip, notify-send
- Notifications: delivery unavailable (native notifications require Super-Herdr
  to run on the desktop); click to jump unavailable
- Herdr 0.8.0 protocol 19; 2 target(s), 1 reachable
- Super-Herdr 0.3.1 at commit 9a00be9
- Automated checks: 11 passed, 0 failed

Covered: item 1; item 2 in full, including failure isolation against a target
that was genuinely unreachable, colored `OK`/`FAIL` on a pty, no ANSI escapes
when redirected or under `--json` or `NO_COLOR=1`, and a clean exit when a
consumer closes the pipe early; and the item 16 claim that a nested run reports
native delivery as unavailable without affecting the TUI, which rendered its
targets, agents, and attention pane and quit on `Ctrl+] q` with exit code 0. A
probe before and after showed the observed session unchanged.

Outstanding for this row: items 3 through 15, 17, and 18. Each needs a person
at a desktop — dragging a selection, pasting with the terminal's own command,
right-clicking a resource, clicking a delivered notification — and none of them
is claimed here.

The macOS, Wayland, and X11 rows have no recorded run at all; this host has no
desktop session, so it cannot stand in for them.
