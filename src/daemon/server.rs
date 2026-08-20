//! The daemon's I/O layer.
//!
//! This module owns everything the broker deliberately does not: a listening
//! socket, connections, Herdr terminal routes, operation execution, and the
//! durable attention index. It performs the [`Effect`] values the broker
//! returns and feeds the results back in, so every rule about who may type into
//! a pane is decided in one place that never touches a file descriptor.
//!
//! The daemon listens on a Unix socket with owner-only permissions. It is not
//! published to a network: a client on another machine reaches it the same way
//! Super-Herdr already reaches a Herdr socket, by forwarding it over OpenSSH.
//! Device pairing and a network transport are a later, separate decision, and
//! until then the daemon inherits SSH's authentication rather than inventing
//! its own.

use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream,
};
use tokio::net::UnixListener;
use tokio::process::Child;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::attention::{AttentionIndex, AttentionStore};
use crate::clipboard;
use crate::config::{Config, Target, TransportConfig};
use crate::daemon::broker::{Broker, ClientId, Effect};
use crate::daemon::web;
use crate::model::{PaneId, TargetSession};
use crate::operation::Operation;
use crate::protocol::{ClientMessage, MAX_MESSAGE_BYTES, ServerMessage, decode, encode};
use crate::state::{FederationState, FederationStore, SupervisorOptions, target_key};
use crate::terminal::{
    TerminalAccess, TerminalEvent, parse_terminal_event, spawn_terminal, terminal_input_command,
    terminal_resize_command, terminal_scroll_command,
};
use crate::transport::{
    CliSnapshotTransport, expand_discovered_sessions, run_herdr_operation, send_pane_input,
};
use crate::workspace_move;

/// How often the daemon re-reads its configuration and re-runs session
/// discovery. This matches the frontend's own refresh cadence, because both are
/// bounded reads of the same durable file.
pub const CONFIG_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct DaemonOptions {
    pub socket: PathBuf,
    /// Where the durable attention index lives. `None` discovers the standard
    /// state path.
    pub attention_state: Option<PathBuf>,
    pub refresh_interval: Duration,
    /// Serve the browser client on this loopback port. `None` serves no web
    /// client at all, which is the default: a daemon should not open a port
    /// nobody asked for.
    pub web_port: Option<u16>,
}

impl DaemonOptions {
    /// The runtime directory is preferred because a socket is not state worth
    /// keeping across a reboot.
    pub fn discover() -> Result<Self> {
        let root = if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
            PathBuf::from(runtime)
        } else if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
            PathBuf::from(state)
        } else {
            let home = std::env::var_os("HOME").context(
                "XDG_RUNTIME_DIR, XDG_STATE_HOME, or HOME is required to place the daemon socket",
            )?;
            PathBuf::from(home).join(".local/state")
        };
        Ok(Self {
            socket: root.join("super-herdr/daemon.sock"),
            attention_state: None,
            refresh_interval: CONFIG_REFRESH_INTERVAL,
            web_port: None,
        })
    }
}

/// Everything the broker loop reacts to, from clients and from the federation
/// alike, so ordering is decided in one place.
enum Input {
    Connected {
        outbox: mpsc::UnboundedSender<ServerMessage>,
        reply: oneshot::Sender<ClientId>,
    },
    Received {
        client: ClientId,
        message: ClientMessage,
    },
    Disconnected {
        client: ClientId,
    },
    Federation(FederationState),
    Frame {
        pane: PaneId,
        sequence: u64,
        width: u16,
        height: u16,
        full: bool,
        bytes: Arc<[u8]>,
    },
    RouteClosed {
        pane: PaneId,
    },
    OperationDone {
        client: ClientId,
        request: u64,
        applied: bool,
        message: String,
    },
    UploadFinished {
        client: ClientId,
        request: u64,
        result: Result<(String, u64), String>,
    },
    /// Stop serving. The loop leaves through the same exit a closed input
    /// channel uses, so shutdown has one path rather than two.
    Shutdown,
    /// The refresh deadline arrived. Reading the file happens off the loop.
    RefreshDue,
    /// A completed refresh. `None` means the read failed and the running
    /// federation keeps whatever it already had.
    Reconfigured(Option<Config>),
}

/// The largest payload a client may offer for upload.
///
/// It matches the frontend's own clipboard ceiling, and is enforced here
/// because the daemon is what writes to the target host.
pub const MAX_UPLOAD_BYTES: u64 = 32 * 1024 * 1024;

/// A transfer in progress, held only in memory.
///
/// Nothing reaches the target until the digest verifies, so a connection that
/// drops mid-transfer leaves nothing behind — the refusal and the abandonment
/// have the same result rather than the abandonment producing a file nothing
/// checked.
struct Upload {
    pane: PaneId,
    mime: String,
    length: u64,
    bytes: Vec<u8>,
}

struct Route {
    child: Child,
    commands: Option<mpsc::UnboundedSender<Vec<u8>>>,
    reader: JoinHandle<()>,
    writer: Option<JoinHandle<()>>,
}

impl Route {
    /// Ending a route ends only this observation. Herdr keeps running, and the
    /// pane's processes are untouched — a client closing a terminal view must
    /// never cost somebody their shell.
    fn shutdown(mut self) {
        self.reader.abort();
        if let Some(writer) = self.writer {
            writer.abort();
        }
        let _ = self.child.start_kill();
    }
}

struct Daemon {
    broker: Broker,
    outboxes: BTreeMap<ClientId, mpsc::UnboundedSender<ServerMessage>>,
    routes: BTreeMap<PaneId, Route>,
    targets: BTreeMap<TargetSession, Target>,
    transport: TransportConfig,
    state: FederationState,
    attention: AttentionIndex,
    attention_store: Option<AttentionStore>,
    attention_cursor: Option<u64>,
    command_timeout: Duration,
    inputs: mpsc::UnboundedSender<Input>,
    uploads: BTreeMap<(ClientId, u64), Upload>,
    /// Panes whose route opened with less access than was asked for, drained
    /// into the broker on the next pass.
    downgraded: Vec<PaneId>,
    /// The expanded configuration currently driving supervisors, kept so a
    /// refresh can tell a real change from a re-read of the same file.
    active: Config,
    config_path: Option<PathBuf>,
    refresh_inflight: bool,
    store: Option<FederationStore>,
    watcher: Option<JoinHandle<()>>,
}

/// One in-memory client attachment, for a frontend hosting its own daemon.
pub const IN_PROCESS_BUFFER: usize = 64 * 1024;

/// A running daemon that in-process clients can attach to.
pub struct DaemonHandle {
    attach: mpsc::UnboundedSender<DuplexStream>,
    task: JoinHandle<Result<()>>,
}

impl DaemonHandle {
    /// Attach an in-process client and return its end of the pipe. The daemon
    /// sees it as an ordinary connection.
    pub fn attach(&self) -> Result<DuplexStream> {
        let (theirs, ours) = tokio::io::duplex(IN_PROCESS_BUFFER);
        self.attach
            .send(theirs)
            .map_err(|_| anyhow::anyhow!("the daemon is no longer running"))?;
        Ok(ours)
    }

    pub async fn shutdown(self) {
        drop(self.attach);
        self.task.abort();
    }
}

/// Run a daemon inside this process, with no socket at all.
///
/// A single-machine install should not have to operate a service, and should
/// not leave a socket behind for anything else to find.
pub fn spawn_in_process(
    config: Config,
    config_path: Option<PathBuf>,
    options: DaemonOptions,
) -> DaemonHandle {
    let (attach, attachments) = mpsc::unbounded_channel();
    // A hosted daemon does not watch for signals: the process belongs to the
    // frontend, which decides what a signal means for itself.
    let task = tokio::spawn(run(
        config,
        config_path,
        options,
        None,
        attachments,
        std::future::pending(),
    ));
    DaemonHandle { attach, task }
}

/// Run the daemon until the listener fails or the process is stopped.
///
/// `config_path` is the durable file the running federation is refreshed from.
/// `None` pins the daemon to the configuration it was given, which is what the
/// tests want and nothing else should.
pub async fn serve(
    config: Config,
    config_path: Option<PathBuf>,
    options: DaemonOptions,
) -> Result<()> {
    serve_until(config, config_path, options, terminated()).await
}

/// Serve until the given future resolves. Tests supply their own stop signal so
/// the cleanup below is exercised without sending the test runner a signal.
async fn serve_until(
    config: Config,
    config_path: Option<PathBuf>,
    options: DaemonOptions,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let listener = bind(&options.socket)?;
    let socket = options.socket.clone();
    let (attach, attachments) = mpsc::unbounded_channel();

    // The browser client is an ordinary in-process attachment, so it speaks the
    // same framing through the same handshake as every other client.
    let web = match options.web_port {
        Some(port) => {
            let address = web::loopback(port);
            let listener = web::bind(address).await?;
            let attach = attach.clone();
            let open: web::Attach = std::sync::Arc::new(move || {
                let (theirs, ours) = tokio::io::duplex(IN_PROCESS_BUFFER);
                attach
                    .send(theirs)
                    .map_err(|_| anyhow::anyhow!("the daemon is no longer running"))?;
                Ok(ours)
            });
            Some((address, tokio::spawn(web::serve(listener, open))))
        }
        None => None,
    };
    if let Some((address, _)) = web.as_ref() {
        // Printed rather than logged, because the address is what a person
        // needs in order to forward it.
        println!("super-herdr web client on http://{address}");
    }
    let result = run(
        config,
        config_path,
        options,
        Some(listener),
        attachments,
        shutdown,
    )
    .await;
    // The socket outlives the daemon otherwise, and the next start would have
    // to decide whether the path it found belongs to a live process.
    if let Some((_, task)) = web {
        task.abort();
    }
    let _ = fs::remove_file(&socket);
    result
}

/// Resolve when the process is asked to stop.
///
/// A daemon is normally ended by a signal rather than by returning, so without
/// this the cleanup below would only ever run in tests. SIGHUP is deliberately
/// not included: it conventionally means reload, the configuration is already
/// refreshed on its own schedule, and exiting on it would surprise anyone who
/// closed a terminal.
async fn terminated() {
    let mut interrupt = match signal(SignalKind::interrupt()) {
        Ok(interrupt) => interrupt,
        // Without a handler the default disposition still ends the process; the
        // daemon simply loses its cleanup rather than refusing to run.
        Err(_) => return std::future::pending().await,
    };
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(terminate) => terminate,
        Err(_) => return std::future::pending().await,
    };
    tokio::select! {
        _ = interrupt.recv() => {}
        _ = terminate.recv() => {}
    }
}

async fn run(
    config: Config,
    config_path: Option<PathBuf>,
    options: DaemonOptions,
    listener: Option<UnixListener>,
    mut attachments: mpsc::UnboundedReceiver<DuplexStream>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let active = expand_discovered_sessions(config).await;
    let (inputs, mut received) = mpsc::unbounded_channel();

    let attention_store = match options.attention_state.clone() {
        Some(path) => Some(AttentionStore::at(path)),
        None => AttentionStore::discover().ok(),
    };
    let attention = attention_store
        .as_ref()
        .and_then(|store| store.load().ok())
        .unwrap_or_default();
    let attention_cursor = attention.events().next_back().map(|event| event.id);
    let mut daemon = Daemon {
        broker: Broker::new(env!("CARGO_PKG_VERSION"), vec!["terminal".to_owned()]),
        outboxes: BTreeMap::new(),
        routes: BTreeMap::new(),
        targets: target_map(&active),
        transport: active.transport.clone(),
        state: FederationState::default(),
        attention,
        attention_store,
        attention_cursor,
        command_timeout: Duration::from_secs(active.transport.command_timeout_seconds),
        inputs: inputs.clone(),
        uploads: BTreeMap::new(),
        downgraded: Vec::new(),
        active,
        config_path: config_path.clone(),
        refresh_inflight: false,
        store: None,
        watcher: None,
    };
    daemon.start_federation();

    let refreshing = config_path.is_some().then(|| {
        let inputs = inputs.clone();
        let interval = options.refresh_interval;
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(interval);
            // The first tick completes immediately; the configuration was just
            // read.
            ticks.tick().await;
            loop {
                ticks.tick().await;
                if inputs.send(Input::RefreshDue).is_err() {
                    return;
                }
            }
        })
    });

    let stopping = inputs.clone();
    let signalled = tokio::spawn(async move {
        shutdown.await;
        let _ = stopping.send(Input::Shutdown);
    });

    let accepting = listener.map(|listener| tokio::spawn(accept(listener, inputs.clone())));
    loop {
        tokio::select! {
            input = received.recv() => {
                let Some(input) = input else { break };
                if matches!(input, Input::Shutdown) {
                    break;
                }
                daemon.handle(input);
            }
            attachment = attachments.recv() => {
                match attachment {
                    Some(stream) => {
                        tokio::spawn(connection(stream, inputs.clone()));
                    }
                    // Every attach handle is gone, so no in-process client can
                    // arrive again. A socket-served daemon keeps running.
                    None => attachments.close(),
                }
            }
        }
    }

    if let Some(accepting) = accepting {
        accepting.abort();
    }
    signalled.abort();
    if let Some(refreshing) = refreshing {
        refreshing.abort();
    }
    daemon.stop_federation().await;
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A MIME type a client reported is untrusted text. It is metadata rather than
/// payload and may be named in a refusal, but only bounded and without control
/// characters.
fn bounded_mime(mime: &str) -> String {
    mime.chars()
        .filter(|character| !character.is_control())
        .take(64)
        .collect()
}

fn target_map(config: &Config) -> BTreeMap<TargetSession, Target> {
    config
        .targets
        .iter()
        .cloned()
        .map(|target| (target_key(&target), target))
        .collect()
}

/// Which open routes a configuration change invalidates.
///
/// A route is an SSH child already talking to a Herdr server, so a refresh that
/// does not touch its target leaves it alone: re-reading the file must not cost
/// somebody the terminal they are working in. A changed transport invalidates
/// every route, because the command that opened them would now be built
/// differently. A target that changed or disappeared invalidates only its own.
fn invalidated_routes<'a>(
    previous: &Config,
    next: &Config,
    routed: impl Iterator<Item = &'a PaneId>,
) -> Vec<PaneId> {
    let (previous_targets, next_targets) = (target_map(previous), target_map(next));
    routed
        .filter(|pane| {
            let key = pane.target_session();
            previous.transport != next.transport
                || previous_targets.get(&key) != next_targets.get(&key)
        })
        .cloned()
        .collect()
}

/// Bind the socket without evicting a daemon that is already serving it. A path
/// that refuses a connection is a leftover from a process that did not clean up
/// and is safe to replace; a path that accepts one is somebody else's.
fn bind(path: &Path) -> Result<UnixListener> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => bail!(
            "a Super-Herdr daemon is already listening on {}",
            path.display()
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(_) => {
            fs::remove_file(path).with_context(|| {
                format!("failed to replace the stale socket at {}", path.display())
            })?;
        }
    }
    let listener = UnixListener::bind(path)
        .with_context(|| format!("failed to listen on {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to restrict {} to its owner", path.display()))?;
    Ok(listener)
}

async fn accept(listener: UnixListener, inputs: mpsc::UnboundedSender<Input>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(connection(stream, inputs.clone()));
            }
            // One failed accept must not end the daemon; the next connection
            // may well succeed.
            Err(_) => continue,
        }
    }
}

/// Serve one client connection over any byte stream.
///
/// An in-process frontend attaches through an in-memory pipe rather than the
/// socket, and takes this same path deliberately: local and remote clients then
/// share one implementation of framing, handshake, and every rule behind them,
/// so a bug cannot hide in the mode nobody runs on their own machine.
async fn connection<S>(stream: S, inputs: mpsc::UnboundedSender<Input>)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let (outbox, mut outgoing) = mpsc::unbounded_channel();
    let (reply, assigned) = oneshot::channel();
    if inputs.send(Input::Connected { outbox, reply }).is_err() {
        return;
    }
    let Ok(client) = assigned.await else {
        return;
    };

    let sending = tokio::spawn(async move {
        while let Some(message) = outgoing.recv().await {
            let Ok(line) = encode(&message) else {
                continue;
            };
            if writer.write_all(&line).await.is_err() {
                break;
            }
        }
        let _ = writer.shutdown().await;
    });

    let mut buffered = BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        // Bounding the read rather than the parsed message keeps a peer that
        // never sends a newline from growing this buffer without limit.
        let read = (&mut buffered)
            .take(MAX_MESSAGE_BYTES as u64)
            .read_until(b'\n', &mut line)
            .await;
        match read {
            Ok(0) => break,
            Ok(_) if line.last() != Some(&b'\n') => break,
            Ok(_) => {}
            Err(_) => break,
        }
        line.pop();
        let Ok(message) = decode::<ClientMessage>(&line) else {
            break;
        };
        if inputs.send(Input::Received { client, message }).is_err() {
            break;
        }
    }

    sending.abort();
    let _ = inputs.send(Input::Disconnected { client });
}

impl Daemon {
    fn handle(&mut self, input: Input) {
        let effects = match input {
            Input::Connected { outbox, reply } => {
                let client = self.broker.connect();
                self.outboxes.insert(client, outbox);
                if reply.send(client).is_err() {
                    self.outboxes.remove(&client);
                    self.broker.disconnect(client)
                } else {
                    Vec::new()
                }
            }
            Input::Received { client, message } => self.broker.handle(client, message),
            Input::Disconnected { client } => {
                self.outboxes.remove(&client);
                // An abandoned transfer is discarded rather than completed. It
                // never reached the host, so there is nothing to clean up
                // there.
                self.uploads.retain(|(owner, _), _| *owner != client);
                self.broker.disconnect(client)
            }
            Input::Federation(state) => {
                self.state = state.clone();
                let mut effects = self.broker.federation_updated(state);
                effects.extend(self.observe_attention());
                effects
            }
            Input::Frame {
                pane,
                sequence,
                width,
                height,
                full,
                bytes,
            } => self
                .broker
                .pane_frame(&pane, sequence, width, height, full, bytes),
            Input::RouteClosed { pane } => {
                if let Some(route) = self.routes.remove(&pane) {
                    route.shutdown();
                }
                self.broker.pane_route_closed(&pane)
            }
            Input::OperationDone {
                client,
                request,
                applied,
                message,
            } => self
                .broker
                .operation_completed(client, request, applied, message),
            Input::UploadFinished {
                client,
                request,
                result,
            } => {
                let message = match result {
                    Ok((path, bytes)) => ServerMessage::UploadComplete {
                        request,
                        path,
                        bytes,
                    },
                    Err(message) => ServerMessage::Error {
                        request: Some(request),
                        message,
                    },
                };
                if let Some(outbox) = self.outboxes.get(&client) {
                    let _ = outbox.send(message);
                }
                Vec::new()
            }
            // The loop intercepts this before it reaches here; the arm exists so
            // adding a shutdown path later cannot silently do nothing.
            Input::Shutdown => Vec::new(),
            Input::RefreshDue => {
                self.start_refresh();
                Vec::new()
            }
            Input::Reconfigured(config) => self.reconfigure(config),
        };
        self.apply(effects);
    }

    fn apply(&mut self, effects: Vec<Effect>) {
        let mut pending = effects;
        while let Some(effect) = pending.pop_front_compat() {
            // A route that opened with less access than asked for reports the
            // downgrade through the broker, so every subscriber is told by the
            // same path that grants a lease in the first place.
            for pane in std::mem::take(&mut self.downgraded) {
                pending.extend(self.broker.route_downgraded(&pane));
            }
            match effect {
                Effect::Send { client, message } => {
                    if let Some(outbox) = self.outboxes.get(&client) {
                        let _ = outbox.send(message);
                    }
                }
                Effect::Disconnect { client, .. } => {
                    // Dropping the outbox ends the writer task, which closes
                    // the connection once queued messages have been written.
                    self.outboxes.remove(&client);
                    pending.extend(self.broker.disconnect(client));
                }
                Effect::OpenRoute {
                    pane,
                    access,
                    cols,
                    rows,
                } => self.open_route(pane, access, cols, rows),
                Effect::CloseRoute { pane } => {
                    if let Some(route) = self.routes.remove(&pane) {
                        route.shutdown();
                    }
                }
                Effect::RouteInput { pane, bytes } => {
                    if let Ok(command) = terminal_input_command(&bytes) {
                        self.send_to_route(&pane, command);
                    }
                }
                Effect::RouteResize { pane, cols, rows } => {
                    if let Ok(command) = terminal_resize_command(cols, rows) {
                        self.send_to_route(&pane, command);
                    }
                }
                Effect::RouteScroll {
                    pane,
                    direction,
                    lines,
                    column,
                    row,
                    modifiers,
                } => {
                    if let Ok(command) =
                        terminal_scroll_command(direction, lines, column, row, modifiers)
                    {
                        self.send_to_route(&pane, command);
                    }
                }
                Effect::RunOperation {
                    client,
                    request,
                    operation,
                } => self.run_operation(client, request, operation),
                Effect::PastePaneText {
                    client,
                    request,
                    pane,
                    text,
                } => self.paste_pane_text(client, request, pane, text),
                Effect::BeginUpload {
                    client,
                    request,
                    pane,
                    mime,
                    length,
                } => {
                    // Refused before any bytes move, which is the only point at
                    // which refusing is free.
                    if length > MAX_UPLOAD_BYTES {
                        self.refuse(
                            client,
                            request,
                            format!(
                                "refusing a {length} byte upload; the limit is {MAX_UPLOAD_BYTES}"
                            ),
                        );
                    } else {
                        self.uploads.insert(
                            (client, request),
                            Upload {
                                pane,
                                mime,
                                length,
                                bytes: Vec::new(),
                            },
                        );
                    }
                }
                Effect::UploadChunk {
                    client,
                    request,
                    bytes,
                } => {
                    // Stopped at the declared length rather than trusting it: a
                    // lying length is a disk-fill on the target that no digest
                    // would catch.
                    let outcome = match self.uploads.get_mut(&(client, request)) {
                        None => Err("no transfer is in progress".to_owned()),
                        Some(upload)
                            if upload.bytes.len() as u64 + bytes.len() as u64 > upload.length =>
                        {
                            Err(format!(
                                "transfer sent more than the {} bytes it declared",
                                upload.length
                            ))
                        }
                        Some(upload) => {
                            upload.bytes.extend_from_slice(&bytes);
                            Ok(())
                        }
                    };
                    if let Err(message) = outcome {
                        self.uploads.remove(&(client, request));
                        self.refuse(client, request, message);
                    }
                }
                Effect::FinishUpload {
                    client,
                    request,
                    digest,
                } => self.finish_upload(client, request, &digest),
                Effect::CancelUpload { client, request } => {
                    self.uploads.remove(&(client, request));
                }
                Effect::MarkAttentionSeen { pane } => {
                    if self.attention.mark_seen_for_pane(&pane) {
                        pending.extend(self.attention_changed());
                    }
                }
                Effect::MarkAllAttentionSeen => {
                    if self.attention.mark_all_seen() {
                        pending.extend(self.attention_changed());
                    }
                }
                Effect::ClearSeenAttention => {
                    if self.attention.clear_seen() {
                        pending.extend(self.attention_changed());
                    }
                }
                Effect::SendAttentionHistory { client } => {
                    if let Some(outbox) = self.outboxes.get(&client) {
                        let _ = outbox.send(ServerMessage::AttentionHistory {
                            events: self.attention.events().cloned().collect(),
                        });
                    }
                }
            }
        }
    }

    /// Re-read the durable configuration off the loop. One refresh runs at a
    /// time, so a slow discovery on an unreachable host cannot queue up behind
    /// itself.
    fn start_refresh(&mut self) {
        let Some(path) = self.config_path.clone() else {
            return;
        };
        if self.refresh_inflight {
            return;
        }
        self.refresh_inflight = true;
        let inputs = self.inputs.clone();
        tokio::spawn(async move {
            let refreshed = match Config::load(Some(&path)) {
                Ok((configured, _)) => Some(expand_discovered_sessions(configured).await),
                Err(error) => {
                    // The running federation keeps what it has. A person editing
                    // the file needs to know why their change did nothing, and
                    // this is the same diagnostic the frontend already shows.
                    eprintln!("configuration refresh failed: {error:#}");
                    None
                }
            };
            let _ = inputs.send(Input::Reconfigured(refreshed));
        });
    }

    /// Adopt a refreshed configuration. Supervisors are rebuilt, but Herdr is
    /// never started, stopped, or restarted, and a route whose target did not
    /// change keeps running.
    fn reconfigure(&mut self, refreshed: Option<Config>) -> Vec<Effect> {
        self.refresh_inflight = false;
        let Some(refreshed) = refreshed else {
            return Vec::new();
        };
        if refreshed == self.active {
            return Vec::new();
        }

        let mut effects = Vec::new();
        for pane in invalidated_routes(&self.active, &refreshed, self.routes.keys()) {
            if let Some(route) = self.routes.remove(&pane) {
                route.shutdown();
            }
            effects.extend(self.broker.pane_route_closed(&pane));
        }

        self.targets = target_map(&refreshed);
        self.transport = refreshed.transport.clone();
        self.command_timeout = Duration::from_secs(refreshed.transport.command_timeout_seconds);
        self.active = refreshed;
        self.start_federation();
        effects
    }

    /// Replace the supervisor set. The previous store is shut down off the loop
    /// because stopping it waits on its own tasks, and the daemon must keep
    /// serving its clients meanwhile.
    fn start_federation(&mut self) {
        if let Some(watcher) = self.watcher.take() {
            watcher.abort();
        }
        if let Some(store) = self.store.take() {
            tokio::spawn(async move { store.shutdown().await });
        }
        let store = FederationStore::start(
            self.active.clone(),
            Arc::new(CliSnapshotTransport),
            SupervisorOptions::from_config(&self.active),
        );
        let mut updates = store.subscribe();
        let inputs = self.inputs.clone();
        self.watcher = Some(tokio::spawn(async move {
            loop {
                let state = updates.borrow_and_update().clone();
                if inputs.send(Input::Federation(state)).is_err()
                    || updates.changed().await.is_err()
                {
                    return;
                }
            }
        }));
        self.store = Some(store);
    }

    async fn stop_federation(&mut self) {
        if let Some(watcher) = self.watcher.take() {
            watcher.abort();
        }
        if let Some(store) = self.store.take() {
            store.shutdown().await;
        }
    }

    /// Report a refusal, naming which check failed. Only a missing trailer is
    /// likely to be a dropped connection worth retrying, so the three are not
    /// collapsed into one message.
    fn refuse(&self, client: ClientId, request: u64, message: String) {
        if let Some(outbox) = self.outboxes.get(&client) {
            let _ = outbox.send(ServerMessage::Error {
                request: Some(request),
                message,
            });
        }
    }

    /// Hand one atomic paste to Herdr through the session's own socket, which
    /// is what makes it arrive in one piece with the markers Herdr owns.
    fn paste_pane_text(&mut self, client: ClientId, request: u64, pane: PaneId, text: String) {
        let key = pane.target_session();
        let Some(target) = self.targets.get(&key).cloned() else {
            self.refuse(client, request, format!("{key} is not a configured target"));
            return;
        };
        if target.socket.is_none() {
            self.refuse(
                client,
                request,
                format!("{key} has no known Herdr API socket, so an atomic paste is not available"),
            );
            return;
        }
        let transport = self.transport.clone();
        let timeout = self.command_timeout;
        let inputs = self.inputs.clone();
        let local = pane.server_local_id().to_owned();
        tokio::spawn(async move {
            let (applied, message) =
                match send_pane_input(&target, &transport, &local, &text, timeout).await {
                    Ok(()) => (true, "pasted terminal text".to_owned()),
                    Err(error) => (false, error.message),
                };
            let _ = inputs.send(Input::OperationDone {
                client,
                request,
                applied,
                message,
            });
        });
    }

    /// Verify a completed transfer and move it to the target host.
    fn finish_upload(&mut self, client: ClientId, request: u64, digest: &str) {
        let Some(upload) = self.uploads.remove(&(client, request)) else {
            self.refuse(client, request, "no transfer is in progress".to_owned());
            return;
        };
        if upload.bytes.len() as u64 != upload.length {
            self.refuse(
                client,
                request,
                format!(
                    "transfer ended with {} of the {} bytes it declared",
                    upload.bytes.len(),
                    upload.length
                ),
            );
            return;
        }
        if sha256_hex(&upload.bytes) != digest {
            self.refuse(
                client,
                request,
                "transfer does not match the digest its sender attested to".to_owned(),
            );
            return;
        }
        // The client names a type it saw on its own clipboard; the extension
        // that reaches a remote shell comes from this table and never from the
        // wire.
        let Some(media) = clipboard::KNOWN_MEDIA
            .iter()
            .copied()
            .find(|media| media.mime == upload.mime)
        else {
            self.refuse(
                client,
                request,
                format!(
                    "this daemon cannot upload {}; it carries no extension for that type",
                    bounded_mime(&upload.mime)
                ),
            );
            return;
        };
        let key = upload.pane.target_session();
        let Some(target) = self.targets.get(&key).cloned() else {
            self.refuse(client, request, format!("{key} is not a configured target"));
            return;
        };
        let transport = self.transport.clone();
        let inputs = self.inputs.clone();
        tokio::spawn(async move {
            match clipboard::upload_media(&target, &transport, media, &upload.bytes).await {
                Ok(uploaded) => {
                    let _ = inputs.send(Input::UploadFinished {
                        client,
                        request,
                        result: Ok((uploaded.path, uploaded.bytes as u64)),
                    });
                }
                Err(error) => {
                    let _ = inputs.send(Input::UploadFinished {
                        client,
                        request,
                        result: Err(error.to_string()),
                    });
                }
            }
        });
    }

    fn send_to_route(&self, pane: &PaneId, command: Vec<u8>) {
        if let Some(route) = self.routes.get(pane)
            && let Some(commands) = route.commands.as_ref()
        {
            let _ = commands.send(command);
        }
    }

    /// Open one Herdr terminal route. A route that cannot be opened is reported
    /// as closed rather than left pending, so a client waiting for frames is
    /// told instead of hanging.
    fn open_route(&mut self, pane: PaneId, access: TerminalAccess, cols: u16, rows: u16) {
        if let Some(existing) = self.routes.remove(&pane) {
            existing.shutdown();
        }
        let Some((target, executable)) = self.route_target(&pane) else {
            let _ = self.inputs.send(Input::RouteClosed { pane });
            return;
        };
        // A refused control stream falls back to observation rather than
        // costing the client its view of the pane. Herdr is not asked twice for
        // the same thing: the second attempt asks for less.
        let mut process = match spawn_terminal(
            &target,
            &self.transport,
            &executable,
            &pane,
            access,
            rows,
            cols,
        ) {
            Ok(process) => process,
            Err(_) if access == TerminalAccess::Control => {
                match spawn_terminal(
                    &target,
                    &self.transport,
                    &executable,
                    &pane,
                    TerminalAccess::Observe,
                    rows,
                    cols,
                ) {
                    Ok(process) => {
                        self.downgraded.push(pane.clone());
                        process
                    }
                    Err(_) => {
                        let _ = self.inputs.send(Input::RouteClosed { pane });
                        return;
                    }
                }
            }
            Err(_) => {
                let _ = self.inputs.send(Input::RouteClosed { pane });
                return;
            }
        };
        let output = process.output;
        let inputs = self.inputs.clone();
        let reading = pane.clone();
        let reader = tokio::spawn(async move {
            let mut buffered = BufReader::new(output);
            let mut line = Vec::new();
            loop {
                line.clear();
                match buffered.read_until(b'\n', &mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if line.last() == Some(&b'\n') {
                    line.pop();
                }
                match parse_terminal_event(&line) {
                    Ok(TerminalEvent::Frame {
                        sequence,
                        width,
                        height,
                        full,
                        bytes,
                    }) => {
                        if inputs
                            .send(Input::Frame {
                                pane: reading.clone(),
                                sequence,
                                width,
                                height,
                                full,
                                bytes: Arc::from(bytes),
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Ok(TerminalEvent::Closed) => break,
                    // An unparsable line is not a reason to tear down a working
                    // route; the next frame is authoritative anyway.
                    Err(_) => continue,
                }
            }
            let _ = inputs.send(Input::RouteClosed { pane: reading });
        });

        let (commands, writer) = match process.input.take() {
            Some(mut stdin) => {
                let (sender, mut queued) = mpsc::unbounded_channel::<Vec<u8>>();
                let writer = tokio::spawn(async move {
                    while let Some(command) = queued.recv().await {
                        if stdin.write_all(&command).await.is_err() {
                            break;
                        }
                    }
                });
                (Some(sender), Some(writer))
            }
            None => (None, None),
        };

        self.routes.insert(
            pane,
            Route {
                child: process.child,
                commands,
                reader,
                writer,
            },
        );
    }

    fn route_target(&self, pane: &PaneId) -> Option<(Target, String)> {
        let key = pane.target_session();
        let target = self.targets.get(&key)?.clone();
        let executable = self
            .state
            .targets
            .get(&key)
            .and_then(|runtime| runtime.selected_herdr_bin.clone())?;
        Some((target, executable))
    }

    /// Execute one operation off the loop, so a slow or failing target cannot
    /// stall every other client.
    fn run_operation(&mut self, client: ClientId, request: u64, operation: Operation) {
        let description = operation.description();
        let source_key = operation.target_session();
        let destination_key = operation.destination_session();
        let Some(source) = self.targets.get(&source_key).cloned() else {
            let _ = self.inputs.send(Input::OperationDone {
                client,
                request,
                applied: false,
                message: format!("{source_key} is not a configured target"),
            });
            return;
        };
        let Some(destination) = self.targets.get(&destination_key).cloned() else {
            let _ = self.inputs.send(Input::OperationDone {
                client,
                request,
                applied: false,
                message: format!("{destination_key} is not a configured target"),
            });
            return;
        };
        let executable = self
            .state
            .targets
            .get(&source_key)
            .and_then(|runtime| runtime.selected_herdr_bin.clone());
        let transport = self.transport.clone();
        let timeout = self.command_timeout;
        let inputs = self.inputs.clone();

        tokio::spawn(async move {
            let outcome = execute(
                &operation,
                &source,
                &destination,
                executable.as_deref(),
                &transport,
                timeout,
            )
            .await;
            let (applied, message) = match outcome {
                Ok(detail) => (true, detail.unwrap_or(description)),
                Err(error) => (false, error),
            };
            let _ = inputs.send(Input::OperationDone {
                client,
                request,
                applied,
                message,
            });
        });
    }

    /// Derive attention events from the authoritative state and hand the new
    /// ones to every state subscriber. The index is durable and shared, which
    /// is what lets a phone learn an agent is waiting without the desktop being
    /// awake.
    fn observe_attention(&mut self) -> Vec<Effect> {
        if !self.attention.observe(&self.state) {
            return Vec::new();
        }
        self.persist_attention();
        let cursor = self.attention_cursor;
        let fresh = self
            .attention
            .events()
            .filter(|event| cursor.is_none_or(|last| event.id > last))
            .cloned()
            .collect::<Vec<_>>();
        if let Some(newest) = fresh.last() {
            self.attention_cursor = Some(newest.id);
        }
        fresh
            .into_iter()
            .flat_map(|event| self.broker.attention_observed(event))
            .collect()
    }

    /// Persist the index and republish it, so every attached client sees the
    /// same read state rather than its own guess at the result.
    fn attention_changed(&mut self) -> Vec<Effect> {
        self.persist_attention();
        let events = self.attention.events().cloned().collect();
        self.broker.attention_changed(events)
    }

    fn persist_attention(&self) {
        if let Some(store) = self.attention_store.as_ref() {
            // A failed write costs the next restart its history and nothing
            // else; it must not interrupt supervision.
            let _ = store.save(&self.attention);
        }
    }
}

async fn execute(
    operation: &Operation,
    source: &Target,
    destination: &Target,
    executable: Option<&str>,
    transport: &TransportConfig,
    timeout: Duration,
) -> Result<Option<String>, String> {
    match operation {
        Operation::MoveWorkspace {
            workspace,
            destination: into,
        } => workspace_move::move_workspace(
            source,
            transport,
            workspace.server_local_id(),
            into.server_local_id(),
            timeout,
        )
        .await
        .map(|summary| {
            Some(format!(
                "moved {} tab(s) and {} pane(s)",
                summary.tabs, summary.panes
            ))
        }),
        Operation::RecreateWorkspace {
            workspace, label, ..
        } => workspace_move::recreate_workspace(
            source,
            destination,
            transport,
            workspace.server_local_id(),
            label.as_deref(),
            timeout,
        )
        .await
        .map(|summary| {
            Some(format!(
                "recreated {} tab(s) and {} pane(s) as {}",
                summary.tabs, summary.panes, summary.workspace
            ))
        }),
        single => {
            let args = single
                .herdr_args()
                .ok_or_else(|| "this operation has no single command".to_owned())?;
            let executable =
                executable.ok_or_else(|| "no compatible Herdr client is selected".to_owned())?;
            run_herdr_operation(source, transport, executable, &args, timeout)
                .await
                .map(|()| None)
                .map_err(|error| error.message)
        }
    }
}

/// `Vec` has no queue-shaped pop, and effects must be applied in the order the
/// broker produced them.
trait PopFront<T> {
    fn pop_front_compat(&mut self) -> Option<T>;
}

impl<T> PopFront<T> for Vec<T> {
    fn pop_front_compat(&mut self) -> Option<T> {
        if self.is_empty() {
            None
        } else {
            Some(self.remove(0))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;
    use tokio::task::JoinHandle;

    use super::{DaemonOptions, MAX_UPLOAD_BYTES, invalidated_routes, serve, serve_until};
    use crate::config::{Config, Target};
    use crate::model::PaneId;
    use crate::protocol::{ClientMessage, PROTOCOL_VERSION, ServerMessage, decode, encode};
    use crate::terminal::TerminalAccess;

    /// A federation with no targets exercises the whole I/O path without
    /// starting a single Herdr command.
    fn empty_config() -> Config {
        Config {
            transport: Default::default(),
            notifications: Default::default(),
            targets: Vec::new(),
        }
    }

    struct Harness {
        directory: tempfile::TempDir,
        server: JoinHandle<anyhow::Result<()>>,
    }

    impl Harness {
        async fn start() -> Self {
            let directory = tempfile::tempdir().expect("a temporary directory");
            let options = DaemonOptions {
                socket: directory.path().join("daemon.sock"),
                attention_state: Some(directory.path().join("attention.json")),
                // Pinned: these tests are about the socket, not the file.
                refresh_interval: Duration::from_secs(3600),
                web_port: None,
            };
            let server = tokio::spawn(serve(empty_config(), None, options));
            Self { directory, server }
        }

        fn socket(&self) -> std::path::PathBuf {
            self.directory.path().join("daemon.sock")
        }

        /// The listener is bound before the first await inside `serve`, but the
        /// task still has to be polled; retry rather than sleep for a fixed
        /// time.
        async fn connect(&self) -> Connection {
            for _ in 0..200 {
                if let Ok(stream) = UnixStream::connect(self.socket()).await {
                    return Connection {
                        reader: BufReader::new(stream),
                    };
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("the daemon never accepted a connection");
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    struct Connection {
        reader: BufReader<UnixStream>,
    }

    impl Connection {
        async fn send(&mut self, message: ClientMessage) {
            let line = encode(&message).expect("a message encodes");
            self.reader
                .get_mut()
                .write_all(&line)
                .await
                .expect("the daemon accepts the message");
        }

        async fn receive(&mut self) -> Option<ServerMessage> {
            let mut line = Vec::new();
            let read = tokio::time::timeout(
                Duration::from_secs(5),
                self.reader.read_until(b'\n', &mut line),
            )
            .await
            .expect("the daemon answers within the timeout")
            .expect("the connection stays readable");
            if read == 0 {
                return None;
            }
            line.pop();
            Some(decode(&line).expect("the daemon sends a valid message"))
        }

        /// Read past whatever else the daemon is saying — a lease, a route that
        /// could not open — until the transfer reports.
        async fn transfer_result(&mut self) -> ServerMessage {
            loop {
                match self.receive().await.expect("the daemon answers") {
                    message @ (ServerMessage::Error { .. }
                    | ServerMessage::UploadComplete { .. }) => return message,
                    _ => continue,
                }
            }
        }

        async fn hello(&mut self) -> ServerMessage {
            self.send(ClientMessage::Hello {
                protocol: PROTOCOL_VERSION,
                client: "test".to_owned(),
            })
            .await;
            self.receive().await.expect("a greeting")
        }
    }

    fn target_named(name: &str, ssh: Option<&str>) -> Target {
        Target {
            name: name.to_owned(),
            ssh: ssh.map(str::to_owned),
            discover_sessions: false,
            session: None,
            socket: None,
            herdr_bins: vec!["/nonexistent/herdr".to_owned()],
        }
    }

    fn config_with(targets: Vec<Target>) -> Config {
        Config {
            targets,
            ..empty_config()
        }
    }

    #[test]
    fn a_refresh_that_leaves_a_target_alone_keeps_its_route() {
        let previous = config_with(vec![
            target_named("first", None),
            target_named("second", None),
        ]);
        let next = config_with(vec![
            target_named("first", None),
            target_named("second", Some("build-host")),
        ]);
        let routes = [
            PaneId::new("first", "default", "w1:p1"),
            PaneId::new("second", "default", "w1:p1"),
        ];

        // Only the target that actually changed loses its route.
        assert_eq!(
            invalidated_routes(&previous, &next, routes.iter()),
            vec![PaneId::new("second", "default", "w1:p1")]
        );
        // Re-reading the same file changes nothing.
        assert!(invalidated_routes(&previous, &previous, routes.iter()).is_empty());
    }

    #[test]
    fn a_removed_target_and_a_changed_transport_invalidate_routes() {
        let previous = config_with(vec![
            target_named("first", None),
            target_named("second", None),
        ]);
        let routes = [
            PaneId::new("first", "default", "w1:p1"),
            PaneId::new("second", "default", "w1:p1"),
        ];

        let removed = config_with(vec![target_named("first", None)]);
        assert_eq!(
            invalidated_routes(&previous, &removed, routes.iter()),
            vec![PaneId::new("second", "default", "w1:p1")]
        );

        let mut retimed = previous.clone();
        retimed.transport.connect_timeout_seconds += 1;
        assert_eq!(
            invalidated_routes(&previous, &retimed, routes.iter()).len(),
            routes.len(),
            "a changed transport rebuilds every route command"
        );
    }

    #[tokio::test]
    async fn a_target_added_to_the_file_appears_without_a_restart() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("config.toml");
        std::fs::write(
            &path,
            "[[targets]]\nname = \"first\"\nherdr_bins = [\"/nonexistent/herdr\"]\n",
        )
        .expect("the configuration is written");
        let (config, _) = Config::load(Some(&path)).expect("the configuration loads");
        let socket = directory.path().join("daemon.sock");
        let server = tokio::spawn(serve(
            config,
            Some(path.clone()),
            DaemonOptions {
                socket: socket.clone(),
                attention_state: Some(directory.path().join("attention.json")),
                refresh_interval: Duration::from_millis(50),
                web_port: None,
            },
        ));

        let mut connection = None;
        for _ in 0..200 {
            if let Ok(stream) = UnixStream::connect(&socket).await {
                connection = Some(Connection {
                    reader: BufReader::new(stream),
                });
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut connection = connection.expect("the daemon accepts a connection");
        connection.hello().await;
        connection.send(ClientMessage::SubscribeState).await;

        std::fs::write(
            &path,
            "[[targets]]\nname = \"first\"\nherdr_bins = [\"/nonexistent/herdr\"]\n\n\
             [[targets]]\nname = \"second\"\nherdr_bins = [\"/nonexistent/herdr\"]\n",
        )
        .expect("the configuration is rewritten");

        let found = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match connection.receive().await {
                    Some(ServerMessage::TargetState { target, .. })
                        if target.target == "second" =>
                    {
                        return true;
                    }
                    Some(_) => continue,
                    None => return false,
                }
            }
        })
        .await
        .expect("the daemon reports the new target within the timeout");

        server.abort();
        assert!(found, "the added target reached a subscribed client");
    }

    #[tokio::test]
    async fn a_client_shakes_hands_and_subscribes_over_a_real_socket() {
        let harness = Harness::start().await;
        let mut connection = harness.connect().await;

        assert!(matches!(
            connection.hello().await,
            ServerMessage::Hello { protocol, .. } if protocol == PROTOCOL_VERSION
        ));

        connection.send(ClientMessage::SubscribeState).await;
        assert!(matches!(
            connection.receive().await,
            Some(ServerMessage::FederationState { .. })
        ));
    }

    #[tokio::test]
    async fn the_socket_is_restricted_to_its_owner() {
        use std::os::unix::fs::PermissionsExt;

        let harness = Harness::start().await;
        harness.connect().await;

        let mode = std::fs::metadata(harness.socket())
            .expect("the socket exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn a_connection_that_skips_the_handshake_is_closed() {
        let harness = Harness::start().await;
        let mut connection = harness.connect().await;

        connection.send(ClientMessage::SubscribeState).await;

        assert!(matches!(
            connection.receive().await,
            Some(ServerMessage::Error { request: None, .. })
        ));
        assert!(
            connection.receive().await.is_none(),
            "the daemon closes the connection after refusing it"
        );
    }

    /// Drive one upload through a real daemon and read what it says back.
    async fn upload(
        connection: &mut Connection,
        mime: &str,
        declared: u64,
        chunks: &[&[u8]],
        digest: Option<&str>,
    ) -> ServerMessage {
        // A lease dies with its route, and no route can open without a Herdr
        // server, so each attempt takes the lease again.
        let pane = leased_pane(connection).await;
        connection
            .send(ClientMessage::BeginUpload {
                request: 1,
                pane,
                mime: mime.to_owned(),
                length: declared,
            })
            .await;
        for chunk in chunks {
            connection
                .send(ClientMessage::UploadChunk {
                    request: 1,
                    bytes: chunk.to_vec(),
                })
                .await;
        }
        if let Some(digest) = digest {
            connection
                .send(ClientMessage::FinishUpload {
                    request: 1,
                    digest: digest.to_owned(),
                })
                .await;
        }
        connection.transfer_result().await
    }

    /// A pane the daemon will accept a lease for without a Herdr server behind
    /// it: subscribing grants the lease locally, and the route failing after
    /// that does not retract it.
    async fn leased_pane(connection: &mut Connection) -> PaneId {
        let pane = PaneId::new("first", "default", "w1:p1");
        connection
            .send(ClientMessage::SubscribePane {
                pane: pane.clone(),
                access: TerminalAccess::Control,
                cols: 80,
                rows: 24,
            })
            .await;
        pane
    }

    #[tokio::test]
    async fn a_transfer_is_refused_for_each_check_it_fails() {
        let harness = Harness::start().await;
        let mut connection = harness.connect().await;
        connection.hello().await;
        // Declared above the ceiling: refused before a byte moves.
        let refusal = upload(
            &mut connection,
            "image/png",
            MAX_UPLOAD_BYTES + 1,
            &[],
            None,
        )
        .await;
        assert!(
            matches!(&refusal, ServerMessage::Error { message, .. } if message.contains("limit")),
            "{refusal:?}"
        );

        // More bytes than declared: stopped rather than believed.
        let refusal = upload(&mut connection, "image/png", 2, &[b"four".as_slice()], None).await;
        assert!(
            matches!(&refusal, ServerMessage::Error { message, .. } if message.contains("declared")),
            "{refusal:?}"
        );

        // Fewer bytes than declared, with a trailer: the length is checked
        // even though a digest arrived.
        let refusal = upload(
            &mut connection,
            "image/png",
            8,
            &[b"two".as_slice()],
            Some("whatever"),
        )
        .await;
        assert!(
            matches!(&refusal, ServerMessage::Error { message, .. } if message.contains("declared")),
            "{refusal:?}"
        );

        // A digest that does not match what was sent.
        let refusal = upload(
            &mut connection,
            "image/png",
            4,
            &[b"four".as_slice()],
            Some("0000000000000000000000000000000000000000000000000000000000000000"),
        )
        .await;
        assert!(
            matches!(&refusal, ServerMessage::Error { message, .. } if message.contains("digest")),
            "{refusal:?}"
        );

        // A type the daemon carries no extension for.
        let refusal = upload(
            &mut connection,
            "application/x-invented",
            4,
            &[b"four".as_slice()],
            Some(&super::sha256_hex(b"four")),
        )
        .await;
        assert!(
            matches!(&refusal, ServerMessage::Error { message, .. } if message.contains("extension")),
            "{refusal:?}"
        );
    }

    #[tokio::test]
    async fn an_abandoned_transfer_cannot_be_finished_later() {
        let harness = Harness::start().await;
        let mut connection = harness.connect().await;
        connection.hello().await;
        let pane = leased_pane(&mut connection).await;

        connection
            .send(ClientMessage::BeginUpload {
                request: 1,
                pane,
                mime: "image/png".to_owned(),
                length: 4,
            })
            .await;
        connection
            .send(ClientMessage::CancelUpload { request: 1 })
            .await;
        connection
            .send(ClientMessage::FinishUpload {
                request: 1,
                digest: super::sha256_hex(b"four"),
            })
            .await;

        let refusal = connection.transfer_result().await;
        assert!(
            matches!(&refusal, ServerMessage::Error { message, .. } if message.contains("no transfer")),
            "{refusal:?}"
        );
    }

    #[tokio::test]
    async fn a_stopped_daemon_takes_its_socket_with_it() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let socket = directory.path().join("daemon.sock");
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_until(
            empty_config(),
            None,
            DaemonOptions {
                socket: socket.clone(),
                attention_state: Some(directory.path().join("attention.json")),
                refresh_interval: Duration::from_secs(3600),
                web_port: None,
            },
            async move {
                let _ = stopped.await;
            },
        ));

        for _ in 0..200 {
            if UnixStream::connect(&socket).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(socket.exists(), "the daemon bound its socket");

        let _ = stop.send(());
        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("the daemon stops when asked")
            .expect("the task completes")
            .expect("serving ends without error");

        assert!(
            !socket.exists(),
            "a stopped daemon leaves no socket for the next start to interpret"
        );
    }

    #[tokio::test]
    async fn a_second_daemon_refuses_to_evict_the_first() {
        let harness = Harness::start().await;
        harness.connect().await;

        let options = DaemonOptions {
            socket: harness.socket(),
            attention_state: Some(harness.directory.path().join("attention.json")),
            refresh_interval: Duration::from_secs(3600),
            web_port: None,
        };
        let error = serve(empty_config(), None, options)
            .await
            .expect_err("the second daemon refuses to bind");

        assert!(error.to_string().contains("already listening"));
    }

    #[tokio::test]
    async fn a_stale_socket_is_replaced() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let socket = directory.path().join("daemon.sock");
        // A path left behind by a process that did not clean up: it exists, but
        // nothing is listening.
        std::fs::write(&socket, b"").expect("the leftover path is created");

        let server = tokio::spawn(serve(
            empty_config(),
            None,
            DaemonOptions {
                socket: socket.clone(),
                attention_state: Some(directory.path().join("attention.json")),
                refresh_interval: Duration::from_secs(3600),
                web_port: None,
            },
        ));

        for _ in 0..200 {
            if UnixStream::connect(&socket).await.is_ok() {
                server.abort();
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        server.abort();
        panic!("the daemon never replaced the stale socket");
    }
}
