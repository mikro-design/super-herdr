# Security policy

Super-Herdr controls terminal panes running with the user's operating-system
permissions. Treat an authentication, authorization, pairing, routing, input,
file-transfer, or data-disclosure defect as security-sensitive.

## Supported versions

Security fixes are released on the latest tagged version. Upgrade to the latest
release before reporting a defect that may already have been fixed. Browser
pairing releases older than 0.7.14 are not supported.

## Report a vulnerability

Use [GitHub private vulnerability reporting](https://github.com/mikro-design/super-herdr/security/advisories/new).
If that channel is unavailable, email `mikrodesign@proton.me` with the subject
`Super-Herdr security report`.

Do not open a public issue for a suspected vulnerability. Include the affected
version, operating system, connection path, expected security boundary, and a
minimal reproduction. Redact terminal contents, clipboard payloads, pairing
codes, device tokens, SSH material, hostnames, addresses, usernames, and other
credentials.

A proposed feature with its own written threat model, not yet implemented, is
the [remote development preview](REMOTE_PREVIEW.md).

## Trust boundaries

- A paired browser can observe terminal output and, after taking a control
  lease, type commands as the user running the selected pane.
- The hosted bridge is trusted infrastructure. HTTPS/WSS protects both network
  legs, but TLS terminates at the bridge; this is not end-to-end encryption
  against the bridge operator.
- Super-Herdr delegates SSH authentication, host-key checking, jump hosts, and
  routing to OpenSSH.
- Device pairing is for one operator's own devices. It is not team identity,
  delegated authorization, or an enterprise audit system.
- Enabling Web Push adds a new outbound trust boundary: whichever push
  service the browser's subscription names (a vendor choice, not this
  daemon's). Every push this daemon sends is empty — the push service learns
  that a device was woken and nothing else, never an agent, a pane, or any
  notification content.

More detail is in [ARCHITECTURE.md](ARCHITECTURE.md) and the
[security summary in the README](README.md#security-summary).
