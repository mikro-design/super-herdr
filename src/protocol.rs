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
//! * Terminal payloads stay opaque. Frames and input are base64 byte strings
//!   passed through untouched, preserving the rule that encoded ANSI reaches a
//!   renderer without a lossy intermediate screen model.
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
    /// Attention state is durable and lives with the daemon, so marking events
    /// read is a request rather than a local edit.
    #[serde(rename = "attention.mark_seen")]
    MarkAttentionSeen { pane: PaneId },
    #[serde(rename = "attention.mark_all_seen")]
    MarkAllAttentionSeen,
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
        ClientMessage, MAX_MESSAGE_BYTES, PROTOCOL_VERSION, ServerMessage, decode, encode,
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
        round_trip_client(ClientMessage::MarkAttentionSeen { pane: pane() });
        round_trip_client(ClientMessage::MarkAllAttentionSeen);
        round_trip_client(ClientMessage::ClearSeenAttention);
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
        round_trip_server(ServerMessage::AttentionHistory { events: Vec::new() });
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
