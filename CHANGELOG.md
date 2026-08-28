# Changelog

Tagged releases and their generated change lists are available on the
[GitHub releases page](https://github.com/mikro-design/super-herdr/releases).
The notes below retain upgrade and security information that should not be
inferred from commit titles alone.

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
