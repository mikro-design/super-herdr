use std::future::Future;
#[cfg(unix)]
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
#[cfg(unix)]
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::UnixStream;
#[cfg(unix)]
use tokio::process::Child;
use tokio::process::Command;
use tokio::time::timeout;
#[cfg(unix)]
use tokio::time::{Instant, sleep};

use crate::config::{Target, TransportConfig};

pub type SnapshotFuture<'a> =
    Pin<Box<dyn Future<Output = Result<TransportSnapshot, SnapshotError>> + Send + 'a>>;
pub type ChangeFuture<'a> = Pin<Box<dyn Future<Output = Result<(), SnapshotError>> + Send + 'a>>;
pub type OpenChangeStreamFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Box<dyn ChangeStream>, SnapshotError>> + Send + 'a>>;

pub trait ChangeStream: Send {
    fn next(&mut self) -> ChangeFuture<'_>;
}

pub trait SnapshotTransport: Send + Sync + 'static {
    fn snapshot<'a>(
        &'a self,
        target: &'a Target,
        config: &'a TransportConfig,
        command_timeout: Duration,
    ) -> SnapshotFuture<'a>;

    fn open_change_stream<'a>(
        &'a self,
        _target: &'a Target,
        _config: &'a TransportConfig,
        _connect_timeout: Duration,
        _pane_ids: &'a [String],
    ) -> OpenChangeStreamFuture<'a> {
        Box::pin(std::future::pending())
    }
}

#[derive(Debug, Clone)]
pub struct TransportSnapshot {
    pub snapshot: Value,
    pub herdr_bin: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DiscoveredSession {
    pub name: String,
    pub running: bool,
    pub session_dir: String,
    pub socket_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDiscovery {
    pub sessions: Vec<DiscoveredSession>,
    pub herdr_bin: String,
}

#[derive(Debug, Deserialize)]
struct SessionList {
    sessions: Vec<DiscoveredSession>,
}

/// Expand host targets through Herdr's documented `session list --json` command.
/// A discovery failure is isolated to that host by retaining its configured
/// fallback session target.
pub async fn expand_discovered_sessions(config: crate::config::Config) -> crate::config::Config {
    let transport = config.transport.clone();
    let notifications = config.notifications.clone();
    let transfers = config.transfers.clone();
    let web = config.web.clone();
    let devices = config.devices.clone();
    let quick_replies = config.quick_replies.clone();
    let originals = config.targets;
    let mut tasks = tokio::task::JoinSet::new();
    for (index, target) in originals.iter().cloned().enumerate() {
        let transport = transport.clone();
        tasks.spawn(async move {
            let expanded = expand_discovered_target(target, &transport).await;
            (index, expanded)
        });
    }
    let mut per_target = vec![None; originals.len()];
    while let Some(result) = tasks.join_next().await {
        if let Ok((index, expanded)) = result {
            per_target[index] = Some(expanded);
        }
    }
    let expanded = per_target
        .into_iter()
        .enumerate()
        .flat_map(|(index, expanded)| expanded.unwrap_or_else(|| vec![originals[index].clone()]))
        .collect();
    crate::config::Config {
        transport,
        notifications,
        transfers,
        web,
        targets: expanded,
        devices,
        quick_replies,
    }
}

async fn expand_discovered_target(target: Target, transport: &TransportConfig) -> Vec<Target> {
    if !target.discover_sessions {
        return vec![target];
    }
    match discover_running_sessions(&target, transport).await {
        Ok(discovery) => targets_for_discovered_sessions(&target, discovery.sessions),
        _ => vec![target],
    }
}

/// Test access to a host and list its running Herdr sessions through the
/// documented CLI. The command is bounded by the configured timeout and never
/// starts, stops, or restarts a session.
pub async fn discover_running_sessions(
    target: &Target,
    transport: &TransportConfig,
) -> Result<SessionDiscovery, SnapshotError> {
    let command_timeout = Duration::from_secs(transport.command_timeout_seconds);
    match timeout(command_timeout, discover_sessions(target, transport)).await {
        Ok(result) => result,
        Err(_) => Err(SnapshotError::timed_out(command_timeout)),
    }
}

fn targets_for_discovered_sessions(
    target: &Target,
    sessions: Vec<DiscoveredSession>,
) -> Vec<Target> {
    sessions
        .into_iter()
        .map(|session| Target {
            name: target.name.clone(),
            ssh: target.ssh.clone(),
            discover_sessions: false,
            session: Some(session.name),
            socket: Some(session.socket_path),
            herdr_bins: target.herdr_bins.clone(),
            roots: target.roots.clone(),
        })
        .collect()
}

async fn discover_sessions(
    target: &Target,
    transport: &TransportConfig,
) -> Result<SessionDiscovery, SnapshotError> {
    let mut failures = Vec::new();
    for executable in target.candidate_bins() {
        match discover_sessions_once(target, transport, executable).await {
            Ok(sessions) => {
                return Ok(SessionDiscovery {
                    sessions,
                    herdr_bin: executable.to_owned(),
                });
            }
            Err(error) => failures.push(format!("{executable}: {error}")),
        }
    }
    Err(SnapshotError::unavailable(format!(
        "session discovery failed; {}",
        failures.join("; ")
    )))
}

async fn discover_sessions_once(
    target: &Target,
    transport: &TransportConfig,
    executable: &str,
) -> Result<Vec<DiscoveredSession>, &'static str> {
    let mut host = target.clone();
    host.session = None;
    host.socket = None;
    let mut command = build_herdr_command(
        &host,
        transport,
        executable,
        &["session".to_owned(), "list".to_owned(), "--json".to_owned()],
    );
    let output = command
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|_| "failed to start Herdr session discovery")?;
    if !output.status.success() {
        return Err("Herdr session discovery exited unsuccessfully (diagnostics redacted)");
    }
    if output.stdout.len() > 1024 * 1024 {
        return Err("Herdr session discovery exceeded the size limit");
    }
    let value = parse_last_json(&String::from_utf8_lossy(&output.stdout))?;
    let mut sessions = serde_json::from_value::<SessionList>(value)
        .map_err(|_| "Herdr session discovery returned an invalid response")?
        .sessions;
    sessions.retain(|session| session.running && valid_discovered_session(session));
    sessions.sort_by(|left, right| left.name.cmp(&right.name));
    sessions.dedup_by(|left, right| left.name == right.name);
    Ok(sessions)
}

fn valid_discovered_session(session: &DiscoveredSession) -> bool {
    !session.name.is_empty()
        && !session.name.contains('/')
        && !session.name.chars().any(char::is_control)
        && std::path::Path::new(&session.socket_path).is_absolute()
        && !session.socket_path.contains(':')
        && !session.socket_path.chars().any(char::is_control)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotErrorKind {
    Incompatible,
    Timeout,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotError {
    pub kind: SnapshotErrorKind,
    pub message: String,
}

impl SnapshotError {
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: SnapshotErrorKind::Unavailable,
            message: message.into(),
        }
    }

    pub fn incompatible(message: impl Into<String>) -> Self {
        Self {
            kind: SnapshotErrorKind::Incompatible,
            message: message.into(),
        }
    }

    pub fn timed_out(command_timeout: Duration) -> Self {
        Self {
            kind: SnapshotErrorKind::Timeout,
            message: format!("timed out after {} seconds", command_timeout.as_secs_f64()),
        }
    }
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SnapshotError {}

#[derive(Debug, Clone, Copy, Default)]
pub struct CliSnapshotTransport;

impl SnapshotTransport for CliSnapshotTransport {
    fn snapshot<'a>(
        &'a self,
        target: &'a Target,
        config: &'a TransportConfig,
        command_timeout: Duration,
    ) -> SnapshotFuture<'a> {
        Box::pin(async move {
            match timeout(command_timeout, collect_snapshot(target, config)).await {
                Ok(result) => result,
                Err(_) => Err(SnapshotError::timed_out(command_timeout)),
            }
        })
    }

    fn open_change_stream<'a>(
        &'a self,
        target: &'a Target,
        config: &'a TransportConfig,
        connect_timeout: Duration,
        pane_ids: &'a [String],
    ) -> OpenChangeStreamFuture<'a> {
        Box::pin(open_change_stream(
            target,
            config,
            connect_timeout,
            pane_ids,
        ))
    }
}

async fn open_change_stream(
    target: &Target,
    config: &TransportConfig,
    connect_timeout: Duration,
    pane_ids: &[String],
) -> Result<Box<dyn ChangeStream>, SnapshotError> {
    let Some(_) = target.socket.as_deref() else {
        return std::future::pending().await;
    };

    #[cfg(unix)]
    {
        open_socket_event_stream(target, config, connect_timeout, pane_ids)
            .await
            .map(|stream| Box::new(stream) as Box<dyn ChangeStream>)
    }
    #[cfg(not(unix))]
    {
        let _ = (target, config, connect_timeout, pane_ids);
        std::future::pending().await
    }
}

#[cfg(unix)]
const EVENT_SUBSCRIPTIONS: &[&str] = &[
    "workspace.created",
    "workspace.updated",
    "workspace.metadata_updated",
    "workspace.renamed",
    "workspace.moved",
    "workspace.reordered",
    "workspace.closed",
    "workspace.focused",
    "tab.created",
    "tab.closed",
    "tab.focused",
    "tab.renamed",
    "tab.moved",
    "pane.created",
    "pane.updated",
    "pane.closed",
    "pane.focused",
    "pane.moved",
    "pane.exited",
    "pane.agent_detected",
    "layout.updated",
];

#[cfg(unix)]
async fn open_socket_event_stream(
    target: &Target,
    config: &TransportConfig,
    connect_timeout: Duration,
    pane_ids: &[String],
) -> Result<SocketChangeStream, SnapshotError> {
    let mut connection = open_socket_connection(target, config, connect_timeout).await?;
    let request = event_subscription_request(pane_ids);
    let mut encoded = serde_json::to_vec(&request)
        .map_err(|_| SnapshotError::unavailable("failed to encode event subscription"))?;
    encoded.push(b'\n');
    connection
        .stream
        .write_all(&encoded)
        .await
        .map_err(|_| SnapshotError::unavailable("failed to write event subscription"))?;

    let mut reader = BufReader::new(connection.stream);
    let acknowledgement = read_socket_line(&mut reader, connect_timeout).await?;
    if acknowledgement.get("error").is_some()
        || acknowledgement.get("id").and_then(Value::as_str) != Some("super-herdr:events")
    {
        return Err(SnapshotError::unavailable(
            "Herdr rejected the event subscription (diagnostics redacted)",
        ));
    }

    Ok(SocketChangeStream {
        reader,
        _forward: connection._forward.take(),
    })
}

#[cfg(unix)]
struct SocketChangeStream {
    reader: BufReader<UnixStream>,
    _forward: Option<SshSocketForward>,
}

#[cfg(unix)]
impl ChangeStream for SocketChangeStream {
    fn next(&mut self) -> ChangeFuture<'_> {
        Box::pin(async move {
            loop {
                let event = read_socket_line(&mut self.reader, Duration::MAX).await?;
                if event.get("event").and_then(Value::as_str).is_some() {
                    return Ok(());
                }
            }
        })
    }
}

#[cfg(unix)]
fn event_subscription_request(pane_ids: &[String]) -> Value {
    let mut subscriptions = EVENT_SUBSCRIPTIONS
        .iter()
        .map(|event| serde_json::json!({"type": event}))
        .collect::<Vec<_>>();
    subscriptions.extend(pane_ids.iter().map(|pane_id| {
        serde_json::json!({
            "type": "pane.agent_status_changed",
            "pane_id": pane_id,
        })
    }));
    serde_json::json!({
        "id": "super-herdr:events",
        "method": "events.subscribe",
        "params": {
            "subscriptions": subscriptions
        }
    })
}

#[cfg(unix)]
async fn read_socket_line(
    reader: &mut BufReader<UnixStream>,
    read_timeout: Duration,
) -> Result<Value, SnapshotError> {
    const MAX_SOCKET_LINE: usize = 1024 * 1024;
    let mut line = Vec::new();
    let read = async {
        reader
            .read_until(b'\n', &mut line)
            .await
            .map_err(|_| SnapshotError::unavailable("Herdr socket read failed"))
    };
    let length = if read_timeout == Duration::MAX {
        read.await?
    } else {
        timeout(read_timeout, read)
            .await
            .map_err(|_| SnapshotError::timed_out(read_timeout))??
    };
    if length == 0 {
        return Err(SnapshotError::unavailable("Herdr socket closed"));
    }
    if line.len() > MAX_SOCKET_LINE {
        return Err(SnapshotError::unavailable(
            "Herdr socket record exceeded size limit",
        ));
    }
    serde_json::from_slice(&line)
        .map_err(|_| SnapshotError::unavailable("Herdr socket returned invalid JSON"))
}

#[cfg(unix)]
struct SocketConnection {
    stream: UnixStream,
    _forward: Option<SshSocketForward>,
}

#[cfg(unix)]
struct SshSocketForward {
    child: Child,
    _directory: tempfile::TempDir,
}

#[cfg(unix)]
impl Drop for SshSocketForward {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// A reachable Herdr API socket. For an SSH target the forwarding child stays
/// alive with the endpoint, so several one-shot requests share one tunnel.
#[cfg(unix)]
struct SocketEndpoint {
    path: PathBuf,
    forward: Option<SshSocketForward>,
}

#[cfg(unix)]
impl SocketEndpoint {
    async fn connect(&self, connect_timeout: Duration) -> Result<UnixStream, SnapshotError> {
        timeout(connect_timeout, UnixStream::connect(&self.path))
            .await
            .map_err(|_| SnapshotError::timed_out(connect_timeout))?
            .map_err(|_| SnapshotError::unavailable("failed to connect to the Herdr socket"))
    }
}

#[cfg(unix)]
async fn open_socket_connection(
    target: &Target,
    config: &TransportConfig,
    connect_timeout: Duration,
) -> Result<SocketConnection, SnapshotError> {
    let endpoint = open_socket_endpoint(target, config, connect_timeout).await?;
    let stream = endpoint.connect(connect_timeout).await?;
    Ok(SocketConnection {
        stream,
        _forward: endpoint.forward,
    })
}

#[cfg(unix)]
async fn open_socket_endpoint(
    target: &Target,
    config: &TransportConfig,
    connect_timeout: Duration,
) -> Result<SocketEndpoint, SnapshotError> {
    let socket = target
        .socket
        .as_deref()
        .ok_or_else(|| SnapshotError::unavailable("Herdr API socket is not configured"))?;
    let Some(destination) = target.ssh.as_deref() else {
        return Ok(SocketEndpoint {
            path: PathBuf::from(socket),
            forward: None,
        });
    };

    let directory = tempfile::Builder::new()
        .prefix("super-herdr-socket-")
        .tempdir()
        .map_err(|_| SnapshotError::unavailable("failed to create socket forwarding directory"))?;
    let local_socket = directory.path().join("herdr.sock");
    let forward = format!("{}:{socket}", local_socket.display());
    let mut command = Command::new(&config.ssh_bin);
    if config.batch_mode {
        command.arg("-o").arg("BatchMode=yes");
    }
    command
        .arg("-o")
        .arg(format!("ConnectTimeout={}", config.connect_timeout_seconds))
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-N")
        .arg("-L")
        .arg(forward)
        .arg("--")
        .arg(destination)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let child = command
        .spawn()
        .map_err(|_| SnapshotError::unavailable("failed to start SSH socket forwarding"))?;
    let mut guard = SshSocketForward {
        child,
        _directory: directory,
    };
    let deadline = Instant::now() + connect_timeout;
    loop {
        if guard
            .child
            .try_wait()
            .map_err(|_| SnapshotError::unavailable("failed to inspect SSH socket forwarding"))?
            .is_some()
        {
            return Err(SnapshotError::unavailable(
                "SSH socket forwarding exited (diagnostics redacted)",
            ));
        }
        match UnixStream::connect(&local_socket).await {
            // The probe only proves the tunnel is listening. Herdr serves one
            // request per connection, so callers open their own.
            Ok(_) => {
                return Ok(SocketEndpoint {
                    path: local_socket,
                    forward: Some(guard),
                });
            }
            Err(_) if Instant::now() < deadline => sleep(Duration::from_millis(25)).await,
            Err(_) => return Err(SnapshotError::timed_out(connect_timeout)),
        }
    }
}

const MAX_API_REQUEST_BYTES: usize = 1024 * 1024;

/// A private Herdr API endpoint that carries several documented requests.
///
/// Herdr answers one request per connection and then closes it, so every
/// request opens its own socket. For an SSH target the forwarding child is
/// created once and outlives the individual requests, so a multi-step operation
/// pays for one tunnel instead of one per request.
#[cfg(unix)]
pub struct ApiSession {
    endpoint: SocketEndpoint,
    request_timeout: Duration,
    next_id: u64,
}

#[cfg(unix)]
impl ApiSession {
    pub async fn open(
        target: &Target,
        config: &TransportConfig,
        request_timeout: Duration,
    ) -> Result<Self, SnapshotError> {
        Ok(Self {
            endpoint: open_socket_endpoint(target, config, request_timeout).await?,
            request_timeout,
            next_id: 0,
        })
    }

    /// Issue one documented API request and return its `result` value. Server
    /// diagnostics are never surfaced; a rejected request reports only its
    /// method so terminal or agent content cannot leak into the UI.
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value, SnapshotError> {
        self.next_id = self.next_id.saturating_add(1);
        let id = format!("super-herdr:{method}:{}", self.next_id);
        let request = serde_json::json!({
            "id": id,
            "method": method,
            "params": params,
        });
        let mut encoded = serde_json::to_vec(&request)
            .map_err(|_| SnapshotError::unavailable(format!("failed to encode {method}")))?;
        if encoded.len().saturating_add(1) > MAX_API_REQUEST_BYTES {
            return Err(SnapshotError::unavailable(format!(
                "{method} exceeds Herdr's API request size limit"
            )));
        }
        encoded.push(b'\n');

        let stream = self.endpoint.connect(self.request_timeout).await?;
        let mut reader = BufReader::new(stream);
        timeout(self.request_timeout, reader.get_mut().write_all(&encoded))
            .await
            .map_err(|_| SnapshotError::timed_out(self.request_timeout))?
            .map_err(|_| SnapshotError::unavailable(format!("failed to write {method}")))?;

        let response = read_socket_line(&mut reader, self.request_timeout).await?;
        if response.get("id").and_then(Value::as_str) != Some(id.as_str()) {
            return Err(SnapshotError::unavailable(format!(
                "Herdr returned an unexpected {method} response"
            )));
        }
        if response.get("error").is_some() {
            return Err(SnapshotError::unavailable(format!(
                "Herdr rejected {method} (diagnostics redacted)"
            )));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| SnapshotError::unavailable(format!("{method} returned no result")))
    }
}

/// Send text through Herdr's documented semantic pane-input API. Herdr applies
/// its authoritative bracketed-paste state and writes the complete input as one
/// runtime message. The payload travels only in the private socket stream, never
/// in a process argument or diagnostic.
pub async fn send_pane_input(
    target: &Target,
    config: &TransportConfig,
    pane_id: &str,
    text: &str,
    command_timeout: Duration,
) -> Result<(), SnapshotError> {
    #[cfg(unix)]
    {
        timeout(
            command_timeout,
            send_pane_input_unix(target, config, pane_id, text, command_timeout),
        )
        .await
        .map_err(|_| SnapshotError::timed_out(command_timeout))?
    }
    #[cfg(not(unix))]
    {
        let _ = (target, config, pane_id, text, command_timeout);
        Err(SnapshotError::unavailable(
            "semantic pane input requires Unix-socket support",
        ))
    }
}

#[cfg(unix)]
async fn send_pane_input_unix(
    target: &Target,
    config: &TransportConfig,
    pane_id: &str,
    text: &str,
    connect_timeout: Duration,
) -> Result<(), SnapshotError> {
    let params = serde_json::json!({
        "pane_id": pane_id,
        "text": text,
        "keys": [],
    });
    if serde_json::to_vec(&params).map_or(usize::MAX, |encoded| encoded.len())
        >= MAX_API_REQUEST_BYTES
    {
        return Err(SnapshotError::unavailable(
            "paste exceeds Herdr's API request size limit",
        ));
    }
    let mut session = ApiSession::open(target, config, connect_timeout).await?;
    session.request("pane.send_input", params).await?;
    Ok(())
}

#[derive(Debug)]
struct AttemptFailure {
    message: String,
    protocol_mismatch: bool,
}

impl AttemptFailure {
    fn other(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            protocol_mismatch: false,
        }
    }
}

async fn collect_snapshot(
    target: &Target,
    transport: &TransportConfig,
) -> Result<TransportSnapshot, SnapshotError> {
    let mut mismatches = Vec::new();
    for executable in target.candidate_bins() {
        match collect_snapshot_once(target, transport, executable).await {
            Ok(snapshot) => {
                return Ok(TransportSnapshot {
                    snapshot,
                    herdr_bin: executable.to_owned(),
                });
            }
            Err(failure) if failure.protocol_mismatch => {
                mismatches.push(format!("{executable}: {}", failure.message));
            }
            Err(failure) => {
                return Err(SnapshotError::unavailable(format!(
                    "{executable}: {}",
                    failure.message
                )));
            }
        }
    }

    Err(SnapshotError::incompatible(format!(
        "no compatible Herdr client candidate; {}",
        mismatches.join("; ")
    )))
}

async fn collect_snapshot_once(
    target: &Target,
    transport: &TransportConfig,
    executable: &str,
) -> Result<Value, AttemptFailure> {
    let mut command = build_herdr_command(
        target,
        transport,
        executable,
        &["api".to_owned(), "snapshot".to_owned()],
    );
    let output = command
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| {
            AttemptFailure::other(format!("failed to start target {:?}: {error}", target.name))
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let response = parse_last_json(&stdout);

    if let Ok(response) = &response
        && let Some(failure) = api_failure(response)
    {
        return Err(failure);
    }

    if !output.status.success() {
        return Err(AttemptFailure::other(format!(
            "command exited with {} (diagnostic output redacted)",
            output.status
        )));
    }

    let response = response.map_err(AttemptFailure::other)?;
    response
        .pointer("/result/snapshot")
        .cloned()
        .ok_or_else(|| AttemptFailure::other("Herdr response did not contain result.snapshot"))
}

pub fn build_herdr_command(
    target: &Target,
    transport: &TransportConfig,
    executable: &str,
    operation_args: &[String],
) -> Command {
    let herdr_args = herdr_args(target, operation_args);
    if let Some(destination) = &target.ssh {
        build_ssh_command(
            destination,
            transport,
            render_remote_command(executable, &herdr_args),
        )
    } else {
        let mut command = Command::new(executable);
        command.args(herdr_args);
        command
    }
}

pub async fn run_herdr_operation(
    target: &Target,
    transport: &TransportConfig,
    executable: &str,
    operation_args: &[String],
    command_timeout: Duration,
) -> Result<(), SnapshotError> {
    let mut command = build_herdr_command(target, transport, executable, operation_args);
    let status = timeout(
        command_timeout,
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .map_err(|_| SnapshotError::timed_out(command_timeout))?
    .map_err(|_| SnapshotError::unavailable("failed to start the Herdr action"))?;
    if !status.success() {
        return Err(SnapshotError::unavailable(format!(
            "Herdr action exited with {status} (diagnostic output redacted)"
        )));
    }
    Ok(())
}

pub fn build_ssh_command(
    destination: &str,
    transport: &TransportConfig,
    remote_command: String,
) -> Command {
    let mut command = Command::new(&transport.ssh_bin);
    if transport.batch_mode {
        command.arg("-o").arg("BatchMode=yes");
    }
    command
        .arg("-o")
        .arg(format!(
            "ConnectTimeout={}",
            transport.connect_timeout_seconds
        ))
        .arg("--")
        .arg(destination)
        .arg(remote_command);
    command
}

fn herdr_args(target: &Target, operation_args: &[String]) -> Vec<String> {
    let mut args = Vec::with_capacity(operation_args.len() + 2);
    if let Some(session) = &target.session {
        args.push("--session".to_owned());
        args.push(session.clone());
    }
    args.extend_from_slice(operation_args);
    args
}

fn render_remote_command(executable: &str, args: &[String]) -> String {
    std::iter::once(executable)
        .chain(args.iter().map(String::as_str))
        .map(quote_posix)
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_posix(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn parse_last_json(text: &str) -> Result<Value, &'static str> {
    text.lines()
        .rev()
        .find_map(|line| serde_json::from_str(line).ok())
        .ok_or("Herdr did not return a JSON response")
}

fn api_failure(response: &Value) -> Option<AttemptFailure> {
    let error = response.get("error")?;
    let protocol_mismatch = error.get("code").and_then(Value::as_str) == Some("protocol_mismatch");
    Some(AttemptFailure {
        message: format!("Herdr API error: {}", compact_json(error)),
        protocol_mismatch,
    })
}

fn compact_json(value: &Value) -> String {
    const LIMIT: usize = 500;
    let normalized = value
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.chars().count() <= LIMIT {
        return normalized;
    }
    let mut shortened = normalized.chars().take(LIMIT).collect::<String>();
    shortened.push('…');
    shortened
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        EVENT_SUBSCRIPTIONS, SessionList, api_failure, event_subscription_request, herdr_args,
        parse_last_json, quote_posix, targets_for_discovered_sessions, valid_discovered_session,
    };
    use crate::config::Target;

    #[test]
    fn quotes_remote_arguments_without_shell_injection() {
        assert_eq!(quote_posix("plain"), "'plain'");
        assert_eq!(quote_posix("a'b"), "'a'\"'\"'b'");
        assert_eq!(quote_posix("$(touch bad)"), "'$(touch bad)'");
    }

    #[test]
    fn herdr_actions_are_qualified_with_the_selected_session() {
        let target = Target {
            name: "development".to_owned(),
            ssh: Some("development-host".to_owned()),
            discover_sessions: false,
            session: Some("work".to_owned()),
            socket: None,
            herdr_bins: vec!["herdr".to_owned()],
            roots: Vec::new(),
        };

        assert_eq!(
            herdr_args(
                &target,
                &[
                    "pane".to_owned(),
                    "zoom".to_owned(),
                    "w1:p1".to_owned(),
                    "--toggle".to_owned(),
                ],
            ),
            ["--session", "work", "pane", "zoom", "w1:p1", "--toggle"]
        );
    }

    #[test]
    fn tolerates_diagnostic_lines_before_api_json() {
        let parsed = parse_last_json("notice\n{\"result\":{}}\n").unwrap();
        assert!(parsed.get("result").is_some());
    }

    #[test]
    fn parses_and_validates_public_session_list_json() {
        let sessions: SessionList = serde_json::from_str(
            r#"{"sessions":[{"name":"default","running":true,"session_dir":"/srv/herdr","socket_path":"/srv/herdr/herdr.sock"}]}"#,
        )
        .unwrap();

        assert_eq!(sessions.sessions.len(), 1);
        assert!(valid_discovered_session(&sessions.sessions[0]));
    }

    #[test]
    fn an_empty_running_session_registry_removes_the_active_fallback() {
        let target = Target {
            name: "development".to_owned(),
            ssh: Some("development-host".to_owned()),
            discover_sessions: true,
            session: Some("fallback".to_owned()),
            socket: None,
            herdr_bins: vec!["herdr".to_owned()],
            roots: Vec::new(),
        };

        assert!(targets_for_discovered_sessions(&target, Vec::new()).is_empty());
    }

    #[test]
    fn rejects_unsafe_discovered_socket_paths() {
        let mut session = super::DiscoveredSession {
            name: "dev".to_owned(),
            running: true,
            session_dir: "/tmp/dev".to_owned(),
            socket_path: "relative/herdr.sock".to_owned(),
        };
        assert!(!valid_discovered_session(&session));
        session.socket_path = "/tmp/remote:socket".to_owned();
        assert!(!valid_discovered_session(&session));
        session.socket_path = "/tmp/herdr.sock".to_owned();
        session.name = "bad/name".to_owned();
        assert!(!valid_discovered_session(&session));
    }

    #[test]
    fn identifies_only_protocol_errors_as_retryable() {
        let mismatch = api_failure(&json!({"error": {"code": "protocol_mismatch"}})).unwrap();
        let ordinary = api_failure(&json!({"error": {"code": "not_found"}})).unwrap();

        assert!(mismatch.protocol_mismatch);
        assert!(!ordinary.protocol_mismatch);
    }

    #[cfg(unix)]
    #[test]
    fn event_subscription_covers_resource_and_layout_changes() {
        let request = event_subscription_request(&["w1:p1".to_owned()]);
        let subscriptions = request
            .pointer("/params/subscriptions")
            .and_then(serde_json::Value::as_array)
            .unwrap();

        assert_eq!(subscriptions.len(), EVENT_SUBSCRIPTIONS.len() + 1);
        assert!(
            subscriptions
                .iter()
                .any(|value| value["type"] == "workspace.created")
        );
        assert!(subscriptions.iter().any(|value| {
            value["type"] == "pane.agent_status_changed" && value["pane_id"] == "w1:p1"
        }));
        assert!(
            subscriptions
                .iter()
                .any(|value| value["type"] == "layout.updated")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn documented_local_event_stream_stays_open_across_events() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        use super::{CliSnapshotTransport, SnapshotTransport};
        use crate::config::Config;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            let request: serde_json::Value = serde_json::from_str(&request).unwrap();
            assert_eq!(request["method"], "events.subscribe");
            let stream = reader.get_mut();
            stream
                .write_all(
                    b"{\"id\":\"super-herdr:events\",\"result\":{\"type\":\"subscribed\"}}\n",
                )
                .await
                .unwrap();
            stream
                .write_all(b"{\"event\":\"layout.updated\",\"data\":{}}\n")
                .await
                .unwrap();
            stream
                .write_all(b"{\"event\":\"pane.focused\",\"data\":{}}\n")
                .await
                .unwrap();
        });
        let config = Config::parse(&format!(
            "[[targets]]\nname = \"local\"\nsocket = {:?}\n",
            socket.display().to_string()
        ))
        .unwrap();

        let mut changes = CliSnapshotTransport
            .open_change_stream(
                &config.targets[0],
                &config.transport,
                std::time::Duration::from_secs(1),
                &[],
            )
            .await
            .unwrap();
        changes.next().await.unwrap();
        changes.next().await.unwrap();
        server.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn semantic_pane_input_sends_one_private_socket_request() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        use super::send_pane_input;
        use crate::config::Config;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            let request: serde_json::Value = serde_json::from_str(&request).unwrap();
            let id = request["id"].as_str().unwrap().to_owned();
            assert!(id.starts_with("super-herdr:pane.send_input:"));
            assert_eq!(request["method"], "pane.send_input");
            assert_eq!(request["params"]["pane_id"], "w2:p3");
            assert_eq!(request["params"]["text"], "first\nsecond\nthird");
            assert_eq!(request["params"]["keys"], serde_json::json!([]));
            reader
                .get_mut()
                .write_all(format!("{{\"id\":{id:?},\"result\":{{\"type\":\"ok\"}}}}\n").as_bytes())
                .await
                .unwrap();
        });
        let config = Config::parse(&format!(
            "[[targets]]\nname = \"local\"\nsocket = {:?}\n",
            socket.display().to_string()
        ))
        .unwrap();

        send_pane_input(
            &config.targets[0],
            &config.transport,
            "w2:p3",
            "first\nsecond\nthird",
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
        server.await.unwrap();
    }
}
