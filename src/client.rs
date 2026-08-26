//! The client side of the daemon protocol.
//!
//! A frontend talks to the federation only through this module, whether the
//! daemon runs in its own process or inside this one. Both cases speak the same
//! protocol over the same framing; the only difference is whether the bytes
//! cross a socket or an in-memory pipe.
//!
//! Federation state is republished as a `watch::Receiver<FederationState>` —
//! the shape a frontend already consumes — so a renderer does not have to know
//! that state now arrives as a snapshot followed by per-target deltas. Anything
//! that is not state is forwarded verbatim, because frames, leases, and results
//! are events a frontend must see in order rather than a value it can sample.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::daemon::server::DaemonHandle;
use crate::model::PaneId;
use crate::operation::Operation;
use crate::protocol::{
    ClientMessage, MAX_MESSAGE_BYTES, PROTOCOL_VERSION, PaneRepresentation, ServerMessage, decode,
    encode,
};
use crate::state::FederationState;
use crate::terminal::{TerminalAccess, TerminalScrollDirection};

/// How much of a payload travels in one message.
///
/// Comfortably inside the protocol's own bound once base64 has inflated it, so
/// a large clipboard image becomes many ordinary messages rather than one the
/// far side must refuse.
pub const UPLOAD_CHUNK_BYTES: usize = 512 * 1024;

/// A connection to a daemon.
///
/// Dropping this ends the connection: the daemon releases every lease and route
/// this client held.
pub struct Client {
    commands: ClientCommands,
    state: watch::Receiver<FederationState>,
    tasks: Vec<JoinHandle<()>>,
}

/// A cheap handle for sending commands, cloneable so a frontend can keep one
/// beside the rest of its state instead of threading a connection through every
/// function that might type into a pane.
#[derive(Clone)]
pub struct ClientCommands {
    commands: mpsc::UnboundedSender<ClientMessage>,
    next_request: Arc<AtomicU64>,
}

impl Drop for Client {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Everything the daemon says that is not federation state.
pub type Events = mpsc::UnboundedReceiver<ServerMessage>;

impl Client {
    /// Attach to a daemon running inside this process.
    pub async fn attach(daemon: &DaemonHandle, name: &str) -> Result<(Self, Events)> {
        Self::connect(daemon.attach()?, name).await
    }

    /// Connect to a daemon over its Unix socket.
    pub async fn connect_socket(path: &Path, name: &str) -> Result<(Self, Events)> {
        let stream = UnixStream::connect(path).await.with_context(|| {
            format!("failed to reach a Super-Herdr daemon at {}", path.display())
        })?;
        Self::connect(stream, name).await
    }

    async fn connect<S>(stream: S, name: &str) -> Result<(Self, Events)>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        let (reader, mut writer) = tokio::io::split(stream);
        let hello = encode(&ClientMessage::Hello {
            protocol: PROTOCOL_VERSION,
            client: name.to_owned(),
        })?;
        writer
            .write_all(&hello)
            .await
            .context("failed to greet the daemon")?;

        let mut buffered = BufReader::new(reader);
        let mut line = Vec::new();
        // The daemon answers a handshake before anything else, so a mismatch is
        // an error here rather than a surprise several messages later.
        let read = (&mut buffered)
            .take(MAX_MESSAGE_BYTES as u64)
            .read_until(b'\n', &mut line)
            .await
            .context("failed to read the daemon's greeting")?;
        if read == 0 || line.last() != Some(&b'\n') {
            anyhow::bail!("the daemon closed the connection during the handshake");
        }
        line.pop();
        match decode::<ServerMessage>(&line)? {
            ServerMessage::Hello { protocol, .. } if protocol == PROTOCOL_VERSION => {}
            ServerMessage::Hello { protocol, .. } => anyhow::bail!(
                "the daemon speaks protocol {protocol}; this client speaks {PROTOCOL_VERSION}"
            ),
            ServerMessage::Error { message, .. } => {
                anyhow::bail!("the daemon refused this connection: {message}")
            }
            _ => anyhow::bail!("the daemon answered the handshake with something else"),
        }

        let (commands, mut outgoing) = mpsc::unbounded_channel::<ClientMessage>();
        let sending = tokio::spawn(async move {
            while let Some(message) = outgoing.recv().await {
                let Ok(line) = encode(&message) else {
                    continue;
                };
                if writer.write_all(&line).await.is_err() {
                    break;
                }
            }
        });

        let (state, state_receiver) = watch::channel(FederationState::default());
        let (events, event_receiver) = mpsc::unbounded_channel();
        let receiving = tokio::spawn(async move {
            let mut line = Vec::new();
            loop {
                line.clear();
                let read = (&mut buffered)
                    .take(MAX_MESSAGE_BYTES as u64)
                    .read_until(b'\n', &mut line)
                    .await;
                match read {
                    Ok(0) | Err(_) => break,
                    Ok(_) if line.last() != Some(&b'\n') => break,
                    Ok(_) => {}
                }
                line.pop();
                let Ok(message) = decode::<ServerMessage>(&line) else {
                    break;
                };
                match message {
                    ServerMessage::FederationState { state: whole } => {
                        state.send_replace(whole);
                    }
                    ServerMessage::TargetState { target, state: one } => {
                        state.send_modify(|federation| {
                            match one {
                                Some(one) => federation.targets.insert(target, one),
                                None => federation.targets.remove(&target),
                            };
                            // The revision is this client's own count of
                            // applied changes; it is never the daemon's, and
                            // nothing may treat the two as comparable.
                            federation.revision = federation.revision.saturating_add(1);
                        });
                    }
                    other => {
                        if events.send(other).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok((
            Self {
                commands: ClientCommands {
                    commands,
                    next_request: Arc::new(AtomicU64::new(0)),
                },
                state: state_receiver,
                tasks: vec![sending, receiving],
            },
            event_receiver,
        ))
    }

    /// Federation state, in the shape a frontend already consumes.
    pub fn state(&self) -> watch::Receiver<FederationState> {
        self.state.clone()
    }

    pub fn commands(&self) -> ClientCommands {
        self.commands.clone()
    }

    pub fn subscribe_state(&self) {
        self.commands.subscribe_state();
    }
}

impl ClientCommands {
    pub fn subscribe_state(&self) {
        self.send(ClientMessage::SubscribeState);
    }

    /// Subscribe for encoded frames, which is what a client with its own
    /// emulator wants and what every caller here wants today.
    pub fn subscribe_pane(&self, pane: PaneId, access: TerminalAccess, cols: u16, rows: u16) {
        self.subscribe_pane_as(pane, access, cols, rows, PaneRepresentation::Frames);
    }

    /// Subscribe naming the representation. Separate from `subscribe_pane`
    /// rather than a fifth argument on it: the emulator-owning case is the
    /// common one, and making every caller restate it would obscure the rare
    /// client that genuinely cannot parse.
    pub fn subscribe_pane_as(
        &self,
        pane: PaneId,
        access: TerminalAccess,
        cols: u16,
        rows: u16,
        representation: PaneRepresentation,
    ) {
        self.send(ClientMessage::SubscribePane {
            pane,
            access,
            cols,
            rows,
            representation,
        });
    }

    pub fn unsubscribe_pane(&self, pane: PaneId) {
        self.send(ClientMessage::UnsubscribePane { pane });
    }

    pub fn take_pane_control(&self, pane: PaneId) {
        self.send(ClientMessage::TakePaneControl { pane });
    }

    pub fn send_input(&self, pane: PaneId, bytes: Vec<u8>) {
        self.send(ClientMessage::PaneInput { pane, bytes });
    }

    pub fn resize_pane(&self, pane: PaneId, cols: u16, rows: u16) {
        self.send(ClientMessage::PaneResize { pane, cols, rows });
    }

    pub fn scroll_pane(
        &self,
        pane: PaneId,
        direction: TerminalScrollDirection,
        lines: u16,
        column: u16,
        row: u16,
        modifiers: u8,
    ) {
        self.send(ClientMessage::PaneScroll {
            pane,
            direction,
            lines,
            column,
            row,
            modifiers,
        });
    }

    /// Ask the daemon to run one operation, returning the request identifier
    /// its result will carry.
    pub fn run_operation(&self, operation: Operation) -> u64 {
        let request = self.next_request();
        self.send(ClientMessage::RunOperation { request, operation });
        request
    }

    /// Ask the daemon to deliver text to a pane as one atomic paste.
    pub fn paste_pane_text(&self, pane: PaneId, text: String) -> u64 {
        let request = self.next_request();
        self.send(ClientMessage::PastePaneText {
            request,
            pane,
            text,
        });
        request
    }

    /// Offer a clipboard payload for upload, chunked to stay inside the message
    /// bound, with the digest computed over the bytes actually sent.
    ///
    /// Sent whole rather than streamed because a clipboard payload is already
    /// in memory; a device file that is not would compute its digest while
    /// sending instead, which is why the trailer carries it either way.
    pub fn upload_media(&self, pane: PaneId, mime: String, bytes: &[u8]) -> u64 {
        self.offer_upload(pane, mime, None, bytes)
    }

    /// Offer a file under a name, for a caller holding one that has a name
    /// worth keeping.
    ///
    /// The name is what separates this from a clipboard payload: a screenshot
    /// is bytes and a type, while a file is bytes and something a person will
    /// look for afterwards. It is checked by the daemon before anything opens,
    /// and refused rather than adjusted, so a caller learns its file would have
    /// been called something else instead of discovering it later.
    pub fn upload_file(&self, pane: PaneId, name: String, mime: String, bytes: &[u8]) -> u64 {
        self.offer_upload(pane, mime, Some(name), bytes)
    }

    /// Continue a transfer that stopped, under the token it was given.
    ///
    /// Chunks do not follow immediately: the daemon answers with
    /// `upload.accepted`, and its `staged` is the offset the next byte belongs
    /// at. It comes from the host rather than from anything either side
    /// remembered, so a caller sends from there rather than from where it
    /// believes it stopped.
    pub fn resume_upload(&self, pane: PaneId, transfer: String, length: u64) -> u64 {
        let request = self.next_request();
        self.send(ClientMessage::ResumeUpload {
            request,
            transfer,
            pane,
            length,
        });
        request
    }

    /// Send part of a transfer that is already under way.
    ///
    /// Separate from the one-shot helpers because a resumed transfer's bytes
    /// are decided by an answer that has not arrived yet.
    pub fn send_upload_chunk(&self, request: u64, bytes: Vec<u8>) {
        self.send(ClientMessage::UploadChunk { request, bytes });
    }

    /// Attest to the whole content, including anything an earlier attempt
    /// delivered. The digest spans the file, not the attempt: it is compared
    /// against what the host computed over what it stored.
    pub fn finish_upload(&self, request: u64, digest: String) {
        self.send(ClientMessage::FinishUpload { request, digest });
    }

    pub fn cancel_upload(&self, request: u64) {
        self.send(ClientMessage::CancelUpload { request });
    }

    fn offer_upload(&self, pane: PaneId, mime: String, name: Option<String>, bytes: &[u8]) -> u64 {
        use sha2::{Digest, Sha256};

        let request = self.next_request();
        self.send(ClientMessage::BeginUpload {
            request,
            pane,
            mime,
            name,
            length: bytes.len() as u64,
        });
        let mut hasher = Sha256::new();
        for chunk in bytes.chunks(UPLOAD_CHUNK_BYTES) {
            hasher.update(chunk);
            self.send(ClientMessage::UploadChunk {
                request,
                bytes: chunk.to_vec(),
            });
        }
        let digest = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        self.send(ClientMessage::FinishUpload { request, digest });
        request
    }

    /// Ask for a file on the pane's host.
    ///
    /// Nothing arrives until it is pulled. The daemon answers with
    /// `download.offer` — a length and the host's digest — and then waits,
    /// because the queue to a client is unbounded and a file that outran a slow
    /// link would otherwise sit in the daemon's memory.
    pub fn begin_download(&self, pane: PaneId, path: String) -> u64 {
        let request = self.next_request();
        self.send(ClientMessage::BeginDownload {
            request,
            pane,
            path,
        });
        request
    }

    /// Allow the daemon to send up to this many more chunks.
    ///
    /// A caller grants a window at the start and tops it up as it consumes, so
    /// what is in flight is bounded by what it asked for rather than by the
    /// size of the file.
    pub fn pull_download(&self, request: u64, chunks: u32) {
        self.send(ClientMessage::PullDownload { request, chunks });
    }

    pub fn cancel_download(&self, request: u64) {
        self.send(ClientMessage::CancelDownload { request });
    }

    /// Move a file from one target to another without it touching this device.
    ///
    /// The daemon holds both connections, so the bytes never come here. What
    /// comes back is the destination's staged path, once the destination's own
    /// digest matches the source's.
    pub fn transfer_between(
        &self,
        source: PaneId,
        path: String,
        destination: PaneId,
        name: Option<String>,
    ) -> u64 {
        let request = self.next_request();
        self.send(ClientMessage::TransferBetween {
            request,
            source,
            path,
            destination,
            name,
        });
        request
    }

    /// Ask for a pairing code to show a person.
    pub fn request_pairing_code(&self) -> u64 {
        let request = self.next_request();
        self.send(ClientMessage::RequestPairingCode { request });
        request
    }

    pub fn decide_pairing(&self, attempt: String, approve: bool) {
        self.send(ClientMessage::DecidePairing { attempt, approve });
    }

    pub fn mark_attention_seen(&self, pane: PaneId) {
        self.send(ClientMessage::MarkAttentionSeen { pane });
    }

    pub fn mark_all_attention_seen(&self) {
        self.send(ClientMessage::MarkAllAttentionSeen);
    }

    pub fn clear_seen_attention(&self) {
        self.send(ClientMessage::ClearSeenAttention);
    }

    /// Requests share one counter, so a result can be matched to whatever asked
    /// for it without knowing which kind of request it was.
    fn next_request(&self) -> u64 {
        self.next_request.fetch_add(1, Ordering::Relaxed)
    }

    /// Commands are fire-and-forget. A dead connection is reported by the event
    /// stream ending, so every call site does not have to handle it.
    fn send(&self, message: ClientMessage) {
        let _ = self.commands.send(message);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::Client;
    use crate::config::{Config, Target};
    use crate::daemon::server::{DaemonHandle, DaemonOptions, spawn_in_process};
    use crate::model::PaneId;
    use crate::model::TargetSession;
    use crate::protocol::ServerMessage;
    use crate::terminal::TerminalAccess;

    fn target_named(name: &str) -> Target {
        Target {
            name: name.to_owned(),
            ssh: None,
            discover_sessions: false,
            session: None,
            socket: None,
            herdr_bins: vec!["/nonexistent/herdr".to_owned()],
        }
    }

    fn daemon_with(targets: Vec<Target>) -> (DaemonHandle, tempfile::TempDir) {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let config = Config {
            transport: Default::default(),
            notifications: Default::default(),
            transfers: Default::default(),
            web: Default::default(),
            targets,
            devices: Vec::new(),
        };
        let options = DaemonOptions {
            // No socket is bound in this mode; the path is never used.
            socket: directory.path().join("unused.sock"),
            attention_state: Some(directory.path().join("attention.json")),
            refresh_interval: Duration::from_secs(3600),
            web_port: None,
            web_address: None,
            web_url: None,
            web_bridge: None,
        };
        (spawn_in_process(config, None, options), directory)
    }

    /// Wait for the mirror to satisfy a condition, so a test never depends on
    /// how many deltas the daemon needed to get there.
    async fn wait_for(
        state: &mut tokio::sync::watch::Receiver<crate::state::FederationState>,
        mut ready: impl FnMut(&crate::state::FederationState) -> bool,
    ) -> bool {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if ready(&state.borrow_and_update()) {
                    return true;
                }
                if state.changed().await.is_err() {
                    return false;
                }
            }
        })
        .await
        .unwrap_or(false)
    }

    #[tokio::test]
    async fn a_frontend_hosting_its_own_daemon_needs_no_socket() {
        let (daemon, directory) = daemon_with(vec![target_named("first")]);

        let (client, _events) = Client::attach(&daemon, "test").await.expect("attaches");
        client.subscribe_state();
        let mut state = client.state();

        assert!(
            wait_for(&mut state, |state| {
                state
                    .targets
                    .contains_key(&TargetSession::new("first", "default"))
            })
            .await
        );
        assert!(
            !directory.path().join("unused.sock").exists(),
            "an in-process daemon binds nothing"
        );
    }

    /// A clipboard image reaches a local target through a daemon this process
    /// is hosting.
    ///
    /// The path a single-machine install actually takes, and the one nothing
    /// covered: every other transfer test drives a daemon over a socket, while
    /// a frontend hosting its own daemon speaks through a 64 KiB in-memory pipe
    /// and sends 512 KiB chunks into it.
    #[tokio::test]
    async fn a_clipboard_upload_crosses_an_in_process_daemon() {
        let (daemon, directory) = daemon_with(vec![Target {
            name: "first".to_owned(),
            // The sink is this machine, which is what "native" means here.
            ssh: None,
            discover_sessions: false,
            session: None,
            socket: None,
            herdr_bins: vec!["/nonexistent/herdr".to_owned()],
        }]);
        let _ = directory;

        let (client, mut events) = Client::attach(&daemon, "test").await.expect("attaches");
        let commands = client.commands();
        let pane = PaneId::new("first", "default", "w1:p1");
        commands.subscribe_pane(pane.clone(), TerminalAccess::Control, 80, 24);

        // Bigger than one chunk and bigger than the pipe between the two.
        let mut payload = b"\x89PNG\r\n\x1a\n".to_vec();
        payload.extend((0..900_000_u32).map(|index| index as u8));
        commands.upload_media(pane, "image/png".to_owned(), &payload);

        let result = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match events.recv().await {
                    Some(ServerMessage::UploadComplete { path, bytes, .. }) => {
                        return Some((path, bytes));
                    }
                    Some(ServerMessage::Error { message, .. }) => panic!("{message}"),
                    Some(_) => continue,
                    None => return None,
                }
            }
        })
        .await
        .expect("an in-process upload finishes rather than wedging");

        let (path, bytes) = result.expect("the daemon answers");
        assert_eq!(bytes as usize, payload.len());
        assert_eq!(std::fs::read(&path).unwrap(), payload);
        crate::clipboard::discard_local_upload(std::path::Path::new(&path));
    }

    #[tokio::test]
    async fn a_snapshot_and_its_deltas_rebuild_the_same_federation() {
        let (daemon, _directory) = daemon_with(vec![target_named("first"), target_named("second")]);

        let (client, _events) = Client::attach(&daemon, "test").await.expect("attaches");
        client.subscribe_state();
        let mut state = client.state();

        // Both targets arrive, and every mirrored entry is filed under its own
        // qualified key rather than whatever the message happened to say.
        assert!(
            wait_for(&mut state, |state| state.targets.len() == 2).await,
            "the mirror holds both targets"
        );
        let mirrored = state.borrow().clone();
        for (key, target) in &mirrored.targets {
            assert_eq!(&target.key, key);
        }
        assert!(
            mirrored
                .targets
                .contains_key(&TargetSession::new("second", "default"))
        );
    }

    #[tokio::test]
    async fn two_frontends_share_one_daemon() {
        let (daemon, _directory) = daemon_with(vec![target_named("first")]);

        let (first, _first_events) = Client::attach(&daemon, "first").await.expect("attaches");
        let (second, _second_events) = Client::attach(&daemon, "second").await.expect("attaches");
        first.subscribe_state();
        second.subscribe_state();

        let (mut one, mut two) = (first.state(), second.state());
        assert!(wait_for(&mut one, |state| !state.targets.is_empty()).await);
        assert!(wait_for(&mut two, |state| !state.targets.is_empty()).await);
    }

    #[tokio::test]
    async fn a_client_that_goes_away_does_not_stop_the_daemon() {
        let (daemon, _directory) = daemon_with(vec![target_named("first")]);

        {
            let (client, _events) = Client::attach(&daemon, "leaving").await.expect("attaches");
            client.subscribe_state();
        }

        let (client, _events) = Client::attach(&daemon, "arriving").await.expect("attaches");
        client.subscribe_state();
        let mut state = client.state();
        assert!(wait_for(&mut state, |state| !state.targets.is_empty()).await);
    }
}
