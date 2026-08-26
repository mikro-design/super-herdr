//! The transport shared by Super-Herdr daemons and the standalone public bridge.
//!
//! The daemon never accepts an Internet connection. It keeps the ordinary web
//! listener on loopback and opens one WebSocket to the public bridge. Browser
//! HTTP connections travel over that socket as bounded opaque chunks, then
//! terminate at the existing loopback listener. The bridge does not interpret
//! daemon protocol messages and never logs the bytes it relays.
//!
//! TLS terminates at the bridge, so this is a trusted relay, not end-to-end
//! encryption. Device pairing authenticates every browser request, including
//! one made directly to the loopback listener.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io;
use std::io::Read as _;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, header};

/// The rendezvous used by an installation that has no explicit direct route.
pub const DEFAULT_BRIDGE_URL: &str = "https://super-herdr.key-value.co";

const HOST_PATH: &str = "/_bridge/host/";
const HEALTH_PATH: &str = "/_bridge/health";
const DEVICE_PATH: &str = "/r/";
const ROUTE_CHARACTERS: usize = 32;
const SECRET_CHARACTERS: usize = 64;
const MAX_HTTP_HEAD_BYTES: usize = 16 * 1024;
const MAX_TUNNEL_CHUNK_BYTES: usize = 32 * 1024;
const MAX_PAIR_BODY_BYTES: usize = 1024;
const MAX_PAIR_RESPONSE_BYTES: usize = 16 * 1024;
const PAIR_APPROVAL_TIMEOUT: Duration = Duration::from_secs(70);
const PACKET_QUEUE_DEPTH: usize = 32;
const MAX_ROUTES: usize = 4096;
const MAX_DEVICES_PER_ROUTE: usize = 16;
const MAX_PAIR_SOURCES: usize = 4096;
const MAX_PAIR_ATTEMPTS_PER_SOURCE: u32 = 10;
const PAIR_ATTEMPT_WINDOW: Duration = Duration::from_secs(10 * 60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);

const OPEN: u8 = 1;
const DEVICE_DATA: u8 = 2;
const DEVICE_CLOSED: u8 = 3;
const HOST_DATA: u8 = 4;
const HOST_CLOSED: u8 = 5;
const PAIRING_CODE: u8 = 6;
const PACKET_HEAD_BYTES: usize = 9;

/// Pairing-code shape shared by the daemon that issues a code and the bridge
/// that temporarily indexes it. The alphabet remains the daemon's policy.
pub const PAIRING_CODE_CHARACTERS: usize = 8;
pub const PAIRING_CODE_LIFETIME: Duration = Duration::from_secs(300);

const LOGIN_PAGE: &str = include_str!("bridge.html");

type PacketSender = mpsc::Sender<Packet>;
type PacketReceiver = mpsc::Receiver<Packet>;
type DeviceSenders = Arc<Mutex<BTreeMap<u64, PacketSender>>>;

/// Accept a pairing code however a person typed it. Kept in this transport
/// package so the daemon and independently deployed bridge cannot drift.
pub fn normalize_pairing_code(entered: &str) -> String {
    entered
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_uppercase())
        .take(PAIRING_CODE_CHARACTERS)
        .collect()
}

fn registration_token() -> Result<String> {
    let mut bytes = [0_u8; 32];
    File::open("/dev/urandom")
        .context("failed to open /dev/urandom for a bridge secret")?
        .read_exact(&mut bytes)
        .context("failed to read a bridge secret")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// One daemon's unguessable place on the bridge.
///
/// The route may appear in a browser URL. The registration secret never does:
/// it is carried in the WebSocket authorization header and redacted from Debug.
#[derive(Clone, PartialEq, Eq)]
pub struct Route {
    base_url: String,
    id: String,
    secret: String,
}

impl fmt::Debug for Route {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Route")
            .field("base_url", &self.base_url)
            .field("id", &self.id)
            .field("secret", &"[redacted]")
            .finish()
    }
}

impl Route {
    pub fn new(base_url: &str) -> Result<Self> {
        let base_url = validate_base_url(base_url)?;
        let route_token = registration_token()?;
        let id = route_token
            .get(..ROUTE_CHARACTERS)
            .context("failed to make a bridge route")?
            .to_owned();
        Ok(Self {
            base_url,
            id,
            secret: registration_token()?,
        })
    }

    #[cfg(test)]
    fn fixed(base_url: &str, id: &str, secret: &str) -> Result<Self> {
        let base_url = validate_base_url(base_url)?;
        ensure_route(id)?;
        ensure_secret(secret)?;
        Ok(Self {
            base_url,
            id: id.to_owned(),
            secret: secret.to_owned(),
        })
    }

    pub fn login_url(&self) -> String {
        self.base_url.clone()
    }

    fn socket_url(&self) -> String {
        let base = self
            .base_url
            .strip_prefix("https://")
            .map(|rest| format!("wss://{rest}"))
            .or_else(|| {
                self.base_url
                    .strip_prefix("http://")
                    .map(|rest| format!("ws://{rest}"))
            })
            .expect("a validated bridge URL has an HTTP scheme");
        format!("{base}{HOST_PATH}{}", self.id)
    }
}

/// Check a configured bridge before it can produce a convincing but unusable
/// pairing QR. Plain HTTP is accepted only for loopback integration tests and
/// local development; a remote bridge necessarily carries credentials and
/// terminal contents and therefore requires HTTPS.
pub fn validate_base_url(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    let (scheme, authority) = url
        .split_once("://")
        .context("a bridge URL needs a scheme, as in https://bridge.example")?;
    if authority.is_empty() || authority.contains(['/', '?', '#']) {
        bail!("a bridge URL must contain only a scheme and host");
    }
    if scheme == "https" {
        return Ok(url.to_owned());
    }
    if scheme == "http"
        && (authority.starts_with("127.0.0.1:")
            || authority == "127.0.0.1"
            || authority.starts_with("localhost:")
            || authority == "localhost")
    {
        return Ok(url.to_owned());
    }
    bail!("a remote bridge must use https")
}

#[derive(Debug)]
struct Packet {
    kind: u8,
    connection: u64,
    payload: Vec<u8>,
}

impl Packet {
    fn encode(self) -> Message {
        let mut bytes = Vec::with_capacity(PACKET_HEAD_BYTES + self.payload.len());
        bytes.push(self.kind);
        bytes.extend_from_slice(&self.connection.to_be_bytes());
        bytes.extend_from_slice(&self.payload);
        Message::Binary(bytes.into())
    }

    fn decode(message: Message) -> Option<Self> {
        let bytes = match message {
            Message::Binary(bytes) => bytes,
            _ => return None,
        };
        if bytes.len() < PACKET_HEAD_BYTES
            || bytes.len() > PACKET_HEAD_BYTES + MAX_TUNNEL_CHUNK_BYTES
        {
            return None;
        }
        let kind = bytes[0];
        let connection = u64::from_be_bytes(bytes.get(1..9)?.try_into().ok()?);
        let payload = bytes.get(PACKET_HEAD_BYTES..)?.to_vec();
        Some(Self {
            kind,
            connection,
            payload,
        })
    }
}

/// Keep one daemon connected to its bridge until the daemon task ends.
///
/// Each failed attempt is isolated and bounded. Losing this optional path does
/// not stop the daemon, its local listener, or any Herdr session.
pub fn spawn_connector(
    route: Route,
    local: SocketAddr,
    pairing_codes: watch::Receiver<Option<String>>,
) -> JoinHandle<()> {
    install_crypto_provider();
    tokio::spawn(async move {
        loop {
            let _ = connect_once(&route, local, pairing_codes.clone()).await;
            tokio::time::sleep(RECONNECT_DELAY).await;
        }
    })
}

/// Rustls intentionally leaves provider choice to an application whose
/// dependency graph does not enable exactly one provider feature. The bridge
/// is also linked into the desktop binary on Linux, so make the choice explicit
/// before the connector opens TLS instead of letting a background task panic.
#[cfg(target_os = "linux")]
fn install_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

#[cfg(target_os = "macos")]
fn install_crypto_provider() {}

async fn connect_once(
    route: &Route,
    local: SocketAddr,
    mut pairing_codes: watch::Receiver<Option<String>>,
) -> Result<()> {
    let mut request = route
        .socket_url()
        .into_client_request()
        .context("failed to prepare the bridge connection")?;
    let authorization = HeaderValue::from_str(&format!("Bearer {}", route.secret))
        .context("failed to prepare bridge authorization")?;
    request
        .headers_mut()
        .insert(header::AUTHORIZATION, authorization);
    let (socket, _) =
        tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request))
            .await
            .context("bridge connection timed out")??;
    let (mut socket_writer, mut socket_reader) = socket.split();
    let (outgoing, mut packets) = mpsc::channel::<Packet>(PACKET_QUEUE_DEPTH);
    let local_connections = Arc::new(Mutex::new(BTreeMap::<u64, PacketSender>::new()));

    let writing = tokio::spawn(async move {
        let initial_code = pairing_codes.borrow_and_update().clone();
        if let Some(code) = initial_code
            && socket_writer
                .send(
                    Packet {
                        kind: PAIRING_CODE,
                        connection: 0,
                        payload: code.into_bytes(),
                    }
                    .encode(),
                )
                .await
                .is_err()
        {
            return;
        }
        let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
        keepalive.tick().await;
        loop {
            let message = tokio::select! {
                packet = packets.recv() => match packet {
                    Some(packet) => packet.encode(),
                    None => break,
                },
                changed = pairing_codes.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    Packet {
                        kind: PAIRING_CODE,
                        connection: 0,
                        payload: {
                            let code = pairing_codes.borrow_and_update().clone();
                            code.unwrap_or_default().into_bytes()
                        },
                    }.encode()
                },
                _ = keepalive.tick() => Message::Ping(Vec::new().into()),
            };
            if socket_writer.send(message).await.is_err() {
                break;
            }
        }
    });

    while let Some(message) = socket_reader.next().await {
        let Ok(message) = message else {
            break;
        };
        let Some(packet) = Packet::decode(message) else {
            continue;
        };
        match packet.kind {
            OPEN => {
                let connection = packet.connection;
                let (sender, receiver) = mpsc::channel(PACKET_QUEUE_DEPTH);
                if let Ok(mut connections) = local_connections.lock() {
                    connections.insert(connection, sender);
                }
                let connections = local_connections.clone();
                let outgoing = outgoing.clone();
                tokio::spawn(async move {
                    proxy_to_local(local, packet, receiver, outgoing).await;
                    if let Ok(mut held) = connections.lock() {
                        held.remove(&connection);
                    }
                });
            }
            DEVICE_DATA | DEVICE_CLOSED => {
                let connection = packet.connection;
                let sender = local_connections
                    .lock()
                    .ok()
                    .and_then(|connections| connections.get(&connection).cloned());
                if sender.is_some_and(|sender| sender.try_send(packet).is_err()) {
                    if let Ok(mut connections) = local_connections.lock() {
                        connections.remove(&connection);
                    }
                    let _ = outgoing.try_send(Packet {
                        kind: HOST_CLOSED,
                        connection,
                        payload: Vec::new(),
                    });
                }
            }
            _ => {}
        }
    }

    if let Ok(mut connections) = local_connections.lock() {
        for (connection, sender) in std::mem::take(&mut *connections) {
            let _ = sender.try_send(Packet {
                kind: DEVICE_CLOSED,
                connection,
                payload: Vec::new(),
            });
        }
    }
    writing.abort();
    Ok(())
}

async fn proxy_to_local(
    local: SocketAddr,
    first: Packet,
    mut from_device: PacketReceiver,
    to_bridge: PacketSender,
) {
    let connection = first.connection;
    let stream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(local)).await {
        Ok(Ok(stream)) => stream,
        _ => {
            let _ = to_bridge.try_send(Packet {
                kind: HOST_CLOSED,
                connection,
                payload: Vec::new(),
            });
            return;
        }
    };
    let (mut reader, mut writer) = stream.into_split();
    if !matches!(
        tokio::time::timeout(IO_TIMEOUT, writer.write_all(&first.payload)).await,
        Ok(Ok(()))
    ) {
        let _ = to_bridge.try_send(Packet {
            kind: HOST_CLOSED,
            connection,
            payload: Vec::new(),
        });
        return;
    }
    let mut buffer = vec![0_u8; MAX_TUNNEL_CHUNK_BYTES];
    loop {
        tokio::select! {
            read = reader.read(&mut buffer) => match read {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if to_bridge.try_send(Packet {
                        kind: HOST_DATA,
                        connection,
                        payload: buffer[..read].to_vec(),
                    }).is_err() {
                        break;
                    }
                }
            },
            packet = from_device.recv() => match packet {
                Some(Packet { kind: DEVICE_DATA, payload, .. }) => {
                    if writer.write_all(&payload).await.is_err() {
                        break;
                    }
                }
                Some(Packet { kind: DEVICE_CLOSED, .. }) | None => {
                    let _ = writer.shutdown().await;
                    break;
                }
                _ => {}
            }
        }
    }
    let _ = to_bridge.try_send(Packet {
        kind: HOST_CLOSED,
        connection,
        payload: Vec::new(),
    });
}

#[derive(Clone)]
struct LiveRoute {
    secret_digest: [u8; 32],
    generation: u64,
    host: PacketSender,
    devices: DeviceSenders,
}

#[derive(Clone)]
struct CodeRoute {
    route: String,
    generation: u64,
    expires_at: Instant,
}

struct AttemptWindow {
    opened_at: Instant,
    attempts: u32,
}

#[derive(Default)]
struct RelayState {
    routes: Mutex<BTreeMap<String, LiveRoute>>,
    codes: Mutex<BTreeMap<String, Vec<CodeRoute>>>,
    pair_attempts: Mutex<BTreeMap<String, AttemptWindow>>,
    next_generation: AtomicU64,
    next_connection: AtomicU64,
}

impl RelayState {
    fn register(
        &self,
        route: &str,
        secret: &str,
        host: PacketSender,
    ) -> Result<(u64, DeviceSenders)> {
        ensure_route(route)?;
        ensure_secret(secret)?;
        let digest: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let mut routes = self
            .routes
            .lock()
            .map_err(|_| anyhow!("bridge route table is unavailable"))?;
        if let Some(existing) = routes.get(route)
            && !constant_time_eq(&existing.secret_digest, &digest)
        {
            bail!("bridge route is already registered");
        }
        if !routes.contains_key(route) && routes.len() >= MAX_ROUTES {
            bail!("bridge route capacity is exhausted");
        }
        let replaced_generation = routes.remove(route).map(|existing| {
            if let Ok(mut devices) = existing.devices.lock() {
                for (connection, device) in std::mem::take(&mut *devices) {
                    let _ = device.try_send(Packet {
                        kind: HOST_CLOSED,
                        connection,
                        payload: Vec::new(),
                    });
                }
            }
            existing.generation
        });
        let devices = Arc::new(Mutex::new(BTreeMap::new()));
        routes.insert(
            route.to_owned(),
            LiveRoute {
                secret_digest: digest,
                generation,
                host,
                devices: devices.clone(),
            },
        );
        drop(routes);
        if let Some(replaced_generation) = replaced_generation {
            self.remove_codes(route, replaced_generation);
        }
        Ok((generation, devices))
    }

    fn unregister(&self, route: &str, generation: u64) {
        let Ok(mut routes) = self.routes.lock() else {
            return;
        };
        if routes
            .get(route)
            .is_some_and(|live| live.generation == generation)
            && let Some(live) = routes.remove(route)
            && let Ok(mut devices) = live.devices.lock()
        {
            for (connection, device) in std::mem::take(&mut *devices) {
                let _ = device.try_send(Packet {
                    kind: HOST_CLOSED,
                    connection,
                    payload: Vec::new(),
                });
            }
        }
        drop(routes);
        self.remove_codes(route, generation);
    }

    fn route(&self, route: &str) -> Option<LiveRoute> {
        self.routes
            .lock()
            .ok()
            .and_then(|routes| routes.get(route).cloned())
    }

    fn register_code(&self, route: &str, generation: u64, offered: &[u8]) {
        let Ok(mut codes) = self.codes.lock() else {
            return;
        };
        let now = Instant::now();
        codes.retain(|_, routes| {
            routes.retain(|held| held.expires_at > now);
            !routes.is_empty()
        });
        for routes in codes.values_mut() {
            routes.retain(|held| held.route != route || held.generation != generation);
        }
        codes.retain(|_, routes| !routes.is_empty());
        if offered.is_empty() {
            return;
        }
        let Ok(offered) = std::str::from_utf8(offered) else {
            return;
        };
        let code = normalize_pairing_code(offered);
        if code.len() != PAIRING_CODE_CHARACTERS {
            return;
        }
        codes.entry(code).or_default().push(CodeRoute {
            route: route.to_owned(),
            generation,
            expires_at: now + PAIRING_CODE_LIFETIME,
        });
    }

    fn route_for_code(&self, offered: &str) -> Option<(String, LiveRoute)> {
        let code = normalize_pairing_code(offered);
        if code.len() != PAIRING_CODE_CHARACTERS {
            return None;
        }
        let held = {
            let mut codes = self.codes.lock().ok()?;
            let now = Instant::now();
            codes.retain(|_, routes| {
                routes.retain(|held| held.expires_at > now);
                !routes.is_empty()
            });
            let routes = codes.get(&code)?;
            // A collision is never guessed. Both people can ask for a new code;
            // the next publication removes the ambiguous old entry.
            (routes.len() == 1).then(|| routes[0].clone())?
        };
        let live = self
            .route(&held.route)
            .filter(|live| live.generation == held.generation)?;
        Some((held.route, live))
    }

    fn remove_codes(&self, route: &str, generation: u64) {
        let Ok(mut codes) = self.codes.lock() else {
            return;
        };
        for routes in codes.values_mut() {
            routes.retain(|held| held.route != route || held.generation != generation);
        }
        codes.retain(|_, routes| !routes.is_empty());
    }

    fn admit_pair_attempt(&self, source: &str) -> bool {
        let Ok(mut sources) = self.pair_attempts.lock() else {
            return false;
        };
        let now = Instant::now();
        sources.retain(|_, window| now.duration_since(window.opened_at) < PAIR_ATTEMPT_WINDOW);
        if !sources.contains_key(source) && sources.len() >= MAX_PAIR_SOURCES {
            return false;
        }
        let window = sources.entry(source.to_owned()).or_insert(AttemptWindow {
            opened_at: now,
            attempts: 0,
        });
        if window.attempts >= MAX_PAIR_ATTEMPTS_PER_SOURCE {
            return false;
        }
        window.attempts = window.attempts.saturating_add(1);
        true
    }
}

/// Serve the public relay on a plain HTTP socket. TLS belongs in the reverse
/// proxy in front of this process; the daemon connector always uses the public
/// HTTPS/WSS name.
pub async fn serve(address: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to bind bridge on {address}"))?;
    println!("super-herdr-bridge listening on {address}");
    serve_listener(listener).await
}

async fn serve_listener(listener: TcpListener) -> Result<()> {
    let state = Arc::new(RelayState::default());
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(_) => continue,
        };
        let state = state.clone();
        tokio::spawn(async move {
            let _ = handle_public_connection(stream, state).await;
        });
    }
}

async fn handle_public_connection(mut stream: TcpStream, state: Arc<RelayState>) -> Result<()> {
    let head = match tokio::time::timeout(IO_TIMEOUT, read_http_head(&mut stream)).await {
        Ok(Ok(Some(head))) => head,
        _ => return Ok(()),
    };
    let request = parse_request(&head)?;
    let request_method = request.method.to_owned();
    let request_path = request.path.to_owned();
    let request_length = request.length;
    if request_method == "GET" && matches!(request_path.as_str(), "/" | "/index.html") {
        return write_login_page(&mut stream).await;
    }
    if request_path == HEALTH_PATH {
        return write_health(&mut stream).await;
    }
    if request_method == "POST" && request_path == "/_bridge/pair" {
        if !state.admit_pair_attempt(&pairing_source(&head)) {
            return write_text_status(
                &mut stream,
                "429 Too Many Requests",
                "too many pairing attempts; wait before trying again",
            )
            .await;
        }
        return pair_device(stream, head, request_length, state).await;
    }
    if let Some(route) = request_path.strip_prefix(HOST_PATH) {
        let route = route
            .split(['/', '?'])
            .next()
            .unwrap_or_default()
            .to_owned();
        let secret = bearer_secret(&head).unwrap_or_default().to_owned();
        ensure_route(&route)?;
        ensure_secret(&secret)?;
        return serve_host_socket(stream, head, state, route, secret).await;
    }
    let Some((route, rewritten)) = rewrite_device_request(&head, &request_path) else {
        return write_unavailable(&mut stream, "404 Not Found").await;
    };
    let Some(live) = state.route(&route) else {
        return write_unavailable(&mut stream, "503 Service Unavailable").await;
    };
    let connection = state.next_connection.fetch_add(1, Ordering::Relaxed);
    let (to_device, mut from_host) = mpsc::channel(PACKET_QUEUE_DEPTH);
    let inserted = live.devices.lock().is_ok_and(|mut devices| {
        if devices.len() >= MAX_DEVICES_PER_ROUTE {
            return false;
        }
        devices.insert(connection, to_device);
        true
    });
    if !inserted {
        return write_unavailable(&mut stream, "503 Service Unavailable").await;
    }
    if live
        .host
        .try_send(Packet {
            kind: OPEN,
            connection,
            payload: rewritten,
        })
        .is_err()
    {
        if let Ok(mut devices) = live.devices.lock() {
            devices.remove(&connection);
        }
        return write_unavailable(&mut stream, "503 Service Unavailable").await;
    }

    let (mut reader, mut writer) = stream.into_split();
    let mut buffer = vec![0_u8; MAX_TUNNEL_CHUNK_BYTES];
    loop {
        tokio::select! {
            read = reader.read(&mut buffer) => match read {
                Ok(0) | Err(_) => {
                    let _ = live.host.try_send(Packet {
                        kind: DEVICE_CLOSED,
                        connection,
                        payload: Vec::new(),
                    });
                    break;
                }
                Ok(read) => {
                    if live.host.try_send(Packet {
                        kind: DEVICE_DATA,
                        connection,
                        payload: buffer[..read].to_vec(),
                    }).is_err() {
                        break;
                    }
                }
            },
            packet = from_host.recv() => match packet {
                Some(Packet { kind: HOST_DATA, payload, .. }) => {
                    if writer.write_all(&payload).await.is_err() {
                        break;
                    }
                }
                Some(Packet { kind: HOST_CLOSED, .. }) | None => break,
                _ => {}
            }
        }
    }
    let _ = live.host.try_send(Packet {
        kind: DEVICE_CLOSED,
        connection,
        payload: Vec::new(),
    });
    if let Ok(mut devices) = live.devices.lock() {
        devices.remove(&connection);
    }
    let _ = writer.shutdown().await;
    Ok(())
}

async fn pair_device(
    mut stream: TcpStream,
    head: Vec<u8>,
    length: usize,
    state: Arc<RelayState>,
) -> Result<()> {
    if length == 0 || length > MAX_PAIR_BODY_BYTES {
        return write_text_status(
            &mut stream,
            "400 Bad Request",
            "a small pairing request is required",
        )
        .await;
    }
    let mut body = vec![0_u8; length];
    if !matches!(
        tokio::time::timeout(IO_TIMEOUT, stream.read_exact(&mut body)).await,
        Ok(Ok(_))
    ) {
        return Ok(());
    }
    let code = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| value.get("code")?.as_str().map(str::to_owned));
    let Some(code) = code else {
        return write_text_status(
            &mut stream,
            "400 Bad Request",
            "the pairing code is unreadable",
        )
        .await;
    };
    let Some((route, live)) = state.route_for_code(&code) else {
        return write_text_status(
            &mut stream,
            "404 Not Found",
            "No running Super-Herdr published that code. Keep its pairing screen open, check all eight characters, and try again.",
        )
        .await;
    };

    let connection = state.next_connection.fetch_add(1, Ordering::Relaxed);
    let (to_device, mut from_host) = mpsc::channel(PACKET_QUEUE_DEPTH);
    let inserted = live.devices.lock().is_ok_and(|mut devices| {
        if devices.len() >= MAX_DEVICES_PER_ROUTE {
            return false;
        }
        devices.insert(connection, to_device);
        true
    });
    if !inserted {
        return write_text_status(
            &mut stream,
            "503 Service Unavailable",
            "that Super-Herdr is busy",
        )
        .await;
    }
    let mut request = match rewrite_request_target(&head, "/pair") {
        Some(request) => request,
        None => {
            if let Ok(mut devices) = live.devices.lock() {
                devices.remove(&connection);
            }
            return write_text_status(
                &mut stream,
                "400 Bad Request",
                "the pairing request is unreadable",
            )
            .await;
        }
    };
    request.extend_from_slice(&body);
    if request.len() > MAX_TUNNEL_CHUNK_BYTES
        || live
            .host
            .try_send(Packet {
                kind: OPEN,
                connection,
                payload: request,
            })
            .is_err()
    {
        if let Ok(mut devices) = live.devices.lock() {
            devices.remove(&connection);
        }
        return write_text_status(
            &mut stream,
            "503 Service Unavailable",
            "that Super-Herdr is offline",
        )
        .await;
    }

    let response = tokio::time::timeout(PAIR_APPROVAL_TIMEOUT, async {
        let mut response = Vec::new();
        while let Some(packet) = from_host.recv().await {
            match packet.kind {
                HOST_DATA => {
                    if response.len().saturating_add(packet.payload.len()) > MAX_PAIR_RESPONSE_BYTES
                    {
                        return None;
                    }
                    response.extend_from_slice(&packet.payload);
                }
                HOST_CLOSED => break,
                _ => {}
            }
        }
        Some(response)
    })
    .await
    .ok()
    .flatten();

    let _ = live.host.try_send(Packet {
        kind: DEVICE_CLOSED,
        connection,
        payload: Vec::new(),
    });
    if let Ok(mut devices) = live.devices.lock() {
        devices.remove(&connection);
    }
    let Some(response) = response.filter(|response| !response.is_empty()) else {
        return write_text_status(
            &mut stream,
            "503 Service Unavailable",
            "that Super-Herdr did not answer",
        )
        .await;
    };
    // An exact published code reached its daemon, which is the authority that
    // either spent it or explained why it no longer works. Do not keep a stale
    // rendezvous after that answer.
    state.remove_codes(&route, live.generation);
    let response = scope_pairing_response(&response, &route).unwrap_or(response);
    tokio::time::timeout(IO_TIMEOUT, stream.write_all(&response))
        .await
        .context("pairing response timed out")??;
    let _ = stream.shutdown().await;
    Ok(())
}

fn scope_pairing_response(response: &[u8], route: &str) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(response).ok()?;
    let (head, body) = text.split_once("\r\n\r\n")?;
    if !head
        .lines()
        .next()
        .is_some_and(|status| status.starts_with("HTTP/1.1 204 "))
    {
        return None;
    }
    let route_path = format!("{DEVICE_PATH}{route}");
    let mut scoped = String::new();
    for line in head.lines() {
        if line.to_ascii_lowercase().starts_with("set-cookie:") {
            let line = line.replacen("Path=/", &format!("Path={route_path}"), 1);
            scoped.push_str(&line);
            if !line.to_ascii_lowercase().contains("; secure") {
                scoped.push_str("; Secure");
            }
        } else {
            scoped.push_str(line);
        }
        scoped.push_str("\r\n");
    }
    scoped.push_str(&format!("x-super-herdr-route: {route_path}\r\n\r\n"));
    scoped.push_str(body);
    Some(scoped.into_bytes())
}

async fn serve_host_socket(
    stream: TcpStream,
    head: Vec<u8>,
    state: Arc<RelayState>,
    route: String,
    secret: String,
) -> Result<()> {
    let socket = tokio::time::timeout(
        IO_TIMEOUT,
        tokio_tungstenite::accept_async(PrefixedStream::new(head, stream)),
    )
    .await
    .context("bridge host WebSocket handshake timed out")??;
    let (mut writer, mut reader) = socket.split();
    let (to_host, mut packets) = mpsc::channel::<Packet>(PACKET_QUEUE_DEPTH);
    let (generation, _) = state.register(&route, &secret, to_host)?;
    let writing = tokio::spawn(async move {
        let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
        keepalive.tick().await;
        loop {
            let message = tokio::select! {
                packet = packets.recv() => match packet {
                    Some(packet) => packet.encode(),
                    None => break,
                },
                _ = keepalive.tick() => Message::Ping(Vec::new().into()),
            };
            if writer.send(message).await.is_err() {
                break;
            }
        }
    });
    while let Some(message) = reader.next().await {
        let Ok(message) = message else {
            break;
        };
        let Some(packet) = Packet::decode(message) else {
            continue;
        };
        if packet.kind == PAIRING_CODE {
            if state
                .route(&route)
                .is_some_and(|live| live.generation == generation)
            {
                state.register_code(&route, generation, &packet.payload);
            }
            continue;
        }
        if !matches!(packet.kind, HOST_DATA | HOST_CLOSED) {
            continue;
        }
        let device = state
            .route(&route)
            .filter(|live| live.generation == generation)
            .and_then(|live| {
                live.devices
                    .lock()
                    .ok()
                    .and_then(|devices| devices.get(&packet.connection).cloned())
            });
        if let Some(device) = device {
            let connection = packet.connection;
            if device.try_send(packet).is_err()
                && let Some(live) = state
                    .route(&route)
                    .filter(|live| live.generation == generation)
            {
                if let Ok(mut devices) = live.devices.lock() {
                    devices.remove(&connection);
                }
                let _ = live.host.try_send(Packet {
                    kind: DEVICE_CLOSED,
                    connection,
                    payload: Vec::new(),
                });
            }
        }
    }
    writing.abort();
    state.unregister(&route, generation);
    Ok(())
}

struct ParsedRequest<'a> {
    method: &'a str,
    path: &'a str,
    length: usize,
}

fn parse_request(head: &[u8]) -> Result<ParsedRequest<'_>> {
    let text = std::str::from_utf8(head).context("HTTP request head is not UTF-8")?;
    let mut fields = text.lines().next().unwrap_or_default().split_whitespace();
    let method = fields.next().context("HTTP request has no method")?;
    let path = fields.next().context("HTTP request has no path")?;
    let length = text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse().ok())
            .flatten()
    });
    Ok(ParsedRequest {
        method,
        path,
        length: length.unwrap_or_default(),
    })
}

fn rewrite_device_request(head: &[u8], path: &str) -> Option<(String, Vec<u8>)> {
    let rest = path.strip_prefix(DEVICE_PATH)?;
    let (route, suffix) = rest
        .split_once('/')
        .map_or((rest, ""), |(route, suffix)| (route, suffix));
    let route = route.split('?').next().unwrap_or_default();
    ensure_route(route).ok()?;
    let query = path.split_once('?').map(|(_, query)| query);
    let local_path = if suffix.is_empty() {
        "/".to_owned()
    } else {
        format!("/{suffix}")
    };
    let local_target = match query {
        Some(query) if !local_path.contains('?') => format!("{local_path}?{query}"),
        _ => local_path,
    };
    let rewritten = rewrite_request_target(head, &local_target)?;
    (rewritten.len() <= MAX_HTTP_HEAD_BYTES).then_some((route.to_owned(), rewritten))
}

fn rewrite_request_target(head: &[u8], local_target: &str) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(head).ok()?;
    let line_end = text.find('\n')? + 1;
    let first = text.get(..line_end)?;
    let mut fields = first.split_whitespace();
    let method = fields.next()?;
    let _old_target = fields.next()?;
    let version = fields.next()?;
    let mut rewritten = format!("{method} {local_target} {version}\r\n").into_bytes();
    rewritten.extend_from_slice(text.get(line_end..)?.as_bytes());
    let insertion = rewritten.len().checked_sub(2)?;
    rewritten.splice(
        insertion..insertion,
        b"X-Forwarded-Proto: https\r\n".iter().copied(),
    );
    (rewritten.len() <= MAX_HTTP_HEAD_BYTES).then_some(rewritten)
}

fn bearer_secret(head: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(head).ok()?;
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("authorization")
            .then(|| value.trim().strip_prefix("Bearer "))
            .flatten()
    })
}

fn pairing_source(head: &[u8]) -> String {
    let Some(text) = std::str::from_utf8(head).ok() else {
        return "direct".to_owned();
    };
    text.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if !name.eq_ignore_ascii_case("cf-connecting-ip") {
                return None;
            }
            value
                .trim()
                .parse::<std::net::IpAddr>()
                .ok()
                .map(|address| address.to_string())
        })
        .unwrap_or_else(|| "direct".to_owned())
}

async fn read_http_head(stream: &mut TcpStream) -> Result<Option<Vec<u8>>> {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while head.len() < MAX_HTTP_HEAD_BYTES {
        let read = stream.read(&mut byte).await?;
        if read == 0 {
            return Ok(None);
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
            return Ok(Some(head));
        }
    }
    Ok(None)
}

async fn write_unavailable(stream: &mut TcpStream, status: &str) -> Result<()> {
    let body = if status.starts_with("404") {
        "no such bridge route"
    } else {
        "that Super-Herdr bridge route is offline"
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    tokio::time::timeout(IO_TIMEOUT, stream.write_all(response.as_bytes()))
        .await
        .context("bridge unavailable response timed out")??;
    let _ = stream.shutdown().await;
    Ok(())
}

async fn write_text_status(stream: &mut TcpStream, status: &str, body: &str) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    tokio::time::timeout(IO_TIMEOUT, stream.write_all(response.as_bytes()))
        .await
        .context("bridge response timed out")??;
    let _ = stream.shutdown().await;
    Ok(())
}

async fn write_login_page(stream: &mut TcpStream) -> Result<()> {
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\ncache-control: no-store\r\nreferrer-policy: no-referrer\r\nx-content-type-options: nosniff\r\nconnection: close\r\n\r\n{LOGIN_PAGE}",
        LOGIN_PAGE.len()
    );
    tokio::time::timeout(IO_TIMEOUT, stream.write_all(response.as_bytes()))
        .await
        .context("bridge login page timed out")??;
    let _ = stream.shutdown().await;
    Ok(())
}

async fn write_health(stream: &mut TcpStream) -> Result<()> {
    tokio::time::timeout(
        IO_TIMEOUT,
        stream.write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: 2\r\ncache-control: no-store\r\nconnection: close\r\n\r\nok",
        ),
    )
    .await
    .context("bridge health response timed out")??;
    let _ = stream.shutdown().await;
    Ok(())
}

fn ensure_route(route: &str) -> Result<()> {
    if route.len() == ROUTE_CHARACTERS && route.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        bail!("invalid bridge route")
    }
}

fn ensure_secret(secret: &str) -> Result<()> {
    if secret.len() == SECRET_CHARACTERS && secret.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        bail!("invalid bridge authorization")
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            })
            == 0
}

/// A socket whose already-read HTTP head is replayed into a WebSocket
/// handshake before reads reach the underlying stream.
struct PrefixedStream {
    prefix: Vec<u8>,
    position: usize,
    stream: TcpStream,
}

impl PrefixedStream {
    fn new(prefix: Vec<u8>, stream: TcpStream) -> Self {
        Self {
            prefix,
            position: 0,
            stream,
        }
    }
}

impl AsyncRead for PrefixedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.position < self.prefix.len() {
            let available = &self.prefix[self.position..];
            let count = available.len().min(buffer.remaining());
            buffer.put_slice(&available[..count]);
            self.position += count;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

impl AsyncWrite for PrefixedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(context, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    #[cfg(target_os = "linux")]
    use super::install_crypto_provider;
    use super::{
        LOGIN_PAGE, MAX_PAIR_ATTEMPTS_PER_SOURCE, Packet, RelayState, Route, pairing_source,
        parse_request, read_http_head, rewrite_device_request, serve_listener, spawn_connector,
        validate_base_url,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::watch;
    use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};

    const ROUTE: &str = "0123456789abcdef0123456789abcdef";
    const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[cfg(target_os = "linux")]
    #[test]
    fn the_outbound_tls_connector_selects_a_crypto_provider() {
        install_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }

    #[test]
    fn route_names_are_public_but_registration_secrets_are_not_debugged() {
        let route = Route::fixed("https://super-herdr.key-value.co", ROUTE, SECRET).unwrap();
        assert_eq!(route.login_url(), "https://super-herdr.key-value.co");
        let debug = format!("{route:?}");
        assert!(debug.contains(ROUTE));
        assert!(!debug.contains(SECRET));
        assert!(debug.contains("[redacted]"));
    }

    #[test]
    fn a_remote_bridge_requires_https() {
        assert!(validate_base_url("https://super-herdr.key-value.co/").is_ok());
        assert!(validate_base_url("http://127.0.0.1:8789").is_ok());
        assert!(validate_base_url("http://localhost:8789").is_ok());
        assert!(validate_base_url("http://bridge.example").is_err());
        assert!(validate_base_url("https://bridge.example/path").is_err());
    }

    #[test]
    fn login_page_accepts_a_typed_code_without_putting_it_in_a_url() {
        assert!(LOGIN_PAGE.contains("fetch('/_bridge/pair'"));
        assert!(LOGIN_PAGE.contains("JSON.stringify({"));
        assert!(LOGIN_PAGE.contains("const enteredCode = ()"));
        assert!(LOGIN_PAGE.contains("code,"));
        assert_eq!(LOGIN_PAGE.matches("class=\"code-box\"").count(), 8);
        assert!(LOGIN_PAGE.contains("el('code').onpaste"));
        assert!(LOGIN_PAGE.contains("confirmation,"));
        assert!(LOGIN_PAGE.contains("Waiting for approval"));
        assert!(!LOGIN_PAGE.contains("location.hash"));
        assert!(!LOGIN_PAGE.contains("URLSearchParams"));
    }

    #[test]
    fn device_paths_are_stripped_and_marked_as_forwarded() {
        let head =
            format!("GET /r/{ROUTE}/events?session=one HTTP/1.1\r\nHost: bridge.example\r\n\r\n");
        let (route, rewritten) =
            rewrite_device_request(head.as_bytes(), &format!("/r/{ROUTE}/events?session=one"))
                .unwrap();
        let rewritten = String::from_utf8(rewritten).unwrap();
        assert_eq!(route, ROUTE);
        assert!(rewritten.starts_with("GET /events?session=one HTTP/1.1\r\n"));
        assert!(rewritten.contains("X-Forwarded-Proto: https\r\n"));
        assert!(!rewritten.contains(&format!("/r/{ROUTE}/events")));
    }

    #[test]
    fn packets_are_binary_bounded_and_round_trip() {
        let encoded = Packet {
            kind: 4,
            connection: 42,
            payload: b"opaque terminal bytes".to_vec(),
        }
        .encode();
        let decoded = Packet::decode(encoded).unwrap();
        assert_eq!(decoded.kind, 4);
        assert_eq!(decoded.connection, 42);
        assert_eq!(decoded.payload, b"opaque terminal bytes");
        assert!(Packet::decode(Message::Text(Utf8Bytes::from_static("not a packet"))).is_none());
    }

    #[test]
    fn concurrent_people_are_routed_by_code_and_collisions_are_refused() {
        let state = RelayState::default();
        let (first_host, _) = tokio::sync::mpsc::channel(1);
        let first_generation = state.register(ROUTE, SECRET, first_host).unwrap().0;
        state.register_code(ROUTE, first_generation, b"ABCD-2345");

        let second_route = "fedcba9876543210fedcba9876543210";
        let second_secret = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
        let (second_host, _) = tokio::sync::mpsc::channel(1);
        let second_generation = state
            .register(second_route, second_secret, second_host)
            .unwrap()
            .0;
        state.register_code(second_route, second_generation, b"WXYZ-6789");

        assert_eq!(state.route_for_code("abcd 2345").unwrap().0, ROUTE);
        assert_eq!(state.route_for_code("wxyz-6789").unwrap().0, second_route);

        state.register_code(second_route, second_generation, b"ABCD-2345");
        assert!(state.route_for_code("ABCD-2345").is_none());
    }

    #[test]
    fn pairing_guesses_are_bounded_per_cloudflare_source() {
        let state = RelayState::default();
        for _ in 0..MAX_PAIR_ATTEMPTS_PER_SOURCE {
            assert!(state.admit_pair_attempt("203.0.113.7"));
        }
        assert!(!state.admit_pair_attempt("203.0.113.7"));
        assert!(state.admit_pair_attempt("203.0.113.8"));

        let head = b"POST /_bridge/pair HTTP/1.1\r\nCF-Connecting-IP: 203.0.113.9\r\n\r\n";
        assert_eq!(pairing_source(head), "203.0.113.9");
        assert_eq!(pairing_source(b"POST / HTTP/1.1\r\n\r\n"), "direct");
    }

    #[tokio::test]
    async fn an_outbound_connector_carries_a_browser_request_to_loopback() {
        let bridge_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bridge_address = bridge_listener.local_addr().unwrap();
        let bridge = tokio::spawn(serve_listener(bridge_listener));

        let local_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_address = local_listener.local_addr().unwrap();
        let local = tokio::spawn(async move {
            let (mut stream, _) = local_listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("GET /session HTTP/1.1\r\n"));
            assert!(request.contains("X-Forwarded-Proto: https\r\n"));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 12\r\nconnection: close\r\n\r\nthrough here",
                )
                .await
                .unwrap();
        });

        let route = Route::fixed(&format!("http://{bridge_address}"), ROUTE, SECRET).unwrap();
        let (_pairing_code_sender, pairing_codes) = watch::channel(None);
        let connector = spawn_connector(route, local_address, pairing_codes);
        let response = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let mut browser = TcpStream::connect(bridge_address).await.unwrap();
                browser
                    .write_all(
                        format!(
                            "GET /r/{ROUTE}/session HTTP/1.1\r\nHost: bridge.example\r\nconnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                let mut response = Vec::new();
                browser.read_to_end(&mut response).await.unwrap();
                if response.windows(12).any(|part| part == b"through here") {
                    break response;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the connector never registered with the bridge");
        assert!(response.starts_with(b"HTTP/1.1 200 OK"));

        connector.abort();
        bridge.abort();
        local.await.unwrap();
    }

    #[tokio::test]
    async fn typing_a_published_code_pairs_once_and_scopes_the_device_cookie() {
        let bridge_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bridge_address = bridge_listener.local_addr().unwrap();
        let bridge = tokio::spawn(serve_listener(bridge_listener));

        let local_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_address = local_listener.local_addr().unwrap();
        let local = tokio::spawn(async move {
            let (mut stream, _) = local_listener.accept().await.unwrap();
            let head = read_http_head(&mut stream).await.unwrap().unwrap();
            let request = parse_request(&head).unwrap();
            assert_eq!(request.method, "POST");
            assert_eq!(request.path, "/pair");
            let mut body = vec![0_u8; request.length];
            stream.read_exact(&mut body).await.unwrap();
            let body = String::from_utf8(body).unwrap();
            assert!(body.contains("ABCD-2345"));
            stream
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nset-cookie: sh_device=0123456789abcdef; Path=/; Max-Age=60; HttpOnly; SameSite=Strict\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });

        let route = Route::fixed(&format!("http://{bridge_address}"), ROUTE, SECRET).unwrap();
        let (_codes, pairing_codes) = watch::channel(Some("ABCD-2345".to_owned()));
        let connector = spawn_connector(route, local_address, pairing_codes);
        let body = br#"{"code":"ABCD-2345","name":"phone"}"#;
        let response = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let mut browser = TcpStream::connect(bridge_address).await.unwrap();
                browser
                    .write_all(
                        format!(
                            "POST /_bridge/pair HTTP/1.1\r\nHost: bridge.example\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                browser.write_all(body).await.unwrap();
                let mut response = Vec::new();
                browser.read_to_end(&mut response).await.unwrap();
                if response.starts_with(b"HTTP/1.1 204 No Content") {
                    break response;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the code was never published at the bridge");
        let response = String::from_utf8(response).unwrap();
        assert!(response.contains(&format!("x-super-herdr-route: /r/{ROUTE}\r\n")));
        assert!(response.contains(&format!("Path=/r/{ROUTE};")));
        assert!(response.contains("; Secure\r\n"));
        assert!(!response.contains("ABCD-2345"));

        connector.abort();
        bridge.abort();
        local.await.unwrap();
    }
}
