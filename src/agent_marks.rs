//! What a person marked on an agent card: pins, mutes, and snoozes.
//!
//! These are Super-Herdr's own opinions about someone's inbox, and they are
//! deliberately not Herdr's. Pinning an agent does not rename, move, focus, or
//! otherwise touch the pane it runs in; muting one does not stop it working or
//! change what its target reports. Nothing here crosses back to a host, so a
//! mark can never be the reason a session behaves differently.
//!
//! They are keyed by [`AgentId`], the same qualified identity the cards use, so
//! pinning an agent on one host cannot silence a same-named agent on another.
//! The file is bounded in every direction — how many agents may be marked, how
//! large it may be on disk, how far ahead a snooze may reach — because it is
//! written by a request from a paired device and a device should not be able to
//! grow a file on somebody's desktop without limit.
//!
//! A snooze is stored as a deadline the daemon computed from its own clock. A
//! client asks for a duration, never a moment: a phone with a wrong clock would
//! otherwise be able to snooze an agent until next year, and the daemon has no
//! way to tell that from a deliberate request.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::attention::{set_directory_permissions, set_file_permissions};
use crate::model::AgentId;

const AGENT_MARKS_VERSION: u32 = 1;
const MAX_AGENT_MARKS_BYTES: u64 = 256 * 1024;

/// How many agents may carry a mark at once.
///
/// Marks are per-agent and a person only has so many agents; the cap is here
/// so a paired device cannot make the file grow forever, and it is enforced by
/// forgetting the least recently touched mark rather than by refusing the
/// request, which would strand a person who genuinely reached the limit.
const MAX_MARKED_AGENTS: usize = 512;

/// The longest a snooze may run. Long enough to cover a working day, short
/// enough that a forgotten snooze surfaces again by itself.
pub const MAX_SNOOZE_MINUTES: u32 = 24 * 60;

/// One agent's marks. All-default means "not marked", which is why an unmarked
/// agent costs nothing to describe and nothing to store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMarkState {
    pub pinned: bool,
    pub muted: bool,
    /// When a snooze ends, as daemon-computed wall-clock milliseconds. Absent
    /// means not snoozed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snoozed_until_ms: Option<u64>,
}

impl AgentMarkState {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }

    pub fn snoozed_at(&self, now_ms: u64) -> bool {
        self.snoozed_until_ms
            .is_some_and(|deadline| deadline > now_ms)
    }

    /// Whether this agent should stop competing for the top of the inbox.
    pub fn quiet_at(&self, now_ms: u64) -> bool {
        self.muted || self.snoozed_at(now_ms)
    }
}

/// What a client asks for. A duration rather than a deadline, and a boolean
/// rather than a toggle, so the same request twice reaches the same state
/// instead of undoing itself when one of them is retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentMarkRequest {
    Pin {
        pinned: bool,
    },
    Mute {
        muted: bool,
    },
    /// `None` clears a snooze. A duration beyond [`MAX_SNOOZE_MINUTES`] is
    /// clamped rather than refused: the person meant "for a long time", and
    /// the bound is the daemon's business rather than an error to explain.
    Snooze {
        minutes: Option<u32>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentMarks {
    marks: BTreeMap<AgentId, Marked>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Marked {
    state: AgentMarkState,
    /// When this mark was last set, used only to decide what to forget first
    /// when the cap is reached.
    touched_ms: u64,
}

impl AgentMarks {
    /// Apply one request. Returns whether anything changed, so the daemon can
    /// skip a persist and a republish for a request that asked for the state
    /// the agent was already in.
    pub fn apply(&mut self, agent: &AgentId, request: AgentMarkRequest, now_ms: u64) -> bool {
        let mut state = self.state(agent);
        match request {
            AgentMarkRequest::Pin { pinned } => state.pinned = pinned,
            AgentMarkRequest::Mute { muted } => state.muted = muted,
            AgentMarkRequest::Snooze { minutes } => {
                state.snoozed_until_ms = minutes.map(|minutes| {
                    let bounded = u64::from(minutes.min(MAX_SNOOZE_MINUTES));
                    now_ms.saturating_add(bounded.saturating_mul(60_000))
                });
            }
        }
        if state == self.state(agent) {
            return false;
        }
        if state.is_default() {
            self.marks.remove(agent);
        } else {
            self.marks.insert(
                agent.clone(),
                Marked {
                    state,
                    touched_ms: now_ms,
                },
            );
            self.enforce_cap();
        }
        true
    }

    pub fn state(&self, agent: &AgentId) -> AgentMarkState {
        self.marks
            .get(agent)
            .map(|marked| marked.state)
            .unwrap_or_default()
    }

    /// Drop snoozes that have run out, so an expired one stops being carried
    /// in the file and in every projection built from it. Returns whether
    /// anything changed.
    pub fn expire(&mut self, now_ms: u64) -> bool {
        let expired = self
            .marks
            .iter()
            .filter(|(_, marked)| {
                marked
                    .state
                    .snoozed_until_ms
                    .is_some_and(|deadline| deadline <= now_ms)
            })
            .map(|(agent, _)| agent.clone())
            .collect::<Vec<_>>();
        if expired.is_empty() {
            return false;
        }
        for agent in expired {
            let Some(marked) = self.marks.get_mut(&agent) else {
                continue;
            };
            marked.state.snoozed_until_ms = None;
            if marked.state.is_default() {
                self.marks.remove(&agent);
            }
        }
        true
    }

    pub fn is_empty(&self) -> bool {
        self.marks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.marks.len()
    }

    fn enforce_cap(&mut self) {
        while self.marks.len() > MAX_MARKED_AGENTS {
            let Some(oldest) = self
                .marks
                .iter()
                .min_by_key(|(agent, marked)| (marked.touched_ms, (*agent).clone()))
                .map(|(agent, _)| agent.clone())
            else {
                break;
            };
            self.marks.remove(&oldest);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedMark {
    agent: AgentId,
    #[serde(flatten)]
    state: AgentMarkState,
    touched_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedMarks {
    version: u32,
    marks: Vec<PersistedMark>,
}

impl From<&AgentMarks> for PersistedMarks {
    fn from(marks: &AgentMarks) -> Self {
        Self {
            version: AGENT_MARKS_VERSION,
            marks: marks
                .marks
                .iter()
                .map(|(agent, marked)| PersistedMark {
                    agent: agent.clone(),
                    state: marked.state,
                    touched_ms: marked.touched_ms,
                })
                .collect(),
        }
    }
}

impl TryFrom<PersistedMarks> for AgentMarks {
    type Error = anyhow::Error;

    fn try_from(persisted: PersistedMarks) -> Result<Self> {
        if persisted.version != AGENT_MARKS_VERSION {
            bail!("persisted agent marks have an unsupported version");
        }
        if persisted.marks.len() > MAX_MARKED_AGENTS {
            bail!("persisted agent marks name too many agents");
        }
        let mut marks = BTreeMap::new();
        for held in persisted.marks {
            if marks
                .insert(
                    held.agent,
                    Marked {
                        state: held.state,
                        touched_ms: held.touched_ms,
                    },
                )
                .is_some()
            {
                bail!("persisted agent marks name one agent twice");
            }
        }
        Ok(Self { marks })
    }
}

#[derive(Debug, Clone)]
pub struct AgentMarkStore {
    path: PathBuf,
}

impl AgentMarkStore {
    pub fn discover() -> Result<Self> {
        let root = if let Some(root) = env::var_os("XDG_STATE_HOME") {
            PathBuf::from(root)
        } else {
            let home: OsString = env::var_os("HOME")
                .context("XDG_STATE_HOME or HOME is required to persist Super-Herdr agent marks")?;
            PathBuf::from(home).join(".local/state")
        };
        Ok(Self {
            path: root.join("super-herdr/agent-marks.json"),
        })
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Result<AgentMarks> {
        let metadata = match fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AgentMarks::default());
            }
            Err(error) => return Err(error).context("failed to inspect agent marks"),
        };
        if metadata.len() > MAX_AGENT_MARKS_BYTES {
            bail!("persisted agent marks exceed the size limit");
        }
        let bytes = fs::read(&self.path).context("failed to read agent marks")?;
        let persisted: PersistedMarks =
            serde_json::from_slice(&bytes).context("persisted agent marks are invalid")?;
        persisted.try_into()
    }

    pub fn save(&self, marks: &AgentMarks) -> Result<()> {
        let directory = self
            .path
            .parent()
            .context("agent marks path has no parent directory")?;
        fs::create_dir_all(directory).context("failed to create the agent marks directory")?;
        set_directory_permissions(directory)?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".agent-marks-")
            .tempfile_in(directory)
            .context("failed to create a temporary agent marks file")?;
        set_file_permissions(temporary.path())?;
        serde_json::to_writer(&mut temporary, &PersistedMarks::from(marks))
            .context("failed to encode agent marks")?;
        temporary
            .write_all(b"\n")
            .context("failed to finish agent marks")?;
        if temporary
            .as_file()
            .metadata()
            .context("failed to inspect encoded agent marks")?
            .len()
            > MAX_AGENT_MARKS_BYTES
        {
            bail!("encoded agent marks exceed the size limit");
        }
        temporary
            .as_file()
            .sync_all()
            .context("failed to synchronize agent marks")?;
        temporary
            .persist(&self.path)
            .context("failed to atomically replace agent marks")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AgentMarkRequest, AgentMarkStore, AgentMarks, MAX_MARKED_AGENTS, MAX_SNOOZE_MINUTES,
    };
    use crate::model::AgentId;

    const NOW: u64 = 1_700_000_000_000;

    #[test]
    fn marks_one_agent_without_touching_its_namesake_on_another_host() {
        let mut marks = AgentMarks::default();
        let here = AgentId::new("host-a", "work", "p1");
        let there = AgentId::new("host-b", "work", "p1");

        assert!(marks.apply(&here, AgentMarkRequest::Pin { pinned: true }, NOW));

        assert!(marks.state(&here).pinned);
        assert!(!marks.state(&there).pinned);
    }

    #[test]
    fn reports_a_request_that_changes_nothing() {
        let mut marks = AgentMarks::default();
        let agent = AgentId::new("host-a", "work", "p1");

        assert!(marks.apply(&agent, AgentMarkRequest::Mute { muted: true }, NOW));
        assert!(!marks.apply(&agent, AgentMarkRequest::Mute { muted: true }, NOW));
    }

    #[test]
    fn forgets_an_agent_whose_marks_are_all_cleared() {
        let mut marks = AgentMarks::default();
        let agent = AgentId::new("host-a", "work", "p1");
        marks.apply(&agent, AgentMarkRequest::Pin { pinned: true }, NOW);

        marks.apply(&agent, AgentMarkRequest::Pin { pinned: false }, NOW);

        assert!(
            marks.is_empty(),
            "an unmarked agent should cost nothing to store"
        );
    }

    #[test]
    fn turns_a_requested_duration_into_a_deadline_the_daemon_owns() {
        let mut marks = AgentMarks::default();
        let agent = AgentId::new("host-a", "work", "p1");

        marks.apply(&agent, AgentMarkRequest::Snooze { minutes: Some(30) }, NOW);

        assert_eq!(
            marks.state(&agent).snoozed_until_ms,
            Some(NOW + 30 * 60_000)
        );
        assert!(marks.state(&agent).snoozed_at(NOW));
        assert!(!marks.state(&agent).snoozed_at(NOW + 31 * 60_000));
    }

    #[test]
    fn clamps_a_snooze_that_reaches_further_than_a_day() {
        let mut marks = AgentMarks::default();
        let agent = AgentId::new("host-a", "work", "p1");

        marks.apply(
            &agent,
            AgentMarkRequest::Snooze {
                minutes: Some(u32::MAX),
            },
            NOW,
        );

        assert_eq!(
            marks.state(&agent).snoozed_until_ms,
            Some(NOW + u64::from(MAX_SNOOZE_MINUTES) * 60_000),
            "a wrong clock on a phone must not silence an agent for a year"
        );
    }

    #[test]
    fn expires_a_snooze_that_has_run_out_and_keeps_the_rest() {
        let mut marks = AgentMarks::default();
        let expiring = AgentId::new("host-a", "work", "p1");
        let pinned = AgentId::new("host-a", "work", "p2");
        marks.apply(
            &expiring,
            AgentMarkRequest::Snooze { minutes: Some(5) },
            NOW,
        );
        marks.apply(&pinned, AgentMarkRequest::Pin { pinned: true }, NOW);

        assert!(!marks.expire(NOW));
        assert!(marks.expire(NOW + 5 * 60_000));

        assert!(marks.state(&expiring).snoozed_until_ms.is_none());
        assert!(
            marks.is_empty() || marks.state(&pinned).pinned,
            "expiring one mark must not clear another"
        );
        assert_eq!(marks.len(), 1);
    }

    #[test]
    fn keeps_a_snooze_alongside_a_pin_on_the_same_agent() {
        let mut marks = AgentMarks::default();
        let agent = AgentId::new("host-a", "work", "p1");
        marks.apply(&agent, AgentMarkRequest::Pin { pinned: true }, NOW);

        marks.apply(&agent, AgentMarkRequest::Snooze { minutes: Some(5) }, NOW);
        marks.expire(NOW + 5 * 60_000);

        assert!(marks.state(&agent).pinned);
        assert!(!marks.state(&agent).snoozed_at(NOW + 5 * 60_000));
    }

    #[test]
    fn forgets_the_least_recently_marked_agent_rather_than_refusing_a_request() {
        let mut marks = AgentMarks::default();
        for index in 0..=MAX_MARKED_AGENTS {
            let agent = AgentId::new("host-a", "work", &format!("p{index}"));
            marks.apply(
                &agent,
                AgentMarkRequest::Pin { pinned: true },
                NOW + index as u64,
            );
        }

        assert_eq!(marks.len(), MAX_MARKED_AGENTS);
        assert!(!marks.state(&AgentId::new("host-a", "work", "p0")).pinned);
        assert!(
            marks
                .state(&AgentId::new(
                    "host-a",
                    "work",
                    &format!("p{MAX_MARKED_AGENTS}")
                ))
                .pinned,
            "the newest request is the one that survives"
        );
    }

    #[test]
    fn round_trips_marks_through_a_bounded_file() {
        let directory = tempfile::tempdir().unwrap();
        let store = AgentMarkStore::at(directory.path().join("agent-marks.json"));
        let mut marks = AgentMarks::default();
        let agent = AgentId::new("host-a", "work", "p1");
        marks.apply(&agent, AgentMarkRequest::Pin { pinned: true }, NOW);
        marks.apply(&agent, AgentMarkRequest::Snooze { minutes: Some(10) }, NOW);

        store.save(&marks).unwrap();
        let restored = store.load().unwrap();

        assert_eq!(restored, marks);
        assert!(restored.state(&agent).pinned);
        assert_eq!(
            restored.state(&agent).snoozed_until_ms,
            Some(NOW + 10 * 60_000)
        );
    }

    #[test]
    fn treats_a_missing_file_as_nothing_marked() {
        let directory = tempfile::tempdir().unwrap();
        let store = AgentMarkStore::at(directory.path().join("absent.json"));

        assert!(store.load().unwrap().is_empty());
    }
}
