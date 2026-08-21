//! Wire contract between the Super-Herdr daemon and its clients.
//!
//! The daemon is the federation authority; a client renders. This module owns
//! the only vocabulary they share, so the boundary is a reviewable contract
//! rather than whatever a frontend happened to reach for. Messages are
//! newline-delimited JSON objects tagged with `type`, matching the shape of
//! Herdr's own documented socket protocol so one framing style runs end to end.
//!
//! Three invariants hold here and are enforced by the types:
//!
//! * Every resource crosses the wire as a `QualifiedId`. A client never sends a
//!   server-local identifier and never learns which target a raw ID belongs to,
//!   so routing stays the daemon's job exactly as it is inside the frontend
//!   today.
//! * A client is granted state, frames, and outcomes. Nothing here carries SSH
//!   material, a Herdr socket path, a `herdr` binary candidate, or any other
//!   credential or transport detail, so a paired device cannot reach a host
//!   except through the daemon's own routing.
//! * Terminal payloads stay opaque on the frame path. Frames and input are
//!   base64 byte strings passed through untouched, so encoded ANSI reaches a
//!   renderer without a *double* parse: nothing here decodes it and re-encodes
//!   it on the way, which is what would hand a renderer something degraded.
//!   A client that cannot carry an emulator may ask for a rendered screen
//!   instead (`PaneRepresentation::Screen`). That is one parse moved to the end
//!   that can afford it, not a lossy middle, and it reaches the same fidelity
//!   ceiling a client-side `vt100` would. The frame path is unchanged by it and
//!   remains the primary one.
//!
//! Unknown fields are ignored rather than rejected: a newer daemon must be able
//! to add a field without breaking an older client that negotiated the same
//! protocol version. An unknown `type` is still an error, because acting on a
//! message whose meaning is unknown is worse than refusing it.

use std::sync::Arc;

use anyhow::{Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::attention::AttentionEvent;
use crate::model::{PaneId, TargetSession};
use crate::operation::Operation;
use crate::screen::{Diff as ScreenDiff, Snapshot};
use crate::state::{FederationState, TargetRuntimeState};
use crate::terminal::{TerminalAccess, TerminalScrollDirection};

/// Incremented only for a change a peer at the previous version cannot honor.
/// A mismatch closes the connection during the handshake; it never degrades
/// into a partially understood session.
pub const PROTOCOL_VERSION: u32 = 1;

/// One message may not exceed this encoded size. The largest legitimate
/// messages are a full-screen terminal frame and a bracketed paste, and the
/// clipboard path already bounds pasted text at 1 MiB, which base64 inflates by
/// a third. This leaves room for both while keeping a peer from forcing
/// unbounded buffering by never sending a newline.
pub const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// What a pane subscriber is sent for each frame.
///
/// Two representations of the same terminal, chosen per subscription rather
/// than per daemon, because the right answer differs by client in the same
/// federation: the TUI owns a parser and wants the bytes, a browser cannot
/// carry one and wants the result. The daemon parses only for panes somebody is
/// actually watching this way, which is the cost the TUI already pays per
/// visible pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneRepresentation {
    /// Encoded terminal output, rendered by a client that owns an emulator.
    #[default]
    Frames,
    /// A rendered screen, for a client that cannot.
    Screen,
}

/// Sent by a client. Requests that report an outcome carry a `request`
/// identifier the daemon echoes, so a client can correlate a result without
/// assuming responses arrive in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    /// First message on a connection. The daemon answers with `server.hello`
    /// or closes.
    #[serde(rename = "client.hello")]
    Hello { protocol: u32, client: String },
    /// Begin the federation state stream: one `state.full`, then one
    /// `state.target` per target that changes.
    #[serde(rename = "state.subscribe")]
    SubscribeState,
    /// Ask for frames from one pane. `access` is a request, not a grant; the
    /// daemon answers with `pane.lease` naming what it actually gave.
    #[serde(rename = "pane.subscribe")]
    SubscribePane {
        pane: PaneId,
        access: TerminalAccess,
        cols: u16,
        rows: u16,
        /// What this subscriber wants to receive. Absent means `frames`, so a
        /// client written before this existed keeps the behaviour it had.
        #[serde(default)]
        representation: PaneRepresentation,
    },
    #[serde(rename = "pane.unsubscribe")]
    UnsubscribePane { pane: PaneId },
    /// Take the control lease for a pane another client holds. Control is never
    /// stolen implicitly, and the previous holder is told it was downgraded
    /// rather than discovering it from silence.
    #[serde(rename = "pane.take_control")]
    TakePaneControl { pane: PaneId },
    #[serde(rename = "pane.input")]
    PaneInput {
        pane: PaneId,
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
    },
    #[serde(rename = "pane.resize")]
    PaneResize { pane: PaneId, cols: u16, rows: u16 },
    #[serde(rename = "pane.scroll")]
    PaneScroll {
        pane: PaneId,
        direction: TerminalScrollDirection,
        lines: u16,
        column: u16,
        row: u16,
        modifiers: u8,
    },
    /// Run one resolved operation. The operation keeps its qualified identity
    /// all the way to the daemon, which extracts the server-local ID only at
    /// the final transport step. Prompting and confirmation happen on the
    /// client, so what arrives here is already agreed to.
    #[serde(rename = "operation.run")]
    RunOperation { request: u64, operation: Operation },
    /// Deliver text to a pane as one atomic paste.
    ///
    /// This is not `PaneInput`: Herdr writes it in one piece through the
    /// session's own socket, adding bracketed-paste markers where they belong,
    /// so a multiline paste cannot arrive as several messages. It needs a route
    /// to the target, which is why it is the daemon's to perform.
    #[serde(rename = "pane.paste")]
    PastePaneText {
        request: u64,
        pane: PaneId,
        text: String,
    },
    /// Offer a clipboard payload for upload to the pane's host.
    ///
    /// Reading a clipboard is a desktop-session capability and stays with the
    /// client; moving the bytes needs a route to the host and does not. The
    /// MIME type is what the client saw, and the daemon resolves it to a file
    /// extension — a client never names a path.
    ///
    /// `length` is enforced rather than believed: an offer above the ceiling is
    /// refused before any bytes move, and the transfer is stopped at the
    /// declared length, because a lying length would otherwise write unbounded
    /// bytes onto the target host.
    #[serde(rename = "upload.begin")]
    BeginUpload {
        request: u64,
        pane: PaneId,
        mime: String,
        /// What to call the file on the target host, when the caller has a name
        /// worth keeping.
        ///
        /// Absent is the clipboard's case: a screenshot has no name, and the
        /// daemon writes it under one derived from its type. A name that is
        /// present is checked before anything is opened and refused rather than
        /// mangled, because a silently renamed file tells its sender it got
        /// what it asked for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        length: u64,
    },
    /// Continue a transfer that stopped without finishing.
    ///
    /// The token was issued when the transfer began, and is the only thing that
    /// names what the host already holds. It is not authority on its own: the
    /// pane's control lease is checked exactly as it is for a new transfer, and
    /// the declared length must be the one the transfer started with — a resume
    /// that declares a different length is a different file wearing this one's
    /// token.
    ///
    /// The daemon answers with `upload.accepted`, whose `staged` is the offset
    /// the next byte belongs at. It comes from the host's own count rather than
    /// from anything the daemon remembered, because an attempt that died
    /// mid-chunk left a length nobody predicted.
    #[serde(rename = "upload.resume")]
    ResumeUpload {
        request: u64,
        transfer: String,
        pane: PaneId,
        length: u64,
    },
    #[serde(rename = "upload.chunk")]
    UploadChunk {
        request: u64,
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
    },
    /// End the transfer and attest to what was sent.
    ///
    /// The digest is computed by the sender over the bytes it sent, so it
    /// attests to those rather than to a separate earlier read. An explicit
    /// frame rather than end-of-input is what keeps a dropped connection from
    /// being mistaken for a finished transfer.
    #[serde(rename = "upload.finish")]
    FinishUpload { request: u64, digest: String },
    #[serde(rename = "upload.cancel")]
    CancelUpload { request: u64 },
    /// Ask for a file on the pane's host.
    ///
    /// A client names the path here, which is the reverse of an upload, where
    /// it never does. That is not new authority: a client holding the pane's
    /// control lease can already type `cat` into a shell on that host and read
    /// the same bytes back through the terminal. This is the same reach through
    /// a channel that can say what it moved.
    ///
    /// The daemon answers with `download.offer` — a length and the digest the
    /// host computed — and then sends nothing until it is asked to.
    #[serde(rename = "download.begin")]
    BeginDownload {
        request: u64,
        pane: PaneId,
        path: String,
    },
    /// Allow the daemon to send up to this many more chunks.
    ///
    /// Flow control lives in the protocol for this direction only, and the
    /// asymmetry is not an oversight. An upload is backpressured by the socket
    /// itself: the daemon stops reading, so the client stops writing. Going the
    /// other way the daemon is the sender, and the queue to a client is
    /// unbounded — a browser on a slow link would otherwise have a gigabyte
    /// waiting for it in the daemon's memory. So a client says how much it is
    /// ready for, and peak memory is a window rather than a file.
    #[serde(rename = "download.pull")]
    PullDownload { request: u64, chunks: u32 },
    #[serde(rename = "download.cancel")]
    CancelDownload { request: u64 },
    /// Move a file from one target to another without it touching this device.
    ///
    /// The direction the desktop-bound design could not express. Only the
    /// daemon holds live connections to both hosts, so only the daemon can move
    /// a build artifact from a build host to a development host without routing
    /// it through whatever laptop happens to be asking.
    ///
    /// Both panes are named because both are checked: this reads from one host
    /// and writes to another, so it answers to the control lease on each. The
    /// name is what the file will be called at the destination, defaulting to
    /// what it is called at the source.
    #[serde(rename = "transfer.between")]
    TransferBetween {
        request: u64,
        source: PaneId,
        path: String,
        destination: PaneId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Attention state is durable and lives with the daemon, so marking events
    /// read is a request rather than a local edit.
    #[serde(rename = "attention.mark_seen")]
    MarkAttentionSeen { pane: PaneId },
    #[serde(rename = "attention.mark_all_seen")]
    MarkAllAttentionSeen,
    /// Ask for a pairing code to show a person.
    ///
    /// Pairing is initiated from a client that is already trusted, so the code
    /// is shown on a screen someone already has rather than mailed, printed, or
    /// left in a file.
    #[serde(rename = "pairing.request")]
    RequestPairingCode { request: u64 },
    /// Drop events that have been read. History is durable and shared, so
    /// forgetting part of it is a request like any other mutation.
    #[serde(rename = "attention.clear_seen")]
    ClearSeenAttention,
}

/// Sent by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    #[serde(rename = "server.hello")]
    Hello {
        protocol: u32,
        server_version: String,
        features: Vec<String>,
    },
    /// The whole federation, sent once per state subscription.
    #[serde(rename = "state.full")]
    FederationState { state: FederationState },
    /// One target changed. Targets reconcile independently, so a slow or failing
    /// target never forces a full resend. A null `state` means the target left
    /// the federation.
    #[serde(rename = "state.target")]
    TargetState {
        target: TargetSession,
        state: Option<TargetRuntimeState>,
    },
    #[serde(rename = "pane.frame")]
    PaneFrame {
        pane: PaneId,
        sequence: u64,
        width: u16,
        height: u16,
        full: bool,
        /// Shared rather than owned: one frame is fanned out to every
        /// subscriber, and copying it per recipient scales with the number of
        /// attached clients for no benefit.
        #[serde(with = "base64_bytes")]
        bytes: Arc<[u8]>,
    },
    /// A rendered screen, whole. Sent to a `screen` subscriber for its first
    /// update, and again whenever a diff cannot express the change — a resize
    /// makes every row describe different cells, and a full repaint restarts
    /// the emulator, so in both cases a snapshot is the only honest answer.
    #[serde(rename = "pane.screen")]
    PaneScreen { pane: PaneId, screen: Snapshot },
    /// The rows of a rendered screen that changed.
    ///
    /// `follows` names the update it applies to, so a client that missed one
    /// refuses to paint instead of holding a grid that is wrong forever and
    /// silent about it. The recovery is to resubscribe, which yields a fresh
    /// `PaneScreen`.
    #[serde(rename = "pane.screen_diff")]
    PaneScreenDiff { pane: PaneId, diff: ScreenDiff },
    #[serde(rename = "pane.closed")]
    PaneClosed { pane: PaneId },
    /// The access a pane subscription actually holds. A control lease that is
    /// refused or lost is reported as a downgrade to `observe` rather than as a
    /// failure, matching the frontend's existing fallback.
    #[serde(rename = "pane.lease")]
    PaneLease {
        pane: PaneId,
        access: TerminalAccess,
    },
    #[serde(rename = "operation.result")]
    OperationResult {
        request: u64,
        applied: bool,
        message: String,
    },
    /// A pairing code and how long it lasts. Never persisted: a code that
    /// survived a restart would be a credential nobody knew was outstanding.
    #[serde(rename = "pairing.code")]
    PairingCode {
        request: u64,
        code: String,
        expires_in_seconds: u64,
    },
    /// A transfer may proceed, and this is where to send from.
    ///
    /// `staged` is zero for a transfer that is starting, and the host's own
    /// count of what it already holds for one that is resuming. The token is
    /// what a sender needs to come back after a dropped connection, and is
    /// worth keeping for as long as the sender intends to finish.
    #[serde(rename = "upload.accepted")]
    UploadAccepted {
        request: u64,
        transfer: String,
        staged: u64,
    },
    /// A transfer stopped without finishing, and the host is still holding what
    /// arrived.
    ///
    /// Sent where there is anybody left to tell — the usual reason to stop is
    /// that there is not. It says nothing about the file being usable: nothing
    /// is verified and no path is named, because a path is what a client would
    /// paste into a pane.
    #[serde(rename = "upload.interrupted")]
    UploadInterrupted {
        request: u64,
        transfer: String,
        staged: u64,
    },
    /// What a requested file is, before any of it is sent.
    ///
    /// The digest is the host's, computed in its own pass over the file, and
    /// the daemon carries it without checking it — verifying is the receiving
    /// end's job, exactly as attesting is the sending end's in the other
    /// direction. Nothing follows this until the client pulls.
    #[serde(rename = "download.offer")]
    DownloadOffer {
        request: u64,
        /// The file's own last path component, never a path.
        name: String,
        length: u64,
        digest: String,
    },
    #[serde(rename = "download.chunk")]
    DownloadChunk {
        request: u64,
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
    },
    /// Every byte the offer declared has been sent.
    ///
    /// It carries no digest, because for this direction the digest arrived
    /// first: a host cannot hash while it sends without leaving POSIX behind.
    /// What this frame is for is the same as a trailer's — a client that has
    /// counted fewer bytes than the offer declared knows the difference between
    /// a transfer still arriving and one that stopped.
    #[serde(rename = "download.finished")]
    DownloadFinished { request: u64 },
    /// A transfer that arrived intact and verified on the host. The path is
    /// the daemon's, derived from a private staging directory; nothing a client
    /// sent is used to name it.
    #[serde(rename = "upload.complete")]
    UploadComplete {
        request: u64,
        path: String,
        bytes: u64,
    },
    #[serde(rename = "attention.event")]
    Attention { event: AttentionEvent },
    /// The whole bounded history, sent when a client subscribes and again after
    /// any change to what is read or retained.
    ///
    /// Read state changes touch many events at once, and a client that applied
    /// such a change locally would be guessing at the daemon's result. The
    /// index is capped at a few hundred payload-free entries, so resending it
    /// costs less than a way to desynchronize from it.
    #[serde(rename = "attention.history")]
    AttentionHistory { events: Vec<AttentionEvent> },
    /// A failure that is scoped to one request when `request` is present, and
    /// to the connection otherwise. Diagnostics are summarized here; raw
    /// command output never crosses this boundary.
    #[serde(rename = "error")]
    Error {
        request: Option<u64>,
        message: String,
    },
}

/// Encode one message as a protocol line, including its terminating newline.
pub fn encode<M>(message: &M) -> Result<Vec<u8>>
where
    M: Serialize,
{
    let mut line = serde_json::to_vec(message)?;
    if line.len() >= MAX_MESSAGE_BYTES {
        bail!(
            "refusing to send a {} byte protocol message; the limit is {MAX_MESSAGE_BYTES}",
            line.len()
        );
    }
    line.push(b'\n');
    Ok(line)
}

/// Decode one protocol line, without its terminating newline.
pub fn decode<M>(line: &[u8]) -> Result<M>
where
    M: DeserializeOwned,
{
    if line.len() >= MAX_MESSAGE_BYTES {
        bail!(
            "refusing to parse a {} byte protocol message; the limit is {MAX_MESSAGE_BYTES}",
            line.len()
        );
    }
    Ok(serde_json::from_slice(line)?)
}

/// Terminal payloads are arbitrary bytes rather than text, so they travel as
/// base64 exactly as Herdr's own terminal envelopes do.
mod base64_bytes {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S, B>(bytes: &B, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        B: AsRef<[u8]> + ?Sized,
    {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes.as_ref()))
    }

    pub fn deserialize<'de, D, B>(deserializer: D) -> Result<B, D::Error>
    where
        D: Deserializer<'de>,
        B: From<Vec<u8>>,
    {
        let encoded = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(encoded.as_bytes())
            .map(B::from)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::{
        ClientMessage, MAX_MESSAGE_BYTES, PROTOCOL_VERSION, PaneRepresentation, ServerMessage,
        decode, encode,
    };
    use crate::attention::{AttentionEvent, AttentionEventKind};
    use crate::model::{PaneId, TargetSession, WorkspaceId};
    use crate::operation::Operation;
    use crate::resource_action::SplitDirection;
    use crate::state::{
        FederationState, NormalizedSnapshot, PaneState, TargetConnectionState, TargetRuntimeState,
        TargetUpdateMode, WorkspaceState,
    };
    use crate::terminal::{TerminalAccess, TerminalScrollDirection};

    fn round_trip_client(message: ClientMessage) {
        let line = encode(&message).expect("message encodes");
        assert_eq!(line.last(), Some(&b'\n'));
        let decoded: ClientMessage = decode(&line[..line.len() - 1]).expect("message decodes");
        assert_eq!(decoded, message);
    }

    fn round_trip_server(message: ServerMessage) {
        let line = encode(&message).expect("message encodes");
        assert_eq!(line.last(), Some(&b'\n'));
        let decoded: ServerMessage = decode(&line[..line.len() - 1]).expect("message decodes");
        assert_eq!(decoded, message);
    }

    fn pane() -> PaneId {
        PaneId::new("development", "work", "w1:p1")
    }

    #[test]
    fn every_client_message_round_trips() {
        round_trip_client(ClientMessage::Hello {
            protocol: PROTOCOL_VERSION,
            client: "super-herdr-tui".to_owned(),
        });
        round_trip_client(ClientMessage::SubscribeState);
        round_trip_client(ClientMessage::SubscribePane {
            pane: pane(),
            access: TerminalAccess::Control,
            cols: 120,
            rows: 40,
            representation: PaneRepresentation::Frames,
        });
        round_trip_client(ClientMessage::SubscribePane {
            pane: pane(),
            access: TerminalAccess::Observe,
            cols: 120,
            rows: 40,
            representation: PaneRepresentation::Screen,
        });
        round_trip_client(ClientMessage::UnsubscribePane { pane: pane() });
        round_trip_client(ClientMessage::TakePaneControl { pane: pane() });
        round_trip_client(ClientMessage::PaneResize {
            pane: pane(),
            cols: 80,
            rows: 24,
        });
        round_trip_client(ClientMessage::PaneScroll {
            pane: pane(),
            direction: TerminalScrollDirection::Up,
            lines: 3,
            column: 10,
            row: 4,
            modifiers: 0,
        });
        round_trip_client(ClientMessage::RunOperation {
            request: 7,
            operation: Operation::SplitPane {
                pane: pane(),
                direction: SplitDirection::Down,
            },
        });
        round_trip_client(ClientMessage::PastePaneText {
            request: 3,
            pane: pane(),
            text: "one\ntwo\n".to_owned(),
        });
        round_trip_client(ClientMessage::BeginUpload {
            request: 4,
            pane: pane(),
            mime: "image/png".to_owned(),
            name: None,
            length: 2048,
        });
        round_trip_client(ClientMessage::BeginUpload {
            request: 5,
            pane: pane(),
            mime: "application/octet-stream".to_owned(),
            name: Some("build-log.txt".to_owned()),
            length: 2048,
        });
        round_trip_client(ClientMessage::UploadChunk {
            request: 4,
            bytes: vec![0x89, b'P', b'N', b'G'],
        });
        round_trip_client(ClientMessage::FinishUpload {
            request: 4,
            digest: "abc123".to_owned(),
        });
        round_trip_client(ClientMessage::CancelUpload { request: 4 });
        round_trip_client(ClientMessage::BeginDownload {
            request: 6,
            pane: pane(),
            path: "/var/log/build.txt".to_owned(),
        });
        round_trip_client(ClientMessage::PullDownload {
            request: 6,
            chunks: 4,
        });
        round_trip_client(ClientMessage::CancelDownload { request: 6 });
        round_trip_client(ClientMessage::TransferBetween {
            request: 7,
            source: pane(),
            path: "/srv/artifacts/build.tar.gz".to_owned(),
            destination: PaneId::new("second", "default", "w1:p1"),
            name: None,
        });
        round_trip_client(ClientMessage::MarkAttentionSeen { pane: pane() });
        round_trip_client(ClientMessage::MarkAllAttentionSeen);
        round_trip_client(ClientMessage::ClearSeenAttention);
        round_trip_client(ClientMessage::RequestPairingCode { request: 9 });
    }

    #[test]
    fn every_server_message_round_trips() {
        round_trip_server(ServerMessage::Hello {
            protocol: PROTOCOL_VERSION,
            server_version: "0.3.1".to_owned(),
            features: vec!["terminal".to_owned()],
        });
        round_trip_server(ServerMessage::PaneClosed { pane: pane() });
        round_trip_server(ServerMessage::PaneLease {
            pane: pane(),
            access: TerminalAccess::Observe,
        });
        round_trip_server(ServerMessage::OperationResult {
            request: 7,
            applied: true,
            message: "split pane".to_owned(),
        });
        round_trip_server(ServerMessage::Attention {
            event: AttentionEvent {
                id: 12,
                pane: pane(),
                agent: "claude".to_owned(),
                workspace: "review".to_owned(),
                status: "waiting".to_owned(),
                kind: AttentionEventKind::NeedsAttention,
                occurred_at_ms: 1_700_000_000_000,
                unread: true,
            },
        });
        round_trip_server(ServerMessage::DownloadOffer {
            request: 6,
            name: "build.txt".to_owned(),
            length: 4096,
            digest: "a".repeat(64),
        });
        round_trip_server(ServerMessage::DownloadChunk {
            request: 6,
            bytes: vec![1, 2, 3],
        });
        round_trip_server(ServerMessage::DownloadFinished { request: 6 });
        round_trip_server(ServerMessage::UploadComplete {
            request: 4,
            path: "/tmp/super-herdr-clipboard.abc/payload.png".to_owned(),
            bytes: 2048,
        });
        round_trip_server(ServerMessage::AttentionHistory { events: Vec::new() });
        round_trip_server(ServerMessage::PairingCode {
            request: 9,
            code: "ABCD-2345".to_owned(),
            expires_in_seconds: 300,
        });
        round_trip_server(ServerMessage::Error {
            request: Some(7),
            message: "target is unreachable".to_owned(),
        });
        round_trip_server(ServerMessage::Error {
            request: None,
            message: "protocol version mismatch".to_owned(),
        });
    }

    #[test]
    fn terminal_payloads_survive_bytes_that_are_not_utf8() {
        let bytes = vec![0x1b, b'[', b'3', b'1', b'm', 0xff, 0x00, 0xfe];
        round_trip_client(ClientMessage::PaneInput {
            pane: pane(),
            bytes: bytes.clone(),
        });
        round_trip_server(ServerMessage::PaneFrame {
            pane: pane(),
            sequence: 42,
            width: 80,
            height: 24,
            full: true,
            bytes: Arc::from(bytes),
        });
    }

    fn runtime_state(target: &TargetSession) -> TargetRuntimeState {
        let workspace = WorkspaceId::new(&target.target, &target.session, "w1");
        let pane = PaneId::new(&target.target, &target.session, "w1:p1");
        let mut snapshot = NormalizedSnapshot {
            server_version: Some("0.8.0".to_owned()),
            protocol: Some(19),
            focused_pane: Some(pane.clone()),
            ..NormalizedSnapshot::default()
        };
        snapshot.workspaces.insert(
            workspace.clone(),
            WorkspaceState {
                id: workspace,
                active_tab: None,
                label: Some("review".to_owned()),
                number: Some(1),
                focused: true,
                agent_status: None,
            },
        );
        snapshot.panes.insert(
            pane.clone(),
            PaneState {
                id: pane,
                workspace: None,
                tab: None,
                terminal: None,
                label: None,
                focused: true,
                agent: Some("claude".to_owned()),
                agent_status: Some("waiting".to_owned()),
                revision: Some(3),
            },
        );
        TargetRuntimeState {
            key: target.clone(),
            endpoint: "development-host".to_owned(),
            connection: TargetConnectionState::Backoff { attempt: 2 },
            update_mode: TargetUpdateMode::Events,
            event_error: None,
            connection_generation: 5,
            selected_herdr_bin: Some("herdr".to_owned()),
            snapshot: Some(Arc::new(snapshot)),
            last_error: Some("connection reset".to_owned()),
            last_success: None,
            retry_at: None,
        }
    }

    #[test]
    fn federation_state_round_trips_with_its_qualified_map_keys() {
        let first = TargetSession::new("development", "work");
        let second = TargetSession::new("build", "toolchains");
        let state = FederationState {
            revision: 9,
            targets: BTreeMap::from([
                (first.clone(), runtime_state(&first)),
                (second.clone(), runtime_state(&second)),
            ]),
        };

        let line = encode(&ServerMessage::FederationState {
            state: state.clone(),
        })
        .expect("state encodes");
        let decoded: ServerMessage = decode(&line[..line.len() - 1]).expect("state decodes");
        let ServerMessage::FederationState { state: decoded } = decoded else {
            panic!("decoded a different message");
        };

        assert_eq!(decoded.revision, state.revision);
        assert_eq!(
            decoded.targets.keys().collect::<Vec<_>>(),
            vec![&second, &first]
        );
        for (key, target) in &decoded.targets {
            assert_eq!(&target.key, key);
            let snapshot = target.snapshot.as_ref().expect("snapshot survives");
            for (id, workspace) in &snapshot.workspaces {
                assert_eq!(&workspace.id, id);
                assert_eq!(&id.target, &key.target);
            }
            for (id, pane) in &snapshot.panes {
                assert_eq!(&pane.id, id);
            }
        }
    }

    #[test]
    fn a_removed_target_is_distinguishable_from_an_unchanged_one() {
        let target = TargetSession::new("build", "toolchains");
        round_trip_server(ServerMessage::TargetState {
            target: target.clone(),
            state: Some(runtime_state(&target)),
        });
        round_trip_server(ServerMessage::TargetState {
            target,
            state: None,
        });
    }

    #[test]
    fn an_unknown_message_type_is_refused_rather_than_ignored() {
        let line = br#"{"type":"pane.detonate","pane":"w1:p1"}"#;
        assert!(decode::<ClientMessage>(line).is_err());
        assert!(decode::<ServerMessage>(line).is_err());
    }

    #[test]
    fn an_unknown_field_is_tolerated_so_a_newer_peer_can_add_one() {
        let line = br#"{"type":"state.subscribe","future_field":true}"#;
        assert_eq!(
            decode::<ClientMessage>(line).expect("tolerates an unknown field"),
            ClientMessage::SubscribeState
        );
    }

    #[test]
    fn oversized_messages_are_refused_in_both_directions() {
        let bytes = Arc::from(vec![0_u8; MAX_MESSAGE_BYTES]);
        let message = ServerMessage::PaneFrame {
            pane: pane(),
            sequence: 1,
            width: 80,
            height: 24,
            full: true,
            bytes,
        };
        assert!(encode(&message).is_err());
        assert!(decode::<ClientMessage>(&vec![b' '; MAX_MESSAGE_BYTES]).is_err());
    }
}
