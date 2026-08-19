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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::attention::AttentionEvent;
use crate::model::PaneId;
use crate::operation::Operation;
use crate::protocol::{ClientMessage, PROTOCOL_VERSION, ServerMessage};
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
    MarkAttentionSeen {
        pane: PaneId,
    },
    MarkAllAttentionSeen,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneSubscription {
    requested: TerminalAccess,
    cols: u16,
    rows: u16,
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

pub struct Broker {
    server_version: String,
    features: Vec<String>,
    clients: BTreeMap<ClientId, Client>,
    routes: BTreeMap<PaneId, Route>,
    state: FederationState,
    next_client: u64,
}

impl Broker {
    pub fn new(server_version: impl Into<String>, features: Vec<String>) -> Self {
        Self {
            server_version: server_version.into(),
            features,
            clients: BTreeMap::new(),
            routes: BTreeMap::new(),
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
            }
            ClientMessage::SubscribePane {
                pane,
                access,
                cols,
                rows,
            } => self.subscribe_pane(client, pane, access, cols, rows, &mut effects),
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
            ClientMessage::MarkAttentionSeen { pane } => {
                effects.push(Effect::MarkAttentionSeen { pane });
            }
            ClientMessage::MarkAllAttentionSeen => effects.push(Effect::MarkAllAttentionSeen),
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
        route
            .subscribers
            .iter()
            .map(|client| Effect::Send {
                client: *client,
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
            .collect()
    }

    /// The route's own stream ended. Subscribers keep their subscription
    /// records only until they are told, so no client is left waiting on a
    /// stream that no longer exists.
    pub fn pane_route_closed(&mut self, pane: &PaneId) -> Vec<Effect> {
        let Some(route) = self.routes.remove(pane) else {
            return Vec::new();
        };
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

    pub fn attention_observed(&mut self, event: AttentionEvent) -> Vec<Effect> {
        let mut effects = Vec::new();
        self.broadcast_state(ServerMessage::Attention { event }, &mut effects);
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

    fn subscribe_pane(
        &mut self,
        client: ClientId,
        pane: PaneId,
        access: TerminalAccess,
        cols: u16,
        rows: u16,
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
        if route.subscribers.is_empty() {
            self.routes.remove(pane);
            effects.push(Effect::CloseRoute { pane: pane.clone() });
            return;
        }
        if route.control != Some(client) {
            return;
        }

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

    use super::{Broker, ClientId, Effect};
    use crate::attention::{AttentionEvent, AttentionEventKind};
    use crate::model::{PaneId, TargetSession};
    use crate::operation::Operation;
    use crate::protocol::{ClientMessage, PROTOCOL_VERSION, ServerMessage};
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
        broker.handle(
            client,
            ClientMessage::SubscribePane {
                pane: pane(resource),
                access,
                cols,
                rows,
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
