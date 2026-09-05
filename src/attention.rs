use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::model::PaneId;
use crate::state::{FederationState, TargetConnectionState};

const ATTENTION_STATE_VERSION: u32 = 1;
const MAX_ATTENTION_STATE_BYTES: u64 = 512 * 1024;
const MAX_ATTENTION_EVENTS: usize = 256;
const MAX_AGENT_OBSERVATIONS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionEventKind {
    NeedsAttention,
    Working,
    Completed,
    StatusChanged,
    Disappeared,
}

impl AttentionEventKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::NeedsAttention => "needs input",
            Self::Working => "working",
            Self::Completed => "completed",
            Self::StatusChanged => "status changed",
            Self::Disappeared => "disappeared",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttentionEvent {
    pub id: u64,
    pub pane: PaneId,
    pub agent: String,
    pub workspace: String,
    pub status: String,
    pub kind: AttentionEventKind,
    pub occurred_at_ms: u64,
    pub unread: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentObservation {
    pane: PaneId,
    agent: String,
    workspace: String,
    status: String,
    interactive_ready: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentPhase {
    Attention,
    Working,
    Idle,
    Unknown,
}

#[derive(Debug, Clone, Default)]
pub struct AttentionIndex {
    next_event_id: u64,
    observations: BTreeMap<PaneId, AgentObservation>,
    events: Vec<AttentionEvent>,
}

impl AttentionIndex {
    pub fn observe(&mut self, state: &FederationState) -> bool {
        let mut current = BTreeMap::new();
        let mut live_sessions = BTreeSet::new();
        let known_sessions = state.targets.keys().cloned().collect::<BTreeSet<_>>();
        for target in state
            .targets
            .values()
            .filter(|target| target.connection == TargetConnectionState::Live)
        {
            let Some(snapshot) = target.snapshot.as_deref() else {
                continue;
            };
            live_sessions.insert(target.key.clone());
            for agent in snapshot.agents.values() {
                let pane = snapshot.panes.get(&agent.pane);
                let status = bounded_metadata(
                    agent
                        .status
                        .as_deref()
                        .or_else(|| pane.and_then(|pane| pane.agent_status.as_deref()))
                        .unwrap_or("unknown"),
                    64,
                );
                let workspace = bounded_metadata(
                    pane.and_then(|pane| pane.workspace.as_ref())
                        .and_then(|workspace| snapshot.workspaces.get(workspace))
                        .and_then(|workspace| workspace.label.as_deref())
                        .or_else(|| {
                            pane.and_then(|pane| pane.workspace.as_ref())
                                .map(|workspace| workspace.resource.as_str())
                        })
                        .unwrap_or("unassigned"),
                    128,
                );
                let observation = AgentObservation {
                    pane: agent.pane.clone(),
                    agent: bounded_metadata(
                        agent
                            .name
                            .as_deref()
                            .or(agent.agent.as_deref())
                            .or_else(|| pane.and_then(|pane| pane.agent.as_deref()))
                            .unwrap_or(&agent.pane.resource),
                        128,
                    ),
                    workspace,
                    status,
                    interactive_ready: agent.interactive_ready.unwrap_or(false),
                };
                current.insert(observation.pane.clone(), observation);
            }
        }

        let mut changed = false;
        for observation in current.values() {
            if let Some(previous) = self.observations.get(&observation.pane).cloned() {
                if let Some(kind) = transition_kind(&previous, observation) {
                    self.push_event(observation, kind);
                    changed = true;
                }
                if &previous != observation {
                    self.observations
                        .insert(observation.pane.clone(), observation.clone());
                    changed = true;
                }
            } else {
                self.observations
                    .insert(observation.pane.clone(), observation.clone());
                changed = true;
            }
        }

        let disappeared = self
            .observations
            .keys()
            .filter(|pane| {
                live_sessions.contains(&pane.target_session()) && !current.contains_key(*pane)
            })
            .cloned()
            .collect::<Vec<_>>();
        for pane in disappeared {
            if let Some(observation) = self.observations.remove(&pane) {
                self.push_event(&observation, AttentionEventKind::Disappeared);
                changed = true;
            }
        }
        let removed_sessions = self
            .observations
            .keys()
            .filter(|pane| !known_sessions.contains(&pane.target_session()))
            .cloned()
            .collect::<Vec<_>>();
        for pane in removed_sessions {
            self.observations.remove(&pane);
            changed = true;
        }
        changed
    }

    /// Adopt a history observed elsewhere.
    ///
    /// A client does not derive attention: the daemon owns the durable index,
    /// because two processes deriving it would number their events
    /// independently and write the same file over each other. What a client
    /// keeps is a mirror, and this is how it is seeded and resynchronized.
    pub fn mirror(events: Vec<AttentionEvent>) -> Self {
        let next_event_id = events
            .iter()
            .map(|event| event.id.saturating_add(1))
            .max()
            .unwrap_or_default();
        Self {
            next_event_id,
            observations: BTreeMap::new(),
            events,
        }
    }

    /// Apply one event observed elsewhere, replacing any earlier copy of it.
    /// Returns whether this was new, so a client can tell an arrival from a
    /// repeat without comparing histories.
    pub fn apply(&mut self, event: AttentionEvent) -> bool {
        if let Some(existing) = self.events.iter_mut().find(|held| held.id == event.id) {
            *existing = event;
            return false;
        }
        self.next_event_id = self.next_event_id.max(event.id.saturating_add(1));
        self.events.push(event);
        self.events.sort_by_key(|event| event.id);
        if self.events.len() > MAX_ATTENTION_EVENTS {
            self.events
                .drain(..self.events.len().saturating_sub(MAX_ATTENTION_EVENTS));
        }
        true
    }

    pub fn events(&self) -> impl DoubleEndedIterator<Item = &AttentionEvent> {
        self.events.iter()
    }

    pub fn unread_count(&self) -> usize {
        self.events.iter().filter(|event| event.unread).count()
    }

    pub fn has_unread_for_pane(&self, pane: &PaneId) -> bool {
        self.events
            .iter()
            .any(|event| event.unread && &event.pane == pane)
    }

    pub fn mark_seen_for_pane(&mut self, pane: &PaneId) -> bool {
        let mut changed = false;
        for event in &mut self.events {
            if event.unread && &event.pane == pane {
                event.unread = false;
                changed = true;
            }
        }
        changed
    }

    pub fn mark_all_seen(&mut self) -> bool {
        let mut changed = false;
        for event in &mut self.events {
            if event.unread {
                event.unread = false;
                changed = true;
            }
        }
        changed
    }

    pub fn clear_seen(&mut self) -> bool {
        let before = self.events.len();
        self.events.retain(|event| event.unread);
        before != self.events.len()
    }

    fn push_event(&mut self, observation: &AgentObservation, kind: AttentionEventKind) {
        let id = self.next_event_id;
        self.next_event_id = self.next_event_id.saturating_add(1);
        self.events.push(AttentionEvent {
            id,
            pane: observation.pane.clone(),
            agent: observation.agent.clone(),
            workspace: observation.workspace.clone(),
            status: observation.status.clone(),
            kind,
            occurred_at_ms: unix_time_ms(),
            unread: true,
        });
        if self.events.len() > MAX_ATTENTION_EVENTS {
            self.events
                .drain(..self.events.len().saturating_sub(MAX_ATTENTION_EVENTS));
        }
    }
}

fn transition_kind(
    previous: &AgentObservation,
    current: &AgentObservation,
) -> Option<AttentionEventKind> {
    let previous_phase = agent_phase(&previous.status, previous.interactive_ready);
    let current_phase = agent_phase(&current.status, current.interactive_ready);
    if previous_phase == current_phase {
        return (current.interactive_ready && !previous.interactive_ready)
            .then_some(AttentionEventKind::NeedsAttention);
    }
    Some(match current_phase {
        AgentPhase::Attention => AttentionEventKind::NeedsAttention,
        AgentPhase::Working => AttentionEventKind::Working,
        AgentPhase::Idle
            if matches!(previous_phase, AgentPhase::Attention | AgentPhase::Working) =>
        {
            AttentionEventKind::Completed
        }
        AgentPhase::Idle | AgentPhase::Unknown => AttentionEventKind::StatusChanged,
    })
}

pub(crate) fn agent_phase(status: &str, interactive_ready: bool) -> AgentPhase {
    if interactive_ready
        || matches!(
            status.to_ascii_lowercase().as_str(),
            "blocked" | "waiting" | "waiting_for_input" | "needs_input" | "ready"
        )
    {
        AgentPhase::Attention
    } else if matches!(
        status.to_ascii_lowercase().as_str(),
        "working" | "running" | "busy" | "active"
    ) {
        AgentPhase::Working
    } else if matches!(
        status.to_ascii_lowercase().as_str(),
        "idle" | "completed" | "done"
    ) {
        AgentPhase::Idle
    } else {
        AgentPhase::Unknown
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub(crate) fn bounded_metadata(value: &str, maximum_characters: usize) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(maximum_characters)
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedAttention {
    version: u32,
    next_event_id: u64,
    observations: Vec<AgentObservation>,
    events: Vec<AttentionEvent>,
}

impl From<&AttentionIndex> for PersistedAttention {
    fn from(index: &AttentionIndex) -> Self {
        Self {
            version: ATTENTION_STATE_VERSION,
            next_event_id: index.next_event_id,
            observations: index.observations.values().cloned().collect(),
            events: index.events.clone(),
        }
    }
}

impl TryFrom<PersistedAttention> for AttentionIndex {
    type Error = anyhow::Error;

    fn try_from(persisted: PersistedAttention) -> Result<Self> {
        if persisted.version != ATTENTION_STATE_VERSION {
            bail!("persisted attention state has an unsupported version");
        }
        if persisted.observations.len() > MAX_AGENT_OBSERVATIONS {
            bail!("persisted attention state has too many agent observations");
        }
        if persisted.events.len() > MAX_ATTENTION_EVENTS {
            bail!("persisted attention state has too many events");
        }
        let mut observations = BTreeMap::new();
        for observation in persisted.observations {
            if observations
                .insert(observation.pane.clone(), observation)
                .is_some()
            {
                bail!("persisted attention state has duplicate agent observations");
            }
        }
        let next_event_id = persisted
            .events
            .iter()
            .map(|event| event.id)
            .max()
            .map_or(persisted.next_event_id, |id| {
                persisted.next_event_id.max(id.saturating_add(1))
            });
        Ok(Self {
            next_event_id,
            observations,
            events: persisted.events,
        })
    }
}

#[derive(Debug, Clone)]
pub struct AttentionStore {
    path: PathBuf,
}

impl AttentionStore {
    pub fn discover() -> Result<Self> {
        let root = if let Some(root) = env::var_os("XDG_STATE_HOME") {
            PathBuf::from(root)
        } else {
            let home: OsString = env::var_os("HOME").context(
                "XDG_STATE_HOME or HOME is required to persist Super-Herdr attention state",
            )?;
            PathBuf::from(home).join(".local/state")
        };
        Ok(Self {
            path: root.join("super-herdr/attention-state.json"),
        })
    }

    /// Use an explicit path instead of the discovered one. The daemon needs
    /// this so a test never writes into the running user's real history.
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Result<AttentionIndex> {
        let metadata = match fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AttentionIndex::default());
            }
            Err(error) => return Err(error).context("failed to inspect attention state"),
        };
        if metadata.len() > MAX_ATTENTION_STATE_BYTES {
            bail!("persisted attention state exceeds the size limit");
        }
        let bytes = fs::read(&self.path).context("failed to read attention state")?;
        let persisted: PersistedAttention =
            serde_json::from_slice(&bytes).context("persisted attention state is invalid")?;
        persisted.try_into()
    }

    pub fn save(&self, index: &AttentionIndex) -> Result<()> {
        let directory = self
            .path
            .parent()
            .context("persisted attention state path has no parent directory")?;
        fs::create_dir_all(directory).context("failed to create the attention state directory")?;
        set_directory_permissions(directory)?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".attention-state-")
            .tempfile_in(directory)
            .context("failed to create a temporary attention state file")?;
        set_file_permissions(temporary.path())?;
        serde_json::to_writer(&mut temporary, &PersistedAttention::from(index))
            .context("failed to encode attention state")?;
        temporary
            .write_all(b"\n")
            .context("failed to finish attention state")?;
        if temporary
            .as_file()
            .metadata()
            .context("failed to inspect encoded attention state")?
            .len()
            > MAX_ATTENTION_STATE_BYTES
        {
            bail!("encoded attention state exceeds the size limit");
        }
        temporary
            .as_file()
            .sync_all()
            .context("failed to synchronize attention state")?;
        temporary
            .persist(&self.path)
            .context("failed to atomically replace attention state")?;
        Ok(())
    }
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .context("failed to secure the attention state directory")
}

#[cfg(not(unix))]
fn set_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .context("failed to secure the attention state file")
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod mirror_tests {
    use super::{AttentionEvent, AttentionEventKind, AttentionIndex, MAX_ATTENTION_EVENTS};
    use crate::model::PaneId;

    fn event(id: u64, unread: bool) -> AttentionEvent {
        AttentionEvent {
            id,
            pane: PaneId::new("host-a", "dev", "w1:p1"),
            agent: "claude".to_owned(),
            workspace: "review".to_owned(),
            status: "waiting".to_owned(),
            kind: AttentionEventKind::NeedsAttention,
            occurred_at_ms: id,
            unread,
        }
    }

    #[test]
    fn a_mirror_holds_what_it_was_given_without_deriving_anything() {
        let index = AttentionIndex::mirror(vec![event(4, true), event(5, false)]);

        assert_eq!(index.events().count(), 2);
        assert_eq!(index.unread_count(), 1);
        // The next identifier follows the history, so a mirror that later
        // derives nothing still cannot reuse an identifier it has seen.
        assert_eq!(index.next_event_id, 6);
    }

    #[test]
    fn applying_the_same_event_twice_updates_rather_than_duplicates() {
        let mut index = AttentionIndex::mirror(Vec::new());

        assert!(index.apply(event(1, true)));
        assert!(!index.apply(event(1, false)));

        assert_eq!(index.events().count(), 1);
        assert_eq!(index.unread_count(), 0, "the later copy wins");
    }

    #[test]
    fn a_mirror_stays_ordered_and_bounded() {
        let mut index = AttentionIndex::mirror(Vec::new());
        // Arriving out of order is normal: a history and a live event can race.
        index.apply(event(9, true));
        index.apply(event(2, true));

        assert_eq!(
            index.events().map(|event| event.id).collect::<Vec<_>>(),
            vec![2, 9]
        );

        for id in 100..(100 + MAX_ATTENTION_EVENTS as u64 + 10) {
            index.apply(event(id, true));
        }
        assert_eq!(index.events().count(), MAX_ATTENTION_EVENTS);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use serde_json::json;

    use super::{AttentionEventKind, AttentionIndex, AttentionStore};
    use crate::model::{PaneId, TargetSession};
    use crate::state::{
        FederationState, NormalizedSnapshot, TargetConnectionState, TargetRuntimeState,
        TargetUpdateMode,
    };

    #[test]
    fn records_transitions_once_and_marks_qualified_panes_seen() {
        let mut index = AttentionIndex::default();
        let working = state_with_agent("working", false);
        assert!(index.observe(&working));
        assert_eq!(index.events().count(), 0);

        let blocked = state_with_agent("blocked", false);
        assert!(index.observe(&blocked));
        assert!(!index.observe(&blocked));
        let events = index.events().collect::<Vec<_>>();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, AttentionEventKind::NeedsAttention);
        assert_eq!(index.unread_count(), 1);

        let pane = PaneId::new("host-a", "work", "w1:p1");
        assert!(index.mark_seen_for_pane(&pane));
        assert_eq!(index.unread_count(), 0);
        assert!(!index.mark_seen_for_pane(&pane));
    }

    #[test]
    fn disconnects_do_not_create_disappearance_events() {
        let mut index = AttentionIndex::default();
        assert!(index.observe(&state_with_agent("working", false)));

        let disconnected = state_without_agent(TargetConnectionState::Backoff { attempt: 1 });
        assert!(!index.observe(&disconnected));
        assert_eq!(index.events().count(), 0);

        let live_without_agent = state_without_agent(TargetConnectionState::Live);
        assert!(index.observe(&live_without_agent));
        assert_eq!(
            index.events().next().map(|event| event.kind),
            Some(AttentionEventKind::Disappeared)
        );
    }

    #[test]
    fn atomically_round_trips_metadata_only_attention_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("attention-state.json");
        let store = AttentionStore::at(path.clone());
        let mut index = AttentionIndex::default();
        index.observe(&state_with_agent("working", false));
        index.observe(&state_with_agent("blocked", true));

        store.save(&index).unwrap();
        let mut restored = store.load().unwrap();
        assert_eq!(restored.unread_count(), 1);
        assert_eq!(restored.events().count(), 1);
        assert!(!restored.observe(&state_with_agent("blocked", true)));
        assert_eq!(restored.events().count(), 1);
        let encoded = fs::read_to_string(path).unwrap();
        assert!(!encoded.contains("terminal"));
        assert!(!encoded.contains("clipboard"));
    }

    fn state_with_agent(status: &str, interactive_ready: bool) -> FederationState {
        let key = TargetSession::new("host-a", "work");
        let snapshot = NormalizedSnapshot::from_value(
            &key,
            &json!({
                "workspaces": [{"workspace_id": "w1", "label": "compiler"}],
                "panes": [{
                    "pane_id": "w1:p1",
                    "workspace_id": "w1",
                    "agent": "builder",
                    "agent_status": status
                }],
                "agents": [{
                    "pane_id": "w1:p1",
                    "name": "builder",
                    "agent_status": status,
                    "interactive_ready": interactive_ready
                }]
            }),
        );
        state_with_snapshot(key, TargetConnectionState::Live, snapshot)
    }

    fn state_without_agent(connection: TargetConnectionState) -> FederationState {
        let key = TargetSession::new("host-a", "work");
        let snapshot = NormalizedSnapshot::from_value(
            &key,
            &json!({"workspaces": [], "panes": [], "agents": []}),
        );
        state_with_snapshot(key, connection, snapshot)
    }

    fn state_with_snapshot(
        key: TargetSession,
        connection: TargetConnectionState,
        snapshot: NormalizedSnapshot,
    ) -> FederationState {
        let mut state = FederationState::default();
        state.targets.insert(
            key.clone(),
            TargetRuntimeState {
                key,
                endpoint: "test".to_owned(),
                connection,
                update_mode: TargetUpdateMode::Events,
                event_error: None,
                connection_generation: 1,
                selected_herdr_bin: Some("herdr".to_owned()),
                snapshot: Some(Arc::new(snapshot)),
                last_error: None,
                last_success: None,
                retry_at: None,
            },
        );
        state
    }
}
