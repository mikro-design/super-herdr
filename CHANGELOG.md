# Changelog

Tagged releases and their generated change lists are available on the
[GitHub releases page](https://github.com/mikro-design/super-herdr/releases).
The notes below retain upgrade and security information that should not be
inferred from commit titles alone.

## 0.7.21

- Fixed file transfers to and from hosts without GNU coreutils. Every transfer
  script hashed with `sha256sum`, which a stock macOS does not have, so
  transfers failed there with an error about a shell script rather than about a
  missing program. The scripts now try `sha256sum`, `shasum -a 256`, then
  `openssl`.
- Added an agent inbox: a daemon-owned projection of agents into needs-you,
  working and recent, keyed by qualified agent identity and re-resolved against
  the live federation before anything is sent. The browser opens on it with the
  full hierarchy one tap away, and agents can be pinned, muted or snoozed.
- Agent identity now prefers the agent session Herdr reports where one is
  present, so an agent that moves pane keeps its card, its place in the queue
  and any pin. The field is optional and absent at protocol 19, so panes remain
  the fallback.
- Browser quick replies are configured rather than inferred. Herdr's documented
  API does not describe what a blocked agent is asking, so semantic response
  buttons remain deliberately unavailable rather than guessed from terminal
  contents. Configure them with `[[quick_replies]]`.
- Added opt-in paired-device attention alerts under the desktop's existing
  filters, coalescing and rate limits, with per-agent needs-you-only, mute and
  snooze modes. Alerts carry bounded metadata only and reach a device while its
  page is running; waking a closed browser needs Web Push and is not built.
- Added file transfer surfaces that the protocol has carried since it was
  written but no client offered: browser fetch from a target with size, source
  and digest shown before any byte moves, a bounded remote file picker over
  configured roots, and desktop save and host-to-host transfer from the same
  right-click menus.
- Added `super-herdr doctor`, a read-only pass over configuration, targets, the
  daemon socket, the browser route, pairing, clipboard, notifications and
  transfer dependencies. It reports corrective commands without running them and
  redacts host names, destinations, paths and URLs so its output can be shared.
- Added `super-herdr plugins`, showing what is installed on every host and what
  differs. Plugins are matched by the source they were installed from rather
  than by the server-local id a host gave them. Produces a pinned lockfile and
  an install plan; it runs nothing.
- Added local names, favourites and numbered jump slots that never rename a
  Herdr resource. A name is suspended rather than applied when the host calls
  the resource something else, because a reused id must not inherit a name.
- Added `super-herdr target import`, which lists the hosts in an OpenSSH
  configuration and adds only the aliases named explicitly, probing each on its
  own first. Targets can carry local tags.
- Documented the four ways a paired device can reach a daemon as peer choices,
  and added a threat model for the proposed remote development preview, which
  remains unimplemented.

## 0.7.20

- Prepared the repository for public contributions with a security policy,
  contribution guide, issue forms, pull-request checklist, and broader ignore
  rules for local credentials and editor state.
- Added a public-preview warning and quick start to the README, moved upgrade
  history into this changelog, and removed personal machine identifiers from
  test fixtures.
- Updated release examples and the roadmap to match the deployed hosted bridge.

## 0.7.19

- Replaced the three-row phone terminal-key grid with visible one-tap replies
  and one scrolling key rail.
- Kept the file picker pinned, returned the reclaimed height to the terminal,
  and opened the first waiting attention batch without another tap.

## 0.7.18

- Added a bounded 32 MiB browser file picker for PDF, DOC/DOCX, PPT/PPTX, and
  arbitrary files.
- Verified the remote byte count and SHA-256 digest, retained ordinary
  filenames safely, and reported the daemon's exact refusal when transfer
  failed.

## 0.7.17

- Kept the exact selected pane through disconnects, reconnects, and pane
  disappearance until the user explicitly navigates elsewhere.
- Rebuilt the phone view around a readable, pannable terminal and one compact
  attention section.

## 0.7.16

- Reported a paired-device name collision before consuming the one-time code,
  allowing the browser to choose another name with the same code.

## 0.7.15

- Selected the Rustls crypto provider before the hosted connector opens TLS.
- Recognized the reserved hosted address when explicitly configured and
  presented the pairing code as eight single-character boxes.

## 0.7.14

This is the minimum browser release recommended for secure pairing.

- Removed the pairing code from the scanned URL and required the user to type
  it into the fixed site.
- Added a fresh six-digit comparison number and required explicit approval in
  the trusted Super-Herdr TUI before creating a device.
- Required a paired device on every browser route, including loopback.

## 0.7.1

This release closed a reverse-proxy authentication problem in 0.7.0. Version
0.7.0 treated every connection arriving from loopback as local. A reverse proxy
that terminated TLS and forwarded to loopback therefore let remote visitors
bypass pairing.

If version 0.7.0 was served through a reverse proxy, assume every device that
could reach that proxy had the same access as a paired device. Those visitors
hold no token that `device remove` can revoke. A daemon reached through an SSH
forward of its Unix socket, or bound to loopback with nothing in front of it,
was not affected.

Version 0.7.2 also fixed atomic paste for targets whose Herdr API socket was
supplied by session discovery rather than written directly in configuration.
