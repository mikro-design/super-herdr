# Super-Herdr bridge

`super-herdr-bridge` is the separately deployed public rendezvous for
Super-Herdr browser clients. It is not installed on end-user machines and does
not talk to Herdr. Daemons connect outward over an authenticated WebSocket; the
bridge carries bounded HTTP chunks to their loopback browser listeners.

Build and run the loopback origin from the workspace root:

```sh
cargo build --release --locked --package super-herdr-bridge
./target/release/super-herdr-bridge --address 127.0.0.1:8789
```

`GET /_bridge/health` returns `ok`. TLS must terminate in front of this origin.
For the self-hosted deployment, a Cloudflare Tunnel publishes
`super-herdr.key-value.co` to `http://localhost:8789`; no inbound port is
opened. Run both processes as boot services and do not enable payload or header
logging.

The repository includes `deploy/super-herdr-bridge.service` and
`deploy/super-herdr-cloudflared.service` as hardened user services. Install the
built bridge and `cloudflared` under `~/.local/bin/`, put the tunnel config and
credentials under `~/.cloudflared/`, copy both units to
`~/.config/systemd/user/`, then enable them with
`systemctl --user enable --now super-herdr-bridge super-herdr-cloudflared`.

The bridge intentionally keeps its route and pairing-code registry in memory.
Run exactly one process. Multiple replicas require an explicit shared routing
design; placing this binary behind a random load balancer would split a daemon
WebSocket from the browser meant to reach it.

The fixed public page never receives a pairing code in its URL. A person types
the short code, the bridge bounds submissions per Cloudflare source, and a
correct code only routes a pending request to its daemon. The browser generates
a six-digit comparison number, and no device token exists until that same number
is explicitly approved in an already trusted Super-Herdr TUI. The bridge must
not log request bodies, authorization headers, codes, or tunneled payloads.
