//! The daemon's connection state machine.
//!
//! Everything here is pure: messages and observations go in, [`Effect`] values
//! come out. No socket, no process, no clock. The I/O layer performs the
//! effects and reports what happened back through the same broker, so the rules
//! that decide who may type into a pane are testable without a Herdr server.
//!
//! The broker holds one route per pane rather than one per client. Two people
//! watching the same pane cost one observe stream on the target, which is the
//! reason a daemon exists at all. Fanning one route out to several subscribers
//! then forces the question the single-frontend design never had to answer:
//! who owns the keyboard. Exactly one client may hold a pane's control lease.
//! Control is never taken implicitly — a second client asking for it while it
//! is held is given observation instead — and a client that does take it over
//! explicitly causes the previous holder to be told it was downgraded, because
//! discovering that your keystrokes stopped arriving is worse than being told.
//!
//! A pane's size follows its control lease. Only the control holder may resize,
//! so an observer on a phone cannot reshape a pane someone is working in, and
//! the route is resized when the lease moves to a client of a different size.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use crate::attention::AttentionEvent;
use crate::model::PaneId;
use crate::operation::Operation;
use crate::protocol::{ClientMessage, PROTOCOL_VERSION, PaneRepresentation, ServerMessage};
use crate::screen::{Diff as ScreenDiff, Snapshot};
use crate::state::{FederationState, TargetConnectionState};
use crate::terminal::{TerminalAccess, TerminalScrollDirection};

/// Identifies one connected client for the lifetime of its connection.
/// Identifiers are never reused, so a late effect naming a departed client is
/// discarded rather than delivered to its replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientId(u64);

/// Work the I/O layer must perform. Route effects are addressed to a pane
/// rather than a client, because a route is shared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Send {
        client: ClientId,
        message: ServerMessage,
    },
    /// Close the connection after any queued `Send` effects have been written.
    Disconnect {
        client: ClientId,
        reason: String,
    },
    OpenRoute {
        pane: PaneId,
        access: TerminalAccess,
        cols: u16,
        rows: u16,
    },
    CloseRoute {
        pane: PaneId,
    },
    RouteInput {
        pane: PaneId,
        bytes: Vec<u8>,
    },
    RouteResize {
        pane: PaneId,
        cols: u16,
        rows: u16,
    },
    RouteScroll {
        pane: PaneId,
        direction: TerminalScrollDirection,
        lines: u16,
        column: u16,
        row: u16,
        modifiers: u8,
    },
    RunOperation {
        client: ClientId,
        request: u64,
        operation: Operation,
    },
    PastePaneText {
        client: ClientId,
        request: u64,
        pane: PaneId,
        text: String,
    },
    /// Transfer steps carry their payload through the broker rather than being
    /// held by it: the rules about who may do this live here, the bytes do not.
    BeginUpload {
        client: ClientId,
        request: u64,
        pane: PaneId,
        mime: String,
        name: Option<String>,
        length: u64,
    },
    ResumeUpload {
        client: ClientId,
        request: u64,
        transfer: String,
        pane: PaneId,
        length: u64,
    },
    UploadChunk {
        client: ClientId,
        request: u64,
        bytes: Vec<u8>,
    },
    FinishUpload {
        client: ClientId,
        request: u64,
        digest: String,
    },
    CancelUpload {
        client: ClientId,
        request: u64,
    },
    BeginDownload {
        client: ClientId,
        request: u64,
        pane: PaneId,
        path: String,
    },
    PullDownload {
        client: ClientId,
        request: u64,
        chunks: u32,
    },
    CancelDownload {
        client: ClientId,
        request: u64,
    },
    TransferBetween {
        client: ClientId,
        request: u64,
        source: PaneId,
        path: String,
        destination: PaneId,
        name: Option<String>,
    },
    MarkAttentionSeen {
        pane: PaneId,
    },
    MarkAllAttentionSeen,
    ClearSeenAttention,
    IssuePairingCode {
        client: ClientId,
        request: u64,
    },
    /// Send this client the durable history. The broker does not hold it, so
    /// the I/O layer fills it in.
    SendAttentionHistory {
        client: ClientId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneSubscription {
    requested: TerminalAccess,
    cols: u16,
    rows: u16,
    representation: PaneRepresentation,
    /// The rendered update last sent to this client. `None` means it has been
    /// sent nothing yet and must receive a whole screen before a diff can mean
    /// anything to it.
    sent: Option<u64>,
    /// The sequences sent to this client and not yet acknowledged, oldest
    /// first, at most `MAX_OUTSTANDING_SCREEN_UPDATES` of them.
    ///
    /// The sequences themselves rather than a count of them, so that one
    /// acknowledgement can settle every update at or below it. A viewer that
    /// paints four and reports only the newest is then telling the truth about
    /// all four, and a client that never acknowledges an update it painted
    /// costs itself nothing beyond that update — the next acknowledgement
    /// repairs it. Counting would have made both of those permanent, and would
    /// have made the protocol depend on a client acknowledging exactly once per
    /// update without anywhere saying so.
    ///
    /// These sequences mean something only within one [`ScreenState`]. A pane's
    /// emulator is dropped when nobody is watching it as a screen and rebuilt
    /// at sequence zero for the next viewer, so the numbers here can be higher
    /// than anything the pane will issue again. What keeps that harmless is
    /// that `subscribe_pane` replaces the whole `PaneSubscription`, queue
    /// included, so no queue outlives the sequence space that filled it.
    ///
    /// Anything that changes subscribing to update a subscription in place —
    /// keeping `sent` across a resize to spare a viewer a whole screen would be
    /// a reasonable reason to try — has to clear this queue as well. A stale
    /// queue against a restarted sequence space never drains: it is full, so
    /// nothing new is sent, and no acknowledgement names anything in it.
    outstanding: VecDeque<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Route {
    access: TerminalAccess,
    cols: u16,
    rows: u16,
    control: Option<ClientId>,
    subscribers: BTreeSet<ClientId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Client {
    greeted: bool,
    state_subscribed: bool,
    panes: BTreeMap<PaneId, PaneSubscription>,
}

impl Client {
    fn new() -> Self {
        Self {
            greeted: false,
            state_subscribed: false,
            panes: BTreeMap::new(),
        }
    }
}

/// How many rendered updates one screen subscriber may have outstanding.
///
/// A frame subscriber needs no such number: the socket stops the daemon reading
/// when a client stops draining. A rendered update is queued into an unbounded
/// outbox instead, so nothing in the transport pushes back and a viewer on a
/// slow link watching a busy pane would accumulate without limit in this
/// process. The daemon chooses the depth; an acknowledgement reports progress
/// against what was actually sent and cannot ask for more than this.
const MAX_OUTSTANDING_SCREEN_UPDATES: usize = 4;

/// One pane's rendered screen, kept only while somebody is watching it that
/// way.
///
/// The emulator lives here rather than on `Route` because it is a rendering
/// concern, not a routing one: a route exists as soon as anyone subscribes,
/// while a parser should exist only for the panes actually being rendered.
struct ScreenState {
    parser: vt100::Parser,
    /// The last screen anyone was told about. A diff is computed against this,
    /// and its sequence is the one a diff says it follows.
    last: Snapshot,
}

/// What one frame did to a rendered screen.
struct Rendered {
    screen: Snapshot,
    /// Absent when a diff cannot express the change, which a resize guarantees.
    diff: Option<ScreenDiff>,
    /// False when the frame left the visible screen exactly as it was. Frames
    /// that change nothing visible are common — a program writing a control
    /// sequence, or output scrolled off the visible region — and sending an
    /// empty diff for each would be traffic with nothing in it.
    changed: bool,
}

pub struct Broker {
    server_version: String,
    features: Vec<String>,
    clients: BTreeMap<ClientId, Client>,
    routes: BTreeMap<PaneId, Route>,
    screens: BTreeMap<PaneId, ScreenState>,
    state: FederationState,
    next_client: u64,
}

impl ScreenState {
    fn new(width: u16, height: u16) -> Self {
        // No scrollback: this renders the visible screen, and scrolling is
        // routed through Herdr so the server owns the history a viewer sees.
        let parser = vt100::Parser::new(height, width, 0);
        let last = Snapshot::of(parser.screen(), 0);
        Self { parser, last }
    }

    /// Feed one frame in and say what changed.
    fn advance(&mut self, width: u16, height: u16, full: bool, bytes: &[u8]) -> Rendered {
        let (rows, cols) = self.parser.screen().size();
        // A full repaint restarts the program's idea of the screen, and a
        // resize makes every cell describe something else. Both rebuild the
        // emulator rather than feed it, which is the rule the TUI already
        // follows for exactly the same reason.
        if full || rows != height || cols != width {
            self.parser = vt100::Parser::new(height, width, 0);
        }
        self.parser.process(bytes);

        let candidate = Snapshot::of(self.parser.screen(), self.last.sequence + 1);
        let unchanged = candidate.width == self.last.width
            && candidate.height == self.last.height
            && candidate.rows == self.last.rows
            && candidate.cursor == self.last.cursor;
        if unchanged {
            // The sequence deliberately does not advance: a client that missed
            // nothing should not be told it is behind.
            return Rendered {
                screen: self.last.clone(),
                diff: None,
                changed: false,
            };
        }

        let diff = ScreenDiff::between(&self.last, &candidate);
        self.last = candidate.clone();
        Rendered {
            screen: candidate,
            diff,
            changed: true,
        }
    }
}

impl Broker {
    pub fn new(server_version: impl Into<String>, features: Vec<String>) -> Self {
        Self {
            server_version: server_version.into(),
            features,
            clients: BTreeMap::new(),
            routes: BTreeMap::new(),
            screens: BTreeMap::new(),
            state: FederationState::default(),
            next_client: 0,
        }
    }

    /// Register a connection that has not yet said anything.
    pub fn connect(&mut self) -> ClientId {
        let client = ClientId(self.next_client);
        self.next_client += 1;
        self.clients.insert(client, Client::new());
        client
    }

    /// Release everything one connection held. Routes outlive the client that
    /// opened them only while another client is still subscribed.
    pub fn disconnect(&mut self, client: ClientId) -> Vec<Effect> {
        let Some(departing) = self.clients.remove(&client) else {
            return Vec::new();
        };
        let mut effects = Vec::new();
        for pane in departing.panes.keys() {
            self.release_pane(client, pane, &mut effects);
        }
        effects
    }

    pub fn handle(&mut self, client: ClientId, message: ClientMessage) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(session) = self.clients.get(&client) else {
            return effects;
        };

        // Nothing is answered before the version handshake, so a peer that
        // cannot be understood is refused rather than partially served.
        if !session.greeted {
            match message {
                ClientMessage::Hello { protocol, .. } if protocol == PROTOCOL_VERSION => {
                    if let Some(session) = self.clients.get_mut(&client) {
                        session.greeted = true;
                    }
                    effects.push(Effect::Send {
                        client,
                        message: ServerMessage::Hello {
                            protocol: PROTOCOL_VERSION,
                            server_version: self.server_version.clone(),
                            features: self.features.clone(),
                        },
                    });
                }
                ClientMessage::Hello { protocol, .. } => {
                    let reason = format!(
                        "protocol {protocol} is not supported; this daemon speaks {PROTOCOL_VERSION}"
                    );
                    effects.push(Effect::Send {
                        client,
                        message: ServerMessage::Error {
                            request: None,
                            message: reason.clone(),
                        },
                    });
                    effects.push(Effect::Disconnect { client, reason });
                }
                _ => {
                    let reason = "the first message must be client.hello".to_owned();
                    effects.push(Effect::Send {
                        client,
                        message: ServerMessage::Error {
                            request: None,
                            message: reason.clone(),
                        },
                    });
                    effects.push(Effect::Disconnect { client, reason });
                }
            }
            return effects;
        }

        match message {
            ClientMessage::Hello { .. } => {
                let reason = "client.hello was already accepted on this connection".to_owned();
                effects.push(Effect::Send {
                    client,
                    message: ServerMessage::Error {
                        request: None,
                        message: reason.clone(),
                    },
                });
                effects.push(Effect::Disconnect { client, reason });
            }
            ClientMessage::SubscribeState => {
                if let Some(session) = self.clients.get_mut(&client) {
                    session.state_subscribed = true;
                }
                effects.push(Effect::Send {
                    client,
                    message: ServerMessage::FederationState {
                        state: self.state.clone(),
                    },
                });
                // A client renders attention it did not derive, so it needs the
                // history before the next event rather than after it.
                effects.push(Effect::SendAttentionHistory { client });
            }
            ClientMessage::SubscribePane {
                pane,
                access,
                cols,
                rows,
                representation,
            } => self.subscribe_pane(
                client,
                pane,
                access,
                cols,
                rows,
                representation,
                &mut effects,
            ),
            ClientMessage::AckPaneScreen { pane, sequence } => {
                self.ack_pane_screen(client, &pane, sequence, &mut effects);
            }
            ClientMessage::UnsubscribePane { pane } => {
                if let Some(session) = self.clients.get_mut(&client)
                    && session.panes.remove(&pane).is_some()
                {
                    self.release_pane(client, &pane, &mut effects);
                }
            }
            ClientMessage::TakePaneControl { pane } => {
                self.take_control(client, &pane, &mut effects);
            }
            ClientMessage::PaneInput { pane, bytes } => {
                if self.holds_control(client, &pane, &mut effects) {
                    effects.push(Effect::RouteInput { pane, bytes });
                }
            }
            ClientMessage::PaneResize { pane, cols, rows } => {
                if self.holds_control(client, &pane, &mut effects) {
                    if let Some(session) = self.clients.get_mut(&client)
                        && let Some(subscription) = session.panes.get_mut(&pane)
                    {
                        subscription.cols = cols;
                        subscription.rows = rows;
                    }
                    if let Some(route) = self.routes.get_mut(&pane) {
                        route.cols = cols;
                        route.rows = rows;
                    }
                    effects.push(Effect::RouteResize { pane, cols, rows });
                }
            }
            ClientMessage::PaneScroll {
                pane,
                direction,
                lines,
                column,
                row,
                modifiers,
            } => {
                // Scrolling is routed through Herdr so the server owns
                // alternate-screen and scrollback behaviour, which makes it a
                // control operation rather than a local view change.
                if self.holds_control(client, &pane, &mut effects) {
                    effects.push(Effect::RouteScroll {
                        pane,
                        direction,
                        lines,
                        column,
                        row,
                        modifiers,
                    });
                }
            }
            ClientMessage::RunOperation { request, operation } => {
                effects.push(Effect::RunOperation {
                    client,
                    request,
                    operation,
                });
            }
            ClientMessage::PastePaneText {
                request,
                pane,
                text,
            } => {
                // Pasting is input, so it needs the same lease typing does.
                // Herdr writes it through the session socket rather than the
                // terminal stream, but an observer putting text into somebody
                // else's pane is the thing being prevented, not the mechanism.
                if self.holds_control(client, &pane, &mut effects) {
                    effects.push(Effect::PastePaneText {
                        client,
                        request,
                        pane,
                        text,
                    });
                }
            }
            ClientMessage::BeginUpload {
                request,
                pane,
                mime,
                name,
                length,
            } => {
                if self.holds_control(client, &pane, &mut effects) {
                    effects.push(Effect::BeginUpload {
                        client,
                        request,
                        pane,
                        mime,
                        name,
                        length,
                    });
                }
            }
            ClientMessage::ResumeUpload {
                request,
                transfer,
                pane,
                length,
            } => {
                // A token is not authority. Resuming writes to a host exactly
                // as beginning does, so it answers to the same lease.
                if self.holds_control(client, &pane, &mut effects) {
                    effects.push(Effect::ResumeUpload {
                        client,
                        request,
                        transfer,
                        pane,
                        length,
                    });
                }
            }
            ClientMessage::UploadChunk { request, bytes } => effects.push(Effect::UploadChunk {
                client,
                request,
                bytes,
            }),
            ClientMessage::FinishUpload { request, digest } => {
                effects.push(Effect::FinishUpload {
                    client,
                    request,
                    digest,
                });
            }
            ClientMessage::CancelUpload { request } => {
                effects.push(Effect::CancelUpload { client, request });
            }
            ClientMessage::BeginDownload {
                request,
                pane,
                path,
            } => {
                // Reading a host's files is the reach a shell on that host
                // already has, so it answers to the lease that grants the
                // shell rather than to the one that grants a view of it.
                if self.holds_control(client, &pane, &mut effects) {
                    effects.push(Effect::BeginDownload {
                        client,
                        request,
                        pane,
                        path,
                    });
                }
            }
            ClientMessage::PullDownload { request, chunks } => {
                effects.push(Effect::PullDownload {
                    client,
                    request,
                    chunks,
                });
            }
            ClientMessage::CancelDownload { request } => {
                effects.push(Effect::CancelDownload { client, request });
            }
            ClientMessage::TransferBetween {
                request,
                source,
                path,
                destination,
                name,
            } => {
                // Both, because this reads from one host and writes to the
                // other. Holding the lease on the destination is not permission
                // to read somebody else's host, and holding it on the source is
                // not permission to write to theirs.
                if self.holds_control(client, &source, &mut effects)
                    && self.holds_control(client, &destination, &mut effects)
                {
                    effects.push(Effect::TransferBetween {
                        client,
                        request,
                        source,
                        path,
                        destination,
                        name,
                    });
                }
            }
            ClientMessage::MarkAttentionSeen { pane } => {
                effects.push(Effect::MarkAttentionSeen { pane });
            }
            ClientMessage::MarkAllAttentionSeen => effects.push(Effect::MarkAllAttentionSeen),
            ClientMessage::ClearSeenAttention => effects.push(Effect::ClearSeenAttention),
            ClientMessage::RequestPairingCode { request } => {
                effects.push(Effect::IssuePairingCode { client, request });
            }
        }

        effects
    }

    /// Replace the federation state and tell subscribed clients what changed.
    /// Targets reconcile independently, so one slow target never forces a full
    /// resend.
    pub fn federation_updated(&mut self, state: FederationState) -> Vec<Effect> {
        let mut changes = Vec::new();
        for (key, target) in &state.targets {
            if self.state.targets.get(key) != Some(target) {
                changes.push(ServerMessage::TargetState {
                    target: key.clone(),
                    state: Some(target.clone()),
                });
            }
        }
        for key in self.state.targets.keys() {
            if !state.targets.contains_key(key) {
                changes.push(ServerMessage::TargetState {
                    target: key.clone(),
                    state: None,
                });
            }
        }

        self.state = state;
        let mut effects = Vec::new();
        for message in changes {
            self.broadcast_state(message, &mut effects);
        }
        self.close_vanished_routes(&mut effects);
        effects
    }

    pub fn pane_frame(
        &mut self,
        pane: &PaneId,
        sequence: u64,
        width: u16,
        height: u16,
        full: bool,
        bytes: Arc<[u8]>,
    ) -> Vec<Effect> {
        let Some(route) = self.routes.get(pane) else {
            return Vec::new();
        };

        // One frame, two audiences. The choice is per subscriber rather than
        // per daemon, so a TUI and a browser can watch the same pane at once
        // and each gets the representation it can actually render.
        let (screen_subscribers, frame_subscribers): (Vec<_>, Vec<_>) =
            route.subscribers.iter().copied().partition(|client| {
                self.representation_of(*client, pane) == PaneRepresentation::Screen
            });

        let mut effects: Vec<Effect> = frame_subscribers
            .into_iter()
            .map(|client| Effect::Send {
                client,
                message: ServerMessage::PaneFrame {
                    pane: pane.clone(),
                    sequence,
                    width,
                    height,
                    full,
                    // A cheap handle to one shared frame, not a copy per
                    // subscriber.
                    bytes: Arc::clone(&bytes),
                },
            })
            .collect();

        if screen_subscribers.is_empty() {
            // Nobody is watching this pane as a screen, so nothing parses it.
            // The parser is dropped rather than kept warm: a federation holds
            // far more panes than anyone observes, and the cost should follow
            // the watching.
            self.screens.remove(pane);
            return effects;
        }

        effects.extend(self.render_for(pane, width, height, full, &bytes, &screen_subscribers));
        effects
    }

    fn representation_of(&self, client: ClientId, pane: &PaneId) -> PaneRepresentation {
        self.clients
            .get(&client)
            .and_then(|session| session.panes.get(pane))
            .map_or(PaneRepresentation::default(), |subscription| {
                subscription.representation
            })
    }

    /// Parse this frame into a screen and tell each screen subscriber what it
    /// needs.
    ///
    /// A client that holds the update a diff follows gets the diff; anything
    /// else gets a whole screen. "Anything else" is deliberately broad — a new
    /// subscriber, a client that fell behind, a resize — because sending a diff
    /// that cannot be applied is worse than sending more bytes than strictly
    /// needed.
    fn render_for(
        &mut self,
        pane: &PaneId,
        width: u16,
        height: u16,
        full: bool,
        bytes: &[u8],
        subscribers: &[ClientId],
    ) -> Vec<Effect> {
        let rendered = self
            .screens
            .entry(pane.clone())
            .or_insert_with(|| ScreenState::new(width, height))
            .advance(width, height, full, bytes);

        let mut effects = Vec::new();
        for &client in subscribers {
            let Some(subscription) = self
                .clients
                .get_mut(&client)
                .and_then(|session| session.panes.get_mut(pane))
            else {
                continue;
            };

            // Up to date and nothing moved: say nothing rather than send an
            // empty diff.
            if !rendered.changed && subscription.sent == Some(rendered.screen.sequence) {
                continue;
            }

            // Already holding as much as this viewer has agreed to carry. The
            // update is not queued behind the others and not remembered: when
            // it catches up it is sent the screen as it stands then, which is
            // what it wants and is bounded however far behind it fell.
            if subscription.outstanding.len() >= MAX_OUTSTANDING_SCREEN_UPDATES {
                continue;
            }

            let message = Self::update_for(subscription, pane, &rendered);
            subscription.sent = Some(rendered.screen.sequence);
            subscription.outstanding.push_back(rendered.screen.sequence);
            effects.push(Effect::Send { client, message });
        }
        effects
    }

    /// What one subscriber needs in order to hold `rendered`.
    ///
    /// A diff is only applicable to the exact update it follows, and the only
    /// client that holds that update is one that was sent it. Anything else —
    /// a new subscriber, a resize, or a viewer that was skipped while it was at
    /// its limit — is sent the whole screen.
    fn update_for(
        subscription: &PaneSubscription,
        pane: &PaneId,
        rendered: &Rendered,
    ) -> ServerMessage {
        match (&rendered.diff, subscription.sent) {
            (Some(diff), Some(sent)) if sent == diff.follows => ServerMessage::PaneScreenDiff {
                pane: pane.clone(),
                diff: diff.clone(),
            },
            _ => ServerMessage::PaneScreen {
                pane: pane.clone(),
                screen: rendered.screen.clone(),
            },
        }
    }

    /// A client painted an update, so it may be sent another.
    ///
    /// The sequence is checked against what this subscription was actually
    /// sent: acknowledging the same update twice, or one never sent, changes
    /// nothing. Progress is reported here, not granted — the depth stays the
    /// daemon's.
    fn ack_pane_screen(
        &mut self,
        client: ClientId,
        pane: &PaneId,
        sequence: u64,
        effects: &mut Vec<Effect>,
    ) {
        let Some(subscription) = self
            .clients
            .get_mut(&client)
            .and_then(|session| session.panes.get_mut(pane))
        else {
            return;
        };
        // The sequence must be one this subscription is actually waiting on,
        // not merely one below the newest thing sent. The queue has gaps in it
        // — a viewer at its limit is skipped, so the updates it missed are
        // never in here — and settling everything below an arbitrary number
        // would let a client clear those gaps by naming an update that was
        // never sent to it. Membership is what makes "checked against what was
        // actually sent" a promise rather than a description of the common
        // case; it also makes acknowledging twice a no-op, since the second
        // acknowledgement names something no longer held.
        if !subscription.outstanding.contains(&sequence) {
            return;
        }
        // Painting is in order, so an acknowledgement settles everything the
        // client was sent before this one as well.
        while subscription
            .outstanding
            .front()
            .is_some_and(|held| *held <= sequence)
        {
            subscription.outstanding.pop_front();
        }

        // Caught up enough to be sent something, and behind what the pane now
        // shows. A pane that has gone quiet produces no further frame, so
        // waiting for one would leave this viewer holding a screen it can see
        // is stale and cannot ask to replace.
        let Some(state) = self.screens.get(pane) else {
            return;
        };
        if subscription.sent == Some(state.last.sequence)
            || subscription.outstanding.len() >= MAX_OUTSTANDING_SCREEN_UPDATES
        {
            return;
        }
        let rendered = Rendered {
            screen: state.last.clone(),
            diff: None,
            changed: true,
        };
        let message = Self::update_for(subscription, pane, &rendered);
        subscription.sent = Some(state.last.sequence);
        subscription.outstanding.push_back(state.last.sequence);
        effects.push(Effect::Send { client, message });
    }

    /// Drop the emulator for a pane nobody is watching as a screen any more.
    fn release_screen_if_unwatched(&mut self, pane: &PaneId) {
        let watched = self.routes.get(pane).is_some_and(|route| {
            route
                .subscribers
                .iter()
                .any(|client| self.representation_of(*client, pane) == PaneRepresentation::Screen)
        });
        if !watched {
            self.screens.remove(pane);
        }
    }

    /// The route's own stream ended. Subscribers keep their subscription
    /// records only until they are told, so no client is left waiting on a
    /// stream that no longer exists.
    pub fn pane_route_closed(&mut self, pane: &PaneId) -> Vec<Effect> {
        let Some(route) = self.routes.remove(pane) else {
            return Vec::new();
        };
        self.screens.remove(pane);
        let mut effects = Vec::new();
        for client in route.subscribers {
            if let Some(session) = self.clients.get_mut(&client) {
                session.panes.remove(pane);
            }
            effects.push(Effect::Send {
                client,
                message: ServerMessage::PaneClosed { pane: pane.clone() },
            });
        }
        effects
    }

    /// Herdr refused a control stream, so the route runs as an observer.
    ///
    /// This is a downgrade rather than a failure: the frontend's rule has
    /// always been that a refused control lease falls back to watching instead
    /// of tearing the pane down, and moving that rule here keeps it true for
    /// every client at once. Whoever held the lease is told, because the
    /// alternative is a person typing into a pane that silently stopped
    /// accepting input.
    pub fn route_downgraded(&mut self, pane: &PaneId) -> Vec<Effect> {
        let Some(route) = self.routes.get_mut(pane) else {
            return Vec::new();
        };
        if route.access == TerminalAccess::Observe && route.control.is_none() {
            return Vec::new();
        }
        route.access = TerminalAccess::Observe;
        route.control = None;
        route
            .subscribers
            .iter()
            .map(|client| Effect::Send {
                client: *client,
                message: ServerMessage::PaneLease {
                    pane: pane.clone(),
                    access: TerminalAccess::Observe,
                },
            })
            .collect()
    }

    pub fn attention_observed(&mut self, event: AttentionEvent) -> Vec<Effect> {
        let mut effects = Vec::new();
        self.broadcast_state(ServerMessage::Attention { event }, &mut effects);
        effects
    }

    /// Republish the durable history to everyone watching. Read state changes
    /// touch many events at once, so the authoritative result is sent rather
    /// than a change each client would have to reproduce.
    pub fn attention_changed(&mut self, events: Vec<AttentionEvent>) -> Vec<Effect> {
        let mut effects = Vec::new();
        self.broadcast_state(ServerMessage::AttentionHistory { events }, &mut effects);
        effects
    }

    pub fn operation_completed(
        &mut self,
        client: ClientId,
        request: u64,
        applied: bool,
        message: impl Into<String>,
    ) -> Vec<Effect> {
        if !self.clients.contains_key(&client) {
            return Vec::new();
        }
        vec![Effect::Send {
            client,
            message: ServerMessage::OperationResult {
                request,
                applied,
                message: message.into(),
            },
        }]
    }

    fn broadcast_state(&self, message: ServerMessage, effects: &mut Vec<Effect>) {
        for (client, session) in &self.clients {
            if session.state_subscribed {
                effects.push(Effect::Send {
                    client: *client,
                    message: message.clone(),
                });
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn subscribe_pane(
        &mut self,
        client: ClientId,
        pane: PaneId,
        access: TerminalAccess,
        cols: u16,
        rows: u16,
        representation: PaneRepresentation,
        effects: &mut Vec<Effect>,
    ) {
        let Some(session) = self.clients.get_mut(&client) else {
            return;
        };
        session.panes.insert(
            pane.clone(),
            PaneSubscription {
                requested: access,
                cols,
                rows,
                representation,
                sent: None,
                outstanding: VecDeque::new(),
            },
        );

        let granted = match self.routes.get_mut(&pane) {
            Some(route) => {
                route.subscribers.insert(client);
                if access == TerminalAccess::Control && route.control.is_none() {
                    route.control = Some(client);
                    Self::resize_for_control(&pane, route, cols, rows, effects);
                    TerminalAccess::Control
                } else if route.control == Some(client) {
                    TerminalAccess::Control
                } else {
                    TerminalAccess::Observe
                }
            }
            None => {
                let control = (access == TerminalAccess::Control).then_some(client);
                self.routes.insert(
                    pane.clone(),
                    Route {
                        access,
                        cols,
                        rows,
                        control,
                        subscribers: BTreeSet::from([client]),
                    },
                );
                effects.push(Effect::OpenRoute {
                    pane: pane.clone(),
                    access,
                    cols,
                    rows,
                });
                access
            }
        };

        // A screen subscriber that arrives mid-stream would otherwise see
        // nothing until the pane next produced output, which for an idle pane
        // is indistinguishable from a broken viewer.
        if representation == PaneRepresentation::Screen
            && let Some(state) = self.screens.get(&pane)
        {
            let screen = state.last.clone();
            if let Some(session) = self.clients.get_mut(&client)
                && let Some(subscription) = session.panes.get_mut(&pane)
            {
                subscription.sent = Some(screen.sequence);
                subscription.outstanding.push_back(screen.sequence);
            }
            effects.push(Effect::Send {
                client,
                message: ServerMessage::PaneScreen {
                    pane: pane.clone(),
                    screen,
                },
            });
        }

        effects.push(Effect::Send {
            client,
            message: ServerMessage::PaneLease {
                pane,
                access: granted,
            },
        });
    }

    fn take_control(&mut self, client: ClientId, pane: &PaneId, effects: &mut Vec<Effect>) {
        let Some(session) = self.clients.get(&client) else {
            return;
        };
        let Some(subscription) = session.panes.get(pane).cloned() else {
            effects.push(Effect::Send {
                client,
                message: ServerMessage::Error {
                    request: None,
                    message: format!("cannot take control of {pane} without subscribing to it"),
                },
            });
            return;
        };
        let Some(route) = self.routes.get_mut(pane) else {
            return;
        };
        if route.control == Some(client) {
            return;
        }

        let previous = route.control.replace(client);
        Self::resize_for_control(pane, route, subscription.cols, subscription.rows, effects);

        // The route itself has to change access when it was opened only to
        // observe, because a stream without an input channel cannot be
        // promoted in place.
        if route.access != TerminalAccess::Control {
            route.access = TerminalAccess::Control;
            let (cols, rows) = (route.cols, route.rows);
            effects.push(Effect::CloseRoute { pane: pane.clone() });
            effects.push(Effect::OpenRoute {
                pane: pane.clone(),
                access: TerminalAccess::Control,
                cols,
                rows,
            });
        }

        if let Some(previous) = previous {
            effects.push(Effect::Send {
                client: previous,
                message: ServerMessage::PaneLease {
                    pane: pane.clone(),
                    access: TerminalAccess::Observe,
                },
            });
        }
        effects.push(Effect::Send {
            client,
            message: ServerMessage::PaneLease {
                pane: pane.clone(),
                access: TerminalAccess::Control,
            },
        });
    }

    fn resize_for_control(
        pane: &PaneId,
        route: &mut Route,
        cols: u16,
        rows: u16,
        effects: &mut Vec<Effect>,
    ) {
        if route.cols == cols && route.rows == rows {
            return;
        }
        route.cols = cols;
        route.rows = rows;
        effects.push(Effect::RouteResize {
            pane: pane.clone(),
            cols,
            rows,
        });
    }

    /// Refuse an operation the client has no lease for. Refusing is deliberate:
    /// silently dropping input would leave a person typing into a pane that
    /// never answers.
    fn holds_control(&self, client: ClientId, pane: &PaneId, effects: &mut Vec<Effect>) -> bool {
        if self.routes.get(pane).and_then(|route| route.control) == Some(client) {
            return true;
        }
        effects.push(Effect::Send {
            client,
            message: ServerMessage::Error {
                request: None,
                message: format!("this connection does not hold the control lease for {pane}"),
            },
        });
        false
    }

    fn release_pane(&mut self, client: ClientId, pane: &PaneId, effects: &mut Vec<Effect>) {
        let Some(route) = self.routes.get_mut(pane) else {
            return;
        };
        route.subscribers.remove(&client);
        let emptied = route.subscribers.is_empty();
        let held_control = route.control == Some(client);
        if emptied {
            self.routes.remove(pane);
            self.screens.remove(pane);
            effects.push(Effect::CloseRoute { pane: pane.clone() });
            return;
        }
        // The one leaving may have been the last watching this pane as a
        // screen, and the emulator should go with them.
        self.release_screen_if_unwatched(pane);
        if !held_control {
            return;
        }
        let Some(route) = self.routes.get_mut(pane) else {
            return;
        };

        // The control holder left. The lease is not handed to an observer that
        // never asked for it; the route drops back to observation and the
        // remaining clients are told what they now hold.
        route.control = None;
        let downgrade = route.access == TerminalAccess::Control;
        if downgrade {
            route.access = TerminalAccess::Observe;
        }
        let subscribers = route.subscribers.iter().copied().collect::<Vec<_>>();
        let (cols, rows) = (route.cols, route.rows);
        if downgrade {
            effects.push(Effect::CloseRoute { pane: pane.clone() });
            effects.push(Effect::OpenRoute {
                pane: pane.clone(),
                access: TerminalAccess::Observe,
                cols,
                rows,
            });
        }
        for subscriber in subscribers {
            effects.push(Effect::Send {
                client: subscriber,
                message: ServerMessage::PaneLease {
                    pane: pane.clone(),
                    access: TerminalAccess::Observe,
                },
            });
        }
    }

    /// Close routes for panes an authoritative snapshot no longer contains. A
    /// disconnected target proves nothing about its panes, so only a live
    /// target's snapshot may retire a route — the same rule attention tracking
    /// uses to avoid inventing disappearances during a reconnect.
    fn close_vanished_routes(&mut self, effects: &mut Vec<Effect>) {
        let vanished = self
            .routes
            .keys()
            .filter(|pane| self.pane_is_gone(pane))
            .cloned()
            .collect::<Vec<_>>();
        for pane in vanished {
            effects.extend(self.pane_route_closed(&pane));
        }
    }

    fn pane_is_gone(&self, pane: &PaneId) -> bool {
        let Some(target) = self.state.targets.get(&pane.target_session()) else {
            // The target left the federation entirely, so nothing routes there.
            return true;
        };
        if target.connection != TargetConnectionState::Live {
            return false;
        }
        target
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| !snapshot.panes.contains_key(pane))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use std::collections::VecDeque;

    use super::{
        Broker, ClientId, Effect, MAX_OUTSTANDING_SCREEN_UPDATES, PaneSubscription, Rendered,
    };
    use crate::attention::{AttentionEvent, AttentionEventKind};
    use crate::model::{PaneId, TargetSession};
    use crate::operation::Operation;
    use crate::protocol::{ClientMessage, PROTOCOL_VERSION, PaneRepresentation, ServerMessage};
    use crate::screen::Diff as ScreenDiff;
    use crate::screen::Snapshot;
    use crate::state::{
        FederationState, NormalizedSnapshot, PaneState, TargetConnectionState, TargetRuntimeState,
        TargetUpdateMode,
    };
    use crate::terminal::{TerminalAccess, TerminalScrollDirection};

    fn broker() -> Broker {
        Broker::new("0.3.1", vec!["terminal".to_owned()])
    }

    /// Every test starts from a completed handshake, because nothing else is
    /// answered before one.
    fn greet(broker: &mut Broker) -> ClientId {
        let client = broker.connect();
        let effects = broker.handle(
            client,
            ClientMessage::Hello {
                protocol: PROTOCOL_VERSION,
                client: "test".to_owned(),
            },
        );
        assert!(matches!(
            effects.as_slice(),
            [Effect::Send {
                message: ServerMessage::Hello { .. },
                ..
            }]
        ));
        client
    }

    fn pane(resource: &str) -> PaneId {
        PaneId::new("development", "work", resource)
    }

    fn target_state(
        connection: TargetConnectionState,
        panes: &[&str],
        revision: Option<u64>,
    ) -> TargetRuntimeState {
        let key = TargetSession::new("development", "work");
        let mut snapshot = NormalizedSnapshot {
            revision,
            ..NormalizedSnapshot::default()
        };
        for resource in panes {
            let id = pane(resource);
            snapshot.panes.insert(
                id.clone(),
                PaneState {
                    id,
                    workspace: None,
                    tab: None,
                    terminal: None,
                    label: None,
                    focused: false,
                    agent: None,
                    agent_status: None,
                    revision: None,
                },
            );
        }
        TargetRuntimeState {
            key,
            endpoint: "development-host".to_owned(),
            connection,
            update_mode: TargetUpdateMode::Polling,
            event_error: None,
            connection_generation: 1,
            selected_herdr_bin: Some("herdr".to_owned()),
            snapshot: Some(Arc::new(snapshot)),
            last_error: None,
            last_success: None,
            retry_at: None,
        }
    }

    fn federation(target: TargetRuntimeState) -> FederationState {
        FederationState {
            revision: 1,
            targets: BTreeMap::from([(target.key.clone(), target)]),
        }
    }

    fn frame(broker: &mut Broker, resource: &str, bytes: &[u8]) -> Vec<Effect> {
        broker.pane_frame(&pane(resource), 1, 80, 24, false, Arc::from(bytes.to_vec()))
    }

    fn rendered_row(broker: &Broker, resource: &str, row: usize) -> String {
        row_text(&broker.screens[&pane(resource)].last, row)
    }

    fn sequence_of(message: &ServerMessage) -> u64 {
        match message {
            ServerMessage::PaneScreen { screen, .. } => screen.sequence,
            ServerMessage::PaneScreenDiff { diff, .. } => diff.sequence,
            other => panic!("not a rendered update: {other:?}"),
        }
    }

    fn outstanding_sequences(broker: &Broker, client: ClientId, resource: &str) -> Vec<u64> {
        broker.clients[&client].panes[&pane(resource)]
            .outstanding
            .iter()
            .copied()
            .collect()
    }

    fn newest_outstanding(broker: &Broker, client: ClientId, resource: &str) -> u64 {
        *broker.clients[&client].panes[&pane(resource)]
            .outstanding
            .back()
            .expect("the subscription was sent nothing")
    }

    fn ack(broker: &mut Broker, client: ClientId, resource: &str, sequence: u64) -> Vec<Effect> {
        broker.handle(
            client,
            ClientMessage::AckPaneScreen {
                pane: pane(resource),
                sequence,
            },
        )
    }

    /// The oldest update this subscription is still waiting to hear about,
    /// which is what a viewer painting in order acknowledges first.
    fn first_sequence(broker: &Broker, client: ClientId, resource: &str) -> u64 {
        *broker.clients[&client].panes[&pane(resource)]
            .outstanding
            .front()
            .expect("the subscription was sent nothing")
    }

    fn outstanding(broker: &Broker, client: ClientId, resource: &str) -> usize {
        broker.clients[&client].panes[&pane(resource)]
            .outstanding
            .len()
    }

    fn row_text(screen: &Snapshot, row: usize) -> String {
        screen.rows[row]
            .iter()
            .map(|run| run.text.as_str())
            .collect()
    }

    fn messages_for(effects: &[Effect], client: ClientId) -> Vec<ServerMessage> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Send {
                    client: recipient,
                    message,
                } if *recipient == client => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    fn subscribe(
        broker: &mut Broker,
        client: ClientId,
        resource: &str,
        access: TerminalAccess,
        cols: u16,
        rows: u16,
    ) -> Vec<Effect> {
        subscribe_as(
            broker,
            client,
            resource,
            access,
            cols,
            rows,
            PaneRepresentation::Frames,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn subscribe_as(
        broker: &mut Broker,
        client: ClientId,
        resource: &str,
        access: TerminalAccess,
        cols: u16,
        rows: u16,
        representation: PaneRepresentation,
    ) -> Vec<Effect> {
        broker.handle(
            client,
            ClientMessage::SubscribePane {
                pane: pane(resource),
                access,
                cols,
                rows,
                representation,
            },
        )
    }

    #[test]
    fn nothing_is_served_before_the_handshake() {
        let mut broker = broker();
        let client = broker.connect();

        let effects = broker.handle(client, ClientMessage::SubscribeState);

        assert!(matches!(
            effects.as_slice(),
            [
                Effect::Send {
                    message: ServerMessage::Error { request: None, .. },
                    ..
                },
                Effect::Disconnect { .. }
            ]
        ));
    }

    #[test]
    fn a_version_mismatch_closes_the_connection_instead_of_degrading() {
        let mut broker = broker();
        let client = broker.connect();

        let effects = broker.handle(
            client,
            ClientMessage::Hello {
                protocol: PROTOCOL_VERSION + 1,
                client: "future".to_owned(),
            },
        );

        assert!(matches!(effects.last(), Some(Effect::Disconnect { .. })));
    }

    #[test]
    fn a_second_handshake_is_refused() {
        let mut broker = broker();
        let client = greet(&mut broker);

        let effects = broker.handle(
            client,
            ClientMessage::Hello {
                protocol: PROTOCOL_VERSION,
                client: "again".to_owned(),
            },
        );

        assert!(matches!(effects.last(), Some(Effect::Disconnect { .. })));
    }

    #[test]
    fn state_arrives_whole_once_and_then_only_as_changed_targets() {
        let mut broker = broker();
        let subscribed = greet(&mut broker);
        let silent = greet(&mut broker);
        broker.federation_updated(federation(target_state(
            TargetConnectionState::Live,
            &["w1:p1"],
            Some(1),
        )));

        let effects = broker.handle(subscribed, ClientMessage::SubscribeState);
        assert!(matches!(
            messages_for(&effects, subscribed).as_slice(),
            [ServerMessage::FederationState { .. }]
        ));

        let effects = broker.federation_updated(federation(target_state(
            TargetConnectionState::Live,
            &["w1:p1"],
            Some(2),
        )));

        assert!(matches!(
            messages_for(&effects, subscribed).as_slice(),
            [ServerMessage::TargetState { state: Some(_), .. }]
        ));
        assert!(messages_for(&effects, silent).is_empty());
    }

    #[test]
    fn an_unchanged_target_produces_no_traffic() {
        let mut broker = broker();
        let client = greet(&mut broker);
        broker.handle(client, ClientMessage::SubscribeState);
        let state = federation(target_state(
            TargetConnectionState::Live,
            &["w1:p1"],
            Some(1),
        ));
        broker.federation_updated(state.clone());

        assert!(broker.federation_updated(state).is_empty());
    }

    #[test]
    fn a_target_that_leaves_the_federation_is_reported_as_removed() {
        let mut broker = broker();
        let client = greet(&mut broker);
        broker.handle(client, ClientMessage::SubscribeState);
        broker.federation_updated(federation(target_state(
            TargetConnectionState::Live,
            &["w1:p1"],
            Some(1),
        )));

        let effects = broker.federation_updated(FederationState::default());

        assert!(matches!(
            messages_for(&effects, client).as_slice(),
            [ServerMessage::TargetState { state: None, .. }]
        ));
    }

    #[test]
    fn two_clients_watching_one_pane_cost_one_route() {
        let mut broker = broker();
        let first = greet(&mut broker);
        let second = greet(&mut broker);

        let opening = subscribe(&mut broker, first, "w1:p1", TerminalAccess::Observe, 80, 24);
        let joining = subscribe(
            &mut broker,
            second,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
        );

        assert_eq!(
            opening
                .iter()
                .filter(|effect| matches!(effect, Effect::OpenRoute { .. }))
                .count(),
            1
        );
        assert!(
            !joining
                .iter()
                .any(|effect| matches!(effect, Effect::OpenRoute { .. }))
        );
    }

    #[test]
    fn control_is_granted_once_and_never_taken_implicitly() {
        let mut broker = broker();
        let holder = greet(&mut broker);
        let latecomer = greet(&mut broker);

        let granted = subscribe(
            &mut broker,
            holder,
            "w1:p1",
            TerminalAccess::Control,
            80,
            24,
        );
        let refused = subscribe(
            &mut broker,
            latecomer,
            "w1:p1",
            TerminalAccess::Control,
            80,
            24,
        );

        assert!(
            messages_for(&granted, holder).contains(&ServerMessage::PaneLease {
                pane: pane("w1:p1"),
                access: TerminalAccess::Control,
            })
        );
        assert!(
            messages_for(&refused, latecomer).contains(&ServerMessage::PaneLease {
                pane: pane("w1:p1"),
                access: TerminalAccess::Observe,
            })
        );
        // The holder is not told anything, because nothing about its lease
        // changed.
        assert!(messages_for(&refused, holder).is_empty());
    }

    #[test]
    fn an_observer_cannot_type_scroll_or_resize_and_is_told_so() {
        let mut broker = broker();
        let holder = greet(&mut broker);
        let observer = greet(&mut broker);
        subscribe(
            &mut broker,
            holder,
            "w1:p1",
            TerminalAccess::Control,
            80,
            24,
        );
        subscribe(
            &mut broker,
            observer,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
        );

        for message in [
            ClientMessage::PaneInput {
                pane: pane("w1:p1"),
                bytes: b"rm -rf /".to_vec(),
            },
            ClientMessage::PaneResize {
                pane: pane("w1:p1"),
                cols: 40,
                rows: 10,
            },
            ClientMessage::PaneScroll {
                pane: pane("w1:p1"),
                direction: TerminalScrollDirection::Up,
                lines: 3,
                column: 0,
                row: 0,
                modifiers: 0,
            },
        ] {
            let effects = broker.handle(observer, message);
            assert!(matches!(
                messages_for(&effects, observer).as_slice(),
                [ServerMessage::Error { request: None, .. }]
            ));
            assert!(effects.iter().all(|effect| !matches!(
                effect,
                Effect::RouteInput { .. } | Effect::RouteResize { .. } | Effect::RouteScroll { .. }
            )));
        }
    }

    #[test]
    fn the_control_holder_reaches_the_route() {
        let mut broker = broker();
        let holder = greet(&mut broker);
        subscribe(
            &mut broker,
            holder,
            "w1:p1",
            TerminalAccess::Control,
            80,
            24,
        );

        let effects = broker.handle(
            holder,
            ClientMessage::PaneInput {
                pane: pane("w1:p1"),
                bytes: b"ls\n".to_vec(),
            },
        );

        assert_eq!(
            effects,
            vec![Effect::RouteInput {
                pane: pane("w1:p1"),
                bytes: b"ls\n".to_vec(),
            }]
        );
    }

    #[test]
    fn an_explicit_takeover_tells_the_previous_holder_it_was_downgraded() {
        let mut broker = broker();
        let first = greet(&mut broker);
        let second = greet(&mut broker);
        subscribe(&mut broker, first, "w1:p1", TerminalAccess::Control, 80, 24);
        subscribe(
            &mut broker,
            second,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
        );

        let effects = broker.handle(
            second,
            ClientMessage::TakePaneControl {
                pane: pane("w1:p1"),
            },
        );

        assert_eq!(
            messages_for(&effects, first),
            vec![ServerMessage::PaneLease {
                pane: pane("w1:p1"),
                access: TerminalAccess::Observe,
            }]
        );
        assert_eq!(
            messages_for(&effects, second),
            vec![ServerMessage::PaneLease {
                pane: pane("w1:p1"),
                access: TerminalAccess::Control,
            }]
        );

        // The lease really moved: the old holder can no longer type.
        let refused = broker.handle(
            first,
            ClientMessage::PaneInput {
                pane: pane("w1:p1"),
                bytes: b"x".to_vec(),
            },
        );
        assert!(
            !refused
                .iter()
                .any(|effect| matches!(effect, Effect::RouteInput { .. }))
        );
    }

    #[test]
    fn taking_control_of_an_observe_only_route_reopens_it_with_input() {
        let mut broker = broker();
        let first = greet(&mut broker);
        let second = greet(&mut broker);
        subscribe(&mut broker, first, "w1:p1", TerminalAccess::Observe, 80, 24);
        subscribe(
            &mut broker,
            second,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
        );

        let effects = broker.handle(
            second,
            ClientMessage::TakePaneControl {
                pane: pane("w1:p1"),
            },
        );

        let routes = effects
            .iter()
            .filter(|effect| matches!(effect, Effect::CloseRoute { .. } | Effect::OpenRoute { .. }))
            .collect::<Vec<_>>();
        assert_eq!(
            routes,
            vec![
                &Effect::CloseRoute {
                    pane: pane("w1:p1")
                },
                &Effect::OpenRoute {
                    pane: pane("w1:p1"),
                    access: TerminalAccess::Control,
                    cols: 80,
                    rows: 24,
                }
            ]
        );
    }

    #[test]
    fn a_pane_is_sized_by_whoever_holds_control() {
        let mut broker = broker();
        let desktop = greet(&mut broker);
        let phone = greet(&mut broker);
        subscribe(
            &mut broker,
            desktop,
            "w1:p1",
            TerminalAccess::Control,
            200,
            50,
        );

        // Subscribing at a phone's size changes nothing while the desktop holds
        // the lease.
        let joining = subscribe(&mut broker, phone, "w1:p1", TerminalAccess::Observe, 40, 20);
        assert!(
            !joining
                .iter()
                .any(|effect| matches!(effect, Effect::RouteResize { .. }))
        );

        // Taking the lease moves the pane to the new holder's size.
        let effects = broker.handle(
            phone,
            ClientMessage::TakePaneControl {
                pane: pane("w1:p1"),
            },
        );
        assert!(effects.contains(&Effect::RouteResize {
            pane: pane("w1:p1"),
            cols: 40,
            rows: 20,
        }));
    }

    #[test]
    fn taking_control_without_a_subscription_is_refused() {
        let mut broker = broker();
        let client = greet(&mut broker);

        let effects = broker.handle(
            client,
            ClientMessage::TakePaneControl {
                pane: pane("w1:p1"),
            },
        );

        assert!(matches!(
            messages_for(&effects, client).as_slice(),
            [ServerMessage::Error { request: None, .. }]
        ));
    }

    #[test]
    fn a_route_outlives_every_client_but_the_last() {
        let mut broker = broker();
        let first = greet(&mut broker);
        let second = greet(&mut broker);
        subscribe(&mut broker, first, "w1:p1", TerminalAccess::Observe, 80, 24);
        subscribe(
            &mut broker,
            second,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
        );

        let leaving = broker.handle(
            first,
            ClientMessage::UnsubscribePane {
                pane: pane("w1:p1"),
            },
        );
        assert!(
            !leaving
                .iter()
                .any(|effect| matches!(effect, Effect::CloseRoute { .. }))
        );

        let last = broker.handle(
            second,
            ClientMessage::UnsubscribePane {
                pane: pane("w1:p1"),
            },
        );
        assert!(last.contains(&Effect::CloseRoute {
            pane: pane("w1:p1")
        }));
    }

    #[test]
    fn a_departing_control_holder_downgrades_the_route_rather_than_handing_it_on() {
        let mut broker = broker();
        let holder = greet(&mut broker);
        let observer = greet(&mut broker);
        subscribe(
            &mut broker,
            holder,
            "w1:p1",
            TerminalAccess::Control,
            80,
            24,
        );
        subscribe(
            &mut broker,
            observer,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
        );

        let effects = broker.disconnect(holder);

        assert!(effects.contains(&Effect::OpenRoute {
            pane: pane("w1:p1"),
            access: TerminalAccess::Observe,
            cols: 80,
            rows: 24,
        }));
        assert_eq!(
            messages_for(&effects, observer),
            vec![ServerMessage::PaneLease {
                pane: pane("w1:p1"),
                access: TerminalAccess::Observe,
            }]
        );

        // The lease is free, so an observer that asks for it now gets it.
        let effects = broker.handle(
            observer,
            ClientMessage::TakePaneControl {
                pane: pane("w1:p1"),
            },
        );
        assert!(
            messages_for(&effects, observer).contains(&ServerMessage::PaneLease {
                pane: pane("w1:p1"),
                access: TerminalAccess::Control,
            })
        );
    }

    #[test]
    fn a_refused_control_stream_downgrades_instead_of_closing_the_pane() {
        let mut broker = broker();
        let holder = greet(&mut broker);
        let observer = greet(&mut broker);
        subscribe(
            &mut broker,
            holder,
            "w1:p1",
            TerminalAccess::Control,
            80,
            24,
        );
        subscribe(
            &mut broker,
            observer,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
        );

        let effects = broker.route_downgraded(&pane("w1:p1"));

        // Everyone watching learns what the route now is, and the pane is not
        // closed.
        for client in [holder, observer] {
            assert_eq!(
                messages_for(&effects, client),
                vec![ServerMessage::PaneLease {
                    pane: pane("w1:p1"),
                    access: TerminalAccess::Observe,
                }]
            );
        }
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::CloseRoute { .. }))
        );

        // The lease is genuinely gone: the old holder can no longer type, and
        // repeating the downgrade says nothing further.
        let refused = broker.handle(
            holder,
            ClientMessage::PaneInput {
                pane: pane("w1:p1"),
                bytes: b"x".to_vec(),
            },
        );
        assert!(
            !refused
                .iter()
                .any(|effect| matches!(effect, Effect::RouteInput { .. }))
        );
        assert!(broker.route_downgraded(&pane("w1:p1")).is_empty());
    }

    #[test]
    fn frames_reach_subscribers_and_nobody_else() {
        let mut broker = broker();
        let watching = greet(&mut broker);
        let elsewhere = greet(&mut broker);
        subscribe(
            &mut broker,
            watching,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
        );
        subscribe(
            &mut broker,
            elsewhere,
            "w1:p2",
            TerminalAccess::Observe,
            80,
            24,
        );

        let effects = broker.pane_frame(
            &pane("w1:p1"),
            3,
            80,
            24,
            false,
            Arc::from(b"\x1b[31mred".to_vec()),
        );

        assert_eq!(messages_for(&effects, watching).len(), 1);
        assert!(messages_for(&effects, elsewhere).is_empty());
    }

    /// The whole point of the second representation: one pane, two clients,
    /// each rendering what it is able to.
    #[test]
    fn one_frame_serves_an_emulator_client_and_a_screen_client_at_once() {
        let mut broker = broker();
        let tui = greet(&mut broker);
        let browser = greet(&mut broker);
        subscribe(&mut broker, tui, "w1:p1", TerminalAccess::Observe, 80, 24);
        subscribe_as(
            &mut broker,
            browser,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
            PaneRepresentation::Screen,
        );

        let effects = frame(&mut broker, "w1:p1", b"hello");

        assert!(
            matches!(
                messages_for(&effects, tui).as_slice(),
                [ServerMessage::PaneFrame { .. }]
            ),
            "the emulator client still receives untouched bytes"
        );
        let browser_messages = messages_for(&effects, browser);
        let [ServerMessage::PaneScreen { screen, .. }] = browser_messages.as_slice() else {
            panic!("expected one rendered screen, got {browser_messages:?}");
        };
        assert_eq!(row_text(screen, 0), "hello");
    }

    #[test]
    fn a_screen_subscriber_is_sent_a_whole_screen_before_any_diff() {
        let mut broker = broker();
        let viewer = greet(&mut broker);
        subscribe_as(
            &mut broker,
            viewer,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
            PaneRepresentation::Screen,
        );

        let effects = frame(&mut broker, "w1:p1", b"hello");

        assert!(
            matches!(
                messages_for(&effects, viewer).as_slice(),
                [ServerMessage::PaneScreen { .. }]
            ),
            "a client holding nothing cannot apply a diff"
        );
    }

    #[test]
    fn a_later_frame_reaches_a_screen_subscriber_as_a_diff_naming_what_it_follows() {
        let mut broker = broker();
        let viewer = greet(&mut broker);
        subscribe_as(
            &mut broker,
            viewer,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
            PaneRepresentation::Screen,
        );

        let first = frame(&mut broker, "w1:p1", b"hello");
        let held = match messages_for(&first, viewer).as_slice() {
            [ServerMessage::PaneScreen { screen, .. }] => screen.sequence,
            other => panic!("expected a whole screen, got {other:?}"),
        };

        let second = frame(&mut broker, "w1:p1", b"\r\nworld");
        let messages = messages_for(&second, viewer);
        let [ServerMessage::PaneScreenDiff { diff, .. }] = messages.as_slice() else {
            panic!("expected a diff once the client holds a screen, got {messages:?}");
        };
        assert_eq!(
            diff.follows, held,
            "a diff must name the update it applies to"
        );
        assert!(
            diff.rows.iter().any(|update| update.row == 1),
            "the second line changed and the diff should say so: {:?}",
            diff.rows
        );
    }

    /// A terminal emits plenty that leaves the visible screen exactly as it
    /// was. Sending an empty diff for each would be traffic carrying nothing.
    #[test]
    fn a_frame_that_changes_nothing_visible_is_not_sent_as_a_screen() {
        let mut broker = broker();
        let tui = greet(&mut broker);
        let browser = greet(&mut broker);
        subscribe(&mut broker, tui, "w1:p1", TerminalAccess::Observe, 80, 24);
        subscribe_as(
            &mut broker,
            browser,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
            PaneRepresentation::Screen,
        );
        frame(&mut broker, "w1:p1", b"hello");

        // Setting attributes paints no cell and moves no cursor.
        let effects = frame(&mut broker, "w1:p1", b"\x1b[0m");

        assert!(
            messages_for(&effects, browser).is_empty(),
            "nothing visible changed, so the screen client hears nothing"
        );
        assert_eq!(
            messages_for(&effects, tui).len(),
            1,
            "the frame itself is still delivered untouched to an emulator client"
        );
    }

    #[test]
    fn a_resize_sends_a_whole_screen_because_a_diff_cannot_express_it() {
        let mut broker = broker();
        let viewer = greet(&mut broker);
        subscribe_as(
            &mut broker,
            viewer,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
            PaneRepresentation::Screen,
        );
        frame(&mut broker, "w1:p1", b"hello");

        let effects =
            broker.pane_frame(&pane("w1:p1"), 9, 100, 30, false, Arc::from(b"!".to_vec()));

        let messages = messages_for(&effects, viewer);
        let [ServerMessage::PaneScreen { screen, .. }] = messages.as_slice() else {
            panic!("a resized screen cannot arrive as a diff, got {messages:?}");
        };
        assert_eq!((screen.width, screen.height), (100, 30));
    }

    /// A full repaint restarts the program's idea of the screen, so the
    /// emulator is rebuilt rather than fed. The two payloads differ in length
    /// deliberately: an emulator that was appended to would leave the tail of
    /// the old line visible past the end of the new one.
    #[test]
    fn a_full_frame_restarts_the_emulator_rather_than_writing_over_it() {
        let mut broker = broker();
        let viewer = greet(&mut broker);
        subscribe_as(
            &mut broker,
            viewer,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
            PaneRepresentation::Screen,
        );
        frame(&mut broker, "w1:p1", b"a long stale line");

        let effects =
            broker.pane_frame(&pane("w1:p1"), 9, 80, 24, true, Arc::from(b"new".to_vec()));

        assert_eq!(
            rendered_row(&broker, "w1:p1", 0),
            "new",
            "a full repaint must not leave the previous line's tail behind"
        );
        assert!(
            !messages_for(&effects, viewer).is_empty(),
            "the viewer is told about the repaint"
        );
    }

    /// The reason this bound exists: a rendered update is queued into an
    /// unbounded outbox, so a viewer that stops draining would otherwise
    /// accumulate one message per frame in this process for as long as the pane
    /// keeps producing output.
    #[test]
    fn a_viewer_that_stops_painting_stops_being_queued_for() {
        let mut broker = broker();
        let viewer = greet(&mut broker);
        subscribe_as(
            &mut broker,
            viewer,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
            PaneRepresentation::Screen,
        );

        let mut sent = 0;
        for index in 0..20 {
            let effects = frame(&mut broker, "w1:p1", format!("line {index}\r\n").as_bytes());
            sent += messages_for(&effects, viewer).len();
        }

        assert_eq!(
            sent, MAX_OUTSTANDING_SCREEN_UPDATES,
            "twenty frames reached a viewer that acknowledged none of them"
        );
    }

    #[test]
    fn acknowledging_an_update_lets_the_next_one_through() {
        let mut broker = broker();
        let viewer = greet(&mut broker);
        subscribe_as(
            &mut broker,
            viewer,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
            PaneRepresentation::Screen,
        );
        for index in 0..20 {
            frame(&mut broker, "w1:p1", format!("line {index}\r\n").as_bytes());
        }

        let held = first_sequence(&broker, viewer, "w1:p1");
        let effects = ack(&mut broker, viewer, "w1:p1", held);

        assert!(
            !messages_for(&effects, viewer).is_empty(),
            "a viewer that caught up was sent nothing"
        );
    }

    /// What a viewer that fell behind wants is the screen as it stands, not the
    /// history of how it got there. Frames cannot do this; a snapshot can,
    /// because it is self-contained.
    #[test]
    fn a_viewer_that_catches_up_is_sent_the_screen_as_it_stands() {
        let mut broker = broker();
        let viewer = greet(&mut broker);
        subscribe_as(
            &mut broker,
            viewer,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
            PaneRepresentation::Screen,
        );
        for index in 0..20 {
            frame(&mut broker, "w1:p1", format!("line {index}\r\n").as_bytes());
        }

        let held = first_sequence(&broker, viewer, "w1:p1");
        let effects = ack(&mut broker, viewer, "w1:p1", held);

        let messages = messages_for(&effects, viewer);
        let [ServerMessage::PaneScreen { screen, .. }] = messages.as_slice() else {
            panic!("expected the current screen, not a backlog: {messages:?}");
        };
        assert_eq!(
            screen.sequence,
            broker.screens[&pane("w1:p1")].last.sequence,
            "the viewer was sent something other than the newest screen"
        );
        assert_eq!(
            row_text(screen, 19),
            "line 19",
            "the screen sent was not the one the pane is showing"
        );
    }

    /// An acknowledgement reports progress. It does not grant anything, so a
    /// client cannot talk its way into more of this process's memory.
    #[test]
    fn an_acknowledgement_is_checked_against_what_was_actually_sent() {
        let mut broker = broker();
        let viewer = greet(&mut broker);
        subscribe_as(
            &mut broker,
            viewer,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
            PaneRepresentation::Screen,
        );
        for index in 0..20 {
            frame(&mut broker, "w1:p1", format!("line {index}\r\n").as_bytes());
        }

        // Never sent, so it buys nothing.
        assert!(
            messages_for(&ack(&mut broker, viewer, "w1:p1", 9_999), viewer).is_empty(),
            "a sequence that was never sent was accepted"
        );

        // The same update, over and over, is still one update painted.
        let held = first_sequence(&broker, viewer, "w1:p1");
        ack(&mut broker, viewer, "w1:p1", held);
        let mut extra = 0;
        for _ in 0..10 {
            extra += messages_for(&ack(&mut broker, viewer, "w1:p1", held), viewer).len();
        }
        assert_eq!(
            extra, 0,
            "repeating one acknowledgement bought more updates"
        );
    }

    /// A diff is only applicable to the exact update it follows, and in a
    /// working broker every screen subscriber holds that update by the time the
    /// next frame arrives — so nothing driving the daemon can produce a
    /// mismatch, and the rule that decides it would go untested.
    ///
    /// It is tested here directly instead. The alternative is a check that
    /// cannot fail, which is a check nobody has evidence for.
    #[test]
    fn a_diff_is_refused_for_a_subscriber_holding_a_different_update() {
        let subscription = |sent| PaneSubscription {
            requested: TerminalAccess::Observe,
            cols: 80,
            rows: 24,
            representation: PaneRepresentation::Screen,
            sent,
            outstanding: VecDeque::new(),
        };
        let mut parser = vt100::Parser::new(4, 20, 0);
        parser.process(b"before");
        let previous = Snapshot::of(parser.screen(), 7);
        parser.process(b"\r\nafter");
        let current = Snapshot::of(parser.screen(), 8);
        let rendered = Rendered {
            diff: ScreenDiff::between(&previous, &current),
            screen: current,
            changed: true,
        };

        assert!(
            matches!(
                Broker::update_for(&subscription(Some(7)), &pane("w1:p1"), &rendered),
                ServerMessage::PaneScreenDiff { .. }
            ),
            "a subscriber holding update 7 can apply a diff that follows 7"
        );
        for held in [None, Some(6), Some(8)] {
            assert!(
                matches!(
                    Broker::update_for(&subscription(held), &pane("w1:p1"), &rendered),
                    ServerMessage::PaneScreen { .. }
                ),
                "a subscriber holding {held:?} was sent a diff it cannot apply"
            );
        }
    }

    /// The case whose answer is not in question, written first so that a wrong
    /// harness shows up here rather than as a finding three tests later: a
    /// viewer that acknowledges everything it is sent falls behind by nothing.
    #[test]
    fn a_viewer_that_acknowledges_everything_holds_nothing_outstanding() {
        let mut broker = broker();
        let viewer = greet(&mut broker);
        subscribe_as(
            &mut broker,
            viewer,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
            PaneRepresentation::Screen,
        );

        for index in 0..20 {
            let effects = frame(&mut broker, "w1:p1", format!("line {index}\r\n").as_bytes());
            for message in messages_for(&effects, viewer) {
                ack(&mut broker, viewer, "w1:p1", sequence_of(&message));
            }
        }

        assert_eq!(
            outstanding(&broker, viewer, "w1:p1"),
            0,
            "a viewer keeping up should be waiting on nothing"
        );
    }

    /// The reason the queue holds sequences rather than a count. A viewer that
    /// paints four and reports only the newest is telling the truth about all
    /// four, and counting would have called it three behind for ever.
    #[test]
    fn acknowledging_the_newest_settles_everything_below_it() {
        let mut broker = broker();
        let viewer = greet(&mut broker);
        subscribe_as(
            &mut broker,
            viewer,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
            PaneRepresentation::Screen,
        );
        for index in 0..20 {
            frame(&mut broker, "w1:p1", format!("line {index}\r\n").as_bytes());
        }
        assert_eq!(
            outstanding(&broker, viewer, "w1:p1"),
            MAX_OUTSTANDING_SCREEN_UPDATES
        );

        let newest = newest_outstanding(&broker, viewer, "w1:p1");
        let effects = ack(&mut broker, viewer, "w1:p1", newest);

        // One acknowledgement settled all four, so the catch-up update is the
        // only thing now outstanding.
        assert_eq!(
            outstanding(&broker, viewer, "w1:p1"),
            1,
            "acknowledging the newest of four left updates outstanding"
        );
        assert!(!messages_for(&effects, viewer).is_empty());
    }

    /// A dropped acknowledgement costs that update and nothing more: the next
    /// one settles it. Counting made a missed acknowledgement permanent, which
    /// is a thing a client could trip over without ever being told the rule.
    #[test]
    fn a_missed_acknowledgement_is_repaired_by_the_next_one() {
        let mut broker = broker();
        let viewer = greet(&mut broker);
        subscribe_as(
            &mut broker,
            viewer,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
            PaneRepresentation::Screen,
        );
        for index in 0..20 {
            frame(&mut broker, "w1:p1", format!("line {index}\r\n").as_bytes());
        }

        let held: Vec<u64> = outstanding_sequences(&broker, viewer, "w1:p1");
        // Skip the first entirely, as a viewer that dropped one would.
        ack(&mut broker, viewer, "w1:p1", held[1]);

        assert_eq!(
            outstanding(&broker, viewer, "w1:p1"),
            MAX_OUTSTANDING_SCREEN_UPDATES - 2 + 1,
            "the skipped update was not settled by the one after it"
        );
    }

    /// An acknowledgement names an update this subscription was sent. The queue
    /// has gaps — a viewer at its limit is skipped, so what it missed was never
    /// in it — and without membership a client could clear those gaps by naming
    /// an update it never received.
    #[test]
    fn an_update_that_was_never_sent_settles_nothing() {
        let mut broker = broker();
        let viewer = greet(&mut broker);
        subscribe_as(
            &mut broker,
            viewer,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
            PaneRepresentation::Screen,
        );
        for index in 0..20 {
            frame(&mut broker, "w1:p1", format!("line {index}\r\n").as_bytes());
        }
        // Settle one, so the catch-up update leaves a gap behind it.
        let first = first_sequence(&broker, viewer, "w1:p1");
        ack(&mut broker, viewer, "w1:p1", first);

        let held = outstanding_sequences(&broker, viewer, "w1:p1");
        let never_sent = held[held.len() - 1] - 1;
        assert!(!held.contains(&never_sent), "the fixture needs a real gap");

        let effects = ack(&mut broker, viewer, "w1:p1", never_sent);

        assert_eq!(
            outstanding_sequences(&broker, viewer, "w1:p1"),
            held,
            "a sequence that was never sent settled updates that were"
        );
        assert!(
            messages_for(&effects, viewer).is_empty(),
            "an update that was never sent bought a slot"
        );
    }

    /// The bound belongs to the rendered path alone: a frame subscriber is
    /// already stopped by the socket it is not draining.
    #[test]
    fn a_frame_subscriber_is_not_bounded_by_the_screen_limit() {
        let mut broker = broker();
        let tui = greet(&mut broker);
        subscribe(&mut broker, tui, "w1:p1", TerminalAccess::Observe, 80, 24);

        let mut sent = 0;
        for index in 0..20 {
            let effects = frame(&mut broker, "w1:p1", format!("line {index}\r\n").as_bytes());
            sent += messages_for(&effects, tui).len();
        }

        assert_eq!(sent, 20, "frames were withheld from an emulator client");
    }

    #[test]
    fn nothing_is_parsed_for_a_pane_watched_only_as_frames() {
        let mut broker = broker();
        let tui = greet(&mut broker);
        subscribe(&mut broker, tui, "w1:p1", TerminalAccess::Observe, 80, 24);

        frame(&mut broker, "w1:p1", b"hello");

        assert!(
            broker.screens.is_empty(),
            "a pane nobody renders should cost no emulator"
        );
    }

    #[test]
    fn the_emulator_is_dropped_when_the_last_screen_subscriber_leaves() {
        let mut broker = broker();
        let tui = greet(&mut broker);
        let browser = greet(&mut broker);
        subscribe(&mut broker, tui, "w1:p1", TerminalAccess::Observe, 80, 24);
        subscribe_as(
            &mut broker,
            browser,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
            PaneRepresentation::Screen,
        );
        frame(&mut broker, "w1:p1", b"hello");
        assert!(broker.screens.contains_key(&pane("w1:p1")));

        broker.handle(
            browser,
            ClientMessage::UnsubscribePane {
                pane: pane("w1:p1"),
            },
        );

        assert!(
            broker.screens.is_empty(),
            "the route outlives the viewer, but the emulator should not"
        );
    }

    /// An idle pane produces no output, so a viewer that had to wait for the
    /// next frame would be indistinguishable from a broken one.
    #[test]
    fn a_screen_subscriber_arriving_mid_stream_is_sent_the_screen_as_it_stands() {
        let mut broker = broker();
        let first = greet(&mut broker);
        let second = greet(&mut broker);
        subscribe_as(
            &mut broker,
            first,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
            PaneRepresentation::Screen,
        );
        frame(&mut broker, "w1:p1", b"already here");

        let effects = subscribe_as(
            &mut broker,
            second,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
            PaneRepresentation::Screen,
        );

        let messages = messages_for(&effects, second);
        let Some(ServerMessage::PaneScreen { screen, .. }) = messages.first() else {
            panic!("expected the current screen on subscribing, got {messages:?}");
        };
        assert_eq!(row_text(screen, 0), "already here");
    }

    #[test]
    fn a_frame_for_an_unrouted_pane_is_discarded() {
        let mut broker = broker();
        greet(&mut broker);

        assert!(
            broker
                .pane_frame(
                    &pane("w1:p9"),
                    1,
                    80,
                    24,
                    true,
                    Arc::from(b"stale".to_vec())
                )
                .is_empty()
        );
    }

    #[test]
    fn a_closed_route_is_reported_once_and_then_forgotten() {
        let mut broker = broker();
        let client = greet(&mut broker);
        subscribe(
            &mut broker,
            client,
            "w1:p1",
            TerminalAccess::Control,
            80,
            24,
        );

        let effects = broker.pane_route_closed(&pane("w1:p1"));
        assert_eq!(
            messages_for(&effects, client),
            vec![ServerMessage::PaneClosed {
                pane: pane("w1:p1")
            }]
        );

        assert!(broker.pane_route_closed(&pane("w1:p1")).is_empty());
        assert!(
            broker
                .pane_frame(&pane("w1:p1"), 1, 80, 24, true, Arc::from(b"late".to_vec()))
                .is_empty()
        );
    }

    #[test]
    fn a_live_snapshot_retires_a_route_whose_pane_is_gone() {
        let mut broker = broker();
        let client = greet(&mut broker);
        broker.federation_updated(federation(target_state(
            TargetConnectionState::Live,
            &["w1:p1"],
            Some(1),
        )));
        subscribe(
            &mut broker,
            client,
            "w1:p1",
            TerminalAccess::Control,
            80,
            24,
        );

        let effects = broker.federation_updated(federation(target_state(
            TargetConnectionState::Live,
            &[],
            Some(2),
        )));

        assert!(effects.contains(&Effect::Send {
            client,
            message: ServerMessage::PaneClosed {
                pane: pane("w1:p1")
            },
        }));
    }

    #[test]
    fn a_disconnected_target_never_retires_a_route() {
        let mut broker = broker();
        let client = greet(&mut broker);
        broker.federation_updated(federation(target_state(
            TargetConnectionState::Live,
            &["w1:p1"],
            Some(1),
        )));
        subscribe(
            &mut broker,
            client,
            "w1:p1",
            TerminalAccess::Control,
            80,
            24,
        );

        // A target in backoff proves nothing about its panes, even if its
        // retained snapshot is empty.
        let effects = broker.federation_updated(federation(target_state(
            TargetConnectionState::Backoff { attempt: 2 },
            &[],
            Some(2),
        )));

        assert!(!effects.iter().any(|effect| matches!(
            effect,
            Effect::Send {
                message: ServerMessage::PaneClosed { .. },
                ..
            }
        )));
    }

    #[test]
    fn pasting_and_uploading_need_the_lease_that_typing_needs() {
        let mut broker = broker();
        let holder = greet(&mut broker);
        let observer = greet(&mut broker);
        subscribe(
            &mut broker,
            holder,
            "w1:p1",
            TerminalAccess::Control,
            80,
            24,
        );
        subscribe(
            &mut broker,
            observer,
            "w1:p1",
            TerminalAccess::Observe,
            80,
            24,
        );

        for message in [
            ClientMessage::PastePaneText {
                request: 1,
                pane: pane("w1:p1"),
                text: "rm -rf /\n".to_owned(),
            },
            ClientMessage::BeginUpload {
                request: 2,
                pane: pane("w1:p1"),
                mime: "image/png".to_owned(),
                name: None,
                length: 16,
            },
        ] {
            let effects = broker.handle(observer, message.clone());
            assert!(matches!(
                messages_for(&effects, observer).as_slice(),
                [ServerMessage::Error { request: None, .. }]
            ));
            assert!(effects.iter().all(|effect| !matches!(
                effect,
                Effect::PastePaneText { .. } | Effect::BeginUpload { .. }
            )));

            // The same message from the lease holder is forwarded.
            let effects = broker.handle(holder, message);
            assert!(effects.iter().any(|effect| matches!(
                effect,
                Effect::PastePaneText { .. } | Effect::BeginUpload { .. }
            )));
        }
    }

    #[test]
    fn an_action_result_reaches_only_the_client_that_asked() {
        let mut broker = broker();
        let asking = greet(&mut broker);
        let other = greet(&mut broker);

        let effects = broker.handle(
            asking,
            ClientMessage::RunOperation {
                request: 4,
                operation: Operation::CreateTab {
                    workspace: crate::model::WorkspaceId::new("development", "work", "w1"),
                },
            },
        );
        assert!(matches!(
            effects.as_slice(),
            [Effect::RunOperation { request: 4, .. }]
        ));

        let effects = broker.operation_completed(asking, 4, true, "created tab");
        assert!(matches!(
            messages_for(&effects, asking).as_slice(),
            [ServerMessage::OperationResult {
                request: 4,
                applied: true,
                ..
            }]
        ));
        assert!(messages_for(&effects, other).is_empty());
    }

    #[test]
    fn a_result_for_a_departed_client_is_dropped() {
        let mut broker = broker();
        let client = greet(&mut broker);
        broker.handle(
            client,
            ClientMessage::RunOperation {
                request: 1,
                operation: Operation::ClosePane {
                    pane: pane("w1:p1"),
                },
            },
        );
        broker.disconnect(client);

        assert!(
            broker
                .operation_completed(client, 1, true, "done")
                .is_empty()
        );
    }

    #[test]
    fn subscribing_asks_for_the_history_a_client_cannot_derive() {
        let mut broker = broker();
        let client = greet(&mut broker);

        let effects = broker.handle(client, ClientMessage::SubscribeState);

        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::SendAttentionHistory { .. })),
            "a client that does not derive attention is given it"
        );
    }

    #[test]
    fn a_read_state_change_republishes_the_whole_history() {
        let mut broker = broker();
        let subscribed = greet(&mut broker);
        let silent = greet(&mut broker);
        broker.handle(subscribed, ClientMessage::SubscribeState);

        let effects = broker.attention_changed(Vec::new());

        // Every subscriber is corrected by the authority rather than left to
        // reproduce the change.
        assert!(matches!(
            messages_for(&effects, subscribed).as_slice(),
            [ServerMessage::AttentionHistory { .. }]
        ));
        assert!(messages_for(&effects, silent).is_empty());
    }

    #[test]
    fn attention_reaches_state_subscribers_only() {
        let mut broker = broker();
        let subscribed = greet(&mut broker);
        let silent = greet(&mut broker);
        broker.handle(subscribed, ClientMessage::SubscribeState);

        let effects = broker.attention_observed(AttentionEvent {
            id: 1,
            pane: pane("w1:p1"),
            agent: "claude".to_owned(),
            workspace: "review".to_owned(),
            status: "waiting".to_owned(),
            kind: AttentionEventKind::NeedsAttention,
            occurred_at_ms: 1,
            unread: true,
        });

        assert_eq!(messages_for(&effects, subscribed).len(), 1);
        assert!(messages_for(&effects, silent).is_empty());
    }

    #[test]
    fn read_state_is_a_request_because_the_index_is_durable_and_shared() {
        let mut broker = broker();
        let client = greet(&mut broker);

        assert_eq!(
            broker.handle(
                client,
                ClientMessage::MarkAttentionSeen {
                    pane: pane("w1:p1")
                }
            ),
            vec![Effect::MarkAttentionSeen {
                pane: pane("w1:p1")
            }]
        );
        assert_eq!(
            broker.handle(client, ClientMessage::MarkAllAttentionSeen),
            vec![Effect::MarkAllAttentionSeen]
        );
    }

    #[test]
    fn disconnecting_releases_every_route_the_client_held() {
        let mut broker = broker();
        let client = greet(&mut broker);
        subscribe(
            &mut broker,
            client,
            "w1:p1",
            TerminalAccess::Control,
            80,
            24,
        );
        subscribe(
            &mut broker,
            client,
            "w1:p2",
            TerminalAccess::Observe,
            80,
            24,
        );

        let effects = broker.disconnect(client);

        assert!(effects.contains(&Effect::CloseRoute {
            pane: pane("w1:p1")
        }));
        assert!(effects.contains(&Effect::CloseRoute {
            pane: pane("w1:p2")
        }));
        assert!(broker.disconnect(client).is_empty());
    }
}
