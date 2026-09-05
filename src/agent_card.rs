//! Agent cards: the daemon's task-oriented projection of the federation.
//!
//! The federation hierarchy answers "where is it?" — a target holds Herdr
//! sessions, a session holds workspaces, tabs and panes. It is the routing
//! truth and it does not change here. What it does not answer is "what needs
//! me now?", and on a phone that is the only question worth a screen. A card
//! is that second view of the same facts: one agent, one line of bounded
//! metadata, one route.
//!
//! Three properties make the projection safe to act on, and each one is a type
//! or an invariant here rather than a convention a frontend is trusted to keep:
//!
//! * **The daemon owns it.** Two clients deriving sections and ordering
//!   independently would disagree about which agent is first the moment their
//!   snapshots differed by one refresh, and a person moving between the TUI
//!   and a phone would be reading two different inboxes. The projection is
//!   computed once, next to the state it summarises, and rendered everywhere.
//! * **Identity is qualified.** A card is keyed by [`AgentId`], which carries
//!   target and Herdr session alongside the server-local resource. Two hosts
//!   that both call a pane `w1:p1` produce two cards that never collapse into
//!   one, and a card never resolves onto a host it did not come from.
//! * **A card is not a route.** The card records what was true when the
//!   projection was built; [`AgentCardIndex::resolve`] re-derives the live pane
//!   against the current federation before anything is sent, and fails closed
//!   when the agent moved, vanished, or belongs to a target that is no longer
//!   live. A snapshot that is one refresh stale can therefore be rendered
//!   safely, because rendering it is all it is ever used for.
//!
//! Cards carry bounded display metadata only: labels, a status word, a phase,
//! and timestamps. Terminal contents, scrollback, command lines and plugin
//! output are not summarised, indexed or persisted here — the inbox tells a
//! person which agent to open, and opening it is what shows them a terminal.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::attention::{AgentPhase, AttentionIndex, agent_phase, bounded_metadata};
use crate::model::{AgentId, PaneId};
use crate::state::{AgentState, FederationState, NormalizedSnapshot, TargetConnectionState};

/// Incremented when a card's meaning changes in a way an older renderer would
/// get wrong. Added fields do not bump it: unknown fields are ignored on this
/// wire, so a newer daemon can describe more without breaking an older client.
pub const AGENT_CARD_PROJECTION_VERSION: u32 = 1;

/// How many vanished agents stay visible in `recent`.
///
/// History exists so a person can see what an agent did just before it went
/// away, not so the daemon accumulates a log of every agent it has ever seen.
const MAX_HISTORY_CARDS: usize = 64;

const MAX_LABEL_CHARACTERS: usize = 128;
const MAX_STATUS_CHARACTERS: usize = 64;

/// Where a card sits in the inbox.
///
/// The sections are a priority order, not a taxonomy: the first one is what a
/// person opened the app to deal with, and the last one is what they are only
/// checking on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCardSection {
    /// The agent is blocked on a person.
    NeedsYou,
    /// The agent is running and does not need anything.
    Working,
    /// Everything else: idle or unrecognised live agents, and agents that have
    /// gone away. Live cards here remain openable; vanished ones do not.
    Recent,
}

/// What the agent is doing, as far as its target reported.
///
/// `Unknown` is deliberately distinct from `Idle`. A status word Super-Herdr
/// does not recognise means the projection has no opinion, and saying so is
/// more useful than filing it under a phase it might not be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentActivity {
    NeedsInput,
    Working,
    Idle,
    Unknown,
    /// The agent was observed before and is not in its target's snapshot any
    /// more. A card in this state is history and never a route.
    Gone,
}

impl From<AgentPhase> for AgentActivity {
    fn from(phase: AgentPhase) -> Self {
        match phase {
            AgentPhase::Attention => Self::NeedsInput,
            AgentPhase::Working => Self::Working,
            AgentPhase::Idle => Self::Idle,
            AgentPhase::Unknown => Self::Unknown,
        }
    }
}

/// One agent, as the inbox shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCard {
    /// Identity. Carries the target label and Herdr session, so nothing else
    /// on this card is needed to tell two same-named agents apart.
    pub agent: AgentId,
    /// The pane the agent occupied when the projection was built, or absent
    /// for a card that is history. This is a display convenience and a hint;
    /// it is never the route an action uses. See [`AgentCardIndex::resolve`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<PaneId>,
    /// The agent's own name when it has one, else the pane's label, else the
    /// server-local resource. Bounded, never a terminal line.
    pub title: String,
    pub workspace: String,
    pub tab: String,
    pub pane_label: String,
    /// Which agent program this is, when the target said so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub activity: AgentActivity,
    /// The target's own status word, bounded. Shown, never interpreted as
    /// policy: `activity` is the only classification anything acts on.
    pub status: String,
    pub section: AgentCardSection,
    /// Whether this agent has attention history a person has not seen.
    pub unread: bool,
    /// The target stopped being live and this is its last known snapshot. A
    /// stale card still renders, because disappearing from the inbox the
    /// moment a link flaps would be worse than showing it greyed.
    pub stale: bool,
    /// Whether opening this card could resolve to a live pane at projection
    /// time. A client uses it to decide what to offer; the daemon still
    /// re-resolves before acting, so this is never the only check.
    pub actionable: bool,
    /// When this agent last changed state, from attention history. Absent when
    /// the agent has not changed since the daemon started watching it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_change_ms: Option<u64>,
}

/// The whole inbox, in the order it should be rendered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCardProjection {
    pub version: u32,
    /// Bumped only when the cards actually differ, so a client can skip a
    /// repaint that would change nothing and a test can assert that an
    /// unrelated federation refresh was in fact unrelated.
    pub revision: u64,
    pub needs_you: Vec<AgentCard>,
    pub working: Vec<AgentCard>,
    pub recent: Vec<AgentCard>,
}

impl AgentCardProjection {
    pub fn cards(&self) -> impl Iterator<Item = &AgentCard> {
        self.needs_you
            .iter()
            .chain(self.working.iter())
            .chain(self.recent.iter())
    }

    pub fn card(&self, agent: &AgentId) -> Option<&AgentCard> {
        self.cards().find(|card| &card.agent == agent)
    }

    pub fn is_empty(&self) -> bool {
        self.needs_you.is_empty() && self.working.is_empty() && self.recent.is_empty()
    }
}

/// A resolved, current route to an agent's pane.
///
/// The generation is the target connection it was resolved against. An action
/// carrying it can be refused if the target reconnected in between, which is
/// the same rule the plugin registry already uses: a route derived before a
/// reconnect describes a server process that may no longer exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardRoute {
    pub pane: PaneId,
    pub generation: u64,
}

/// Why a card could not be turned into a route.
///
/// Every variant is a refusal. There is deliberately no "best effort" outcome:
/// the failure mode this type exists to prevent is delivering a person's
/// keystroke to whatever pane happened to be nearby.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardRouteError {
    /// The card's target and Herdr session are not in the federation.
    UnknownTarget,
    /// The target is not live, so there is nothing to route to right now.
    TargetUnavailable,
    /// The target is live and the agent is not in its snapshot.
    AgentGone,
    /// The agent exists but its pane does not, so the route is incomplete.
    PaneMissing,
    /// The snapshot disagrees with itself about where the agent is. This
    /// should not happen; if it does, refusing is the only safe answer.
    Ambiguous,
}

impl CardRouteError {
    pub fn message(self) -> &'static str {
        match self {
            Self::UnknownTarget => "that agent's target is no longer in the federation",
            Self::TargetUnavailable => "that agent's target is not connected",
            Self::AgentGone => "that agent is no longer running",
            Self::PaneMissing => "that agent's pane is no longer open",
            Self::Ambiguous => "that agent's location is ambiguous",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Tracked {
    /// When this card entered the section it is in now. Ordering within a
    /// section is by this number, so a card holds its place while unrelated
    /// targets appear, disappear and refresh around it.
    ///
    /// Ordering by section entry rather than by first sighting also gives the
    /// queue the behaviour a person expects from an inbox: the agent that has
    /// been blocked longest is at the top, and answering it does not reshuffle
    /// the ones below.
    rank: u64,
    section: AgentCardSection,
    card: AgentCard,
}

/// The daemon's live projection state.
///
/// It is a state machine, not a cache: ordering and history only make sense
/// across successive federations, so the index remembers what it published
/// last and each call describes the change from it.
#[derive(Debug, Clone, Default)]
pub struct AgentCardIndex {
    tracked: BTreeMap<AgentId, Tracked>,
    history: VecDeque<AgentCard>,
    next_rank: u64,
    revision: u64,
    published: Option<Published>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Published {
    needs_you: Vec<AgentCard>,
    working: Vec<AgentCard>,
    recent: Vec<AgentCard>,
}

impl AgentCardIndex {
    /// Rebuild the inbox from the current federation and attention history.
    ///
    /// Called on every federation update. It is deliberately pure with respect
    /// to the clock: every timestamp on a card comes from attention history,
    /// which the daemon already stamps when it observes the transition, so two
    /// projections of the same inputs are equal and a test can say so.
    pub fn project(
        &mut self,
        state: &FederationState,
        attention: &AttentionIndex,
    ) -> AgentCardProjection {
        let mut seen = BTreeMap::new();
        for target in state.targets.values() {
            let Some(snapshot) = target.snapshot.as_deref() else {
                continue;
            };
            let live = target.connection == TargetConnectionState::Live;
            for agent in snapshot.agents.values() {
                let id = agent_id(&agent.pane);
                let card = self.build_card(agent, snapshot, attention, live);
                seen.insert(id, card);
            }
        }

        // An agent is only *gone* when the target that owned it is live and
        // says so. A target that dropped its connection is not evidence about
        // anything running on the far side of it, and retiring cards on a
        // flapping link would empty a person's inbox for reasons that have
        // nothing to do with their agents.
        let mut vanished = Vec::new();
        for (id, tracked) in &self.tracked {
            if seen.contains_key(id) {
                continue;
            }
            match state.targets.get(&id.target_session()) {
                Some(target) if target.connection == TargetConnectionState::Live => {
                    vanished.push(tracked.card.clone());
                }
                Some(_) => {}
                // The target left the federation entirely, which is a
                // configuration change rather than an agent ending. Its cards
                // go with it instead of becoming history nobody can act on.
                None => {}
            }
        }
        // Keeping a card requires either seeing the agent again or a target
        // that cannot currently be asked. A live target that no longer lists
        // the agent is the one authority that can end it.
        self.tracked.retain(|id, _| {
            seen.contains_key(id)
                || state
                    .targets
                    .get(&id.target_session())
                    .is_some_and(|target| target.connection != TargetConnectionState::Live)
        });
        // What is left unseen belongs to a target that is not live and has no
        // snapshot to rebuild from — a target that dropped its connection
        // before this refresh. Its cards stay, because the agents behind them
        // probably still exist, but they stop claiming to be current and stop
        // offering themselves as a destination.
        for (id, tracked) in &mut self.tracked {
            if seen.contains_key(id) {
                continue;
            }
            tracked.card.stale = true;
            tracked.card.actionable = false;
        }
        for card in vanished {
            self.retire(card);
        }

        for (id, card) in seen {
            let section = card.section;
            match self.tracked.get_mut(&id) {
                Some(tracked) if tracked.section == section => {
                    tracked.card = card;
                }
                _ => {
                    let rank = self.next_rank;
                    self.next_rank = self.next_rank.saturating_add(1);
                    self.tracked.insert(
                        id,
                        Tracked {
                            rank,
                            section,
                            card,
                        },
                    );
                }
            }
        }

        let needs_you = self.section(AgentCardSection::NeedsYou);
        let working = self.section(AgentCardSection::Working);
        let mut recent = self.section(AgentCardSection::Recent);
        recent.extend(self.history.iter().cloned());

        let published = Published {
            needs_you,
            working,
            recent,
        };
        if self.published.as_ref() != Some(&published) {
            self.revision = self.revision.saturating_add(1);
        }
        self.published = Some(published.clone());
        AgentCardProjection {
            version: AGENT_CARD_PROJECTION_VERSION,
            revision: self.revision,
            needs_you: published.needs_you,
            working: published.working,
            recent: published.recent,
        }
    }

    /// Derive the live pane for a card against the current federation.
    ///
    /// This is the only way a card becomes something that can be sent to, and
    /// it deliberately ignores everything the projection recorded: the card
    /// says where the agent *was*, and by the time a person taps it the answer
    /// may have changed. Anything short of an agent that is present, in a pane
    /// that is present, on a target that is live, is a refusal.
    pub fn resolve(agent: &AgentId, state: &FederationState) -> Result<CardRoute, CardRouteError> {
        let target = state
            .targets
            .get(&agent.target_session())
            .ok_or(CardRouteError::UnknownTarget)?;
        if target.connection != TargetConnectionState::Live {
            return Err(CardRouteError::TargetUnavailable);
        }
        let snapshot = target
            .snapshot
            .as_deref()
            .ok_or(CardRouteError::TargetUnavailable)?;
        let pane = pane_id(agent);
        let observed = snapshot
            .agents
            .get(&pane)
            .ok_or(CardRouteError::AgentGone)?;
        // The snapshot keys agents by pane, so this can only fail if a target
        // sent a record that disagrees with its own index. Refuse rather than
        // pick one of the two answers.
        if observed.pane != pane {
            return Err(CardRouteError::Ambiguous);
        }
        if !snapshot.panes.contains_key(&pane) {
            return Err(CardRouteError::PaneMissing);
        }
        Ok(CardRoute {
            pane,
            generation: target.connection_generation,
        })
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn section(&self, section: AgentCardSection) -> Vec<AgentCard> {
        let mut cards = self
            .tracked
            .values()
            .filter(|tracked| tracked.section == section)
            .collect::<Vec<_>>();
        // The rank orders the section; the identity only breaks a tie, which
        // two cards can reach when they entered in the same projection. Both
        // keys are total, so the result is one order rather than whichever one
        // the map happened to yield.
        cards.sort_by(|left, right| {
            left.rank
                .cmp(&right.rank)
                .then_with(|| left.card.agent.cmp(&right.card.agent))
        });
        cards
            .into_iter()
            .map(|tracked| tracked.card.clone())
            .collect()
    }

    fn retire(&mut self, card: AgentCard) {
        self.history.push_front(AgentCard {
            pane: None,
            activity: AgentActivity::Gone,
            section: AgentCardSection::Recent,
            actionable: false,
            stale: false,
            ..card
        });
        while self.history.len() > MAX_HISTORY_CARDS {
            self.history.pop_back();
        }
    }

    fn build_card(
        &self,
        agent: &AgentState,
        snapshot: &NormalizedSnapshot,
        attention: &AttentionIndex,
        live: bool,
    ) -> AgentCard {
        let pane = snapshot.panes.get(&agent.pane);
        let status = bounded_metadata(
            agent
                .status
                .as_deref()
                .or_else(|| pane.and_then(|pane| pane.agent_status.as_deref()))
                .unwrap_or("unknown"),
            MAX_STATUS_CHARACTERS,
        );
        let activity = AgentActivity::from(agent_phase(
            &status,
            agent.interactive_ready.unwrap_or(false),
        ));
        let section = match activity {
            AgentActivity::NeedsInput => AgentCardSection::NeedsYou,
            AgentActivity::Working => AgentCardSection::Working,
            AgentActivity::Idle | AgentActivity::Unknown | AgentActivity::Gone => {
                AgentCardSection::Recent
            }
        };
        let workspace = pane
            .and_then(|pane| pane.workspace.as_ref())
            .map(|workspace| {
                snapshot
                    .workspaces
                    .get(workspace)
                    .and_then(|held| held.label.as_deref())
                    .unwrap_or(workspace.resource.as_str())
            })
            .unwrap_or("unassigned");
        let tab = pane
            .and_then(|pane| pane.tab.as_ref())
            .map(|tab| {
                snapshot
                    .tabs
                    .get(tab)
                    .and_then(|held| held.label.as_deref())
                    .unwrap_or(tab.resource.as_str())
            })
            .unwrap_or("unassigned");
        AgentCard {
            agent: agent_id(&agent.pane),
            pane: Some(agent.pane.clone()),
            title: bounded_metadata(
                agent
                    .name
                    .as_deref()
                    .or_else(|| pane.and_then(|pane| pane.agent.as_deref()))
                    .or_else(|| pane.and_then(|pane| pane.label.as_deref()))
                    .unwrap_or(&agent.pane.resource),
                MAX_LABEL_CHARACTERS,
            ),
            workspace: bounded_metadata(workspace, MAX_LABEL_CHARACTERS),
            tab: bounded_metadata(tab, MAX_LABEL_CHARACTERS),
            pane_label: bounded_metadata(
                pane.and_then(|pane| pane.label.as_deref())
                    .unwrap_or(&agent.pane.resource),
                MAX_LABEL_CHARACTERS,
            ),
            provider: agent
                .agent
                .as_deref()
                .map(|provider| bounded_metadata(provider, MAX_LABEL_CHARACTERS)),
            activity,
            status,
            section,
            unread: attention.has_unread_for_pane(&agent.pane),
            stale: !live,
            // A pane the snapshot does not list cannot be resolved, so the
            // card is shown and not offered as a destination.
            actionable: live && pane.is_some(),
            last_change_ms: attention
                .events()
                .rev()
                .find(|event| event.pane == agent.pane)
                .map(|event| event.occurred_at_ms),
        }
    }
}

/// An agent is identified by the pane it occupies, because that is the only
/// identity Herdr's documented snapshot gives one. Both halves of the
/// conversion keep the target and session, so the qualification survives.
fn agent_id(pane: &PaneId) -> AgentId {
    AgentId::new(&pane.target, &pane.session, &pane.resource)
}

fn pane_id(agent: &AgentId) -> PaneId {
    PaneId::new(&agent.target, &agent.session, &agent.resource)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Value, json};

    use super::{
        AgentActivity, AgentCardIndex, AgentCardSection, CardRouteError, MAX_HISTORY_CARDS,
    };
    use crate::attention::AttentionIndex;
    use crate::model::{AgentId, TargetSession};
    use crate::state::{
        FederationState, NormalizedSnapshot, TargetConnectionState, TargetRuntimeState,
        TargetUpdateMode,
    };

    #[test]
    fn separates_the_agent_that_is_blocked_from_the_one_that_is_busy() {
        let mut index = AgentCardIndex::default();
        let state = federation(vec![(
            "host-a",
            TargetConnectionState::Live,
            Some(json!({
                "workspaces": [{"workspace_id": "w1", "label": "compiler"}],
                "tabs": [{"tab_id": "t1", "workspace_id": "w1", "label": "build"}],
                "panes": [
                    {"pane_id": "p1", "workspace_id": "w1", "tab_id": "t1", "label": "left"},
                    {"pane_id": "p2", "workspace_id": "w1", "tab_id": "t1", "label": "right"}
                ],
                "agents": [
                    {"pane_id": "p1", "name": "reviewer", "agent": "claude", "agent_status": "blocked"},
                    {"pane_id": "p2", "name": "builder", "agent": "codex", "agent_status": "working"}
                ]
            })),
        )]);

        let projection = index.project(&state, &AttentionIndex::default());

        assert_eq!(titles(&projection.needs_you), ["reviewer"]);
        assert_eq!(titles(&projection.working), ["builder"]);
        assert!(projection.recent.is_empty());
        let blocked = &projection.needs_you[0];
        assert_eq!(blocked.activity, AgentActivity::NeedsInput);
        assert_eq!(blocked.section, AgentCardSection::NeedsYou);
        assert_eq!(blocked.workspace, "compiler");
        assert_eq!(blocked.tab, "build");
        assert_eq!(blocked.pane_label, "left");
        assert_eq!(blocked.provider.as_deref(), Some("claude"));
        assert!(blocked.actionable);
        assert!(!blocked.stale);
    }

    #[test]
    fn files_an_idle_or_unrecognised_agent_under_recent_while_it_stays_openable() {
        let mut index = AgentCardIndex::default();
        let state = federation(vec![(
            "host-a",
            TargetConnectionState::Live,
            Some(agents_snapshot(&[("p1", "idle"), ("p2", "meditating")])),
        )]);

        let projection = index.project(&state, &AttentionIndex::default());

        assert!(projection.needs_you.is_empty());
        assert!(projection.working.is_empty());
        assert_eq!(
            projection
                .recent
                .iter()
                .map(|card| card.activity)
                .collect::<Vec<_>>(),
            [AgentActivity::Idle, AgentActivity::Unknown]
        );
        assert!(projection.recent.iter().all(|card| card.actionable));
    }

    #[test]
    fn keeps_two_hosts_that_name_a_pane_identically_apart() {
        let mut index = AgentCardIndex::default();
        let state = federation(vec![
            (
                "host-a",
                TargetConnectionState::Live,
                Some(agents_snapshot(&[("w1:p1", "blocked")])),
            ),
            (
                "host-b",
                TargetConnectionState::Live,
                Some(agents_snapshot(&[("w1:p1", "blocked")])),
            ),
        ]);

        let projection = index.project(&state, &AttentionIndex::default());

        assert_eq!(projection.needs_you.len(), 2);
        let targets = projection
            .needs_you
            .iter()
            .map(|card| card.agent.target.as_str())
            .collect::<Vec<_>>();
        assert_eq!(targets, ["host-a", "host-b"]);
        // The route each card resolves to carries its own host, so the two
        // never collapse into one destination.
        for card in &projection.needs_you {
            let route = AgentCardIndex::resolve(&card.agent, &state).unwrap();
            assert_eq!(route.pane.target, card.agent.target);
            assert_eq!(route.pane.resource, "w1:p1");
        }
    }

    #[test]
    fn holds_a_cards_place_while_an_unrelated_target_churns() {
        let mut index = AgentCardIndex::default();
        let attention = AttentionIndex::default();
        let first = federation(vec![
            (
                "host-a",
                TargetConnectionState::Live,
                Some(agents_snapshot(&[("p1", "blocked")])),
            ),
            (
                "host-b",
                TargetConnectionState::Live,
                Some(agents_snapshot(&[("p9", "blocked")])),
            ),
        ]);
        let before = index.project(&first, &attention);
        assert_eq!(
            order(&before.needs_you),
            ["host-a/work/p1", "host-b/work/p9"]
        );

        // host-c arrives, and host-b's snapshot is refreshed without changing
        // anything a card shows. Neither is a reason to renumber the inbox.
        let mut second = first.clone();
        second.targets.insert(
            TargetSession::new("host-c", "work"),
            target(
                "host-c",
                TargetConnectionState::Live,
                Some(agents_snapshot(&[("p1", "blocked")])),
            ),
        );
        let after = index.project(&second, &attention);

        assert_eq!(
            order(&after.needs_you),
            ["host-a/work/p1", "host-b/work/p9", "host-c/work/p1"],
            "an arriving target appends and never reorders what was there"
        );
        assert_eq!(
            after.revision,
            before.revision + 1,
            "the inbox did change: it gained a card"
        );

        // A refresh that changes nothing at all publishes nothing at all.
        let unchanged = index.project(&second, &attention);
        assert_eq!(unchanged.revision, after.revision);
        assert_eq!(unchanged.needs_you, after.needs_you);
    }

    #[test]
    fn sends_an_agent_to_the_back_of_the_section_it_enters() {
        let mut index = AgentCardIndex::default();
        let attention = AttentionIndex::default();
        let blocked_pair = federation(vec![(
            "host-a",
            TargetConnectionState::Live,
            Some(agents_snapshot(&[("p1", "blocked"), ("p2", "blocked")])),
        )]);
        index.project(&blocked_pair, &attention);

        // p1 is answered and starts working, then blocks again. It is now the
        // most recently blocked agent, so it queues behind p2 rather than
        // reclaiming the top of a list p2 has been waiting at.
        let answered = federation(vec![(
            "host-a",
            TargetConnectionState::Live,
            Some(agents_snapshot(&[("p1", "working"), ("p2", "blocked")])),
        )]);
        index.project(&answered, &attention);
        let projection = index.project(&blocked_pair, &attention);

        assert_eq!(
            order(&projection.needs_you),
            ["host-a/work/p2", "host-a/work/p1"]
        );
    }

    #[test]
    fn retires_a_vanished_agent_into_history_that_cannot_be_opened() {
        let mut index = AgentCardIndex::default();
        let attention = AttentionIndex::default();
        let running = federation(vec![(
            "host-a",
            TargetConnectionState::Live,
            Some(agents_snapshot(&[("p1", "blocked")])),
        )]);
        index.project(&running, &attention);

        let ended = federation(vec![(
            "host-a",
            TargetConnectionState::Live,
            Some(agents_snapshot(&[])),
        )]);
        let projection = index.project(&ended, &attention);

        assert!(projection.needs_you.is_empty());
        assert_eq!(projection.recent.len(), 1);
        let card = &projection.recent[0];
        assert_eq!(card.activity, AgentActivity::Gone);
        assert!(!card.actionable);
        assert!(card.pane.is_none(), "history is not a route");
        assert_eq!(
            AgentCardIndex::resolve(&card.agent, &ended),
            Err(CardRouteError::AgentGone)
        );
    }

    #[test]
    fn treats_an_agent_that_moved_pane_as_a_new_card_and_refuses_the_old_route() {
        let mut index = AgentCardIndex::default();
        let attention = AttentionIndex::default();
        let before = federation(vec![(
            "host-a",
            TargetConnectionState::Live,
            Some(agents_snapshot(&[("p1", "blocked")])),
        )]);
        index.project(&before, &attention);

        let after_move = federation(vec![(
            "host-a",
            TargetConnectionState::Live,
            Some(agents_snapshot(&[("p2", "blocked")])),
        )]);
        let projection = index.project(&after_move, &attention);

        assert_eq!(order(&projection.needs_you), ["host-a/work/p2"]);
        assert_eq!(projection.recent.len(), 1);
        let stale_card = AgentId::new("host-a", "work", "p1");
        assert_eq!(
            AgentCardIndex::resolve(&stale_card, &after_move),
            Err(CardRouteError::AgentGone),
            "a card built before the move must not reach the pane it used to name"
        );
        let moved = AgentCardIndex::resolve(&projection.needs_you[0].agent, &after_move).unwrap();
        assert_eq!(moved.pane.resource, "p2");
    }

    #[test]
    fn keeps_cards_when_a_target_drops_but_stops_offering_them() {
        let mut index = AgentCardIndex::default();
        let attention = AttentionIndex::default();
        let live = federation(vec![(
            "host-a",
            TargetConnectionState::Live,
            Some(agents_snapshot(&[("p1", "blocked")])),
        )]);
        index.project(&live, &attention);

        // Disconnected, but the last snapshot is retained: the agent is
        // probably still there, and emptying the inbox on a flapping link
        // would say otherwise.
        let dropped = federation(vec![(
            "host-a",
            TargetConnectionState::Backoff { attempt: 1 },
            Some(agents_snapshot(&[("p1", "blocked")])),
        )]);
        let projection = index.project(&dropped, &attention);

        assert_eq!(projection.needs_you.len(), 1);
        let card = &projection.needs_you[0];
        assert!(card.stale);
        assert!(!card.actionable);
        assert_eq!(
            AgentCardIndex::resolve(&card.agent, &dropped),
            Err(CardRouteError::TargetUnavailable)
        );
        assert!(
            projection.recent.is_empty(),
            "a disconnect is not evidence that an agent ended"
        );
    }

    #[test]
    fn marks_cards_stale_when_a_target_drops_its_snapshot_entirely() {
        let mut index = AgentCardIndex::default();
        let attention = AttentionIndex::default();
        index.project(
            &federation(vec![(
                "host-a",
                TargetConnectionState::Live,
                Some(agents_snapshot(&[("p1", "blocked")])),
            )]),
            &attention,
        );

        let projection = index.project(
            &federation(vec![("host-a", TargetConnectionState::Connecting, None)]),
            &attention,
        );

        assert_eq!(projection.needs_you.len(), 1);
        assert!(projection.needs_you[0].stale);
        assert!(!projection.needs_you[0].actionable);
    }

    #[test]
    fn forgets_the_cards_of_a_target_that_left_the_federation() {
        let mut index = AgentCardIndex::default();
        let attention = AttentionIndex::default();
        index.project(
            &federation(vec![(
                "host-a",
                TargetConnectionState::Live,
                Some(agents_snapshot(&[("p1", "blocked")])),
            )]),
            &attention,
        );

        let projection = index.project(&FederationState::default(), &attention);

        assert!(
            projection.is_empty(),
            "removing a target is a configuration change, not agent history"
        );
    }

    #[test]
    fn refuses_a_card_whose_pane_the_snapshot_does_not_list() {
        let mut index = AgentCardIndex::default();
        let state = federation(vec![(
            "host-a",
            TargetConnectionState::Live,
            Some(json!({
                "workspaces": [],
                "panes": [],
                "agents": [{"pane_id": "p1", "name": "orphan", "agent_status": "blocked"}]
            })),
        )]);

        let projection = index.project(&state, &AttentionIndex::default());

        assert_eq!(projection.needs_you.len(), 1);
        assert!(!projection.needs_you[0].actionable);
        assert_eq!(
            AgentCardIndex::resolve(&projection.needs_you[0].agent, &state),
            Err(CardRouteError::PaneMissing)
        );
    }

    #[test]
    fn refuses_a_card_for_a_target_that_is_not_in_the_federation() {
        assert_eq!(
            AgentCardIndex::resolve(
                &AgentId::new("host-gone", "work", "p1"),
                &FederationState::default()
            ),
            Err(CardRouteError::UnknownTarget)
        );
    }

    #[test]
    fn resolves_against_the_live_connection_generation() {
        let mut state = federation(vec![(
            "host-a",
            TargetConnectionState::Live,
            Some(agents_snapshot(&[("p1", "blocked")])),
        )]);
        let key = TargetSession::new("host-a", "work");
        state.targets.get_mut(&key).unwrap().connection_generation = 7;

        let route = AgentCardIndex::resolve(&AgentId::new("host-a", "work", "p1"), &state).unwrap();

        assert_eq!(route.generation, 7);
        assert_eq!(route.pane.to_string(), "host-a/work/p1");
    }

    #[test]
    fn bounds_the_metadata_a_card_carries() {
        let mut index = AgentCardIndex::default();
        let noisy = format!("alpha\u{1b}[2Jbeta{}", "x".repeat(400));
        let state = federation(vec![(
            "host-a",
            TargetConnectionState::Live,
            Some(json!({
                "workspaces": [],
                "panes": [{"pane_id": "p1"}],
                "agents": [{
                    "pane_id": "p1",
                    "name": noisy,
                    "agent_status": noisy
                }]
            })),
        )]);

        let projection = index.project(&state, &AttentionIndex::default());
        let card = projection.cards().next().unwrap();

        assert_eq!(card.title.chars().count(), 128);
        assert_eq!(card.status.chars().count(), 64);
        assert!(!card.title.contains('\u{1b}'));
        assert!(!card.status.contains('\u{1b}'));
    }

    #[test]
    fn bounds_how_much_history_recent_accumulates() {
        let mut index = AgentCardIndex::default();
        let attention = AttentionIndex::default();
        for pane in 0..MAX_HISTORY_CARDS + 10 {
            let name = format!("p{pane}");
            index.project(
                &federation(vec![(
                    "host-a",
                    TargetConnectionState::Live,
                    Some(agents_snapshot(&[(name.as_str(), "blocked")])),
                )]),
                &attention,
            );
        }
        let projection = index.project(
            &federation(vec![(
                "host-a",
                TargetConnectionState::Live,
                Some(agents_snapshot(&[])),
            )]),
            &attention,
        );

        assert_eq!(projection.recent.len(), MAX_HISTORY_CARDS);
        assert_eq!(
            projection.recent[0].agent.resource,
            format!("p{}", MAX_HISTORY_CARDS + 9),
            "the newest departure is the first one a person sees"
        );
    }

    #[test]
    fn reports_attention_state_on_the_card_it_belongs_to() {
        let mut attention = AttentionIndex::default();
        let state = federation(vec![(
            "host-a",
            TargetConnectionState::Live,
            Some(agents_snapshot(&[("p1", "working"), ("p2", "idle")])),
        )]);
        attention.observe(&state);
        let blocked = federation(vec![(
            "host-a",
            TargetConnectionState::Live,
            Some(agents_snapshot(&[("p1", "blocked"), ("p2", "idle")])),
        )]);
        attention.observe(&blocked);

        let projection = AgentCardIndex::default().project(&blocked, &attention);

        assert!(projection.needs_you[0].unread);
        assert!(projection.needs_you[0].last_change_ms.is_some());
        assert!(!projection.recent[0].unread);
        assert!(projection.recent[0].last_change_ms.is_none());
    }

    fn titles(cards: &[super::AgentCard]) -> Vec<&str> {
        cards.iter().map(|card| card.title.as_str()).collect()
    }

    fn order(cards: &[super::AgentCard]) -> Vec<String> {
        cards.iter().map(|card| card.agent.to_string()).collect()
    }

    fn agents_snapshot(agents: &[(&str, &str)]) -> Value {
        json!({
            "workspaces": [],
            "panes": agents
                .iter()
                .map(|(pane, _)| json!({"pane_id": pane}))
                .collect::<Vec<_>>(),
            "agents": agents
                .iter()
                .map(|(pane, status)| json!({"pane_id": pane, "agent_status": status}))
                .collect::<Vec<_>>(),
        })
    }

    fn federation(targets: Vec<(&str, TargetConnectionState, Option<Value>)>) -> FederationState {
        let mut state = FederationState::default();
        for (name, connection, snapshot) in targets {
            state.targets.insert(
                TargetSession::new(name, "work"),
                target(name, connection, snapshot),
            );
        }
        state
    }

    fn target(
        name: &str,
        connection: TargetConnectionState,
        snapshot: Option<Value>,
    ) -> TargetRuntimeState {
        let key = TargetSession::new(name, "work");
        TargetRuntimeState {
            key: key.clone(),
            endpoint: "test".to_owned(),
            connection,
            update_mode: TargetUpdateMode::Events,
            event_error: None,
            connection_generation: 1,
            selected_herdr_bin: Some("herdr".to_owned()),
            snapshot: snapshot.map(|value| Arc::new(NormalizedSnapshot::from_value(&key, &value))),
            last_error: None,
            last_success: None,
            retry_at: None,
        }
    }
}
