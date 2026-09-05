# Threat model: remote development preview

Required by `PRODUCT_BACKLOG.md` step 9 before any implementation branch is
opened. This is the design review's input, not a summary of a design that
exists: nothing described here is built.

## What the feature would be

A person running an agent on a remote host is often running a development
server beside it — `localhost:3000` on that host. Today, seeing it means an SSH
tunnel set up by hand, which a phone cannot do. The proposal is that a paired
device can ask Super-Herdr to show one such origin.

What it is not, and must never quietly become:

- A general-purpose proxy into a target's network.
- A browser. It renders one origin somebody named, not whatever that origin
  links to.
- An authenticated client. It carries no credentials, on purpose.

## Why this needs a threat model at all

A paired device already holds pane control. It can type `curl http://localhost:3000`
into a shell on the target and read the answer. So the obvious framing — "this
is SSRF, and SSRF is bad" — is not quite the risk, because the principal doing
it is already trusted to run commands.

The capability this adds is not *reaching* the origin. It is **rendering the
response in a browser that is already authenticated to the daemon.**

That is a different and much sharper problem, and it is the one an
implementation would get wrong by default. Super-Herdr authenticates a paired
browser with the `sh_device` cookie, set `HttpOnly; SameSite=Strict`. `HttpOnly`
stops script reading the token. It does not stop the browser *attaching* it: any
same-origin request a page makes carries it automatically, and `SameSite=Strict`
does nothing here because the request is same-site by definition.

So if preview content were served from the Super-Herdr origin — a path on the
daemon, a path under the bridge, anything sharing that cookie's scope — then a
development server that is hostile, compromised, or merely running somebody's
half-finished code could do this from a `<script>` tag:

```js
fetch('/command?session=...', { method: 'POST', body: JSON.stringify({
  type: 'pane.input', pane: {…}, bytes: '…'
}) })
```

It never reads the token. The browser supplies it. That is pane control on every
target in the federation, obtained by getting somebody to preview a page.

Everything below follows from that, and the first design constraint is the
consequence of it.

## Assets

1. **The device token.** Compromise is pane control on every configured target.
2. **The daemon's network position.** It holds live SSH connections to hosts a
   phone cannot otherwise reach.
3. **The targets' private networks.** Reachable from a target, not from the
   device.
4. **Terminal contents and transfer payloads**, which must not become reachable
   through a new path.

## Adversaries

- **A hostile or compromised development server** on a target the person
  legitimately uses. The most likely adversary, and the least dramatic: it is
  usually somebody's own dependency tree, not an attacker.
- **A malicious service elsewhere on the target's network**, reached because the
  preview was aimed at it.
- **A network observer** on the path between device and daemon.
- **A revoked device** whose grant has not been cleaned up.

Explicitly *not* an adversary: the person operating Super-Herdr. This is a
single-operator tool. A paired device is a trusted principal, and controls that
only defend against its operator would be theatre.

## Threats and controls

### T1. Preview content executes as the Super-Herdr origin

**Attack:** as above — hostile page issues authenticated same-origin requests.
**Severity:** total. Pane control on every target.

**Control.** Preview content must never be served in a browsing context that
shares the `sh_device` cookie's scope. Two candidate mechanisms:

- Render inside an `<iframe sandbox>` **without** `allow-same-origin`. The
  content gets an opaque origin, so same-origin requests to the daemon are not
  same-origin any more and the cookie is not attached. `allow-scripts` may be
  granted; `allow-scripts` together with `allow-same-origin` must never be,
  because that combination lets the frame remove its own sandbox.
- Serve preview responses from a distinct origin the cookie does not cover.
  Stronger, and harder: the hosted bridge gives each daemon a path, not a host,
  so a separate origin is not available on the default route.

The sandbox route is the only one that works on every route Super-Herdr
supports, so it is the one to design against. `sandbox` must also withhold
`allow-top-navigation` and `allow-popups`, or the frame can navigate the page
that holds the session out from under the person.

### T2. SSRF and private-network traversal

**Attack:** the preview is aimed at `169.254.169.254`, a database admin port, or
another host on the target's network.

**Control.** The grant names a **port from a configured allowlist, not a URL.**
The connection is made by asking SSH to forward to `127.0.0.1:<port>` on the
selected target. There is no hostname anywhere in the request path, so there is
nothing to resolve, nothing to rebind, and no way to express a different host.
This is why "configured loopback origins" in the backlog should be read as
configured *ports*: a URL-shaped grant would reintroduce the entire class.

DNS rebinding is out of scope by construction rather than by mitigation, which
is the only way it stays out of scope.

### T3. Redirects

**Attack:** the previewed server answers `302 Location: http://internal-admin/`,
and a following client walks straight out of the allowlist.

**Control.** Redirects are not followed. A `3xx` is reported to the person as a
redirect, with its status, and the body is discarded. Following one would make
every control in T2 advisory.

### T4. Credentials as a confused deputy

**Attack:** the daemon holds a cookie jar or forwards an `Authorization` header,
and a preview reaches something it should not because *the daemon* is
authenticated to it.

**Control.** No cookie jar. No credential store. No `Authorization`,
`Proxy-Authorization` or `Cookie` header is ever sent, and none accepted from
the client to be forwarded. `Set-Cookie` in a response is dropped rather than
stored or relayed. A development server that needs authentication is out of
scope for a first version; saying so is better than a design that half-supports
it.

### T5. Response size, time, and rate

**Attack:** an endless stream, a very large body, or a fast-changing page fills
the daemon's memory or the device's link.

**Control.** Bounded body (a low ceiling — this is a page, not a download),
bounded headers, bounded time to first byte and total duration, one connection
per grant, and a minimum interval between fetches. Exceeding any bound ends that
fetch and says so; it does not truncate silently, because a page truncated
without a word is a page somebody draws a conclusion from.

### T6. Content type

**Attack:** the response is not a page — it is a 4 GB tarball, or something the
browser sniffs into a different type than it claims.

**Control.** An allowlist of content types that may be rendered at all, the
declared type relayed verbatim, and `X-Content-Type-Options: nosniff` on the way
out. Anything outside the allowlist is described rather than rendered.

### T7. Preview grant becoming terminal access

**Attack:** the preview path shares state with the pane path — a lease, a route,
a subscription — and a preview grant becomes a way to reach a terminal.

**Control.** The preview holds no pane lease, sends no input, subscribes to no
pane, and names no pane in its grant. A grant is (device, target, port,
expiry) and nothing else. It is worth testing that a device holding only a
preview grant is refused on every terminal path, because this is the kind of
coupling that arrives later by accident.

### T8. Grant lifetime and revocation

**Attack:** a grant outlives the reason it was made — a revoked device, a
removed target, a person who has forgotten they granted it.

**Control.** Grants expire on a short clock and are never persisted. A grant
that survived a daemon restart would be an outstanding capability nobody knew
was outstanding, which is the same argument that keeps pairing codes in memory.
Revoking a device, removing a target, or restarting the daemon ends every grant
immediately, and revocation is checked at fetch time rather than only at grant
time.

### T9. Logging

**Attack:** the URL path carries a session token, the response carries personal
data, and both end up in a log or a support bundle.

**Control.** No URL path, query, header, or body is logged at any level. What
may be logged is the target, the port, the status class, and the byte count. The
existing rule — clipboard payloads and terminal contents are never logged —
extends here unchanged, and `super-herdr doctor` must not learn to report
preview URLs.

### T10. The relay sees the traffic

**Attack:** on the default hosted bridge, TLS terminates at the relay, so
preview content passes through a third party in cleartext at that hop.

**Control.** None available at this layer; this is the bridge's documented
property, not a new one. What is required is that the person is told, at the
moment they grant a preview, when the route in use is the hosted bridge. A
development server often holds more than a terminal does — source, fixtures,
customer-shaped test data — so the disclosure that is adequate for a terminal
should be repeated here rather than assumed to carry over.

## Residual risks, accepted

- **A hostile page can still exhaust the device's own browser** inside its
  sandbox. Bounded by the frame and by T5, not eliminated.
- **A person can aim the preview at something sensitive on purpose.** They can
  already `curl` it; the preview does not widen this, and controls aimed at the
  operator are theatre.
- **Sandbox escapes exist.** The design leans on the browser's sandbox for T1.
  A separate origin would be stronger, and is not available on the default
  route; if Super-Herdr ever serves preview content from its own origin, this
  document is wrong and the feature must be reconsidered.

## What must be true before an implementation branch opens

1. The grant names a **port from configuration**, never a URL or a host.
2. Preview content renders only in a sandboxed frame **without**
   `allow-same-origin`, and the code that builds that attribute is covered by a
   test that fails if `allow-same-origin` is ever added.
3. Redirects are not followed, and there is a test that proves it.
4. No credential header is sent or forwarded, and no cookie is stored.
5. Grants are in-memory, expiring, and revocation-checked at fetch time.
6. The preview path cannot reach any pane API, proven by a test that gives a
   client only a preview grant and asserts every terminal path refuses it.
7. Nothing logs a path, a header, or a body.

If any of these cannot be met, the right outcome is not to relax it. It is to
ship nothing here: everything this feature offers can be had today with an SSH
tunnel, by a person at a desktop, and "convenient on a phone" does not buy much
risk.

## Test plan

Deterministic, against a local server standing in for a target's:

- A page that fetches `/command` from inside the frame, asserting the request is
  unauthenticated (no `sh_device` attached) or blocked.
- A `302` to another port and to another host: neither is followed.
- A response with `Set-Cookie`: not stored, not relayed.
- A body over the ceiling, and a server that never finishes: both bounded, both
  reported.
- A content type outside the allowlist: described, not rendered.
- A revoked device, an expired grant, and a removed target: each refused at
  fetch time, not only at grant time.
- A client holding only a preview grant: refused on `pane.subscribe`,
  `pane.input`, `pane.take_control`, and every transfer path.
- The log output of a full preview session, asserted to contain no path, header
  or body.
