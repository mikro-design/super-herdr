use std::future::Future;
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

#[derive(Debug, Deserialize)]
struct SessionList {
    sessions: Vec<DiscoveredSession>,
}

/// Expand host targets through Herdr's documented `session list --json` command.
/// A discovery failure is isolated to that host by retaining its configured
/// fallback session target.
pub async fn expand_discovered_sessions(config: crate::config::Config) -> crate::config::Config {
    let mut expanded = Vec::new();
    for target in &config.targets {
        if !target.discover_sessions {
            expanded.push(target.clone());
            continue;
        }
        match timeout(
            Duration::from_secs(config.transport.command_timeout_seconds),
            discover_sessions(target, &config.transport),
        )
        .await
        {
            Ok(Ok(sessions)) if !sessions.is_empty() => {
                expanded.extend(sessions.into_iter().map(|session| Target {
                    name: target.name.clone(),
                    ssh: target.ssh.clone(),
                    discover_sessions: false,
                    session: Some(session.name),
                    socket: Some(session.socket_path),
                    herdr_bins: target.herdr_bins.clone(),
                }));
            }
            _ => expanded.push(target.clone()),
        }
    }
    crate::config::Config {
        transport: config.transport,
        targets: expanded,
    }
}

async fn discover_sessions(
    target: &Target,
    transport: &TransportConfig,
) -> Result<Vec<DiscoveredSession>, SnapshotError> {
    let mut failures = Vec::new();
    for executable in target.candidate_bins() {
        match discover_sessions_once(target, transport, executable).await {
            Ok(sessions) => return Ok(sessions),
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
    let mut connection = open_event_connection(target, config, connect_timeout).await?;
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
    let acknowledgement = read_event_line(&mut reader, connect_timeout).await?;
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
                let event = read_event_line(&mut self.reader, Duration::MAX).await?;
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
async fn read_event_line(
    reader: &mut BufReader<UnixStream>,
    read_timeout: Duration,
) -> Result<Value, SnapshotError> {
    const MAX_EVENT_LINE: usize = 1024 * 1024;
    let mut line = Vec::new();
    let read = async {
        reader
            .read_until(b'\n', &mut line)
            .await
            .map_err(|_| SnapshotError::unavailable("event stream read failed"))
    };
    let length = if read_timeout == Duration::MAX {
        read.await?
    } else {
        timeout(read_timeout, read)
            .await
            .map_err(|_| SnapshotError::timed_out(read_timeout))??
    };
    if length == 0 {
        return Err(SnapshotError::unavailable("event stream closed"));
    }
    if line.len() > MAX_EVENT_LINE {
        return Err(SnapshotError::unavailable(
            "event record exceeded size limit",
        ));
    }
    serde_json::from_slice(&line)
        .map_err(|_| SnapshotError::unavailable("event stream returned invalid JSON"))
}

#[cfg(unix)]
struct EventConnection {
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

#[cfg(unix)]
async fn open_event_connection(
    target: &Target,
    config: &TransportConfig,
    connect_timeout: Duration,
) -> Result<EventConnection, SnapshotError> {
    let socket = target
        .socket
        .as_deref()
        .ok_or_else(|| SnapshotError::unavailable("event socket is not configured"))?;
    let Some(destination) = target.ssh.as_deref() else {
        let stream = timeout(connect_timeout, UnixStream::connect(socket))
            .await
            .map_err(|_| SnapshotError::timed_out(connect_timeout))?
            .map_err(|_| SnapshotError::unavailable("failed to connect to local event socket"))?;
        return Ok(EventConnection {
            stream,
            _forward: None,
        });
    };

    let directory = tempfile::Builder::new()
        .prefix("super-herdr-events-")
        .tempdir()
        .map_err(|_| SnapshotError::unavailable("failed to create event forwarding directory"))?;
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
        .map_err(|_| SnapshotError::unavailable("failed to start SSH event forwarding"))?;
    let mut guard = SshSocketForward {
        child,
        _directory: directory,
    };
    let deadline = Instant::now() + connect_timeout;
    loop {
        if guard
            .child
            .try_wait()
            .map_err(|_| SnapshotError::unavailable("failed to inspect SSH event forwarding"))?
            .is_some()
        {
            return Err(SnapshotError::unavailable(
                "SSH event forwarding exited (diagnostics redacted)",
            ));
        }
        match UnixStream::connect(&local_socket).await {
            Ok(stream) => {
                return Ok(EventConnection {
                    stream,
                    _forward: Some(guard),
                });
            }
            Err(_) if Instant::now() < deadline => sleep(Duration::from_millis(25)).await,
            Err(_) => return Err(SnapshotError::timed_out(connect_timeout)),
        }
    }
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
        EVENT_SUBSCRIPTIONS, SessionList, api_failure, event_subscription_request, parse_last_json,
        quote_posix, valid_discovered_session,
    };

    #[test]
    fn quotes_remote_arguments_without_shell_injection() {
        assert_eq!(quote_posix("plain"), "'plain'");
        assert_eq!(quote_posix("a'b"), "'a'\"'\"'b'");
        assert_eq!(quote_posix("$(touch bad)"), "'$(touch bad)'");
    }

    #[test]
    fn tolerates_diagnostic_lines_before_api_json() {
        let parsed = parse_last_json("notice\n{\"result\":{}}\n").unwrap();
        assert!(parsed.get("result").is_some());
    }

    #[test]
    fn parses_and_validates_public_session_list_json() {
        let sessions: SessionList = serde_json::from_str(
            r#"{"sessions":[{"name":"default","running":true,"session_dir":"/home/user/.config/herdr","socket_path":"/home/user/.config/herdr/herdr.sock"}]}"#,
        )
        .unwrap();

        assert_eq!(sessions.sessions.len(), 1);
        assert!(valid_discovered_session(&sessions.sessions[0]));
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
}
