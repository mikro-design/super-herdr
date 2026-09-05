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

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::future::Future;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream,
    ReadBuf,
};
use tokio::net::UnixListener;
use tokio::process::Child;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::agent_card::{AgentCardIndex, agent_key_for_pane};
use crate::agent_marks::{AgentMarkStore, AgentMarks};
use crate::attention::{AttentionEventKind, AttentionIndex, AttentionStore, unix_time_ms};
use crate::clipboard;
use crate::config::{Config, Device, Target, TransportConfig};
use crate::daemon::broker::{Broker, ClientId, Effect};
use crate::daemon::web;
use crate::model::{PaneId, TargetSession};
use crate::notifications::NotificationQueue;
use crate::operation::Operation;
use crate::pairing::{self, PendingPairing};
use crate::plugin;
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

/// How often the device sink looks for a delivery whose coalescing window has
/// closed. The window is half a second, so an alert waits at most one tick
/// past the moment it became due.
const NOTIFICATION_TICK: Duration = Duration::from_millis(500);
const MAX_PENDING_PAIRING_APPROVALS: usize = 16;

#[derive(Debug, Clone)]
pub struct DaemonOptions {
    pub socket: PathBuf,
    /// Where the durable agent marks live. `None` discovers the standard
    /// location; a test points this somewhere disposable.
    pub agent_marks: Option<PathBuf>,
    /// Where the durable attention index lives. `None` discovers the standard
    /// state path.
    pub attention_state: Option<PathBuf>,
    pub refresh_interval: Duration,
    /// Serve the browser client on this loopback port. `None` serves no web
    /// client at all, which is the default: a daemon should not open a port
    /// nobody asked for.
    pub web_port: Option<u16>,
    /// Where the browser client listens. `None` is loopback.
    pub web_address: Option<std::net::IpAddr>,
    /// The address a device outside this machine reaches the browser client on.
    ///
    /// The resolver supplies either the hosted bridge route, an explicit
    /// operator URL, or a direct private/mesh route. `None` means a client
    /// shows the pairing code alone.
    pub web_url: Option<String>,
    /// Outbound public route for the loopback web listener. Its registration
    /// secret is memory-only and redacted by the route's Debug implementation.
    pub web_bridge: Option<crate::bridge::Route>,
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
            agent_marks: None,
            attention_state: None,
            refresh_interval: CONFIG_REFRESH_INTERVAL,
            web_port: None,
            web_address: None,
            web_url: None,
            web_bridge: None,
        })
    }
}

/// Everything the broker loop reacts to, from clients and from the federation
/// alike, so ordering is decided in one place.
enum Input {
    /// A coalescing window may have closed.
    NotificationsDue,
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
        plugin_run: Option<plugin::PluginRun>,
    },
    /// A transfer's chunks, handed over by the connection that will carry them.
    ///
    /// It arrives immediately before the `upload.begin` it belongs to, because
    /// the connection creates the queue in order to be able to block on it. The
    /// rules about whether the transfer may happen at all are still the
    /// broker's, and a refusal drops this end of the queue.
    UploadOffered {
        client: ClientId,
        request: u64,
        chunks: mpsc::Receiver<RelayItem>,
    },
    /// One attempt at a transfer ended, however it ended.
    RelayFinished {
        client: ClientId,
        request: u64,
        transfer: String,
        outcome: Relayed,
    },
    /// A download finished, failed, or ran out of client. Either way the daemon
    /// stops holding it.
    DownloadEnded {
        client: ClientId,
        request: u64,
    },
    /// The host has been asked what it already holds, and a resuming sender can
    /// be told where to continue from.
    ResumeOffset {
        client: ClientId,
        request: u64,
        transfer: String,
        staged: u64,
    },
    /// A browser proved knowledge of the short code. The durable device is not
    /// created until a trusted client compares its confirmation number.
    PairingRequested {
        attempt: String,
        name: String,
        confirmation: String,
        expires_at: SystemTime,
        decision: oneshot::Sender<std::result::Result<String, String>>,
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

/// How many chunks a relay holds between the connection carrying a transfer
/// and the target taking it.
///
/// A queue rather than a buffer: when it fills, the connection stops reading,
/// which stops the client writing, which is the only backpressure that reaches
/// the sender at all. Four of the client's 512 KiB chunks keep a
/// screenshot-sized payload from ever stalling, while a genuinely large file
/// waits for the target instead of accumulating here.
///
/// The cost is that a connection moving a large file is not reading its own
/// other messages meanwhile. One ordered stream per connection is what makes
/// that so, and it is the transfer's own client that waits for it.
const RELAY_DEPTH: usize = 4;

/// How much of a downloaded file travels in one message.
///
/// The same size the client's own uploads use, and for the same reason: it sits
/// comfortably inside the protocol's message bound once base64 has inflated it.
const DOWNLOAD_CHUNK_BYTES: usize = 256 * 1024;

/// The most a download will hold for a client, whatever the client asks for.
///
/// Credit is what a client says it is ready for, and a client is not a reliable
/// witness about itself — a grant computed from a file's size rather than from a
/// buffer is not an attack, just a loop written the obvious way. Since the queue
/// to a client is unbounded, an unclamped grant is the whole file in this
/// process, which is what credit exists to prevent. So a grant is a request and
/// this is the answer: pull as often as you like, at any rate the link
/// sustains, but the daemon decides how much it is holding. It is the same
/// decision `RELAY_DEPTH` makes for the other direction, and the same reasoning
/// as a resume token — a grant is not authority either.
const MAX_OUTSTANDING_CHUNKS: u32 = 8;

/// A download in progress, and the way to tell it to keep going.
///
/// The task holds the client's outbox and writes to it directly rather than
/// through the loop, because routing a file through the daemon's single input
/// channel would put it in memory a second time. What bounds it is credit: the
/// task sends what it has been asked for and then waits.
struct Download {
    credit: mpsc::UnboundedSender<u32>,
    task: JoinHandle<()>,
}

impl Download {
    fn stop(self) {
        self.task.abort();
    }
}

/// How long an interrupted transfer is kept before the host gets its disk back.
///
/// Long enough that a reconnect after a dropped link, a closed laptop, or a
/// walk between rooms still finds it; short enough that what a host holds for
/// senders who never came back is bounded by minutes rather than by trust.
const RETAIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How many interrupted transfers may be kept at once.
///
/// Retention is the one thing here that lets a sender leave bytes on a host
/// without finishing, so it is the one thing that needs a count as well as a
/// clock: what may be occupied is this many times the transfer ceiling, and
/// nothing about a client's behaviour changes that.
const MAX_RETAINED_TRANSFERS: usize = 4;

/// A transfer that has not finished, kept in case its sender returns.
///
/// Keeping it does not weaken the rule that nothing partial is ever named,
/// reported, or acted on: no path is reported, what a sender is told is a byte
/// count rather than a location, and nothing reaches a pane until a digest
/// verifies. What it buys is the difference between a dropped connection
/// costing a reconnect and costing a gigabyte.
struct Retained {
    /// The pane it was for. A resuming client must still hold that pane's
    /// control lease, so returning with a token is not on its own authority to
    /// write to a host.
    pane: PaneId,
    mime: String,
    name: Option<String>,
    /// The whole content's length. A resume that declares a different one is a
    /// different transfer wearing this one's token.
    length: u64,
    /// None until bytes have actually reached the host.
    path: Option<String>,
    staged: u64,
    /// Set while an attempt is running, so two attempts cannot append to one
    /// file at the same time.
    in_flight: bool,
    expires_at: Instant,
}

/// How long a relay waits for a trailer after the last declared byte.
///
/// A client that sends everything it declared and then goes quiet would
/// otherwise hold a staged file on the target host for as long as it kept the
/// connection open.
const TRAILER_TIMEOUT: Duration = Duration::from_secs(30);

/// One step of a transfer, crossing from the connection that received it to the
/// relay that is moving it.
enum RelayItem {
    Chunk(Vec<u8>),
    /// The digest the sender attests to, over the bytes it sent.
    Finish(String),
    /// The sender withdrew. The transfer is still refused and unstaged like any
    /// other that stops short, but nobody is told about a refusal they asked
    /// for.
    Cancel,
}

/// What a sender attested to, once its declared bytes had been relayed.
enum Trailer {
    Digest(String),
    /// A frame arrived after the declared length was already met.
    Overrun,
    /// The transfer ended without one. A dropped connection ends this way,
    /// which is why it is refused rather than treated as an ending.
    Missing,
}

/// The client's side of a transfer, read as a stream.
///
/// It yields what a client sends and holds nothing beyond the chunk being
/// handed over. It deliberately does not hash: the host computes a digest over
/// the file it stored, and the sender attests to one over the same file, so a
/// third digest taken in the middle would prove only that the middle agrees
/// with itself — and could not span a transfer assembled from several attempts
/// anyway.
struct RelayReader {
    chunks: mpsc::Receiver<RelayItem>,
    pending: Vec<u8>,
    offset: usize,
    attested: Option<String>,
    cancelled: bool,
}

impl RelayReader {
    fn new(chunks: mpsc::Receiver<RelayItem>) -> Self {
        Self {
            chunks,
            pending: Vec::new(),
            offset: 0,
            attested: None,
            cancelled: false,
        }
    }

    /// What the sender attested to, once the declared bytes have been relayed.
    ///
    /// A source is only read up to the length it declared, so on a transfer
    /// that delivers exactly what it promised the trailer is still queued when
    /// the transfer itself is finished. This is where it is collected.
    async fn trailer(&mut self) -> Trailer {
        if let Some(digest) = self.attested.take() {
            return Trailer::Digest(digest);
        }
        // Bytes still held here are bytes the transfer did not want, which
        // means the sender declared a length shorter than what it sent.
        if self.offset < self.pending.len() {
            return Trailer::Overrun;
        }
        match tokio::time::timeout(TRAILER_TIMEOUT, self.chunks.recv()).await {
            Ok(Some(RelayItem::Finish(digest))) => Trailer::Digest(digest),
            Ok(Some(RelayItem::Chunk(_))) => Trailer::Overrun,
            Ok(Some(RelayItem::Cancel)) => {
                self.cancelled = true;
                Trailer::Missing
            }
            Ok(None) | Err(_) => Trailer::Missing,
        }
    }
}

impl AsyncRead for RelayReader {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        loop {
            if this.offset < this.pending.len() {
                let taken = buffer.remaining().min(this.pending.len() - this.offset);
                buffer.put_slice(&this.pending[this.offset..this.offset + taken]);
                this.offset += taken;
                return Poll::Ready(Ok(()));
            }
            match this.chunks.poll_recv(context) {
                Poll::Pending => return Poll::Pending,
                // Ending here is a transfer that stopped early — an abandoned
                // one, or a cancelled one. It is reported as the end of the
                // stream rather than as an error, so the check it fails is
                // named by whoever is counting bytes rather than by this.
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Ready(Some(RelayItem::Finish(digest))) => {
                    this.attested = Some(digest);
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Some(RelayItem::Cancel)) => {
                    this.cancelled = true;
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Some(RelayItem::Chunk(bytes))) => {
                    // An empty chunk is not the end of anything, and returning
                    // it as read bytes would say it was.
                    if bytes.is_empty() {
                        continue;
                    }
                    this.pending = bytes;
                    this.offset = 0;
                }
            }
        }
    }
}

/// What one attempt at a transfer came to.
enum Relayed {
    /// Verified end to end: the digest the host computed over the file it
    /// stored is the one its sender attested to.
    Done { path: String, bytes: u64 },
    /// Refused, with nothing left on the host.
    Refused(String),
    /// Stopped early, with what did arrive still on the host. The sender may
    /// come back for it, and until it expires it can.
    Interrupted { path: String, staged: u64 },
    /// Withdrawn. Nothing kept, and nobody who needs telling.
    Withdrawn,
}

/// Move one attempt at a transfer to its target, and check the promise its
/// sender made about the result.
///
/// The declared length is enforced by the attempt itself, which reads exactly
/// what is outstanding: a sender that declares more than it sends runs out of
/// stream, and one that declares less leaves bytes behind that show up here as
/// an overrun. What nothing else can check is the trailer, which arrives after
/// the last byte and attests to the whole file — including any part of it that
/// arrived during an earlier attempt. It is compared against the host's own
/// digest, so what is verified is the file rather than the journey.
async fn relay(
    target: &Target,
    transport: &TransportConfig,
    plan: clipboard::TransferPlan<'_>,
    chunks: mpsc::Receiver<RelayItem>,
) -> Relayed {
    let mut source = RelayReader::new(chunks);
    let uploaded = match clipboard::upload_stream(target, transport, plan, &mut source).await {
        Ok(clipboard::Transferred::Complete(uploaded)) => uploaded,
        Ok(clipboard::Transferred::Interrupted { path, staged }) => {
            // A stream that stopped short is a withdrawal, a broken promise, or
            // a dropped connection, and only the reader can tell them apart.
            // A sender that attested to a transfer it did not deliver said it
            // was finished, so there is nothing to come back for; an
            // interruption is the absence of that, which is what makes keeping
            // the bytes worth anything.
            if source.cancelled {
                clipboard::discard_upload(target, transport, &path).await;
                return Relayed::Withdrawn;
            }
            if source.attested.is_some() {
                clipboard::discard_upload(target, transport, &path).await;
                return Relayed::Refused(format!(
                    "transfer ended with {staged} of the {} bytes it declared",
                    plan.length
                ));
            }
            return Relayed::Interrupted { path, staged };
        }
        Err(error) => {
            return if source.cancelled {
                Relayed::Withdrawn
            } else {
                Relayed::Refused(error.to_string())
            };
        }
    };

    // Every declared byte is on the host. Only the promise about them is left.
    let refusal = match source.trailer().await {
        Trailer::Digest(digest) if digest == uploaded.digest => {
            return Relayed::Done {
                path: uploaded.path,
                bytes: uploaded.bytes as u64,
            };
        }
        Trailer::Digest(_) => {
            "transfer does not match the digest its sender attested to".to_owned()
        }
        Trailer::Overrun => format!(
            "transfer sent more than the {} bytes it declared",
            plan.length
        ),
        // The file is whole and nobody has vouched for it. That is not a
        // refusal so much as an unfinished transfer: a sender that comes back
        // has nothing left to send and only has to say what it sent.
        Trailer::Missing if !source.cancelled => {
            return Relayed::Interrupted {
                path: uploaded.path,
                staged: uploaded.bytes as u64,
            };
        }
        Trailer::Missing => {
            clipboard::discard_upload(target, transport, &uploaded.path).await;
            return Relayed::Withdrawn;
        }
    };
    // A refusal leaves nothing behind: a staged file cannot be told apart from
    // one that passed once its path has been injected into a pane.
    clipboard::discard_upload(target, transport, &uploaded.path).await;
    Relayed::Refused(refusal)
}

/// Send one file to a client, at the rate the client asks for.
///
/// The daemon counts and carries; it does not hash. The digest in the offer is
/// the host's, and whoever receives the bytes is the one who checks them
/// against it — the same division as an upload, with the ends swapped.
async fn download(
    target: &Target,
    transport: &TransportConfig,
    path: &str,
    request: u64,
    outbox: &mpsc::UnboundedSender<ServerMessage>,
    mut granted: mpsc::UnboundedReceiver<u32>,
) -> Result<()> {
    let mut source = clipboard::open_source(target, transport, path).await?;
    let length = source.length;
    outbox
        .send(ServerMessage::DownloadOffer {
            request,
            name: source.name.clone(),
            length,
            digest: source.digest.clone(),
        })
        .map_err(|_| anyhow::anyhow!("the client stopped listening"))?;

    let mut sent = 0u64;
    let mut credit = 0u32;
    let mut buffer = vec![0u8; DOWNLOAD_CHUNK_BYTES];
    while sent < length {
        if credit == 0 {
            // Nothing is read from the host while nobody is ready for it, so a
            // client that stops pulling stops the pipe rather than filling this
            // process with what it has not asked for.
            let Some(granted) = granted.recv().await else {
                return Ok(());
            };
            credit = credit.saturating_add(granted).min(MAX_OUTSTANDING_CHUNKS);
            continue;
        }
        let wanted = usize::try_from(length - sent)
            .unwrap_or(DOWNLOAD_CHUNK_BYTES)
            .min(DOWNLOAD_CHUNK_BYTES);
        let read = source
            .read(&mut buffer[..wanted])
            .await
            .context("failed to read the file from the host")?;
        if read == 0 {
            bail!("the host sent {sent} of the {length} bytes it declared");
        }
        outbox
            .send(ServerMessage::DownloadChunk {
                request,
                bytes: buffer[..read].to_vec(),
            })
            .map_err(|_| anyhow::anyhow!("the client stopped listening"))?;
        sent += read as u64;
        credit -= 1;
    }
    // Explicit, so a client that has counted fewer bytes than the offer
    // declared can tell a transfer still arriving from one that stopped.
    let _ = outbox.send(ServerMessage::DownloadFinished { request });
    Ok(())
}

/// Move a file from one target to another without it touching this device.
///
/// Both halves already existed and this is mostly their composition, which is
/// the point: the host that has the file describes and sends it, the host
/// receiving it stages and hashes what it stored, and the daemon holds both
/// connections at once so the bytes never land anywhere in between. The reader
/// is the source's SSH output and the writer is the destination's SSH input,
/// so one chunk is in this process at a time and backpressure is end to end
/// without anything having to arrange it.
///
/// Nothing is computed here either. Both digests are the hosts' own, and the
/// daemon compares them — the only role the middle has ever had in this bridge.
async fn between(
    source: &Target,
    destination: &Target,
    transport: &TransportConfig,
    path: &str,
    name: Option<&str>,
    ceiling: u64,
) -> Result<(String, u64)> {
    let mut reading = clipboard::open_source(source, transport, path).await?;
    if reading.length > ceiling {
        bail!(
            "refusing a {} byte transfer; the limit is {ceiling}",
            reading.length
        );
    }
    // A file keeps its own name unless the caller has a better one. A source
    // whose name this bridge will not write is refused rather than renamed
    // behind the caller's back — naming it explicitly is the way through.
    let staged = name.unwrap_or(&reading.name).to_owned();
    let attested = reading.digest.clone();
    let plan = clipboard::TransferPlan {
        media: clipboard::OPAQUE,
        staging: clipboard::Staging::Fresh {
            name: Some(&staged),
        },
        length: reading.length,
    };
    let written = clipboard::upload_stream(destination, transport, plan, &mut reading).await?;
    let stored = match written {
        clipboard::Transferred::Complete(stored) => stored,
        // Nothing is kept. Unlike a client's upload, the bytes still exist
        // where they started: the source host has them, so a second attempt
        // costs a re-read rather than a file nobody can reproduce. There is
        // nothing here to come back for.
        clipboard::Transferred::Interrupted { path, staged } => {
            clipboard::discard_upload(destination, transport, &path).await;
            bail!(
                "transfer stopped after {staged} of {} bytes; the file is untouched on {}",
                reading.length,
                source.name
            );
        }
    };
    accept_copy(destination, transport, stored, &attested, &source.name).await
}

/// Keep a copy only if both hosts agree about what it is.
///
/// Separated from the move itself so it can be exercised: a mismatch cannot be
/// produced on demand by a system that is working, and a check nothing can fail
/// is a check nobody has tested. The comparison is between two digests neither
/// of which this process computed — the source host's, and the destination
/// host's over what it stored.
async fn accept_copy(
    destination: &Target,
    transport: &TransportConfig,
    stored: clipboard::UploadedFile,
    attested: &str,
    source_name: &str,
) -> Result<(String, u64)> {
    if stored.digest != attested {
        // Nothing is left behind by a refusal here either, and the reason is
        // the same: a staged file cannot be told apart from one that passed.
        clipboard::discard_upload(destination, transport, &stored.path).await;
        bail!(
            "what {} stored is not what {source_name} sent",
            destination.name
        );
    }
    Ok((stored.path, stored.bytes as u64))
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
    /// The inbox projection. It lives beside the attention index because it
    /// reads from it, and beside the federation state because it summarises
    /// it — the one place in the process that holds both.
    agent_cards: AgentCardIndex,
    agent_marks: AgentMarks,
    agent_mark_store: Option<AgentMarkStore>,
    /// Alerts for paired devices. A second sink beside the desktop's own,
    /// which the frontend runs against its attention mirror — this one lives
    /// here because the point of it is a phone learning that an agent is
    /// waiting while the desktop is asleep.
    device_notifications: NotificationQueue,
    command_timeout: Duration,
    inputs: mpsc::UnboundedSender<Input>,
    /// Queues handed over by connections, waiting for the broker to say whether
    /// their transfer may proceed. An entry lives only from the `upload.begin`
    /// that precedes it until that message has been decided.
    offers: BTreeMap<(ClientId, u64), mpsc::Receiver<RelayItem>>,
    /// Transfers that stopped without finishing, by the token their sender was
    /// given. Keyed by token rather than by client, because surviving the
    /// client is the entire point.
    retained: BTreeMap<String, Retained>,
    /// Files being read back off a target, by the client and request that asked.
    /// These do not outlive their client: a download nobody is receiving is a
    /// pipe nobody is emptying.
    downloads: BTreeMap<(ClientId, u64), Download>,
    pending_pairing: Arc<Mutex<Option<PendingPairing>>>,
    pending_pairing_approvals: BTreeMap<String, PendingPairingApproval>,
    /// Where a device outside this machine reaches the browser client, when
    /// somebody told the daemon. Never derived: see `DaemonOptions::web_url`.
    web_url: Option<String>,
    /// The short code currently published at the multi-tenant bridge. A watch
    /// channel retains it across connector retries without writing it to disk.
    bridge_pairing: Option<watch::Sender<Option<String>>>,
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

struct PendingPairingApproval {
    name: String,
    expires_at: SystemTime,
    decision: oneshot::Sender<std::result::Result<String, String>>,
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
        attach.clone(),
        attachments,
        std::future::pending(),
        // Shared with the browser client this daemon may serve, so a code the
        // frontend issues is a code a phone can spend.
        Arc::new(Mutex::new(None)),
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
    let pending_pairing: Arc<Mutex<Option<PendingPairing>>> = Arc::new(Mutex::new(None));

    let result = run(
        config,
        config_path,
        options,
        Some(listener),
        attach,
        attachments,
        shutdown,
        pending_pairing,
    )
    .await;
    // The socket outlives the daemon otherwise, and the next start would have
    // to decide whether the path it found belongs to a live process.
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

/// Serve the browser client, if one was asked for.
///
/// Called from `run` rather than from the socket path alone, because the
/// frontend hosts its own daemon and skips that path entirely: for as long as
/// this lived beside the socket listener, a pairing code offered by the
/// frontend named a port nothing was listening on. The browser client is an
/// ordinary in-process attachment, so it speaks the same framing through the
/// same handshake as every other client.
async fn serve_web_client(
    options: &DaemonOptions,
    config: &Config,
    config_path: Option<&Path>,
    attach: &mpsc::UnboundedSender<DuplexStream>,
    inputs: &mpsc::UnboundedSender<Input>,
    pending_pairing: &Arc<Mutex<Option<PendingPairing>>>,
    bridge_pairing: &watch::Sender<Option<String>>,
) -> Result<Option<(std::net::SocketAddr, JoinHandle<()>, Option<JoinHandle<()>>)>> {
    let Some(port) = options.web_port else {
        return Ok(None);
    };
    let address = match options.web_address {
        Some(address) => std::net::SocketAddr::new(address, port),
        None => web::loopback(port),
    };
    let listener = web::bind(address).await?;
    let attach = attach.clone();
    let open: web::Attach = std::sync::Arc::new(move || {
        let (theirs, ours) = tokio::io::duplex(IN_PROCESS_BUFFER);
        attach
            .send(theirs)
            .map_err(|_| anyhow::anyhow!("the daemon is no longer running"))?;
        Ok(ours)
    });
    // Devices are not affected by session discovery, so the configuration is
    // read as given rather than expanded a second time — discovery reaches
    // every host, and doing it twice at startup would double that for nothing.
    let policy: std::sync::Arc<dyn web::Devices> = std::sync::Arc::new(DevicePolicy {
        config_path: config_path.map(Path::to_path_buf),
        devices: Mutex::new(config.devices.clone()),
        inputs: inputs.clone(),
        pending: pending_pairing.clone(),
        bridge_pairing: options.web_bridge.as_ref().map(|_| bridge_pairing.clone()),
    });
    let bridge = options
        .web_bridge
        .clone()
        .map(|route| crate::bridge::spawn_connector(route, address, bridge_pairing.subscribe()));
    Ok(Some((
        address,
        tokio::spawn(web::serve(listener, open, policy)),
        bridge,
    )))
}

#[allow(clippy::too_many_arguments)]
async fn run(
    config: Config,
    config_path: Option<PathBuf>,
    options: DaemonOptions,
    listener: Option<UnixListener>,
    attach: mpsc::UnboundedSender<DuplexStream>,
    mut attachments: mpsc::UnboundedReceiver<DuplexStream>,
    shutdown: impl Future<Output = ()> + Send + 'static,
    // Shared with the web layer when one is serving, so a code minted for a
    // person on one screen is the code a browser can spend.
    pending_pairing: Arc<Mutex<Option<PendingPairing>>>,
) -> Result<()> {
    let (bridge_pairing, _) = watch::channel::<Option<String>>(None);
    let (inputs, mut received) = mpsc::unbounded_channel();
    // Before anything slow, because a browser client that cannot bind is a
    // pairing code that will name a port nothing answers on — and that is a
    // failure worth having at startup rather than at the pairing screen.
    let web = serve_web_client(
        &options,
        &config,
        config_path.as_deref(),
        &attach,
        &inputs,
        &pending_pairing,
        &bridge_pairing,
    )
    .await?;
    if listener.is_some() {
        // Printed only by a daemon that owns its standard output. A frontend
        // hosting one is drawing a terminal UI on it.
        //
        // Announced only once everything it names is actually listening.
        // Printing as each one binds meant a daemon that failed to start still
        // said it was serving, which is the first thing an operator reads and
        // the last thing they should have to doubt.
        println!(
            "super-herdr daemon listening on {}",
            options.socket.display()
        );
        if let Some((address, _, bridge)) = web.as_ref() {
            // Printed rather than logged, because the address is what a person
            // needs in order to forward it.
            println!("super-herdr web client on http://{address}");
            if address.ip().is_loopback() {
                if bridge.is_some() {
                    println!("  public bridge connector enabled");
                } else {
                    println!("  forward this port to reach it from another device");
                }
            } else if config.devices.is_empty() {
                println!("  no device is paired yet; ask the terminal client for a pairing code");
            }
        }
    }

    let active = expand_discovered_sessions(config).await;
    let attention_store = match options.attention_state.clone() {
        Some(path) => Some(AttentionStore::at(path)),
        None => AttentionStore::discover().ok(),
    };
    let attention = attention_store
        .as_ref()
        .and_then(|store| store.load().ok())
        .unwrap_or_default();
    let attention_cursor = attention.events().next_back().map(|event| event.id);
    let agent_mark_store = match options.agent_marks.clone() {
        Some(path) => Some(AgentMarkStore::at(path)),
        None => AgentMarkStore::discover().ok(),
    };
    let agent_marks = agent_mark_store
        .as_ref()
        .and_then(|store| store.load().ok())
        .unwrap_or_default();
    // The device sink borrows the desktop's filters, coalescing and rate
    // limits, and is switched on separately: wanting alerts on a phone is not
    // the same request as wanting them on the laptop being sat at.
    let mut device_notifications = active.notifications.clone();
    device_notifications.enabled = device_notifications.devices;
    let notify_devices = device_notifications.enabled;
    let mut daemon = Daemon {
        broker: Broker::new(
            env!("CARGO_PKG_VERSION"),
            vec![
                "terminal".to_owned(),
                "plugin_actions".to_owned(),
                "agent_cards".to_owned(),
            ],
            active
                .quick_replies
                .clone()
                .unwrap_or_else(crate::config::default_quick_replies),
        ),
        web_url: options.web_url.clone(),
        bridge_pairing: options.web_bridge.as_ref().map(|_| bridge_pairing.clone()),
        outboxes: BTreeMap::new(),
        routes: BTreeMap::new(),
        targets: target_map(&active),
        transport: active.transport.clone(),
        state: FederationState::default(),
        attention,
        attention_store,
        attention_cursor,
        agent_cards: AgentCardIndex::default(),
        agent_marks,
        agent_mark_store,
        device_notifications: NotificationQueue::new(device_notifications, None),
        command_timeout: Duration::from_secs(active.transport.command_timeout_seconds),
        inputs: inputs.clone(),
        offers: BTreeMap::new(),
        retained: BTreeMap::new(),
        downloads: BTreeMap::new(),
        pending_pairing,
        pending_pairing_approvals: BTreeMap::new(),
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

    // Coalescing means a delivery becomes due after the event that started it,
    // so something has to come back and look. Spawned only when the sink is on,
    // so a daemon nobody asked to notify has no timer at all.
    let notifying = notify_devices.then(|| {
        let inputs = inputs.clone();
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(NOTIFICATION_TICK);
            ticks.tick().await;
            loop {
                ticks.tick().await;
                if inputs.send(Input::NotificationsDue).is_err() {
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
    if let Some(notifying) = notifying {
        notifying.abort();
    }
    if let Some(refreshing) = refreshing {
        refreshing.abort();
    }
    if let Some((_, serving, bridge)) = web {
        serving.abort();
        if let Some(bridge) = bridge {
            bridge.abort();
        }
    }
    daemon.discard_retained().await;
    daemon.stop_federation().await;
    Ok(())
}

/// The daemon's answer to the web layer's pairing questions.
///
/// Devices are read from the durable configuration on every check rather than
/// cached, so revoking one takes effect at the next request instead of at the
/// next restart — which is the whole point of being able to revoke it.
struct DevicePolicy {
    config_path: Option<PathBuf>,
    devices: Mutex<Vec<Device>>,
    inputs: mpsc::UnboundedSender<Input>,
    pending: Arc<Mutex<Option<PendingPairing>>>,
    bridge_pairing: Option<watch::Sender<Option<String>>>,
}

impl DevicePolicy {
    fn current(&self) -> Vec<Device> {
        if let Some(path) = self.config_path.as_ref()
            && let Ok((config, _)) = Config::load(Some(path))
        {
            if let Ok(mut held) = self.devices.lock() {
                held.clone_from(&config.devices);
            }
            return config.devices;
        }
        self.devices
            .lock()
            .map(|held| held.clone())
            .unwrap_or_default()
    }

    fn clear_bridge_pairing(&self) {
        if let Some(pairing) = self.bridge_pairing.as_ref() {
            pairing.send_replace(None);
        }
    }
}

impl web::Devices for DevicePolicy {
    fn admits(&self, token: &str) -> bool {
        let offered = pairing::fingerprint(token);
        self.current()
            .iter()
            .any(|device| pairing::matches(&device.token_sha256, &offered))
    }

    fn pair(&self, code: &str, name: &str, confirmation: &str) -> Result<web::PairingStart> {
        if self.config_path.is_none() {
            anyhow::bail!("this daemon has no configuration file to record a device in");
        }
        if confirmation.len() != 6 || !confirmation.bytes().all(|byte| byte.is_ascii_digit()) {
            anyhow::bail!("the browser did not provide a valid confirmation number");
        }
        let name = device_name(name);
        let attempt = pairing::token()?;
        let (decision, waiting) = oneshot::channel();
        let now = SystemTime::now();
        // The code is consumed by a match, not by an attempt: a wrong entry is
        // far more often a typo than an attack, and making somebody fetch a new
        // code for each slip would teach them to leave one outstanding.
        {
            let mut held = self
                .pending
                .lock()
                .map_err(|_| anyhow::anyhow!("pairing state is unavailable"))?;
            let Some(pending) = held.as_mut() else {
                anyhow::bail!("no pairing code is waiting; ask for one from the terminal client");
            };
            if pending.expired(now) {
                *held = None;
                self.clear_bridge_pairing();
                anyhow::bail!("that pairing code has expired; ask for another");
            }
            if !pending.accepts(code, now) {
                let spent = pending.record_failure();
                let remaining = pending.attempts_remaining();
                if spent {
                    *held = None;
                    self.clear_bridge_pairing();
                    anyhow::bail!("too many wrong codes; ask for another");
                }
                anyhow::bail!(
                    "that is not the code waiting; {remaining} attempt(s) left before it is discarded"
                );
            }
            if self.current().iter().any(|existing| existing.name == name) {
                return Ok(web::PairingStart::RetryWithSameCode {
                    message: format!(
                        "A device named {name:?} is already paired. Choose a different device name and try this code again."
                    ),
                });
            }
            // Matched, so it is spent: a code overheard after use opens nothing.
            *held = None;
        }
        self.clear_bridge_pairing();
        self.inputs
            .send(Input::PairingRequested {
                attempt,
                name,
                confirmation: confirmation.to_owned(),
                expires_at: now + web::PAIRING_APPROVAL_LIFETIME,
                decision,
            })
            .map_err(|_| anyhow::anyhow!("the daemon stopped before it could ask for approval"))?;
        Ok(web::PairingStart::AwaitingApproval(waiting))
    }

    fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_owned()
    }
}

/// A device name comes from a person typing into a browser, so it is bounded
/// and stripped of anything that would make the configuration file or a listing
/// hard to read.
fn device_name(offered: &str) -> String {
    let cleaned = offered
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, ' ' | '-' | '_' | '.')
        })
        .take(48)
        .collect::<String>();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "paired device".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// The digest a sender attests to, which the daemon now only ever computes
/// while relaying. Tests still need it over a payload they hold whole.
#[cfg(test)]
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
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

/// The longest socket path a Unix domain socket can carry.
///
/// `sun_path` is 108 bytes on Linux and 104 on macOS, so the smaller is used:
/// a path that bound on one and not the other would be a difference nobody
/// would think to look for.
const MAX_SOCKET_PATH: usize = 103;

/// Bind the socket without evicting a daemon that is already serving it. A path
/// that refuses a connection is a leftover from a process that did not clean up
/// and is safe to replace; a path that accepts one is somebody else's.
fn bind(path: &Path) -> Result<UnixListener> {
    // Checked first, because every failure below reports what the kernel said
    // about a path it never accepted — and "No such file or directory" for a
    // socket nobody expected to exist reads as a bug rather than a path that is
    // simply too long.
    let length = path.as_os_str().len();
    anyhow::ensure!(
        length <= MAX_SOCKET_PATH,
        "the socket path is {length} bytes; a Unix socket allows at most {MAX_SOCKET_PATH}. \
         Choose a shorter --socket path."
    );
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

    // Transfers this connection is carrying. Their chunks never reach the
    // central loop: forwarding them from here is what lets a full queue stop
    // this connection being read, and a loop serving every client cannot wait
    // on one client's target.
    let mut relays: HashMap<u64, mpsc::Sender<RelayItem>> = HashMap::new();
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
        match message {
            ClientMessage::BeginUpload { request, .. }
            | ClientMessage::ResumeUpload { request, .. } => {
                let (sender, chunks) = mpsc::channel(RELAY_DEPTH);
                // Handed over before the message it belongs to, so the loop
                // has the queue by the time it decides whether to use it.
                if inputs
                    .send(Input::UploadOffered {
                        client,
                        request,
                        chunks,
                    })
                    .is_err()
                {
                    break;
                }
                relays.insert(request, sender);
                if inputs.send(Input::Received { client, message }).is_err() {
                    break;
                }
            }
            ClientMessage::UploadChunk { request, bytes } => {
                // Awaiting here is the whole point: a queue that is full stops
                // this loop, which stops draining the socket, which stops the
                // client. A refused transfer has closed its queue, and its
                // remaining chunks fall through to be refused one by one, the
                // same as a chunk for a transfer that never began.
                match relays.get(&request) {
                    Some(sender) => {
                        if sender.send(RelayItem::Chunk(bytes)).await.is_err() {
                            relays.remove(&request);
                        }
                    }
                    None => {
                        let message = ClientMessage::UploadChunk { request, bytes };
                        if inputs.send(Input::Received { client, message }).is_err() {
                            break;
                        }
                    }
                }
            }
            ClientMessage::FinishUpload { request, digest } => match relays.remove(&request) {
                Some(sender) => {
                    let _ = sender.send(RelayItem::Finish(digest)).await;
                }
                None => {
                    let message = ClientMessage::FinishUpload { request, digest };
                    if inputs.send(Input::Received { client, message }).is_err() {
                        break;
                    }
                }
            },
            ClientMessage::CancelUpload { request } => {
                // Withdrawn rather than dropped, so the relay can tell a
                // transfer somebody stopped from one that died: both are
                // refused and unstaged, only one is worth reporting. The loop
                // is told as well, so an offer it has not yet decided on does
                // not outlive the transfer it belongs to.
                if let Some(sender) = relays.remove(&request) {
                    let _ = sender.send(RelayItem::Cancel).await;
                }
                if inputs
                    .send(Input::Received {
                        client,
                        message: ClientMessage::CancelUpload { request },
                    })
                    .is_err()
                {
                    break;
                }
            }
            message => {
                if inputs.send(Input::Received { client, message }).is_err() {
                    break;
                }
            }
        }
    }

    sending.abort();
    let _ = inputs.send(Input::Disconnected { client });
}

impl Daemon {
    fn handle(&mut self, input: Input) {
        let offered = match &input {
            Input::Received {
                client,
                message:
                    ClientMessage::BeginUpload { request, .. }
                    | ClientMessage::ResumeUpload { request, .. },
            } => Some((*client, *request)),
            _ => None,
        };
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
                // An offer that never became a transfer is dropped here. One
                // that did is ended by its own connection releasing the queue,
                // which the relay sees as a stream that stopped short of what
                // it declared — so an abandoned transfer is refused and
                // unstaged rather than left on the host.
                self.offers.retain(|(owner, _), _| *owner != client);
                // A download outliving its client is a pipe nobody is
                // emptying, and unlike an upload there is nothing to come back
                // for: the file is still on the host, where it always was.
                let reading: Vec<(ClientId, u64)> = self
                    .downloads
                    .keys()
                    .filter(|(owner, _)| *owner == client)
                    .copied()
                    .collect();
                for key in reading {
                    if let Some(download) = self.downloads.remove(&key) {
                        download.stop();
                    }
                }
                self.broker.disconnect(client)
            }
            Input::UploadOffered {
                client,
                request,
                chunks,
            } => {
                self.offers.insert((client, request), chunks);
                Vec::new()
            }
            Input::NotificationsDue => self.device_alerts(),
            Input::Federation(state) => {
                self.state = state.clone();
                let mut effects = self.broker.federation_updated(state);
                effects.extend(self.observe_attention());
                // After attention, not before: a card carries the attention
                // state of its agent, so projecting first would publish an
                // inbox that disagreed with the events sent alongside it.
                effects.extend(self.project_agent_cards());
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
                plugin_run,
            } => self
                .broker
                .operation_completed(client, request, applied, message, plugin_run),
            Input::RelayFinished {
                client,
                request,
                transfer,
                outcome,
            } => {
                self.relay_finished(client, request, transfer, outcome);
                Vec::new()
            }
            Input::DownloadEnded { client, request } => {
                self.downloads.remove(&(client, request));
                Vec::new()
            }
            Input::ResumeOffset {
                client,
                request,
                transfer,
                staged,
            } => {
                if let Some(retained) = self.retained.get_mut(&transfer) {
                    retained.staged = staged;
                }
                self.tell(
                    client,
                    ServerMessage::UploadAccepted {
                        request,
                        transfer,
                        staged,
                    },
                );
                Vec::new()
            }
            Input::PairingRequested {
                attempt,
                name,
                confirmation,
                expires_at,
                decision,
            } => {
                if self.pending_pairing_approvals.len() >= MAX_PENDING_PAIRING_APPROVALS {
                    let _ = decision.send(Err(
                        "too many device approvals are already waiting".to_owned()
                    ));
                    Vec::new()
                } else {
                    let expires_in_seconds = expires_at
                        .duration_since(SystemTime::now())
                        .unwrap_or_default()
                        .as_secs();
                    self.pending_pairing_approvals.insert(
                        attempt.clone(),
                        PendingPairingApproval {
                            name: name.clone(),
                            expires_at,
                            decision,
                        },
                    );
                    self.broker.pairing_approval_required(
                        attempt,
                        name,
                        confirmation,
                        expires_in_seconds,
                    )
                }
            }
            // The loop intercepts this before it reaches here; the arm exists so
            // adding a shutdown path later cannot silently do nothing.
            Input::Shutdown => Vec::new(),
            Input::RefreshDue => {
                self.start_refresh();
                // The same tick that re-reads configuration gives a host back
                // whatever nobody came for. It needs no timer of its own: what
                // it bounds is measured in minutes.
                self.sweep_retained();
                self.sweep_pairing_approvals()
            }
            Input::Reconfigured(config) => self.reconfigure(config),
        };
        self.apply(effects);
        // An offer nothing claimed — one whose transfer the lease check refused
        // before it ever became an effect — does not outlive the message it
        // arrived with.
        if let Some(key) = offered {
            self.offers.remove(&key);
        }
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
                Effect::RouteUnclaimed { pane } => {
                    // Nothing waits yet, so a pane nobody is watching is closed
                    // in the same pass — which is exactly what happened before
                    // this effect existed. The wait belongs to whoever holds a
                    // clock and arrives separately.
                    pending.extend(self.broker.close_if_unclaimed(&pane));
                }
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
                Effect::ListPluginActions {
                    client,
                    request,
                    target,
                } => self.list_plugin_actions(client, request, target),
                Effect::GetPluginRun {
                    client,
                    request,
                    run,
                } => self.get_plugin_run(client, request, run),
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
                    name,
                    length,
                } => self.begin_relay(client, request, pane, mime, name, length),
                Effect::ResumeUpload {
                    client,
                    request,
                    transfer,
                    pane,
                    length,
                } => self.resume_relay(client, request, transfer, pane, length),
                // A chunk only arrives here when its connection has no queue to
                // put it in, which means the transfer was refused or never
                // began.
                Effect::UploadChunk {
                    client, request, ..
                }
                | Effect::FinishUpload {
                    client, request, ..
                } => self.refuse(client, request, "no transfer is in progress".to_owned()),
                Effect::CancelUpload { client, request } => {
                    self.offers.remove(&(client, request));
                    // A transfer between hosts is cancelled the same way, since
                    // from a client's side it is the same thing under the same
                    // request: something it started and no longer wants.
                    self.stop_download(client, request);
                }
                Effect::BeginDownload {
                    client,
                    request,
                    pane,
                    path,
                } => self.begin_download(client, request, pane, path),
                Effect::PullDownload {
                    client,
                    request,
                    chunks,
                } => self.pull_download(client, request, chunks),
                Effect::CancelDownload { client, request } => {
                    self.stop_download(client, request);
                }
                Effect::TransferBetween {
                    client,
                    request,
                    source,
                    path,
                    destination,
                    name,
                } => self.begin_between(client, request, source, path, destination, name),
                Effect::MarkAgent {
                    client,
                    request,
                    agent,
                    mark,
                } => {
                    let changed = self.agent_marks.apply(&agent, mark, unix_time_ms());
                    if changed {
                        self.persist_agent_marks();
                        pending.extend(self.project_agent_cards());
                    }
                    // Answered either way. A mark that was already set is not
                    // a failure, and a client that heard nothing back could
                    // not tell that from a request that never arrived.
                    if let Some(outbox) = self.outboxes.get(&client) {
                        let _ = outbox.send(ServerMessage::OperationResult {
                            request,
                            applied: true,
                            message: String::new(),
                            plugin_run: None,
                        });
                    }
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
                Effect::IssuePairingCode { client, request } => {
                    self.issue_pairing_code(client, request);
                }
                Effect::DecidePairing { attempt, approve } => {
                    pending.extend(self.decide_pairing(attempt, approve));
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

    /// Give a host back everything nobody can come for any more.
    ///
    /// `RETAIN_TIMEOUT` is the right bound for a daemon that is running and no
    /// bound at all for one that is stopping. The token that names a retained
    /// transfer lives in this process, so a transfer that outlives it is
    /// unresumable by construction: keeping the bytes past this point cannot
    /// serve anybody, and what it leaves is partial files in a private
    /// directory on somebody else's machine, with the reaper that would remove
    /// them living inside a process that no longer exists.
    ///
    /// Two cases are out of reach, and both are the same case. A crash leaves
    /// them, and so does an attempt still in flight: a remote staging path is
    /// made by the script and only reported when the attempt ends, so a
    /// transfer stopped mid-flight has bytes on a host that nothing in this
    /// process can name. That is a killed process holding a temporary file,
    /// which the sweep on the next start cannot help with either — it is the
    /// limit of what a daemon can clean up about itself.
    async fn discard_retained(&mut self) {
        let mut discarding = tokio::task::JoinSet::new();
        for (_, retained) in std::mem::take(&mut self.retained) {
            let Some(path) = retained.path else {
                continue;
            };
            let Some(target) = self.targets.get(&retained.pane.target_session()).cloned() else {
                continue;
            };
            let transport = self.transport.clone();
            discarding.spawn(async move {
                clipboard::discard_upload(&target, &transport, &path).await;
            });
        }
        // Together rather than in turn: each of these is a bounded SSH command,
        // and a stopping daemon should not take four timeouts to stop.
        while discarding.join_next().await.is_some() {}
    }

    async fn stop_federation(&mut self) {
        if let Some(watcher) = self.watcher.take() {
            watcher.abort();
        }
        if let Some(store) = self.store.take() {
            store.shutdown().await;
        }
    }

    /// Mint a pairing code and hold it in memory for a bounded time.
    ///
    /// One code is outstanding at a time: asking again replaces the previous
    /// one, so a code shown on a screen somebody walked away from stops working
    /// as soon as the next is asked for.
    fn issue_pairing_code(&mut self, client: ClientId, request: u64) {
        let now = SystemTime::now();
        let message = match PendingPairing::new(now) {
            Ok(pending) => {
                let code = pending.code().to_owned();
                let expires_in_seconds = pending
                    .expires_at()
                    .duration_since(now)
                    .unwrap_or_default()
                    .as_secs();
                match self.pending_pairing.lock() {
                    Ok(mut held) => {
                        *held = Some(pending);
                        if let Some(pairing) = self.bridge_pairing.as_ref() {
                            pairing.send_replace(Some(code.clone()));
                        }
                        ServerMessage::PairingCode {
                            request,
                            code,
                            expires_in_seconds,
                            url: self.web_url.clone(),
                        }
                    }
                    Err(_) => ServerMessage::Error {
                        request: Some(request),
                        message: "pairing state is unavailable".to_owned(),
                    },
                }
            }
            Err(error) => ServerMessage::Error {
                request: Some(request),
                message: error.to_string(),
            },
        };
        if let Some(outbox) = self.outboxes.get(&client) {
            let _ = outbox.send(message);
        }
    }

    fn decide_pairing(&mut self, attempt: String, approve: bool) -> Vec<Effect> {
        let Some(pending) = self.pending_pairing_approvals.remove(&attempt) else {
            return self.broker.pairing_decided(
                attempt,
                false,
                "that device approval is no longer waiting".to_owned(),
            );
        };
        if pending.expires_at <= SystemTime::now() || pending.decision.is_closed() {
            let _ = pending
                .decision
                .send(Err("device approval expired".to_owned()));
            return self.broker.pairing_decided(
                attempt,
                false,
                "device approval expired".to_owned(),
            );
        }
        if !approve {
            let _ = pending
                .decision
                .send(Err("device approval was rejected".to_owned()));
            return self.broker.pairing_decided(
                attempt,
                false,
                format!("{} was not paired", pending.name),
            );
        }
        let result = (|| {
            let path = self
                .config_path
                .as_ref()
                .context("this daemon has no configuration file to record a device in")?;
            let token = pairing::token()?;
            Config::add_device_file(
                Some(path),
                Device {
                    name: pending.name.clone(),
                    token_sha256: pairing::fingerprint(&token),
                    paired_at_ms: pairing::now_ms(SystemTime::now()),
                },
            )?;
            Ok::<String, anyhow::Error>(token)
        })();
        let (approved, message) = match result {
            Ok(token) => {
                let delivered = pending.decision.send(Ok(token)).is_ok();
                (
                    delivered,
                    if delivered {
                        format!("{} paired", pending.name)
                    } else {
                        "the browser left before approval completed".to_owned()
                    },
                )
            }
            Err(error) => {
                let message = error.to_string();
                let _ = pending.decision.send(Err(message.clone()));
                (false, message)
            }
        };
        self.broker.pairing_decided(attempt, approved, message)
    }

    fn sweep_pairing_approvals(&mut self) -> Vec<Effect> {
        let now = SystemTime::now();
        let expired: Vec<String> = self
            .pending_pairing_approvals
            .iter()
            .filter(|(_, pending)| pending.expires_at <= now || pending.decision.is_closed())
            .map(|(attempt, _)| attempt.clone())
            .collect();
        let mut effects = Vec::new();
        for attempt in expired {
            if let Some(pending) = self.pending_pairing_approvals.remove(&attempt) {
                let _ = pending
                    .decision
                    .send(Err("device approval expired".to_owned()));
                effects.extend(self.broker.pairing_decided(
                    attempt,
                    false,
                    "device approval expired".to_owned(),
                ));
            }
        }
        effects
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
                plugin_run: None,
            });
        });
    }

    /// Start moving a transfer to the target host as its bytes arrive.
    ///
    /// Everything that can be refused for free is refused here, before a byte
    /// moves: a length above the ceiling, and a pane whose target is not
    /// configured. What cannot — that the bytes are the ones their sender
    /// attested to — is checked at the far end, where a failure has to unstage
    /// what already arrived.
    fn begin_relay(
        &mut self,
        client: ClientId,
        request: u64,
        pane: PaneId,
        mime: String,
        name: Option<String>,
        length: u64,
    ) {
        let Some(chunks) = self.offers.remove(&(client, request)) else {
            self.refuse(client, request, "no transfer is in progress".to_owned());
            return;
        };
        let ceiling = self.active.transfers.max_bytes;
        if length > ceiling {
            self.refuse(
                client,
                request,
                format!("refusing a {length} byte upload; the limit is {ceiling}"),
            );
            return;
        }
        let Some(target) = self.transfer_target(client, request, &pane) else {
            return;
        };
        // Issued before a byte moves, because its whole purpose is to be worth
        // something to a sender whose connection is about to end.
        let Ok(transfer) = pairing::token() else {
            self.refuse(
                client,
                request,
                "the daemon could not generate a transfer token".to_owned(),
            );
            return;
        };
        self.sweep_retained();
        self.evict_retained();
        self.retained.insert(
            transfer.clone(),
            Retained {
                pane,
                mime: mime.clone(),
                name: name.clone(),
                length,
                path: None,
                staged: 0,
                in_flight: true,
                expires_at: Instant::now() + RETAIN_TIMEOUT,
            },
        );
        self.tell(
            client,
            ServerMessage::UploadAccepted {
                request,
                transfer: transfer.clone(),
                staged: 0,
            },
        );

        let media = clipboard::media_for_mime(&mime);
        let transport = self.transport.clone();
        let inputs = self.inputs.clone();
        tokio::spawn(async move {
            let plan = clipboard::TransferPlan {
                media,
                staging: clipboard::Staging::Fresh {
                    name: name.as_deref(),
                },
                length,
            };
            let outcome = relay(&target, &transport, plan, chunks).await;
            let _ = inputs.send(Input::RelayFinished {
                client,
                request,
                transfer,
                outcome,
            });
        });
    }

    /// Continue a transfer that stopped, from wherever the host actually got to.
    fn resume_relay(
        &mut self,
        client: ClientId,
        request: u64,
        transfer: String,
        pane: PaneId,
        length: u64,
    ) {
        let Some(chunks) = self.offers.remove(&(client, request)) else {
            self.refuse(client, request, "no transfer is in progress".to_owned());
            return;
        };
        self.sweep_retained();
        let Some(retained) = self.retained.get_mut(&transfer) else {
            // Expired, finished, or never issued. They are one answer on
            // purpose: which of them it was is a fact about other people's
            // transfers.
            self.refuse(
                client,
                request,
                "there is no transfer to resume under that token".to_owned(),
            );
            return;
        };
        if retained.in_flight {
            self.refuse(
                client,
                request,
                "that transfer is still being carried".to_owned(),
            );
            return;
        }
        if retained.pane != pane || retained.length != length {
            // A token names one transfer to one pane. Anything else claiming it
            // is a different transfer, and a resume is not a way to redirect
            // bytes at a file somebody else started.
            self.refuse(
                client,
                request,
                "that token belongs to a different transfer".to_owned(),
            );
            return;
        }
        retained.in_flight = true;
        let (mime, name, path) = (
            retained.mime.clone(),
            retained.name.clone(),
            retained.path.clone(),
        );
        let Some(target) = self.transfer_target(client, request, &pane) else {
            if let Some(retained) = self.retained.get_mut(&transfer) {
                retained.in_flight = false;
            }
            return;
        };

        let media = clipboard::media_for_mime(&mime);
        let transport = self.transport.clone();
        let inputs = self.inputs.clone();
        tokio::spawn(async move {
            // Asked of the host rather than remembered: an attempt that died
            // mid-chunk left an offset nobody predicted, and resuming from a
            // remembered number would corrupt the file silently.
            let staged = match path.as_deref() {
                Some(path) => clipboard::staged_bytes(&target, &transport, path)
                    .await
                    .unwrap_or(0),
                None => 0,
            };
            // Nothing survived, so this is a beginning wearing a resume's name.
            // The old directory, if there is one, is not part of it.
            let plan = match (path.as_deref(), staged) {
                (Some(path), staged) if staged > 0 => clipboard::TransferPlan {
                    media,
                    staging: clipboard::Staging::Resume { path, staged },
                    length,
                },
                (path, _) => {
                    if let Some(path) = path {
                        clipboard::discard_upload(&target, &transport, path).await;
                    }
                    clipboard::TransferPlan {
                        media,
                        staging: clipboard::Staging::Fresh {
                            name: name.as_deref(),
                        },
                        length,
                    }
                }
            };
            let _ = inputs.send(Input::ResumeOffset {
                client,
                request,
                transfer: transfer.clone(),
                staged: plan.staged(),
            });
            let outcome = relay(&target, &transport, plan, chunks).await;
            let _ = inputs.send(Input::RelayFinished {
                client,
                request,
                transfer,
                outcome,
            });
        });
    }

    /// Start reading a file back off a target.
    ///
    /// Nothing is sent until the client asks for it. The offer goes out as soon
    /// as the host has described the file, and then the task waits for credit,
    /// so a client that never pulls costs a pipe and a task rather than a
    /// gigabyte of this process.
    fn begin_download(&mut self, client: ClientId, request: u64, pane: PaneId, path: String) {
        if self.downloads.contains_key(&(client, request)) {
            self.refuse(
                client,
                request,
                "that request is already reading a file".to_owned(),
            );
            return;
        }
        let Some(target) = self.transfer_target(client, request, &pane) else {
            return;
        };
        let Some(outbox) = self.outboxes.get(&client).cloned() else {
            return;
        };
        let (credit, granted) = mpsc::unbounded_channel();
        let transport = self.transport.clone();
        let inputs = self.inputs.clone();
        let task = tokio::spawn(async move {
            let outcome = download(&target, &transport, &path, request, &outbox, granted).await;
            if let Err(error) = outcome {
                let _ = outbox.send(ServerMessage::Error {
                    request: Some(request),
                    message: error.to_string(),
                });
            }
            let _ = inputs.send(Input::DownloadEnded { client, request });
        });
        self.downloads
            .insert((client, request), Download { credit, task });
    }

    /// Start moving a file between two targets.
    ///
    /// The work runs off the loop because both hops are network operations, and
    /// it is tracked so a client that goes away takes it with them: unlike a
    /// client's upload there is nothing to retain, since the file is still on
    /// the host it came from.
    fn begin_between(
        &mut self,
        client: ClientId,
        request: u64,
        source: PaneId,
        path: String,
        destination: PaneId,
        name: Option<String>,
    ) {
        if self.downloads.contains_key(&(client, request)) {
            self.refuse(
                client,
                request,
                "that request is already moving a file".to_owned(),
            );
            return;
        }
        let Some(from) = self.transfer_target(client, request, &source) else {
            return;
        };
        let Some(to) = self.transfer_target(client, request, &destination) else {
            return;
        };
        let Some(outbox) = self.outboxes.get(&client).cloned() else {
            return;
        };
        let transport = self.transport.clone();
        let ceiling = self.active.transfers.max_bytes;
        let inputs = self.inputs.clone();
        // The credit channel goes unused: nothing is being paced toward a
        // client here, because nothing reaches one. Holding the handle is what
        // lets a disconnect stop the work.
        let (credit, _granted) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let message =
                match between(&from, &to, &transport, &path, name.as_deref(), ceiling).await {
                    Ok((path, bytes)) => ServerMessage::UploadComplete {
                        request,
                        path,
                        bytes,
                    },
                    Err(error) => ServerMessage::Error {
                        request: Some(request),
                        message: error.to_string(),
                    },
                };
            let _ = outbox.send(message);
            let _ = inputs.send(Input::DownloadEnded { client, request });
        });
        self.downloads
            .insert((client, request), Download { credit, task });
    }

    /// Let a download send more.
    fn pull_download(&mut self, client: ClientId, request: u64, chunks: u32) {
        let Some(download) = self.downloads.get(&(client, request)) else {
            self.refuse(client, request, "no file is being read".to_owned());
            return;
        };
        let _ = download.credit.send(chunks);
    }

    fn stop_download(&mut self, client: ClientId, request: u64) {
        if let Some(download) = self.downloads.remove(&(client, request)) {
            download.stop();
        }
    }

    /// The target a transfer's pane belongs to, refusing the request if there
    /// is not one.
    fn transfer_target(&self, client: ClientId, request: u64, pane: &PaneId) -> Option<Target> {
        let key = pane.target_session();
        match self.targets.get(&key).cloned() {
            Some(target) => Some(target),
            None => {
                self.refuse(client, request, format!("{key} is not a configured target"));
                None
            }
        }
    }

    /// What a finished attempt leaves behind.
    fn relay_finished(
        &mut self,
        client: ClientId,
        request: u64,
        transfer: String,
        outcome: Relayed,
    ) {
        match outcome {
            Relayed::Done { path, bytes } => {
                self.retained.remove(&transfer);
                self.tell(
                    client,
                    ServerMessage::UploadComplete {
                        request,
                        path,
                        bytes,
                    },
                );
            }
            Relayed::Refused(message) => {
                self.retained.remove(&transfer);
                self.refuse(client, request, message);
            }
            // Withdrawn: the sender asked for this, and the relay has already
            // taken the bytes with it.
            Relayed::Withdrawn => {
                self.retained.remove(&transfer);
            }
            Relayed::Interrupted { path, staged } => {
                let Some(retained) = self.retained.get_mut(&transfer) else {
                    return;
                };
                retained.in_flight = false;
                retained.path = Some(path);
                retained.staged = staged;
                // The clock starts when the transfer stopped, not when it
                // started: a long transfer that dies near the end should get the
                // same window to come back as a short one.
                retained.expires_at = Instant::now() + RETAIN_TIMEOUT;
                self.tell(
                    client,
                    ServerMessage::UploadInterrupted {
                        request,
                        transfer,
                        staged,
                    },
                );
            }
        }
    }

    /// Give up what nobody came back for.
    fn sweep_retained(&mut self) {
        let now = Instant::now();
        let expired: Vec<String> = self
            .retained
            .iter()
            .filter(|(_, retained)| !retained.in_flight && retained.expires_at <= now)
            .map(|(transfer, _)| transfer.clone())
            .collect();
        for transfer in expired {
            self.forget_retained(&transfer);
        }
    }

    /// Keep the number of unfinished transfers bounded as well as timed.
    ///
    /// A clock alone bounds how long one sender may occupy a host; a count is
    /// what bounds how many of them may at once. The oldest goes, because it is
    /// the one whose sender has been away longest.
    fn evict_retained(&mut self) {
        while self.retained.len() >= MAX_RETAINED_TRANSFERS {
            let Some(oldest) = self
                .retained
                .iter()
                .filter(|(_, retained)| !retained.in_flight)
                .min_by_key(|(_, retained)| retained.expires_at)
                .map(|(transfer, _)| transfer.clone())
            else {
                return;
            };
            self.forget_retained(&oldest);
        }
    }

    /// Drop a retained transfer and take its bytes off the host with it.
    fn forget_retained(&mut self, transfer: &str) {
        let Some(retained) = self.retained.remove(transfer) else {
            return;
        };
        let Some(path) = retained.path else {
            return;
        };
        let key = retained.pane.target_session();
        let Some(target) = self.targets.get(&key).cloned() else {
            return;
        };
        let transport = self.transport.clone();
        tokio::spawn(async move {
            clipboard::discard_upload(&target, &transport, &path).await;
        });
    }

    fn tell(&self, client: ClientId, message: ServerMessage) {
        if let Some(outbox) = self.outboxes.get(&client) {
            let _ = outbox.send(message);
        }
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
                plugin_run: None,
            });
            return;
        };
        let Some(destination) = self.targets.get(&destination_key).cloned() else {
            let _ = self.inputs.send(Input::OperationDone {
                client,
                request,
                applied: false,
                message: format!("{destination_key} is not a configured target"),
                plugin_run: None,
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
            let (applied, message, plugin_run) = match outcome {
                Ok(outcome) => (
                    true,
                    outcome.detail.unwrap_or(description),
                    outcome.plugin_run,
                ),
                Err(error) => (false, error, None),
            };
            let _ = inputs.send(Input::OperationDone {
                client,
                request,
                applied,
                message,
                plugin_run,
            });
        });
    }

    /// Read one target's plugin registry off the daemon loop. The target and
    /// generation ride back with the answer so a reconnect cannot make a late
    /// registry look current.
    fn list_plugin_actions(&mut self, client: ClientId, request: u64, key: TargetSession) {
        let Some(target) = self.targets.get(&key).cloned() else {
            self.refuse(client, request, format!("{key} is not a configured target"));
            return;
        };
        let generation = self
            .state
            .targets
            .get(&key)
            .map_or(0, |runtime| runtime.connection_generation);
        let Some(outbox) = self.outboxes.get(&client).cloned() else {
            return;
        };
        let transport = self.transport.clone();
        let request_timeout = self.command_timeout;
        tokio::spawn(async move {
            let message = match plugin::list(&key, &target, &transport, request_timeout).await {
                Ok(actions) => ServerMessage::PluginActions {
                    request,
                    target: key,
                    generation,
                    actions,
                },
                Err(error) => ServerMessage::Error {
                    request: Some(request),
                    message: format!("plugin actions unavailable on {key}: {}", error.message),
                },
            };
            let _ = outbox.send(message);
        });
    }

    /// Poll lifecycle metadata for one plugin command. Herdr's log response
    /// also contains command arguments and process output; `plugin::status`
    /// discards those before the result can cross the daemon boundary.
    fn get_plugin_run(&mut self, client: ClientId, request: u64, run: plugin::PluginRunId) {
        let key = run.target.clone();
        let Some(target) = self.targets.get(&key).cloned() else {
            self.refuse(client, request, format!("{key} is not a configured target"));
            return;
        };
        let Some(outbox) = self.outboxes.get(&client).cloned() else {
            return;
        };
        let transport = self.transport.clone();
        let request_timeout = self.command_timeout;
        tokio::spawn(async move {
            let message = match plugin::status(&run, &target, &transport, request_timeout).await {
                Ok(run) => ServerMessage::PluginRun { request, run },
                Err(error) => ServerMessage::Error {
                    request: Some(request),
                    message: format!("plugin run unavailable on {key}: {}", error.message),
                },
            };
            let _ = outbox.send(message);
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
        // Each fresh event is offered to the device sink before it is
        // broadcast. The per-agent mark decides: an agent somebody muted or
        // snoozed to get it out of their inbox has not asked to keep hearing
        // from it on a phone, and one set to needs-you-only has asked for
        // exactly one kind of interruption.
        let now_ms = unix_time_ms();
        let at = Instant::now();
        for event in &fresh {
            let agent = agent_key_for_pane(&self.state, &event.pane);
            let needs_attention = event.kind == AttentionEventKind::NeedsAttention;
            if self
                .agent_marks
                .state(&agent)
                .may_notify(needs_attention, now_ms)
            {
                self.device_notifications.enqueue(event, at);
            }
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
        let mut effects = self.broker.attention_changed(events);
        // Marking history seen changes what the inbox should show, so the
        // cards are rebuilt from the same act rather than waiting for whatever
        // federation refresh happens to come next.
        effects.extend(self.project_agent_cards());
        effects
    }

    /// Rebuild the inbox and publish it if it moved.
    ///
    /// The projection is derived, never stored durably: it is a view of the
    /// federation and the attention index, both of which already survive a
    /// restart on their own terms. Rebuilding it is cheap and keeps one
    /// authority for each fact.
    /// Hand one coalesced alert to every subscribed device, if one is due.
    fn device_alerts(&mut self) -> Vec<Effect> {
        let Some(delivery) = self.device_notifications.take_ready(Instant::now()) else {
            return Vec::new();
        };
        let Some(pane) = delivery.pane().cloned() else {
            return Vec::new();
        };
        self.broker.notify_devices(ServerMessage::Notification {
            // Resolved now rather than when the event was recorded: the
            // identity a person taps should name the agent as it currently is,
            // and if it has gone the tap is refused rather than delivered
            // somewhere else.
            agent: agent_key_for_pane(&self.state, &pane),
            title: delivery.title().to_owned(),
            body: delivery.body().to_owned(),
        })
    }

    fn project_agent_cards(&mut self) -> Vec<Effect> {
        let now_ms = unix_time_ms();
        // A snooze that has run out stops being carried before anything is
        // built from it. This runs on the projection path rather than on a
        // timer of its own: the federation refreshes on a bounded interval
        // anyway, so an expiry surfaces within one refresh without another
        // clock in the process to keep correct.
        if self.agent_marks.expire(now_ms) {
            self.persist_agent_marks();
        }
        let projection =
            self.agent_cards
                .project(&self.state, &self.attention, &self.agent_marks, now_ms);
        self.broker.agent_cards_updated(projection)
    }

    fn persist_agent_marks(&self) {
        if let Some(store) = self.agent_mark_store.as_ref() {
            // As with attention: a failed write costs the next restart these
            // marks and nothing else, and must not interrupt supervision.
            let _ = store.save(&self.agent_marks);
        }
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
) -> Result<ExecutionOutcome, String> {
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
        .map(|summary| ExecutionOutcome {
            detail: Some(format!(
                "moved {} tab(s) and {} pane(s)",
                summary.tabs, summary.panes
            )),
            plugin_run: None,
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
        .map(|summary| ExecutionOutcome {
            detail: Some(format!(
                "recreated {} tab(s) and {} pane(s) as {}",
                summary.tabs, summary.panes, summary.workspace
            )),
            plugin_run: None,
        }),
        Operation::InvokePluginAction {
            action, context, ..
        } => plugin::invoke(action, context, source, transport, timeout)
            .await
            .map(|plugin_run| ExecutionOutcome {
                detail: Some("plugin action started".to_owned()),
                plugin_run: Some(plugin_run),
            })
            .map_err(|error| error.message),
        single => {
            let args = single
                .herdr_args()
                .ok_or_else(|| "this operation has no single command".to_owned())?;
            let executable =
                executable.ok_or_else(|| "no compatible Herdr client is selected".to_owned())?;
            run_herdr_operation(source, transport, executable, &args, timeout)
                .await
                .map(|()| ExecutionOutcome {
                    detail: None,
                    plugin_run: None,
                })
                .map_err(|error| error.message)
        }
    }
}

struct ExecutionOutcome {
    detail: Option<String>,
    plugin_run: Option<plugin::PluginRun>,
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

    use super::{DaemonOptions, MAX_RETAINED_TRANSFERS, invalidated_routes, serve, serve_until};
    use crate::config::{Config, Device, Target};
    use crate::model::PaneId;
    use crate::protocol::{
        ClientMessage, PROTOCOL_VERSION, PaneRepresentation, ServerMessage, decode, encode,
    };
    use crate::terminal::TerminalAccess;

    /// A frontend hosting its own daemon must serve the browser client too.
    ///
    /// The bug this is here for: the listener was started beside the *socket*,
    /// which only the `daemon` subcommand creates. Run as the frontend — the
    /// way this is normally run — the daemon skipped it entirely, so a pairing
    /// code named a port nothing was listening on. Scanning it produced a URL
    /// that opened nothing, which looks like a broken QR and is not one.
    #[tokio::test]
    async fn a_hosted_daemon_serves_the_browser_client_too() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};

        // Asked for and released, so the port is one this machine will give.
        let port = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let directory = tempfile::tempdir().unwrap();
        let options = DaemonOptions {
            socket: directory.path().join("daemon.sock"),
            agent_marks: None,
            attention_state: Some(directory.path().join("attention.json")),
            refresh_interval: Duration::from_secs(3600),
            web_port: Some(port),
            // Loopback, because a test may not have any other address.
            web_address: None,
            web_url: None,
            web_bridge: None,
        };

        let daemon = super::spawn_in_process(empty_config(), None, options);
        let page = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)).await {
                    stream
                        .write_all(b"GET / HTTP/1.1\r\nhost: test\r\nconnection: close\r\n\r\n")
                        .await
                        .unwrap();
                    let mut response = Vec::new();
                    stream.read_to_end(&mut response).await.unwrap();
                    return String::from_utf8_lossy(&response).into_owned();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the hosted daemon never served the browser client");

        assert!(
            page.starts_with("HTTP/1.1 200"),
            "{}",
            &page[..40.min(page.len())]
        );
        assert!(page.contains("<!doctype html>") || page.contains("<!DOCTYPE html>"));
        drop(daemon);
    }

    /// A federation with no targets exercises the whole I/O path without
    /// starting a single Herdr command.
    fn empty_config() -> Config {
        Config {
            transport: Default::default(),
            notifications: Default::default(),
            transfers: Default::default(),
            web: Default::default(),
            targets: Vec::new(),
            devices: Vec::new(),
            quick_replies: None,
        }
    }

    struct Harness {
        directory: tempfile::TempDir,
        server: JoinHandle<anyhow::Result<()>>,
        stop: Option<tokio::sync::oneshot::Sender<()>>,
    }

    /// One target on this machine, so a transfer has somewhere to go without a
    /// host or a fixture. `ssh: None` is what makes the sink local, and the
    /// name is the one `leased_pane` qualifies its pane with.
    fn local_target_config() -> Config {
        Config {
            targets: vec![Target {
                name: "first".to_owned(),
                ssh: None,
                discover_sessions: false,
                session: None,
                socket: None,
                herdr_bins: vec!["/nonexistent/herdr".to_owned()],
            }],
            ..empty_config()
        }
    }

    /// Two targets, both on this machine. A move between them is a real move
    /// as far as the daemon is concerned: two configured targets, two panes,
    /// two leases, and the same code path a pair of hosts would take.
    fn two_local_targets_config() -> Config {
        let mut config = local_target_config();
        config.targets.push(Target {
            name: "second".to_owned(),
            ssh: None,
            discover_sessions: false,
            session: None,
            socket: None,
            herdr_bins: vec!["/nonexistent/herdr".to_owned()],
        });
        config
    }

    impl Harness {
        async fn start() -> Self {
            Self::start_with(empty_config()).await
        }

        async fn start_with(config: Config) -> Self {
            let directory = tempfile::tempdir().expect("a temporary directory");
            let options = DaemonOptions {
                socket: directory.path().join("daemon.sock"),
                agent_marks: None,
                attention_state: Some(directory.path().join("attention.json")),
                // Pinned: these tests are about the socket, not the file.
                refresh_interval: Duration::from_secs(3600),
                web_port: None,
                web_address: None,
                web_url: None,
                web_bridge: None,
            };
            let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
            let server = tokio::spawn(serve_until(config, None, options, async move {
                let _ = stopped.await;
            }));
            Self {
                directory,
                server,
                stop: Some(stop),
            }
        }

        /// Stop the daemon the way a person does, and wait for it.
        ///
        /// Aborting the task instead is a killed process, which leaves whatever
        /// a killed process leaves. A test that ends that way litters a host —
        /// here, the machine running the suite.
        async fn stop(mut self) {
            if let Some(stop) = self.stop.take() {
                let _ = stop.send(());
            }
            // Waited for rather than abandoned: a stopping daemon gives a host
            // back what it was holding, and that is the thing being relied on.
            for _ in 0..500 {
                if self.server.is_finished() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("the daemon did not stop when asked");
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
                    | ServerMessage::UploadComplete { .. }
                    | ServerMessage::UploadInterrupted { .. }) => return message,
                    _ => continue,
                }
            }
        }

        /// Whether the daemon stays quiet about chunks for this long.
        ///
        /// Other messages are allowed through — a lease, a route that could not
        /// open — because what is being asserted is that no *bytes* were sent,
        /// not that the daemon said nothing at all.
        async fn no_chunk_within(&mut self, patience: Duration) -> bool {
            let deadline = tokio::time::Instant::now() + patience;
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return true;
                }
                let mut line = Vec::new();
                let read =
                    tokio::time::timeout(remaining, self.reader.read_until(b'\n', &mut line)).await;
                match read {
                    Err(_) => return true,
                    Ok(Ok(0)) | Ok(Err(_)) => return true,
                    Ok(Ok(_)) => {
                        line.pop();
                        if let Ok(ServerMessage::DownloadChunk { .. }) = decode(&line) {
                            return false;
                        }
                    }
                }
            }
        }

        /// Whatever the daemon says about a download.
        async fn download_offer(&mut self) -> ServerMessage {
            loop {
                match self.receive().await.expect("the daemon answers") {
                    message @ (ServerMessage::DownloadOffer { .. }
                    | ServerMessage::DownloadFinished { .. }
                    | ServerMessage::Error { .. }) => return message,
                    _ => continue,
                }
            }
        }

        /// The token and offset a transfer is given before it may send.
        async fn accepted(&mut self) -> ServerMessage {
            loop {
                match self.receive().await.expect("the daemon answers") {
                    message @ (ServerMessage::UploadAccepted { .. }
                    | ServerMessage::Error { .. }) => return message,
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
                agent_marks: None,
                attention_state: Some(directory.path().join("attention.json")),
                refresh_interval: Duration::from_millis(50),
                web_port: None,
                web_address: None,
                web_url: None,
                web_bridge: None,
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
        offer(connection, mime, None, declared, chunks, digest).await
    }

    /// The same, for a caller that has a name for what it is sending.
    async fn offer(
        connection: &mut Connection,
        mime: &str,
        name: Option<&str>,
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
                name: name.map(str::to_owned),
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

    /// What this daemon will refuse to exceed, straight from the configuration
    /// rather than restated here.
    fn ceiling() -> u64 {
        crate::config::TransferConfig::default().max_bytes
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
                representation: PaneRepresentation::Frames,
            })
            .await;
        pane
    }

    #[tokio::test]
    async fn a_transfer_is_refused_for_each_check_it_fails() {
        // A real sink, because the daemon no longer holds a transfer long
        // enough to judge it on its own: the bytes are relayed as they arrive,
        // and every check below is decided against what actually moved.
        let harness = Harness::start_with(local_target_config()).await;
        let mut connection = harness.connect().await;
        connection.hello().await;
        // Declared above the ceiling: refused before a byte moves.
        let refusal = upload(&mut connection, "image/png", ceiling() + 1, &[], None).await;
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

        // A type the table does not know is carried rather than refused: a
        // file from a device has no clipboard flavor at all, and that is the
        // same case rather than a second one. With a sink configured it does
        // not merely avoid one refusal, it arrives.
        let carried = upload(
            &mut connection,
            "application/x-invented",
            4,
            &[b"four".as_slice()],
            Some(&super::sha256_hex(b"four")),
        )
        .await;
        let ServerMessage::UploadComplete { path, bytes, .. } = &carried else {
            panic!("an unknown type must not be refused for being unknown: {carried:?}");
        };
        assert_eq!(*bytes, 4);
        crate::clipboard::discard_local_upload(std::path::Path::new(path));
    }

    /// What the relay accumulates is what the sink writes.
    ///
    /// The transport itself is qualified separately against a real host, and
    /// the guards are unit tested. What neither covers is the join between
    /// them: that the bytes reassembled from chunks are the bytes that reach a
    /// file. This runs the whole relay against a local target, so it needs no
    /// host and no fixture, and reads the result back off disk.
    #[tokio::test]
    async fn a_relayed_payload_reaches_the_sink_byte_for_byte() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let socket = directory.path().join("daemon.sock");
        let config = Config {
            transport: Default::default(),
            notifications: Default::default(),
            transfers: Default::default(),
            web: Default::default(),
            targets: vec![Target {
                name: "here".to_owned(),
                // No ssh destination: the sink is this machine, which is what
                // makes this runnable anywhere.
                ssh: None,
                discover_sessions: false,
                session: None,
                socket: None,
                herdr_bins: vec!["/nonexistent/herdr".to_owned()],
            }],
            devices: Vec::new(),
            quick_replies: None,
        };
        let server = tokio::spawn(serve(
            config,
            None,
            DaemonOptions {
                socket: socket.clone(),
                agent_marks: None,
                attention_state: Some(directory.path().join("attention.json")),
                refresh_interval: Duration::from_secs(3600),
                web_port: None,
                web_address: None,
                web_url: None,
                web_bridge: None,
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

        // A payload larger than one chunk, so reassembly is actually exercised.
        let mut payload = b"\x89PNG\r\n\x1a\n".to_vec();
        payload.extend((0..200_000_u32).map(|index| index as u8));
        let pane = PaneId::new("here", "default", "w1:p1");
        connection
            .send(ClientMessage::SubscribePane {
                pane: pane.clone(),
                access: TerminalAccess::Control,
                cols: 80,
                rows: 24,
                representation: PaneRepresentation::Frames,
            })
            .await;
        connection
            .send(ClientMessage::BeginUpload {
                request: 1,
                pane,
                mime: "image/png".to_owned(),
                name: None,
                length: payload.len() as u64,
            })
            .await;
        for chunk in payload.chunks(64 * 1024) {
            connection
                .send(ClientMessage::UploadChunk {
                    request: 1,
                    bytes: chunk.to_vec(),
                })
                .await;
        }
        connection
            .send(ClientMessage::FinishUpload {
                request: 1,
                digest: super::sha256_hex(&payload),
            })
            .await;

        let result = connection.transfer_result().await;
        let ServerMessage::UploadComplete { path, bytes, .. } = result else {
            panic!("the relay refused a payload it should have carried: {result:?}");
        };
        assert_eq!(bytes as usize, payload.len());

        let written = std::fs::read(&path).expect("the sink wrote the payload");
        assert_eq!(
            super::sha256_hex(&written),
            super::sha256_hex(&payload),
            "the bytes that reached the sink are not the bytes that were sent"
        );
        // One idiom for this, everywhere, because the shape of a staged
        // transfer has already changed once today: it was a file in the system
        // temporary directory this morning and is a private directory holding
        // that file now. `remove_file` and `remove_dir_all(parent)` each read
        // as correct under one of those and are wrong under the other, so
        // neither is written here. The guard answers what may be removed, and
        // it is the same guard the daemon itself uses.
        crate::clipboard::discard_local_upload(std::path::Path::new(&path));
        server.abort();
    }

    /// A payload no other transfer carries, in this run or a previous one.
    ///
    /// Content is what identifies a staged file here, so the marker has to be
    /// unique per process as well as per test: a leftover from an earlier run —
    /// or from a run that deliberately broke the cleanup to check this test can
    /// fail — would otherwise fail every run after it.
    fn marked_payload(name: &str) -> Vec<u8> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);

        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let mut payload = b"\x89PNG\r\n\x1a\n".to_vec();
        payload
            .extend_from_slice(format!("marker/{name}/{}/{serial}", std::process::id()).as_bytes());
        payload
    }

    /// Whether anything a local sink could have staged holds these bytes.
    ///
    /// Identified by content rather than by counting files, so it does not
    /// depend on what else is running in the same temporary directory at the
    /// same moment.
    fn staged_anywhere(marker: &[u8]) -> bool {
        let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
            return false;
        };
        entries
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("super-herdr-clipboard.")
            })
            // A staged transfer is a private directory with the file inside it
            // under its own name, so the payload is one level down.
            .filter_map(|directory| std::fs::read_dir(directory.path()).ok())
            .flatten()
            .flatten()
            .any(|entry| {
                std::fs::read(entry.path())
                    .is_ok_and(|bytes| bytes.windows(marker.len()).any(|window| window == marker))
            })
    }

    /// Whether something stays true for long enough to believe it will.
    ///
    /// The opposite of waiting for a change: retention has to be shown by
    /// nothing happening, and nothing happening takes as long to observe as
    /// anything else.
    async fn settles_on_absence(condition: impl Fn() -> bool) -> bool {
        for _ in 0..40 {
            if !condition() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    /// Remove whichever staging directory holds these bytes, if any.
    ///
    /// For the one case a stopping daemon cannot reach: an attempt in flight,
    /// whose path exists only on the host until it finishes.
    fn discard_staged(marker: &[u8]) {
        let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
            return;
        };
        for directory in entries.flatten().filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("super-herdr-clipboard.")
        }) {
            let holds = std::fs::read_dir(directory.path())
                .into_iter()
                .flatten()
                .flatten()
                .any(|entry| {
                    std::fs::read(entry.path()).is_ok_and(|bytes| {
                        bytes.windows(marker.len()).any(|window| window == marker)
                    })
                });
            if holds {
                let _ = std::fs::remove_dir_all(directory.path());
            }
        }
    }

    async fn settles_on(condition: impl Fn() -> bool) -> bool {
        for _ in 0..400 {
            if condition() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    /// A transfer nobody finished is kept for its sender to come back to.
    ///
    /// What is kept stays inert: no path is reported, what a sender is told is
    /// a byte count rather than a location, and nothing is injected into a pane
    /// until a digest verifies. The rule is that nothing partial is ever named,
    /// reported, or acted on, and keeping the bytes does not touch it — what it
    /// buys is a dropped connection costing a reconnect rather than a
    /// gigabyte.
    #[tokio::test]
    async fn an_interrupted_transfer_is_kept_and_can_be_finished_later() {
        let harness = Harness::start_with(local_target_config()).await;
        let mut connection = harness.connect().await;
        connection.hello().await;
        let pane = leased_pane(&mut connection).await;

        let payload = marked_payload("resumable");
        let split = payload.len() / 2;
        connection
            .send(ClientMessage::BeginUpload {
                request: 1,
                pane: pane.clone(),
                mime: "image/png".to_owned(),
                name: None,
                length: payload.len() as u64,
            })
            .await;
        let ServerMessage::UploadAccepted {
            transfer, staged, ..
        } = connection.accepted().await
        else {
            panic!("a transfer must be told what it is called before it can be resumed");
        };
        assert_eq!(staged, 0, "a new transfer starts from nothing");
        connection
            .send(ClientMessage::UploadChunk {
                request: 1,
                bytes: payload[..split].to_vec(),
            })
            .await;
        assert!(
            settles_on(|| staged_anywhere(&payload[..split])).await,
            "the relay never reached the sink"
        );

        // The sender vanishes without attesting to anything.
        drop(connection);

        // What arrived stays, because a sender that says nothing has not said
        // it is finished.
        assert!(
            !settles_on_absence(|| staged_anywhere(&payload[..split])).await,
            "an interrupted transfer was discarded rather than kept"
        );

        // A second connection comes back for it. The token is what names it;
        // the lease is what still authorizes it.
        let mut connection = harness.connect().await;
        connection.hello().await;
        let pane = leased_pane(&mut connection).await;
        connection
            .send(ClientMessage::ResumeUpload {
                request: 2,
                transfer: transfer.clone(),
                pane,
                length: payload.len() as u64,
            })
            .await;
        let ServerMessage::UploadAccepted {
            staged: resumed_at, ..
        } = connection.accepted().await
        else {
            panic!("a resumed transfer must be told where to continue from");
        };
        assert_eq!(
            resumed_at as usize, split,
            "the offset must come from what the host holds"
        );

        connection
            .send(ClientMessage::UploadChunk {
                request: 2,
                bytes: payload[resumed_at as usize..].to_vec(),
            })
            .await;
        connection
            .send(ClientMessage::FinishUpload {
                request: 2,
                // Over the whole file, including the half that crossed on a
                // connection that no longer exists.
                digest: super::sha256_hex(&payload),
            })
            .await;

        let result = connection.transfer_result().await;
        let ServerMessage::UploadComplete { path, bytes, .. } = &result else {
            panic!("a resumed transfer was not accepted: {result:?}");
        };
        assert_eq!(*bytes as usize, payload.len());
        assert_eq!(std::fs::read(path).unwrap(), payload);
        crate::clipboard::discard_local_upload(std::path::Path::new(path));
    }

    /// Retention is bounded by a count as well as a clock.
    ///
    /// A clock alone bounds how long one sender may occupy a host. Without a
    /// count, a sender that keeps starting transfers and walking away occupies
    /// it as many times over as it likes, and every one of them is allowed to
    /// be as large as the ceiling.
    #[tokio::test]
    async fn only_so_many_unfinished_transfers_are_kept() {
        let harness = Harness::start_with(local_target_config()).await;
        let mut first: Option<Vec<u8>> = None;
        let mut last: Option<Vec<u8>> = None;

        for index in 0..=MAX_RETAINED_TRANSFERS {
            let mut connection = harness.connect().await;
            connection.hello().await;
            let pane = leased_pane(&mut connection).await;
            let payload = marked_payload("crowded");
            connection
                .send(ClientMessage::BeginUpload {
                    request: 1,
                    pane,
                    mime: "image/png".to_owned(),
                    name: None,
                    // Declared longer than what is sent, so every one of these
                    // stops without finishing.
                    length: payload.len() as u64 + 64,
                })
                .await;
            let _ = connection.accepted().await;
            connection
                .send(ClientMessage::UploadChunk {
                    request: 1,
                    bytes: payload.clone(),
                })
                .await;
            assert!(
                settles_on(|| staged_anywhere(&payload)).await,
                "transfer {index} never reached the sink"
            );
            drop(connection);
            if first.is_none() {
                first = Some(payload.clone());
            }
            last = Some(payload);
        }

        let (first, last) = (
            first.expect("the first transfer was recorded"),
            last.expect("the last transfer was recorded"),
        );
        assert!(
            settles_on(|| !staged_anywhere(&first)).await,
            "the oldest unfinished transfer was kept past the limit"
        );
        // Stopped rather than abandoned, so the ones still retained go back to
        // the host instead of onto whichever machine ran this.
        harness.stop().await;
        // Except the last, whose attempt may still have been in flight: its
        // staging path is not known to the daemon until the attempt ends, so
        // stopping cannot reach it. That is the one case `discard_retained`
        // documents, and the test cleans up after it rather than pretending it
        // does not exist.
        discard_staged(&last);
    }

    /// What the daemon was told is what a client is offered, and nothing is
    /// invented when it was told nothing.
    ///
    /// The whole point of the flag: a URL cannot be derived from the loopback
    /// address this process binds, so it either arrives from configuration or
    /// it is absent. A daemon that filled the gap with its own bind would hand
    /// a client a perfectly valid QR of somewhere no phone can reach.
    #[tokio::test]
    async fn a_pairing_code_carries_the_url_the_daemon_was_told_and_no_other() {
        for told in [Some("https://host.example:8790".to_owned()), None] {
            let directory = tempfile::tempdir().expect("a temporary directory");
            let socket = directory.path().join("daemon.sock");
            let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
            let server = tokio::spawn(serve_until(
                empty_config(),
                None,
                DaemonOptions {
                    socket: socket.clone(),
                    agent_marks: None,
                    attention_state: Some(directory.path().join("attention.json")),
                    refresh_interval: Duration::from_secs(3600),
                    web_port: None,
                    web_address: None,
                    web_url: told.clone(),
                    web_bridge: None,
                },
                async move {
                    let _ = stopped.await;
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
            connection
                .send(ClientMessage::RequestPairingCode { request: 1 })
                .await;

            let issued = loop {
                match connection.receive().await.expect("the daemon answers") {
                    message @ (ServerMessage::PairingCode { .. } | ServerMessage::Error { .. }) => {
                        break message;
                    }
                    _ => continue,
                }
            };
            let ServerMessage::PairingCode { code, url, .. } = &issued else {
                panic!("no pairing code was issued: {issued:?}");
            };
            assert!(!code.is_empty(), "a code is issued either way");
            assert_eq!(url, &told, "the daemon offered a URL it was not given");

            let _ = stop.send(());
            let _ = tokio::time::timeout(Duration::from_secs(10), server).await;
        }
    }

    #[tokio::test]
    async fn a_typed_code_needs_matching_trusted_approval_before_it_creates_a_device() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};

        let directory = tempfile::tempdir().expect("a temporary directory");
        let config_path = directory.path().join("config.toml");
        let config = local_target_config();
        std::fs::write(&config_path, toml::to_string_pretty(&config).unwrap()).unwrap();
        let socket = directory.path().join("daemon.sock");
        let port = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_until(
            config,
            Some(config_path.clone()),
            DaemonOptions {
                socket: socket.clone(),
                agent_marks: None,
                attention_state: Some(directory.path().join("attention.json")),
                refresh_interval: Duration::from_secs(3600),
                web_port: Some(port),
                web_address: None,
                web_url: Some("https://super-herdr.key-value.co".to_owned()),
                web_bridge: None,
            },
            async move {
                let _ = stopped.await;
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
        let mut connection = connection.expect("the daemon accepts a trusted client");
        connection.hello().await;
        connection.send(ClientMessage::SubscribeState).await;
        connection
            .send(ClientMessage::RequestPairingCode { request: 1 })
            .await;
        let code = loop {
            if let ServerMessage::PairingCode { code, .. } = connection
                .receive()
                .await
                .expect("the daemon issues a code")
            {
                break code;
            }
        };

        Config::add_device_file(
            Some(&config_path),
            Device {
                name: "phone".to_owned(),
                token_sha256: crate::pairing::fingerprint("already paired token"),
                paired_at_ms: 1,
            },
        )
        .unwrap();
        let duplicate_body =
            format!("{{\"code\":\"{code}\",\"name\":\"phone\",\"confirmation\":\"482193\"}}");
        let duplicate_response = tokio::time::timeout(Duration::from_secs(2), async {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            stream
                .write_all(
                    format!(
                        "POST /pair HTTP/1.1\r\nhost: localhost\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{duplicate_body}",
                        duplicate_body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            response
        })
        .await
        .expect("a duplicate device name waited for pointless approval");
        assert!(
            duplicate_response.starts_with(b"HTTP/1.1 409 Conflict"),
            "{}",
            String::from_utf8_lossy(&duplicate_response)
        );
        assert!(
            String::from_utf8_lossy(&duplicate_response)
                .contains("Choose a different device name and try this code again")
        );

        let body =
            format!("{{\"code\":\"{code}\",\"name\":\"tablet\",\"confirmation\":\"482193\"}}");
        let browser = tokio::spawn(async move {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            stream
                .write_all(
                    format!(
                        "POST /pair HTTP/1.1\r\nhost: localhost\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            response
        });

        let attempt = loop {
            match connection
                .receive()
                .await
                .expect("the trusted client receives an approval request")
            {
                ServerMessage::PairingApprovalRequired {
                    attempt,
                    name,
                    confirmation,
                    ..
                } => {
                    assert_eq!(name, "tablet");
                    assert_eq!(confirmation, "482193");
                    break attempt;
                }
                _ => continue,
            }
        };
        assert!(
            Config::load(Some(&config_path)).unwrap().0.devices.len() == 1,
            "knowing the short code created a device before approval"
        );

        connection
            .send(ClientMessage::DecidePairing {
                attempt,
                approve: true,
            })
            .await;
        let response = tokio::time::timeout(Duration::from_secs(10), browser)
            .await
            .expect("the browser did not receive the decision")
            .unwrap();
        assert!(response.starts_with(b"HTTP/1.1 204 No Content"));
        let loaded = Config::load(Some(&config_path)).unwrap().0;
        assert_eq!(loaded.devices.len(), 2);
        assert_eq!(loaded.devices[0].name, "phone");
        assert_eq!(loaded.devices[1].name, "tablet");

        let _ = stop.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), server).await;
    }

    /// A caller's name reaches the host, and a bad one never leaves the daemon.
    ///
    /// The name is what makes this a file bridge rather than a clipboard
    /// bridge, and it is the one part of a transfer that is judged before
    /// anything is opened — so the refusal must arrive without a byte moving,
    /// and the acceptance must show up in the path that comes back.
    #[tokio::test]
    async fn a_named_transfer_arrives_under_its_name() {
        let harness = Harness::start_with(local_target_config()).await;
        let mut connection = harness.connect().await;
        connection.hello().await;

        let payload = marked_payload("named");
        let carried = offer(
            &mut connection,
            "application/octet-stream",
            Some("release-notes.md"),
            payload.len() as u64,
            &[payload.as_slice()],
            Some(&super::sha256_hex(&payload)),
        )
        .await;
        let ServerMessage::UploadComplete { path, .. } = &carried else {
            panic!("a named transfer was refused: {carried:?}");
        };
        assert!(path.ends_with("/release-notes.md"), "{path}");
        assert_eq!(std::fs::read(path).unwrap(), payload);
        crate::clipboard::discard_local_upload(std::path::Path::new(path));

        // A name that would leave its directory is refused, and refused where
        // refusing is free: nothing is staged for it anywhere.
        let escaping = marked_payload("escaping");
        let refusal = offer(
            &mut connection,
            "application/octet-stream",
            Some("../escape.md"),
            escaping.len() as u64,
            &[escaping.as_slice()],
            Some(&super::sha256_hex(&escaping)),
        )
        .await;
        assert!(
            matches!(&refusal, ServerMessage::Error { message, .. } if message.contains("name")),
            "{refusal:?}"
        );
        assert!(
            !staged_anywhere(&escaping),
            "a refused name still put bytes on the target"
        );
    }

    /// A file on a target reaches the client that asked for it, intact.
    ///
    /// The whole point of the direction: the daemon carries and counts, the
    /// host says what the file is, and the client is the one that checks. So
    /// the assertion is the client's — the digest the daemon relayed is the
    /// digest of the bytes that arrived.
    #[tokio::test]
    async fn a_file_on_a_target_is_read_back_to_the_client_that_asked() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let source = directory.path().join("build.log");
        // Larger than one chunk, so the pull loop runs more than once.
        let contents: Vec<u8> = (0..600_000_u32).map(|index| index as u8).collect();
        std::fs::write(&source, &contents).unwrap();

        let harness = Harness::start_with(local_target_config()).await;
        let mut connection = harness.connect().await;
        connection.hello().await;
        let pane = leased_pane(&mut connection).await;

        connection
            .send(ClientMessage::BeginDownload {
                request: 1,
                pane,
                path: source.display().to_string(),
            })
            .await;
        let offer = connection.download_offer().await;
        let ServerMessage::DownloadOffer {
            name,
            length,
            digest,
            ..
        } = &offer
        else {
            panic!("the daemon refused a readable file: {offer:?}");
        };
        assert_eq!(name, "build.log", "a client is told a name, never a path");
        assert_eq!(*length as usize, contents.len());

        let expected = digest.clone();
        let length = *length;
        let mut received = Vec::new();
        while (received.len() as u64) < length {
            // Asked for one at a time, which is the case worth exercising: if
            // credit were not honoured, everything would arrive anyway.
            connection
                .send(ClientMessage::PullDownload {
                    request: 1,
                    chunks: 1,
                })
                .await;
            match connection.receive().await.expect("the daemon answers") {
                ServerMessage::DownloadChunk { bytes, .. } => received.extend_from_slice(&bytes),
                ServerMessage::Error { message, .. } => panic!("{message}"),
                _ => continue,
            }
        }
        assert_eq!(received, contents);
        // The client verifies, because the daemon deliberately did not.
        assert_eq!(super::sha256_hex(&received), expected);

        let finished = connection.download_offer().await;
        assert!(
            matches!(finished, ServerMessage::DownloadFinished { .. }),
            "a finished download says so rather than simply stopping: {finished:?}"
        );
        harness.stop().await;
    }

    /// Nothing is sent to a client that has not asked for it.
    ///
    /// This is the whole of the flow control, and it is easy to write a version
    /// that looks right and sends anyway. The queue to a client is unbounded,
    /// so a download that ignored credit would put the file in the daemon's
    /// memory whether or not anybody was reading.
    #[tokio::test]
    async fn a_download_sends_nothing_until_it_is_pulled() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let source = directory.path().join("quiet.bin");
        std::fs::write(&source, vec![4u8; 400_000]).unwrap();

        let harness = Harness::start_with(local_target_config()).await;
        let mut connection = harness.connect().await;
        connection.hello().await;
        let pane = leased_pane(&mut connection).await;

        connection
            .send(ClientMessage::BeginDownload {
                request: 1,
                pane,
                path: source.display().to_string(),
            })
            .await;
        assert!(
            matches!(
                connection.download_offer().await,
                ServerMessage::DownloadOffer { .. }
            ),
            "the offer arrives without being asked for; the bytes do not"
        );

        // The assertion has to be about silence, not about what arrives first.
        // A daemon ignoring credit fills the client's queue immediately, and
        // reading from a full queue looks exactly like reading from one being
        // filled on request — which is how a version of this test that pulled
        // and then read passed against a daemon that ignored credit entirely.
        assert!(
            connection.no_chunk_within(Duration::from_millis(300)).await,
            "a download sent bytes nobody had asked for"
        );

        connection
            .send(ClientMessage::PullDownload {
                request: 1,
                chunks: 1,
            })
            .await;
        let first = connection.receive().await.expect("the daemon answers");
        let ServerMessage::DownloadChunk { bytes, .. } = &first else {
            panic!("the first thing after a pull must be a chunk: {first:?}");
        };
        assert_eq!(
            bytes.len(),
            super::DOWNLOAD_CHUNK_BYTES,
            "one pull is one chunk"
        );
        // And then silence again, because one chunk of credit is one chunk.
        assert!(
            connection.no_chunk_within(Duration::from_millis(300)).await,
            "a download kept sending past the credit it was given"
        );
        harness.stop().await;
    }

    /// A client cannot instruct the daemon to hold more than it agreed to.
    ///
    /// Credit says what a client is ready for, and a client is not a reliable
    /// witness about itself: a window computed from a file's size rather than
    /// from a buffer is an ordinary mistake, and the queue to a client is
    /// unbounded, so an unclamped grant puts the whole file in this process —
    /// exactly what credit exists to prevent.
    #[tokio::test]
    async fn a_grant_larger_than_the_daemon_agreed_to_is_not_authority() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let source = directory.path().join("large.bin");
        // Comfortably more than the daemon will hold at once.
        let contents = vec![7u8; super::DOWNLOAD_CHUNK_BYTES * 20];
        std::fs::write(&source, &contents).unwrap();

        let harness = Harness::start_with(local_target_config()).await;
        let mut connection = harness.connect().await;
        connection.hello().await;
        let pane = leased_pane(&mut connection).await;

        connection
            .send(ClientMessage::BeginDownload {
                request: 1,
                pane,
                path: source.display().to_string(),
            })
            .await;
        assert!(matches!(
            connection.download_offer().await,
            ServerMessage::DownloadOffer { .. }
        ));

        // Everything, at once, from a client that means well.
        connection
            .send(ClientMessage::PullDownload {
                request: 1,
                chunks: u32::MAX,
            })
            .await;

        let mut chunks = 0usize;
        while chunks < super::MAX_OUTSTANDING_CHUNKS as usize {
            match connection.receive().await.expect("the daemon answers") {
                ServerMessage::DownloadChunk { .. } => chunks += 1,
                ServerMessage::Error { message, .. } => panic!("{message}"),
                _ => continue,
            }
        }
        // And then it waits, because what it was granted is not what it holds.
        assert!(
            connection.no_chunk_within(Duration::from_millis(300)).await,
            "a client's grant decided how much the daemon would hold"
        );
        harness.stop().await;
    }

    /// A file moves between hosts without the device in the middle.
    ///
    /// The direction that only the daemon can express, because only the daemon
    /// holds live connections to both. What is asserted is what arrived: the
    /// destination's copy is byte-for-byte the source's, and it was named
    /// without anyone naming it.
    #[tokio::test]
    async fn a_file_moves_between_targets_without_passing_through_the_client() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let source = directory.path().join("artifact.tar");
        // More than one stream chunk, so the copy loop runs more than once.
        let contents: Vec<u8> = (0..300_000_u32).map(|index| (index / 7) as u8).collect();
        std::fs::write(&source, &contents).unwrap();

        let harness = Harness::start_with(two_local_targets_config()).await;
        let mut connection = harness.connect().await;
        connection.hello().await;
        let from = leased_pane(&mut connection).await;
        let to = PaneId::new("second", "default", "w1:p1");
        connection
            .send(ClientMessage::SubscribePane {
                pane: to.clone(),
                access: TerminalAccess::Control,
                representation: PaneRepresentation::Frames,
                cols: 80,
                rows: 24,
            })
            .await;

        connection
            .send(ClientMessage::TransferBetween {
                request: 1,
                source: from,
                path: source.display().to_string(),
                destination: to,
                name: None,
            })
            .await;

        let result = connection.transfer_result().await;
        let ServerMessage::UploadComplete { path, bytes, .. } = &result else {
            panic!("a move between targets was refused: {result:?}");
        };
        assert_eq!(*bytes as usize, contents.len());
        // Named after the file rather than after nothing, without the client
        // having said what to call it.
        assert!(path.ends_with("/artifact.tar"), "{path}");
        assert_eq!(std::fs::read(path).unwrap(), contents);
        // And the source is where it always was.
        assert_eq!(std::fs::read(&source).unwrap(), contents);
        crate::clipboard::discard_local_upload(std::path::Path::new(path));
        harness.stop().await;
    }

    /// A copy the two hosts disagree about is refused, and taken away with it.
    ///
    /// The check that cannot fire in a working system: both hosts hash the same
    /// bytes, so a mismatch means something in between changed them, and no
    /// test can arrange that through the daemon. So the comparison is exercised
    /// where it lives instead, against a real staged file, which also asserts
    /// the half that matters most — that a refusal does not leave one.
    #[tokio::test]
    async fn a_copy_the_two_hosts_disagree_about_is_refused_and_removed() {
        let payload = b"bytes that arrived intact and are still not trusted".as_slice();
        let staged = crate::clipboard::upload_media(
            &Target {
                name: "here".to_owned(),
                ssh: None,
                discover_sessions: false,
                session: None,
                socket: None,
                herdr_bins: vec!["/nonexistent/herdr".to_owned()],
            },
            &Default::default(),
            crate::clipboard::OPAQUE,
            payload,
        )
        .await
        .expect("a local sink stages the payload");
        let path = staged.path.clone();
        assert!(std::path::Path::new(&path).exists());

        let destination = Target {
            name: "second".to_owned(),
            ssh: None,
            discover_sessions: false,
            session: None,
            socket: None,
            herdr_bins: vec!["/nonexistent/herdr".to_owned()],
        };
        let error = super::accept_copy(
            &destination,
            &Default::default(),
            staged,
            // What the source said, which is not what the destination stored.
            &"0".repeat(64),
            "first",
        )
        .await
        .expect_err("a disagreement must not be accepted")
        .to_string();

        assert!(
            error.contains("second") && error.contains("first"),
            "{error}"
        );
        assert!(
            !std::path::Path::new(&path).exists(),
            "a refused copy was left on the destination"
        );
    }

    /// A move needs the lease on both ends, not just the one it writes to.
    #[tokio::test]
    async fn a_move_answers_to_the_lease_on_each_host_it_touches() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let source = directory.path().join("artifact.tar");
        std::fs::write(&source, b"contents").unwrap();

        let harness = Harness::start_with(two_local_targets_config()).await;
        let mut connection = harness.connect().await;
        connection.hello().await;
        // Only the destination is leased. Holding it is not permission to read
        // somebody else's host.
        let to = PaneId::new("second", "default", "w1:p1");
        connection
            .send(ClientMessage::SubscribePane {
                pane: to.clone(),
                access: TerminalAccess::Control,
                representation: PaneRepresentation::Frames,
                cols: 80,
                rows: 24,
            })
            .await;
        connection
            .send(ClientMessage::TransferBetween {
                request: 1,
                source: PaneId::new("first", "default", "w1:p1"),
                path: source.display().to_string(),
                destination: to,
                name: None,
            })
            .await;

        // Read past the lease the subscribe granted; what is being asserted is
        // the one it did not.
        let refusal = connection.transfer_result().await;
        assert!(
            matches!(&refusal, ServerMessage::Error { message, .. } if message.contains("control lease")),
            "{refusal:?}"
        );
        assert!(
            !std::path::Path::new(&format!("{}.copy", source.display())).exists(),
            "nothing should have been written"
        );
        harness.stop().await;
    }

    #[tokio::test]
    async fn a_file_that_cannot_be_read_is_refused_rather_than_streamed() {
        let harness = Harness::start_with(local_target_config()).await;
        let mut connection = harness.connect().await;
        connection.hello().await;
        let pane = leased_pane(&mut connection).await;

        for path in [
            "/nonexistent/nothing-is-here",
            // A directory would otherwise be a stream with no end.
            "/tmp",
        ] {
            connection
                .send(ClientMessage::BeginDownload {
                    request: 1,
                    pane: pane.clone(),
                    path: path.to_owned(),
                })
                .await;
            let refusal = connection.download_offer().await;
            assert!(
                matches!(&refusal, ServerMessage::Error { .. }),
                "{path} should not be readable as a file: {refusal:?}"
            );
        }
        harness.stop().await;
    }

    /// A transfer that fails its trailer is unstaged, not merely reported.
    ///
    /// This is the case the relay alone can catch. Every declared byte has
    /// arrived and the host has stored them, so the file exists and passes the
    /// host's own count-and-digest check; only the promise its sender made
    /// about those bytes is wrong.
    #[tokio::test]
    async fn a_refused_digest_takes_the_staged_file_with_it() {
        let harness = Harness::start_with(local_target_config()).await;
        let mut connection = harness.connect().await;
        connection.hello().await;

        let payload = marked_payload("refused-digest");
        let marker = payload.as_slice();
        let refusal = upload(
            &mut connection,
            "image/png",
            payload.len() as u64,
            &[payload.as_slice()],
            Some("0000000000000000000000000000000000000000000000000000000000000000"),
        )
        .await;

        assert!(
            matches!(&refusal, ServerMessage::Error { message, .. } if message.contains("digest")),
            "{refusal:?}"
        );
        assert!(
            !staged_anywhere(marker),
            "a refused transfer left its bytes staged on the target"
        );
    }

    #[tokio::test]
    async fn an_abandoned_transfer_cannot_be_finished_later() {
        let harness = Harness::start_with(local_target_config()).await;
        let mut connection = harness.connect().await;
        connection.hello().await;
        let pane = leased_pane(&mut connection).await;

        connection
            .send(ClientMessage::BeginUpload {
                request: 1,
                pane,
                mime: "image/png".to_owned(),
                name: None,
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

    /// A stopping daemon takes its unfinished transfers with it.
    ///
    /// Retention exists so a sender can come back with a token, and the token
    /// lives in the daemon's memory: a transfer that outlives the process is
    /// unresumable by construction, so bytes kept past that point serve nobody
    /// and sit in a private directory on a host nobody will look at.
    #[tokio::test]
    async fn a_stopped_daemon_takes_its_unfinished_transfers_with_it() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let socket = directory.path().join("daemon.sock");
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_until(
            local_target_config(),
            None,
            DaemonOptions {
                socket: socket.clone(),
                agent_marks: None,
                attention_state: Some(directory.path().join("attention.json")),
                refresh_interval: Duration::from_secs(3600),
                web_port: None,
                web_address: None,
                web_url: None,
                web_bridge: None,
            },
            async move {
                let _ = stopped.await;
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
        let pane = leased_pane(&mut connection).await;

        let payload = marked_payload("stopped");
        connection
            .send(ClientMessage::BeginUpload {
                request: 1,
                pane,
                mime: "image/png".to_owned(),
                name: None,
                // More than will be sent, so it is still unfinished when the
                // daemon is asked to stop.
                length: payload.len() as u64 + 64,
            })
            .await;
        let _ = connection.accepted().await;
        connection
            .send(ClientMessage::UploadChunk {
                request: 1,
                bytes: payload.clone(),
            })
            .await;
        assert!(
            settles_on(|| staged_anywhere(&payload)).await,
            "the transfer never reached the sink"
        );
        drop(connection);
        assert!(
            !settles_on_absence(|| staged_anywhere(&payload)).await,
            "a running daemon should still be holding this"
        );

        let _ = stop.send(());
        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("the daemon stops when asked")
            .expect("the task completes")
            .expect("serving ends without error");

        assert!(
            !staged_anywhere(&payload),
            "a stopped daemon left bytes nothing can name and nothing will collect"
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
                agent_marks: None,
                attention_state: Some(directory.path().join("attention.json")),
                refresh_interval: Duration::from_secs(3600),
                web_port: None,
                web_address: None,
                web_url: None,
                web_bridge: None,
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
            agent_marks: None,
            attention_state: Some(harness.directory.path().join("attention.json")),
            refresh_interval: Duration::from_secs(3600),
            web_port: None,
            web_address: None,
            web_url: None,
            web_bridge: None,
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
                agent_marks: None,
                attention_state: Some(directory.path().join("attention.json")),
                refresh_interval: Duration::from_secs(3600),
                web_port: None,
                web_address: None,
                web_url: None,
                web_bridge: None,
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
