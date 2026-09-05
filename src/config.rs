use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub transport: TransportConfig,
    #[serde(default, skip_serializing_if = "NotificationsConfig::is_default")]
    pub notifications: NotificationsConfig,
    #[serde(default, skip_serializing_if = "TransferConfig::is_default")]
    pub transfers: TransferConfig,
    #[serde(default, skip_serializing_if = "WebConfig::is_default")]
    pub web: WebConfig,
    pub targets: Vec<Target>,
    /// Devices allowed to reach this daemon over a network. Empty means none,
    /// which is why a daemon with no paired device serves loopback only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<Device>,
    /// One-tap replies offered on a paired device. Absent means the built-in
    /// set; an explicit empty list means none, because turning them off has to
    /// be expressible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quick_replies: Option<Vec<QuickReply>>,
}

/// How many replies a control strip may offer.
///
/// They share one row on a phone. Past this the row wraps into something a
/// person scans rather than taps, which is the opposite of the point.
const MAX_QUICK_REPLIES: usize = 8;

/// How many places one target may offer to look in. A picker with a screenful
/// of roots is a hierarchy again, which is the thing it exists to avoid.
const MAX_TARGET_ROOTS: usize = 8;

/// How many groupings one target may carry. Past a handful a tag stops
/// narrowing anything, which is the only thing a tag is for.
const MAX_TARGET_TAGS: usize = 8;
const MAX_TAG_CHARACTERS: usize = 24;
const MAX_QUICK_REPLY_LABEL_CHARACTERS: usize = 24;
const MAX_QUICK_REPLY_SEND_BYTES: usize = 256;

/// One configured, one-tap reply.
///
/// Deliberately a *reply* and not a *choice*. Herdr's documented API reports an
/// agent's status and whether it is ready for input; it does not describe the
/// options a blocked agent is offering, so there is nothing to render a
/// semantic Yes/No/Approve button from. The alternative would be reading the
/// terminal and guessing, which is exactly the thing Super-Herdr refuses to do:
/// a button that types "y" because the screen looked like a yes/no prompt is a
/// keystroke sent on the strength of a pattern match.
///
/// So these are what a person decided they send often, written down in their
/// own configuration. Nothing here is inferred, and nothing here claims to know
/// what the agent asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuickReply {
    /// What the button says.
    pub label: String,
    /// The text it types. Control characters are refused: a reply is short
    /// text, and the escape, tab and interrupt keys are their own explicit
    /// controls rather than something a configuration can smuggle into a pane.
    pub send: String,
    /// Whether a carriage return follows, submitting the line. On by default,
    /// because a reply that has to be confirmed with a second tap is two taps.
    #[serde(default = "default_true")]
    pub submit: bool,
    /// Ask before sending. There is no structured metadata to learn that a
    /// response is destructive from, so the person who wrote the reply is the
    /// one who declares it.
    #[serde(default)]
    pub confirm: bool,
}

impl QuickReply {
    fn new(label: &str, send: &str) -> Self {
        Self {
            label: label.to_owned(),
            send: send.to_owned(),
            submit: true,
            confirm: false,
        }
    }
}

/// The replies offered when a configuration says nothing.
///
/// Short, common, and unambiguous to type by hand. They are a default rather
/// than a meaning: none of them is chosen by looking at what an agent asked.
pub fn default_quick_replies() -> Vec<QuickReply> {
    vec![
        QuickReply::new("Yes", "y"),
        QuickReply::new("No", "n"),
        QuickReply::new("Continue", "continue"),
        QuickReply::new("Retry", "retry"),
    ]
}

/// What the daemon will move on someone's behalf.
///
/// The ceiling is a statement about the target's disk rather than about this
/// process: a transfer is relayed as it arrives, so nothing here grows with the
/// file. It is configurable because the right answer belongs to whoever owns
/// the host being written to, and it is separate from the clipboard's own limit
/// because a screenshot and a file are not the same question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferConfig {
    #[serde(default = "default_max_transfer_bytes")]
    pub max_bytes: u64,
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            max_bytes: default_max_transfer_bytes(),
        }
    }
}

impl TransferConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            port: default_web_port(),
            address: None,
            url: None,
            bridge: true,
        }
    }
}

/// Zero is how somebody turns the browser client off, since removing the key
/// only restores the default.
fn default_web_port() -> Option<u16> {
    Some(crate::daemon::web::DEFAULT_WEB_PORT)
}

impl WebConfig {
    /// Whether this configuration names the hosted rendezvous rather than an
    /// operator-managed proxy.
    ///
    /// Early bridge instructions told people to write the fixed URL into
    /// `web.url`. Treating that exact reserved address as a generic proxy made
    /// the QR look right while skipping the outbound connector, so its codes
    /// could never exist at the bridge. Accept that spelling, including a
    /// trailing slash, as the hosted bridge it unambiguously names.
    fn hosted_bridge_requested(&self) -> bool {
        if self.address.is_some() {
            return false;
        }
        match self.url.as_deref() {
            Some(url) => url.trim().trim_end_matches('/') == crate::bridge::DEFAULT_BRIDGE_URL,
            None => self.bridge,
        }
    }

    fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// Where a device reaches a direct browser listener, given or worked out.
    ///
    /// Worked out, because asking somebody to write down a URL for a listener
    /// they have already located is a question with an answer the program is
    /// holding: bound to a private address on a known port, the URL is that
    /// address on that port and nothing else.
    ///
    /// A proxy route is resolved separately because its status, rather than its
    /// loopback bind, is what knows the outside URL.
    pub fn scannable_url(&self) -> Option<String> {
        self.scannable_url_from(own_private_address())
    }

    /// What to bind directly: the address given, or this machine's own if it
    /// has one. An explicit proxy URL with no address keeps this as loopback.
    ///
    /// Serving the browser client is only ever for another device, so binding
    /// somewhere no other device can reach makes the feature ornamental.
    pub fn bind_address(&self) -> Option<std::net::IpAddr> {
        self.bind_address_from(own_private_address())
    }

    /// The port actually served, with zero meaning none.
    pub fn served_port(&self) -> Option<u16> {
        self.port.filter(|port| *port != 0)
    }

    /// Split from the detection so the rule is assertable without a network.
    fn scannable_url_from(&self, detected: Option<std::net::IpAddr>) -> Option<String> {
        if let Some(url) = self.url.clone() {
            return Some(url);
        }
        let port = self.port.filter(|port| *port != 0)?;
        let address = self.address.or(detected)?;
        if address.is_loopback() || !crate::daemon::web::bindable(address) {
            return None;
        }
        Some(match address {
            std::net::IpAddr::V4(address) => format!("http://{address}:{port}"),
            std::net::IpAddr::V6(address) => format!("http://[{address}]:{port}"),
        })
    }

    fn bind_address_from(&self, detected: Option<IpAddr>) -> Option<IpAddr> {
        self.served_port()?;
        // An explicit outside URL normally names a proxy whose local side is
        // loopback. Binding a guessed mesh address as well would publish a
        // second, unrequested route around that proxy. An explicit address can
        // still choose otherwise.
        self.address
            .or_else(|| self.url.is_none().then_some(detected).flatten())
    }

    /// Resolve the listener and the URL a phone needs as one decision.
    ///
    /// The public bridge is the unconfigured path and keeps the listener on
    /// loopback. An explicit URL or address is an operator's direct route. If
    /// the bridge is disabled, a persisted Tailscale Serve route may state both
    /// the HTTPS name a device opens and the loopback target to bind. Nothing
    /// here creates or changes Serve or Funnel state.
    pub async fn resolve(&self) -> ResolvedWeb {
        let detected = own_private_address();
        if self.served_port().is_none() || self.address.is_some() {
            return self.resolve_from(detected, None);
        }
        if self.hosted_bridge_requested()
            && let Ok(route) = crate::bridge::Route::new(crate::bridge::DEFAULT_BRIDGE_URL)
        {
            return ResolvedWeb {
                port: self.served_port(),
                address: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                url: Some(route.login_url()),
                bridge: Some(route),
            };
        }
        if self.url.is_some() {
            return self.resolve_from(detected, None);
        }
        let status = tailscale_serve_status().await;
        self.resolve_from(detected, status.as_deref())
    }

    fn resolve_from(&self, detected: Option<IpAddr>, status: Option<&[u8]>) -> ResolvedWeb {
        if self.address.is_none()
            && self.url.is_none()
            && let (Some(port), Some(status)) = (self.served_port(), status)
            && let Some(route) = tailscale_web_route(status, port)
        {
            return ResolvedWeb {
                port: Some(route.bind.port()),
                address: Some(route.bind.ip()),
                url: Some(route.url),
                bridge: None,
            };
        }
        ResolvedWeb {
            port: self.served_port(),
            address: self.bind_address_from(detected),
            url: self.scannable_url_from(detected),
            bridge: None,
        }
    }
}

/// Both sides of the browser route, including an optional outbound connector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedWeb {
    pub port: Option<u16>,
    pub address: Option<IpAddr>,
    pub url: Option<String>,
    pub bridge: Option<crate::bridge::Route>,
}

const TAILSCALE_STATUS_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_TAILSCALE_STATUS_BYTES: usize = 1024 * 1024;

/// Ask only for persisted status. This never invokes `serve`, `funnel`, or a
/// command that changes Tailscale state, and every failure falls back to the
/// direct private-address route.
async fn tailscale_serve_status() -> Option<Vec<u8>> {
    let mut command = tokio::process::Command::new("tailscale");
    command
        .args(["serve", "status", "--json"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(TAILSCALE_STATUS_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    (output.status.success() && output.stdout.len() <= MAX_TAILSCALE_STATUS_BYTES)
        .then_some(output.stdout)
}

#[derive(Debug, Deserialize)]
struct TailscaleServeStatus {
    #[serde(rename = "TCP", default)]
    tcp: BTreeMap<String, TailscaleTcp>,
    #[serde(rename = "Web", default)]
    web: BTreeMap<String, TailscaleWeb>,
    #[serde(rename = "AllowFunnel", default)]
    allow_funnel: BTreeMap<String, bool>,
}

#[derive(Debug, Deserialize)]
struct TailscaleTcp {
    #[serde(rename = "HTTPS", default)]
    https: bool,
}

#[derive(Debug, Deserialize)]
struct TailscaleWeb {
    #[serde(rename = "Handlers", default)]
    handlers: BTreeMap<String, TailscaleHandler>,
}

#[derive(Debug, Deserialize)]
struct TailscaleHandler {
    #[serde(rename = "Proxy")]
    proxy: Option<String>,
}

struct TailscaleWebRoute {
    bind: SocketAddr,
    url: String,
    rank: u8,
}

/// Find one unambiguous root HTTPS proxy associated with Super-Herdr's port.
///
/// Matching either side covers both normal Tailscale forms: external 443 to a
/// service on 8790, and external 8790 to a different loopback port. A tie is
/// refused rather than choosing somebody else's service by map order.
fn tailscale_web_route(status: &[u8], preferred_port: u16) -> Option<TailscaleWebRoute> {
    let status: TailscaleServeStatus = serde_json::from_slice(status).ok()?;
    let mut candidates = status
        .web
        .iter()
        .filter_map(|(endpoint, web)| {
            // Serve stays inside the tailnet. Funnel is public Internet
            // exposure and must remain an explicit `[web].url` decision even
            // when somebody has already configured such a route in Tailscale.
            if status.allow_funnel.get(endpoint).copied().unwrap_or(false) {
                return None;
            }
            let (host, external_port) = tailscale_endpoint(endpoint)?;
            status
                .tcp
                .get(&external_port.to_string())?
                .https
                .then_some(())?;
            let bind = loopback_proxy(web.handlers.get("/")?.proxy.as_deref()?)?;
            let rank = if external_port == preferred_port {
                2
            } else if bind.port() == preferred_port {
                1
            } else {
                return None;
            };
            let authority = if external_port == 443 {
                host.to_owned()
            } else {
                format!("{host}:{external_port}")
            };
            Some(TailscaleWebRoute {
                bind,
                url: format!("https://{authority}"),
                rank,
            })
        })
        .collect::<Vec<_>>();
    let best = candidates.iter().map(|candidate| candidate.rank).max()?;
    candidates.retain(|candidate| candidate.rank == best);
    (candidates.len() == 1).then(|| candidates.remove(0))
}

fn tailscale_endpoint(endpoint: &str) -> Option<(&str, u16)> {
    let (host, port) = endpoint.rsplit_once(':')?;
    let lowercase = host.to_ascii_lowercase();
    (!host.is_empty()
        && lowercase.ends_with(".ts.net")
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')))
    .then_some((host, port.parse().ok()?))
}

fn loopback_proxy(proxy: &str) -> Option<SocketAddr> {
    let authority = proxy.strip_prefix("http://")?;
    if authority.contains(['/', '?', '#']) {
        return None;
    }
    if let Some(port) = authority.strip_prefix("localhost:") {
        return Some(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            port.parse().ok()?,
        ));
    }
    let address: SocketAddr = authority.parse().ok()?;
    address.ip().is_loopback().then_some(address)
}

/// This machine's own address on whatever private network it is on.
///
/// Asking somebody for their own IP is asking for something the machine knows.
/// A connected UDP socket answers it: connecting chooses a route and assigns a
/// local address, and no packet is ever sent — so this costs nothing and talks
/// to nobody. The mesh is probed first because an address that works from
/// another network beats one that only works on this one.
///
/// `None` on a machine with nothing but loopback, which is a machine no phone
/// can reach.
fn own_private_address() -> Option<std::net::IpAddr> {
    // Tailscale's own resolver address, and TEST-NET-1, which exists to be
    // routed nowhere. Neither is contacted.
    ["100.100.100.100:9", "192.0.2.1:9"]
        .into_iter()
        .find_map(address_toward)
}

fn address_toward(remote: &str) -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect(remote).ok()?;
    let address = socket.local_addr().ok()?.ip();
    (!address.is_loopback() && crate::daemon::web::bindable(address)).then_some(address)
}

/// One paired device.
///
/// The secret itself is never here: a configuration file that held working
/// credentials would make a backup of it a way in. Revoking is deleting the
/// entry, which cannot be undone by anything the device still holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    /// What a person calls it when deciding whether to revoke it.
    pub name: String,
    /// SHA-256 of the device's token, as lowercase hex.
    pub token_sha256: String,
    pub paired_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransportConfig {
    #[serde(default = "default_ssh_bin")]
    pub ssh_bin: String,
    #[serde(default = "default_true")]
    pub batch_mode: bool,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_seconds: u64,
    #[serde(default = "default_command_timeout")]
    pub command_timeout_seconds: u64,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            ssh_bin: default_ssh_bin(),
            batch_mode: true,
            connect_timeout_seconds: default_connect_timeout(),
            command_timeout_seconds: default_command_timeout(),
        }
    }
}

/// Where the browser client is served, and where a phone reaches it.
///
/// These lived only on `super-herdr daemon`, which meant the frontend — the way
/// the program is normally run — hosted a daemon that served no browser client
/// and knew no address. Pairing from there could only ever offer a code to be
/// typed into a client that was not running, and a QR needs an address, so
/// there was never one to show. The default is now the fixed public bridge;
/// this table holds the deliberate opt-outs and direct-route overrides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    /// Prefer this browser-client port.
    ///
    /// Served by default on loopback for the outbound public bridge. A direct
    /// listener binds it too. When the bridge is disabled, an automatically
    /// discovered Tailscale route may expose this port while forwarding to a
    /// different loopback port, which is then what Super-Herdr binds.
    /// `port = 0` serves nothing at all.
    #[serde(default = "default_web_port")]
    pub port: Option<u16>,
    /// What to bind for an explicit direct route. Setting it bypasses the
    /// bridge. A private or mesh address is accepted and a public one refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<std::net::IpAddr>,
    /// Where a device reaches an explicit operator-managed proxy. Setting it
    /// bypasses the hosted bridge. It is never derived from the bind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Use the hosted outbound bridge when no explicit direct URL or address
    /// is configured. Set false to use Tailscale Serve or a private address
    /// directly instead.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub bridge: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub needs_attention: bool,
    #[serde(default = "default_true")]
    pub completed: bool,
    #[serde(default = "default_true")]
    pub disappeared: bool,
    #[serde(default)]
    pub working: bool,
    #[serde(default)]
    pub status_changed: bool,
    /// Also deliver to paired devices. Off by default and separate from
    /// `enabled`, which turns on this machine's own desktop notifications: a
    /// person who wants alerts on their phone has not thereby asked for them
    /// on the laptop they are sitting at, or the other way round.
    #[serde(default)]
    pub devices: bool,
    #[serde(default = "default_notification_interval")]
    pub minimum_interval_seconds: u64,
    #[serde(default = "default_notification_timeout")]
    pub command_timeout_seconds: u64,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            needs_attention: true,
            completed: true,
            disappeared: true,
            working: false,
            status_changed: false,
            devices: false,
            minimum_interval_seconds: default_notification_interval(),
            command_timeout_seconds: default_notification_timeout(),
        }
    }
}

impl NotificationsConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<String>,
    /// Discover all Herdr sessions on this host at startup.
    #[serde(default)]
    pub discover_sessions: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Documented Herdr API socket. Enables event subscriptions when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
    /// Ordered client candidates. A protocol mismatch advances to the next one.
    #[serde(default = "default_herdr_bins")]
    pub herdr_bins: Vec<String>,
    /// Local groupings — work, home, lab, a pod name. Super-Herdr's own
    /// labels for filtering and nothing else: a tag never reaches a host and
    /// never takes part in identity, so two hosts tagged "work" are still two
    /// entirely separate targets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Directories the remote file picker may look inside on this host.
    ///
    /// Empty means the picker offers nothing for this target, which is the
    /// default: a browsing surface that starts at `/` is one nobody asked for.
    /// This bounds browsing and searching, and deliberately not reading — a
    /// paired device already holds pane control and can read anything its user
    /// can, so a root here is a place to look rather than a permission.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<String>,
}

impl Target {
    pub fn endpoint(&self) -> &str {
        self.ssh.as_deref().unwrap_or("local")
    }

    pub fn session_name(&self) -> &str {
        self.session.as_deref().unwrap_or("default")
    }

    pub fn candidate_bins(&self) -> impl Iterator<Item = &str> {
        self.herdr_bins.iter().map(String::as_str)
    }
}

impl Config {
    pub fn load(explicit_path: Option<&Path>) -> Result<(Self, PathBuf)> {
        let path = resolve_path(explicit_path)?;
        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let config =
            Self::parse(&text).with_context(|| format!("failed to parse {}", path.display()))?;
        Ok((config, path))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    pub fn add_target_file(explicit_path: Option<&Path>, target: Target) -> Result<PathBuf> {
        let path = resolve_path(explicit_path)?;
        let existing = match fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()));
            }
        };

        let contents = if let Some(mut text) = existing {
            let mut config = Self::parse(&text)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            config.targets.push(target.clone());
            config.validate()?;

            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push('\n');
            text.push_str(&toml::to_string_pretty(&TargetAppend {
                targets: vec![&target],
            })?);
            text
        } else {
            let config = Self {
                transport: TransportConfig::default(),
                notifications: NotificationsConfig::default(),
                transfers: TransferConfig::default(),
                web: Default::default(),
                targets: vec![target],
                devices: Vec::new(),
                quick_replies: None,
            };
            config.validate()?;
            toml::to_string_pretty(&config)?
        };

        Self::parse(&contents).context("generated configuration is invalid")?;
        write_private_atomic(&path, contents.as_bytes())?;
        Ok(path)
    }

    pub fn replace_target_file(
        explicit_path: Option<&Path>,
        existing_name: &str,
        replacement: Target,
    ) -> Result<PathBuf> {
        mutate_target_file(explicit_path, existing_name, Some(replacement))
    }

    pub fn remove_target_file(explicit_path: Option<&Path>, name: &str) -> Result<PathBuf> {
        mutate_target_file(explicit_path, name, None)
    }

    /// Record a paired device, keeping every existing line of the file.
    pub fn add_device_file(explicit_path: Option<&Path>, device: Device) -> Result<PathBuf> {
        let path = resolve_path(explicit_path)?;
        let mut text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let config =
            Self::parse(&text).with_context(|| format!("failed to parse {}", path.display()))?;
        if config
            .devices
            .iter()
            .any(|existing| existing.name == device.name)
        {
            anyhow::bail!("a device named {:?} is already paired", device.name);
        }
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push('\n');
        text.push_str(&toml::to_string_pretty(&DeviceAppend {
            devices: vec![&device],
        })?);
        Self::parse(&text).context("generated configuration is invalid")?;
        write_private_atomic(&path, text.as_bytes())?;
        Ok(path)
    }

    /// Revoke a device. What it holds stops working immediately, because the
    /// daemon has nothing left to compare it against.
    pub fn remove_device_file(explicit_path: Option<&Path>, name: &str) -> Result<PathBuf> {
        let path = resolve_path(explicit_path)?;
        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut config =
            Self::parse(&text).with_context(|| format!("failed to parse {}", path.display()))?;
        let before = config.devices.len();
        config.devices.retain(|device| device.name != name);
        if config.devices.len() == before {
            anyhow::bail!("no device named {name:?} is paired");
        }
        let updated = remove_device_block(&text, name);
        let parsed = Self::parse(&updated).context("generated configuration is invalid")?;
        anyhow::ensure!(
            parsed.devices.len() == config.devices.len(),
            "removing the device would have changed more than its own entry"
        );
        write_private_atomic(&path, updated.as_bytes())?;
        Ok(path)
    }

    pub fn set_notifications_enabled_file(
        explicit_path: Option<&Path>,
        enabled: bool,
    ) -> Result<PathBuf> {
        let path = resolve_path(explicit_path)?;
        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("failed to parse {}", path.display()))?;
        let updated = set_notifications_enabled(&text, enabled);
        Self::parse(&updated).context("generated configuration is invalid")?;
        write_private_atomic(&path, updated.as_bytes())?;
        Ok(path)
    }

    pub fn validate(&self) -> Result<()> {
        if self.targets.is_empty() {
            bail!("configuration must contain at least one [[targets]] entry");
        }
        if self.transport.ssh_bin.trim().is_empty() {
            bail!("transport.ssh_bin must not be empty");
        }
        if self.transport.connect_timeout_seconds == 0 {
            bail!("transport.connect_timeout_seconds must be greater than zero");
        }
        if self.transport.command_timeout_seconds == 0 {
            bail!("transport.command_timeout_seconds must be greater than zero");
        }
        // Checked here rather than where it is served, so a configuration file
        // that cannot work says so when it is read instead of when somebody
        // finally tries to pair a phone.
        if let Some(url) = self.web.url.as_deref() {
            crate::pairing::pairing_url(url)?;
        }
        if self.web.served_port().is_none()
            && (self.web.url.is_some() || self.web.address.is_some())
        {
            bail!("web.port = 0 serves no browser client for web.url or web.address to reach");
        }

        if self.notifications.minimum_interval_seconds == 0
            || self.notifications.minimum_interval_seconds > 3600
        {
            bail!("notifications.minimum_interval_seconds must be between 1 and 3600");
        }
        if self.notifications.command_timeout_seconds == 0
            || self.notifications.command_timeout_seconds > 30
        {
            bail!("notifications.command_timeout_seconds must be between 1 and 30");
        }
        if let Some(replies) = self.quick_replies.as_deref() {
            if replies.len() > MAX_QUICK_REPLIES {
                bail!("at most {MAX_QUICK_REPLIES} [[quick_replies]] entries are supported");
            }
            for reply in replies {
                let label = reply.label.trim();
                if label.is_empty()
                    || label.chars().count() > MAX_QUICK_REPLY_LABEL_CHARACTERS
                    || label.chars().any(char::is_control)
                {
                    bail!(
                        "a quick reply label must be 1 to {MAX_QUICK_REPLY_LABEL_CHARACTERS} \
                         characters and contain no control characters"
                    );
                }
                if reply.send.is_empty() || reply.send.len() > MAX_QUICK_REPLY_SEND_BYTES {
                    bail!(
                        "quick reply {label:?} must send 1 to {MAX_QUICK_REPLY_SEND_BYTES} bytes"
                    );
                }
                if reply.send.chars().any(char::is_control) {
                    bail!(
                        "quick reply {label:?} must not send control characters; Enter, Escape, \
                         Tab and Ctrl-C are separate terminal keys"
                    );
                }
            }
        }

        let mut names = HashSet::new();
        for target in &self.targets {
            if target.name.trim().is_empty()
                || target.name.contains('/')
                || target.name.chars().any(char::is_control)
            {
                bail!("target name must be non-empty and contain no '/' or control characters");
            }
            if !names.insert(target.name.as_str()) {
                bail!("duplicate target name {:?}", target.name);
            }
            if target.herdr_bins.is_empty()
                || target
                    .herdr_bins
                    .iter()
                    .any(|binary| binary.trim().is_empty())
            {
                bail!("target {:?} has an empty herdr_bins entry", target.name);
            }
            if target.tags.len() > MAX_TARGET_TAGS {
                bail!(
                    "target {:?} carries more than {MAX_TARGET_TAGS} tags",
                    target.name
                );
            }
            for tag in &target.tags {
                if tag.is_empty()
                    || tag.chars().count() > MAX_TAG_CHARACTERS
                    || tag.chars().any(|character| {
                        character.is_control() || character.is_whitespace() || character == ','
                    })
                {
                    bail!(
                        "target {:?} has a tag that is not 1 to {MAX_TAG_CHARACTERS} \
                         characters without spaces or commas",
                        target.name
                    );
                }
            }
            if target.roots.len() > MAX_TARGET_ROOTS {
                bail!(
                    "target {:?} declares more than {MAX_TARGET_ROOTS} roots",
                    target.name
                );
            }
            for root in &target.roots {
                if !root.starts_with('/')
                    || root.chars().any(char::is_control)
                    || root.split('/').any(|part| part == "..")
                {
                    bail!(
                        "target {:?} has a root that is not an absolute path without '..'",
                        target.name
                    );
                }
            }
            if let Some(destination) = &target.ssh
                && (destination.is_empty()
                    || destination.starts_with('-')
                    || destination.chars().any(char::is_whitespace)
                    || destination.chars().any(char::is_control))
            {
                bail!("target {:?} has an invalid SSH destination", target.name);
            }
            if target.session.as_ref().is_some_and(|session| {
                session.is_empty() || session.contains('/') || session.chars().any(char::is_control)
            }) {
                bail!("target {:?} has an invalid session name", target.name);
            }
            if let Some(socket) = &target.socket {
                if socket.is_empty()
                    || !Path::new(socket).is_absolute()
                    || socket.chars().any(char::is_control)
                {
                    bail!("target {:?} has an invalid socket path", target.name);
                }
                if target.ssh.is_some() && socket.contains(':') {
                    bail!(
                        "target {:?} has a socket path incompatible with SSH forwarding",
                        target.name
                    );
                }
            }
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct TargetAppend<'a> {
    targets: Vec<&'a Target>,
}

#[derive(Deserialize)]
struct OwnedTargetAppend {
    targets: Vec<Target>,
}

fn mutate_target_file(
    explicit_path: Option<&Path>,
    existing_name: &str,
    replacement: Option<Target>,
) -> Result<PathBuf> {
    let path = resolve_path(explicit_path)?;
    let text =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut config =
        Config::parse(&text).with_context(|| format!("failed to parse {}", path.display()))?;
    let index = config
        .targets
        .iter()
        .position(|target| target.name == existing_name)
        .with_context(|| format!("target {existing_name:?} is not configured"))?;

    match replacement.as_ref() {
        Some(target) => config.targets[index] = target.clone(),
        None if config.targets.len() == 1 => {
            bail!("cannot remove the final configured target");
        }
        None => {
            config.targets.remove(index);
        }
    }
    config.validate()?;

    let range = target_section(&text, existing_name)?;
    let replacement_text = replacement
        .as_ref()
        .map(|target| {
            toml::to_string_pretty(&TargetAppend {
                targets: vec![target],
            })
        })
        .transpose()?;
    let mut updated = String::with_capacity(
        text.len() - range.len() + replacement_text.as_ref().map_or(0, String::len),
    );
    updated.push_str(&text[..range.start]);
    if let Some(replacement_text) = replacement_text {
        updated.push_str(&replacement_text);
    }
    updated.push_str(&text[range.end..]);
    Config::parse(&updated).context("generated configuration is invalid")?;
    write_private_atomic(&path, updated.as_bytes())?;
    Ok(path)
}

fn target_section(text: &str, name: &str) -> Result<Range<usize>> {
    for range in target_sections(text) {
        let section = &text[range.clone()];
        let Ok(parsed) = toml::from_str::<OwnedTargetAppend>(section) else {
            continue;
        };
        if parsed.targets.len() == 1 && parsed.targets[0].name == name {
            return Ok(range);
        }
    }
    bail!("could not locate target {name:?} in the configuration text")
}

fn target_sections(text: &str) -> Vec<Range<usize>> {
    let mut headers = Vec::new();
    let mut offset = 0_usize;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            headers.push((offset, trimmed == "[[targets]]"));
        }
        offset += line.len();
    }
    headers
        .iter()
        .enumerate()
        .filter(|(_, (_, is_target))| *is_target)
        .map(|(index, (start, _))| {
            let end = headers
                .get(index + 1)
                .map(|(offset, _)| *offset)
                .unwrap_or(text.len());
            *start..end
        })
        .collect()
}

fn set_notifications_enabled(text: &str, enabled: bool) -> String {
    let value = if enabled { "true" } else { "false" };
    let headers = table_headers(text);
    if let Some((header_index, (start, _))) = headers
        .iter()
        .enumerate()
        .find(|(_, (_, header))| *header == "[notifications]")
    {
        let end = headers
            .get(header_index + 1)
            .map(|(offset, _)| *offset)
            .unwrap_or(text.len());
        let section = &text[*start..end];
        let mut offset = 0_usize;
        for line in section.split_inclusive('\n') {
            let trimmed = line.trim();
            if trimmed
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == "enabled")
            {
                let line_start = start + offset;
                let line_end = line_start + line.len();
                let (content, newline) = line
                    .strip_suffix('\n')
                    .map_or((line, ""), |content| (content, "\n"));
                let equals = content
                    .find('=')
                    .expect("an enabled assignment always contains '='");
                let right = &content[equals + 1..];
                let value_start = right
                    .find(|character: char| !character.is_whitespace())
                    .unwrap_or(right.len());
                let before_comment = right.find('#').unwrap_or(right.len());
                let value_end = right[..before_comment].trim_end().len();
                let mut updated = String::with_capacity(text.len());
                updated.push_str(&text[..line_start]);
                updated.push_str(&content[..equals + 1]);
                updated.push_str(&right[..value_start]);
                updated.push_str(value);
                updated.push_str(&right[value_end..]);
                updated.push_str(newline);
                updated.push_str(&text[line_end..]);
                return updated;
            }
            offset += line.len();
        }
        let header_end = text[*start..]
            .find('\n')
            .map(|offset| start + offset + 1)
            .unwrap_or(text.len());
        let mut updated = String::with_capacity(text.len() + 16);
        updated.push_str(&text[..header_end]);
        updated.push_str(&format!("enabled = {value}\n"));
        updated.push_str(&text[header_end..]);
        return updated;
    }

    let insert_at = headers
        .iter()
        .find(|(_, header)| *header == "[[targets]]")
        .map(|(offset, _)| *offset)
        .unwrap_or(text.len());
    let mut updated = String::with_capacity(text.len() + 42);
    updated.push_str(&text[..insert_at]);
    if insert_at > 0 && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&format!("[notifications]\nenabled = {value}\n\n"));
    updated.push_str(&text[insert_at..]);
    updated
}

fn table_headers(text: &str) -> Vec<(usize, &str)> {
    let mut headers = Vec::new();
    let mut offset = 0_usize;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            headers.push((offset, trimmed));
        }
        offset += line.len();
    }
    headers
}

#[derive(Serialize)]
struct DeviceAppend<'a> {
    devices: Vec<&'a Device>,
}

/// Drop one `[[devices]]` block, leaving every other line as it was.
///
/// Rewriting the file from the parsed model would silently discard comments a
/// person put there, so the text is edited instead and the result is parsed
/// back to prove only the intended entry left.
fn remove_device_block(text: &str, name: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut in_target_block = false;
    let mut dropping = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_target_block = trimmed == "[[devices]]";
            dropping = false;
        }
        if in_target_block
            && trimmed.starts_with("name")
            && let Some((_, value)) = trimmed.split_once('=')
        {
            {
                dropping = value.trim().trim_matches('"') == name;
                if dropping {
                    // Remove the header line that was already written.
                    while kept.ends_with('\n') {
                        kept.pop();
                    }
                    let header = kept.rfind("[[devices]]").unwrap_or(kept.len());
                    kept.truncate(header);
                }
            }
        }
        if dropping {
            continue;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    while kept.ends_with("\n\n") {
        kept.pop();
    }
    kept
}

fn write_private_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let directory = path
        .parent()
        .context("configuration path has no parent directory")?;
    let directory_exists = directory.exists();
    fs::create_dir_all(directory).context("failed to create the configuration directory")?;
    if !directory_exists {
        set_directory_permissions(directory)?;
    }
    let mut temporary = tempfile::Builder::new()
        .prefix(".config-")
        .tempfile_in(directory)
        .context("failed to create a temporary configuration file")?;
    set_file_permissions(temporary.path())?;
    temporary
        .write_all(contents)
        .context("failed to write configuration")?;
    temporary
        .as_file()
        .sync_all()
        .context("failed to synchronize configuration")?;
    temporary
        .persist(path)
        .context("failed to atomically replace configuration")?;
    Ok(())
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .context("failed to secure the configuration directory")
}

#[cfg(not(unix))]
fn set_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .context("failed to secure the configuration file")
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn resolve_path(explicit_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit_path {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("SUPER_HERDR_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    if let Some(root) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(root).join("super-herdr/config.toml"));
    }
    let home: OsString = std::env::var_os("HOME")
        .context("no --config, SUPER_HERDR_CONFIG, XDG_CONFIG_HOME, or HOME was set")?;
    Ok(PathBuf::from(home).join(".config/super-herdr/config.toml"))
}

fn default_ssh_bin() -> String {
    "ssh".to_owned()
}

fn default_herdr_bins() -> Vec<String> {
    vec!["herdr".to_owned()]
}

const fn default_true() -> bool {
    true
}

const fn is_true(value: &bool) -> bool {
    *value
}

const fn default_connect_timeout() -> u64 {
    10
}

const fn default_command_timeout() -> u64 {
    20
}

/// One gibibyte. Large enough that a build artifact or a core dump is a
/// transfer rather than a refusal, small enough that a mistyped length does not
/// fill a host's disk before anyone notices.
const fn default_max_transfer_bytes() -> u64 {
    1024 * 1024 * 1024
}

const fn default_notification_interval() -> u64 {
    5
}

const fn default_notification_timeout() -> u64 {
    5
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::net::{IpAddr, Ipv4Addr};

    use super::{Config, Device, ResolvedWeb, Target, WebConfig};

    #[test]
    fn quick_replies_default_to_a_short_built_in_set() {
        let config = Config::parse(
            r#"
[[targets]]
name = "one"
ssh = "host"
"#,
        )
        .unwrap();

        assert!(config.quick_replies.is_none());
        assert_eq!(
            super::default_quick_replies()
                .iter()
                .map(|reply| reply.label.as_str())
                .collect::<Vec<_>>(),
            ["Yes", "No", "Continue", "Retry"]
        );
    }

    #[test]
    fn quick_replies_can_be_configured_and_turned_off() {
        let configured = Config::parse(
            r#"
[[targets]]
name = "one"
ssh = "host"

[[quick_replies]]
label = "Approve"
send = "approve"
confirm = true

[[quick_replies]]
label = "Path"
send = "/srv/build"
submit = false
"#,
        )
        .unwrap();

        let replies = configured.quick_replies.as_deref().unwrap();
        assert_eq!(replies[0].label, "Approve");
        assert!(replies[0].submit, "submitting is the default");
        assert!(replies[0].confirm);
        assert!(!replies[1].submit);
        assert!(!replies[1].confirm, "confirming is not");

        // An empty list is how somebody turns them off. Absent means the
        // built-in set, so the two must stay distinguishable.
        let none = Config::parse(
            r#"
quick_replies = []

[[targets]]
name = "one"
ssh = "host"
"#,
        )
        .unwrap();
        assert_eq!(none.quick_replies.as_deref(), Some(&[][..]));
    }

    #[test]
    fn a_quick_reply_may_not_smuggle_control_characters_into_a_pane() {
        // TOML decodes the escape, so what reaches validation is a real
        // escape character — the thing a reply must never carry.
        let error = Config::parse(
            "\n[[targets]]\nname = \"one\"\nssh = \"host\"\n\n\
             [[quick_replies]]\nlabel = \"Escape\"\nsend = \"\\u001B[A\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("control characters"),
            "Escape, Tab and Ctrl-C are separate keys, not something a reply carries: {error}"
        );
    }

    #[test]
    fn a_quick_reply_is_bounded_in_label_and_payload() {
        let long_label = Config::parse(&format!(
            r#"
[[targets]]
name = "one"
ssh = "host"

[[quick_replies]]
label = "{}"
send = "y"
"#,
            "x".repeat(25)
        ));
        assert!(long_label.is_err());

        let empty_send = Config::parse(
            r#"
[[targets]]
name = "one"
ssh = "host"

[[quick_replies]]
label = "Nothing"
send = ""
"#,
        );
        assert!(empty_send.is_err());

        let too_many = Config::parse(&format!(
            "\n[[targets]]\nname = \"one\"\nssh = \"host\"\n{}",
            (0..9)
                .map(|index| format!("\n[[quick_replies]]\nlabel = \"r{index}\"\nsend = \"y\"\n"))
                .collect::<String>()
        ));
        assert!(too_many.is_err());
    }

    #[test]
    fn a_target_root_must_be_an_absolute_path_without_a_climb() {
        let good = Config::parse(
            r#"
[[targets]]
name = "one"
ssh = "host"
roots = ["/srv/build", "/home/example/work"]
"#,
        )
        .unwrap();
        assert_eq!(good.targets[0].roots.len(), 2);

        for bad in ["\"srv/build\"", "\"/srv/../etc\"", "\"\""] {
            assert!(
                Config::parse(&format!(
                    "\n[[targets]]\nname = \"one\"\nssh = \"host\"\nroots = [{bad}]\n"
                ))
                .is_err(),
                "a picker root must be somewhere it cannot climb out of: {bad}"
            );
        }
    }

    #[test]
    fn a_tag_is_a_short_word_without_spaces_or_commas() {
        let good = Config::parse(
            r#"
[[targets]]
name = "one"
ssh = "host"
tags = ["work", "lab"]
"#,
        )
        .unwrap();
        assert_eq!(good.targets[0].tags, ["work", "lab"]);

        for bad in ["\"two words\"", "\"with,comma\"", "\"\""] {
            assert!(
                Config::parse(&format!(
                    "\n[[targets]]\nname = \"one\"\nssh = \"host\"\ntags = [{bad}]\n"
                ))
                .is_err(),
                "a tag that cannot be typed as one word is not a grouping: {bad}"
            );
        }
    }

    #[test]
    fn a_target_offers_no_roots_by_default() {
        let config = Config::parse(
            r#"
[[targets]]
name = "one"
ssh = "host"
"#,
        )
        .unwrap();

        assert!(
            config.targets[0].roots.is_empty(),
            "a browsing surface that starts at / is one nobody asked for"
        );
    }

    #[test]
    fn a_transfer_ceiling_is_configurable_and_has_a_default() {
        // Absent is the common case, and it must not mean zero: a missing
        // section that refused every transfer would be a silent outage.
        let default = Config::parse(
            r#"
                [[targets]]
                name = "local"
            "#,
        )
        .unwrap();
        assert_eq!(default.transfers.max_bytes, 1024 * 1024 * 1024);

        let configured = Config::parse(
            r#"
                [transfers]
                max_bytes = 4096

                [[targets]]
                name = "local"
            "#,
        )
        .unwrap();
        assert_eq!(configured.transfers.max_bytes, 4096);

        // The documented example is parsed rather than assumed to be current:
        // a configuration file nobody reads back drifts from the code.
        let example = Config::parse(&fs::read_to_string("config.example.toml").unwrap()).unwrap();
        assert_eq!(example.transfers.max_bytes, 1024 * 1024 * 1024);
    }

    #[test]
    fn parses_local_and_ssh_targets() {
        let config = Config::parse(
            r#"
                [[targets]]
                name = "local"

                [[targets]]
                name = "remote"
                ssh = "host-alias"
                session = "dev"
                socket = "/srv/herdr/sessions/dev/herdr.sock"
            "#,
        )
        .unwrap();

        assert_eq!(config.targets[0].endpoint(), "local");
        assert_eq!(config.targets[1].endpoint(), "host-alias");
        assert!(!config.targets[0].discover_sessions);
        assert_eq!(config.targets[1].session_name(), "dev");
        assert_eq!(
            config.targets[1].socket.as_deref(),
            Some("/srv/herdr/sessions/dev/herdr.sock")
        );
        assert_eq!(
            config.targets[1].candidate_bins().collect::<Vec<_>>(),
            ["herdr"]
        );
    }

    #[test]
    fn enables_host_session_discovery_explicitly() {
        let config = Config::parse(
            r#"
                [[targets]]
                name = "host"
                discover_sessions = true
            "#,
        )
        .unwrap();

        assert!(config.targets[0].discover_sessions);
    }

    #[test]
    fn notifications_are_opt_in_and_validate_delivery_bounds() {
        let disabled = Config::parse(
            r#"
                [[targets]]
                name = "local"
            "#,
        )
        .unwrap();
        assert!(!disabled.notifications.enabled);
        assert!(disabled.notifications.needs_attention);

        let enabled = Config::parse(
            r#"
                [notifications]
                enabled = true
                minimum_interval_seconds = 7
                completed = false

                [[targets]]
                name = "local"
            "#,
        )
        .unwrap();
        assert!(enabled.notifications.enabled);
        assert_eq!(enabled.notifications.minimum_interval_seconds, 7);
        assert!(!enabled.notifications.completed);
        assert!(enabled.notifications.disappeared);

        let error = Config::parse(
            r#"
                [notifications]
                enabled = true
                minimum_interval_seconds = 0

                [[targets]]
                name = "local"
            "#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("minimum_interval_seconds"));
    }

    #[test]
    fn notification_toggle_preserves_comments_and_filter_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            r#"# retained operator note
[notifications]
completed = false

[[targets]]
name = "local"
"#,
        )
        .unwrap();

        Config::set_notifications_enabled_file(Some(&path), true).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# retained operator note\n"));
        assert!(text.contains("enabled = true"));
        assert!(text.contains("completed = false"));
        let config = Config::parse(&text).unwrap();
        assert!(config.notifications.enabled);
        assert!(!config.notifications.completed);

        fs::write(
            &path,
            text.replace("enabled = true", "enabled = true # retained switch note"),
        )
        .unwrap();
        Config::set_notifications_enabled_file(Some(&path), false).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("enabled = false # retained switch note"));
        assert!(!Config::parse(&text).unwrap().notifications.enabled);

        let inserted_path = directory.path().join("without-notifications.toml");
        Config::add_target_file(
            Some(&inserted_path),
            Target {
                name: "inserted".to_owned(),
                ssh: None,
                discover_sessions: false,
                session: None,
                socket: None,
                herdr_bins: vec!["herdr".to_owned()],
                roots: Vec::new(),
                tags: Vec::new(),
            },
        )
        .unwrap();
        Config::set_notifications_enabled_file(Some(&inserted_path), true).unwrap();
        let inserted = fs::read_to_string(inserted_path).unwrap();
        assert!(inserted.contains("[notifications]\nenabled = true"));
        assert!(Config::parse(&inserted).unwrap().notifications.enabled);
    }

    #[test]
    fn preserves_client_candidate_order() {
        let config = Config::parse(
            r#"
                [[targets]]
                name = "mixed"
                herdr_bins = ["herdr-0.8", "herdr-0.7"]
            "#,
        )
        .unwrap();

        assert_eq!(
            config.targets[0].candidate_bins().collect::<Vec<_>>(),
            ["herdr-0.8", "herdr-0.7"]
        );
    }

    #[test]
    fn rejects_duplicate_target_names() {
        let error = Config::parse(
            r#"
                [[targets]]
                name = "same"
                [[targets]]
                name = "same"
            "#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("duplicate target"));
    }

    /// The URL a phone needs is not a question worth asking somebody who is
    /// holding the machine it names. Given the address, the answer follows;
    /// given nothing, the machine's own address is the answer.
    #[test]
    fn a_scannable_url_is_worked_out_rather_than_asked_for() {
        use std::net::IpAddr;

        let web = |table: &str| {
            Config::parse(&format!(
                r#"
                {table}
                [[targets]]
                name = "development"
                ssh = "development-host"
            "#
            ))
            .map(|config| config.web)
        };
        let mesh: IpAddr = "100.101.102.103".parse().unwrap();

        // Nothing configured at all: the client is served, and the machine's
        // own address is what a phone is told to use.
        assert_eq!(
            web("").unwrap().scannable_url_from(Some(mesh)).as_deref(),
            Some("http://100.101.102.103:8790")
        );
        // A configured address wins over the detected one.
        assert_eq!(
            web("[web]\naddress = \"192.168.1.42\"")
                .unwrap()
                .scannable_url_from(Some(mesh))
                .as_deref(),
            Some("http://192.168.1.42:8790")
        );
        // v6 is bracketed, or it is not a URL.
        assert_eq!(
            web("[web]\naddress = \"fd00::1\"")
                .unwrap()
                .scannable_url_from(None)
                .as_deref(),
            Some("http://[fd00::1]:8790")
        );
        // An explicit URL wins over both: a proxy terminating TLS somewhere
        // else cannot be worked out from anything this machine knows.
        assert_eq!(
            web("[web]\nurl = \"https://host.tailnet.ts.net\"")
                .unwrap()
                .scannable_url_from(Some(mesh))
                .as_deref(),
            Some("https://host.tailnet.ts.net")
        );

        // Nothing to scan when nothing could reach it.
        assert_eq!(web("").unwrap().scannable_url_from(None), None);
        assert_eq!(
            web("[web]\naddress = \"127.0.0.1\"")
                .unwrap()
                .scannable_url_from(Some(mesh)),
            None
        );
        // Off is off: zero serves nothing, so there is nothing to point at.
        assert_eq!(
            web("[web]\nport = 0")
                .unwrap()
                .scannable_url_from(Some(mesh)),
            None
        );
        assert_eq!(web("[web]\nport = 0").unwrap().bind_address(), None);
    }

    #[tokio::test]
    async fn an_unconfigured_browser_uses_the_fixed_public_bridge() {
        let resolved = WebConfig::default().resolve().await;
        assert_eq!(resolved.port, Some(8790));
        assert_eq!(resolved.address, Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert_eq!(
            resolved.url.as_deref(),
            Some("https://super-herdr.key-value.co")
        );
        assert!(resolved.bridge.is_some());
    }

    #[tokio::test]
    async fn the_fixed_bridge_url_in_an_existing_config_still_opens_its_connector() {
        for configured in [
            "[web]\nurl = \"https://super-herdr.key-value.co\"",
            "[web]\nbridge = false\nurl = \"https://super-herdr.key-value.co/\"",
        ] {
            let web = Config::parse(&format!(
                "{configured}\n[[targets]]\nname = \"development\""
            ))
            .unwrap()
            .web;
            let resolved = web.resolve().await;
            assert_eq!(resolved.port, Some(8790));
            assert_eq!(resolved.address, Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
            assert_eq!(
                resolved.url.as_deref(),
                Some("https://super-herdr.key-value.co")
            );
            assert!(
                resolved.bridge.is_some(),
                "configuration was {configured:?}"
            );
        }
    }

    /// Tailscale already knows both halves of a reverse proxy. The outside
    /// port is not necessarily the port the local service binds, so deriving
    /// either half from the other is the exact bug this status avoids.
    #[test]
    fn an_existing_tailscale_route_supplies_its_external_url_and_loopback_target() {
        let web = WebConfig::default();
        let status = br#"
        {
          "TCP": {"8790": {"HTTPS": true}},
          "Web": {
            "desktop.tail123.ts.net:8790": {
              "Handlers": {"/": {"Proxy": "http://127.0.0.1:8795"}}
            }
          }
        }
        "#;

        assert_eq!(
            web.resolve_from(Some("100.101.102.103".parse().unwrap()), Some(status)),
            ResolvedWeb {
                port: Some(8795),
                address: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                url: Some("https://desktop.tail123.ts.net:8790".to_owned()),
                bridge: None,
            }
        );
    }

    #[test]
    fn a_standard_https_route_can_match_the_local_service_port() {
        let status = br#"
        {
          "TCP": {"443": {"HTTPS": true}},
          "Web": {
            "desktop.tail123.ts.net:443": {
              "Handlers": {"/": {"Proxy": "http://localhost:8790"}}
            }
          }
        }
        "#;

        assert_eq!(
            WebConfig::default().resolve_from(None, Some(status)),
            ResolvedWeb {
                port: Some(8790),
                address: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                url: Some("https://desktop.tail123.ts.net".to_owned()),
                bridge: None,
            }
        );
    }

    /// A map can contain many unrelated services. Port association narrows the
    /// candidates, and ambiguity then means fall back rather than expose the
    /// wrong service in a perfectly scannable QR.
    #[test]
    fn ambiguous_non_loopback_or_public_tailscale_routes_are_not_guessed() {
        let ambiguous = br#"
        {
          "TCP": {"8790": {"HTTPS": true}},
          "Web": {
            "one.tail123.ts.net:8790": {
              "Handlers": {"/": {"Proxy": "http://127.0.0.1:8795"}}
            },
            "two.tail123.ts.net:8790": {
              "Handlers": {"/": {"Proxy": "http://127.0.0.1:8796"}}
            }
          }
        }
        "#;
        let public_target = br#"
        {
          "TCP": {"8790": {"HTTPS": true}},
          "Web": {
            "one.tail123.ts.net:8790": {
              "Handlers": {"/": {"Proxy": "http://192.168.1.4:8795"}}
            }
          }
        }
        "#;
        let funnel = br#"
        {
          "TCP": {"8790": {"HTTPS": true}},
          "Web": {
            "one.tail123.ts.net:8790": {
              "Handlers": {"/": {"Proxy": "http://127.0.0.1:8795"}}
            }
          },
          "AllowFunnel": {"one.tail123.ts.net:8790": true}
        }
        "#;
        let mesh: IpAddr = "100.101.102.103".parse().unwrap();
        let direct = ResolvedWeb {
            port: Some(8790),
            address: Some(mesh),
            url: Some("http://100.101.102.103:8790".to_owned()),
            bridge: None,
        };

        assert_eq!(
            WebConfig::default().resolve_from(Some(mesh), Some(ambiguous)),
            direct
        );
        assert_eq!(
            WebConfig::default().resolve_from(Some(mesh), Some(public_target)),
            direct
        );
        assert_eq!(
            WebConfig::default().resolve_from(Some(mesh), Some(funnel)),
            direct
        );
    }

    #[test]
    fn an_explicit_external_url_keeps_the_listener_on_loopback() {
        let web = Config::parse(
            r#"
            [web]
            url = "https://desktop.tail123.ts.net"
            [[targets]]
            name = "development"
            "#,
        )
        .unwrap()
        .web;

        assert_eq!(
            web.resolve_from(Some("100.101.102.103".parse().unwrap()), None),
            ResolvedWeb {
                port: Some(8790),
                address: None,
                url: Some("https://desktop.tail123.ts.net".to_owned()),
                bridge: None,
            }
        );
    }

    /// Turning the client off and then naming an address for it is a
    /// contradiction worth refusing where it is written rather than where it
    /// disappoints somebody holding a phone.
    #[test]
    fn an_address_for_a_client_that_is_switched_off_is_refused() {
        let error = Config::parse(
            r#"
                [web]
                port = 0
                url = "https://host.tailnet.ts.net"
                [[targets]]
                name = "development"
                ssh = "development-host"
            "#,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("serves no browser client"), "{error}");
        // And a URL that could never work is still refused on sight.
        assert!(
            Config::parse(
                r#"
                [web]
                url = "http://example.com"
                [[targets]]
                name = "development"
                ssh = "development-host"
            "#
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_ssh_option_injection() {
        let error = Config::parse(
            r#"
                [[targets]]
                name = "bad"
                ssh = "-oProxyCommand=oops"
            "#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("invalid SSH destination"));
    }

    #[test]
    fn rejects_relative_event_socket_paths() {
        let error = Config::parse(
            r#"
                [[targets]]
                name = "bad"
                socket = "relative/herdr.sock"
            "#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("invalid socket path"));
    }

    #[test]
    fn rejects_ambiguous_qualified_names() {
        let error = Config::parse(
            r#"
                [[targets]]
                name = "bad/name"
            "#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("target name"));
    }

    #[test]
    fn atomically_creates_and_appends_targets_without_losing_comments() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let first = Target {
            name: "development".to_owned(),
            ssh: Some("development-host".to_owned()),
            discover_sessions: true,
            session: None,
            socket: None,
            herdr_bins: vec!["herdr".to_owned()],
            roots: Vec::new(),
            tags: Vec::new(),
        };
        Config::add_target_file(Some(&path), first).unwrap();

        let mut text = fs::read_to_string(&path).unwrap();
        text.insert_str(0, "# retained operator note\n");
        fs::write(&path, text).unwrap();

        let second = Target {
            name: "build".to_owned(),
            ssh: Some("build-host".to_owned()),
            discover_sessions: false,
            session: Some("toolchains".to_owned()),
            socket: None,
            herdr_bins: vec!["herdr".to_owned()],
            roots: Vec::new(),
            tags: Vec::new(),
        };
        Config::add_target_file(Some(&path), second).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# retained operator note\n"));
        let config = Config::parse(&text).unwrap();
        assert_eq!(config.targets.len(), 2);
        assert_eq!(config.targets[1].name, "build");
    }

    #[test]
    fn rejects_duplicate_target_add_without_changing_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let target = Target {
            name: "development".to_owned(),
            ssh: Some("development-host".to_owned()),
            discover_sessions: true,
            session: None,
            socket: None,
            herdr_bins: vec!["herdr".to_owned()],
            roots: Vec::new(),
            tags: Vec::new(),
        };
        Config::add_target_file(Some(&path), target.clone()).unwrap();
        let before = fs::read(&path).unwrap();

        let error = Config::add_target_file(Some(&path), target).unwrap_err();

        assert!(error.to_string().contains("duplicate target"));
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn replaces_and_removes_named_target_blocks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            r#"# top-level comment
[transport]
command_timeout_seconds = 7

[[targets]]
name = "development"
ssh = "old-host"

[[targets]]
name = "build"
ssh = "build-host"
"#,
        )
        .unwrap();
        let replacement = Target {
            name: "development".to_owned(),
            ssh: Some("new-host".to_owned()),
            discover_sessions: true,
            session: None,
            socket: None,
            herdr_bins: vec!["herdr".to_owned()],
            roots: Vec::new(),
            tags: Vec::new(),
        };

        Config::replace_target_file(Some(&path), "development", replacement).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# top-level comment\n"));
        let config = Config::parse(&text).unwrap();
        assert_eq!(config.transport.command_timeout_seconds, 7);
        assert_eq!(config.targets[0].endpoint(), "new-host");
        assert!(config.targets[0].discover_sessions);

        Config::remove_target_file(Some(&path), "build").unwrap();
        let config = Config::load(Some(&path)).unwrap().0;
        assert_eq!(config.targets.len(), 1);
        assert_eq!(config.targets[0].name, "development");
    }

    #[test]
    fn pairing_a_device_keeps_the_rest_of_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "# my hosts\n[[targets]]\nname = \"development\"\nssh = \"dev-host\"\n",
        )
        .unwrap();

        Config::add_device_file(
            Some(&path),
            Device {
                name: "phone".to_owned(),
                token_sha256: "aa".repeat(32),
                paired_at_ms: 1,
            },
        )
        .unwrap();
        Config::add_device_file(
            Some(&path),
            Device {
                name: "tablet".to_owned(),
                token_sha256: "bb".repeat(32),
                paired_at_ms: 2,
            },
        )
        .unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my hosts"), "comments survive");
        let config = Config::parse(&text).unwrap();
        assert_eq!(config.targets.len(), 1);
        assert_eq!(
            config
                .devices
                .iter()
                .map(|device| device.name.as_str())
                .collect::<Vec<_>>(),
            vec!["phone", "tablet"]
        );

        // Pairing twice under one name is refused rather than silently
        // shadowing the first, which would leave a credential nobody can see.
        assert!(
            Config::add_device_file(
                Some(&path),
                Device {
                    name: "phone".to_owned(),
                    token_sha256: "cc".repeat(32),
                    paired_at_ms: 3,
                }
            )
            .is_err()
        );
    }

    #[test]
    fn revoking_a_device_removes_only_that_device() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "[[targets]]\nname = \"development\"\nssh = \"dev-host\"\n",
        )
        .unwrap();
        for (name, digest) in [("phone", "aa"), ("tablet", "bb"), ("laptop", "cc")] {
            Config::add_device_file(
                Some(&path),
                Device {
                    name: name.to_owned(),
                    token_sha256: digest.repeat(32),
                    paired_at_ms: 1,
                },
            )
            .unwrap();
        }

        Config::remove_device_file(Some(&path), "tablet").unwrap();

        let config = Config::parse(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            config
                .devices
                .iter()
                .map(|device| device.name.as_str())
                .collect::<Vec<_>>(),
            vec!["phone", "laptop"]
        );
        assert_eq!(config.targets.len(), 1, "targets are untouched");
        // Revoking what is not paired says so rather than reporting success.
        assert!(Config::remove_device_file(Some(&path), "tablet").is_err());
    }

    #[test]
    fn refuses_to_remove_the_final_target() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        Config::add_target_file(
            Some(&path),
            Target {
                name: "only".to_owned(),
                ssh: None,
                discover_sessions: false,
                session: None,
                socket: None,
                herdr_bins: vec!["herdr".to_owned()],
                roots: Vec::new(),
                tags: Vec::new(),
            },
        )
        .unwrap();
        let before = fs::read(&path).unwrap();

        let error = Config::remove_target_file(Some(&path), "only").unwrap_err();

        assert!(error.to_string().contains("final configured target"));
        assert_eq!(fs::read(path).unwrap(), before);
    }
}
