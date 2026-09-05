//! Names, favourites and jump slots that belong to this Super-Herdr and to
//! nobody else's Herdr.
//!
//! A person running agents on five machines ends up with `w1`, `w3` and `wD`
//! meaning "the compiler one", "the flaky one" and "the customer's". Herdr can
//! be told to rename a workspace, but that changes it for everybody using that
//! host and is somebody else's decision. So these are local: a label this
//! installation shows, a set of destinations worth one action, and nine slots
//! for the ones somebody jumps to constantly.
//!
//! Three rules, and the third is the whole difficulty:
//!
//! * **An alias is never an identity.** Everything is keyed by a qualified id
//!   and looked up by it. A label is what gets drawn, and nothing routes,
//!   resolves or matches by it — which is why two hosts can both have "build"
//!   without anything colliding.
//! * **Nothing here renames anything on a host.** Herdr's own rename is a
//!   separate, explicit action; this never calls it, and a local alias sitting
//!   over a Herdr label leaves that label alone.
//! * **A reused id must not inherit a name.** Herdr's ids are server-local and
//!   can come back: delete `w1` and the next workspace may be `w1` again. An
//!   alias keyed only by that id would silently move onto a different
//!   workspace, and the person would see a name they trust over something they
//!   have never looked at. So an alias also records what the resource was
//!   called when it was named. When that no longer matches, the alias is
//!   *suspended* rather than applied or deleted: shown as needing confirmation,
//!   because guessing either way is worse than asking.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::attention::{set_directory_permissions, set_file_permissions};
use crate::model::{AgentId, PaneId, TabId, TargetSession, WorkspaceId};

const ALIASES_VERSION: u32 = 1;
const MAX_ALIASES_BYTES: u64 = 256 * 1024;
const MAX_ALIASES: usize = 512;
const MAX_FAVOURITES: usize = 64;
pub const MAX_ALIAS_CHARACTERS: usize = 48;
/// Slots are the digits somebody can actually press.
pub const JUMP_SLOTS: std::ops::RangeInclusive<u8> = 1..=9;

/// Anything that can be named, favourited or jumped to.
///
/// One enum rather than five stores, because the rules — bounded, qualified,
/// never routing — are the same for all of them, and a person renaming a pane
/// should not discover that panes were the kind nobody implemented.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResourceRef {
    Target(TargetSession),
    Workspace(WorkspaceId),
    Tab(TabId),
    Pane(PaneId),
    Agent(AgentId),
}

impl std::fmt::Display for ResourceRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Target(id) => write!(formatter, "{id}"),
            Self::Workspace(id) => write!(formatter, "{id}"),
            Self::Tab(id) => write!(formatter, "{id}"),
            Self::Pane(id) => write!(formatter, "{id}"),
            Self::Agent(id) => write!(formatter, "{id}"),
        }
    }
}

/// A local name, and what the resource was called when it was given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alias {
    pub label: String,
    /// What the host called it at the time. Kept as evidence rather than as
    /// display: it is how a reused id is told apart from the resource somebody
    /// actually named.
    pub observed: String,
}

/// Whether an alias may be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AliasState {
    /// The resource still looks like the one that was named.
    Current,
    /// The host now calls it something else. It may have been renamed, or the
    /// id may have been reused by something entirely different, and this
    /// cannot tell which.
    Suspended,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Aliases {
    names: BTreeMap<ResourceRef, Alias>,
    favourites: Vec<ResourceRef>,
    slots: BTreeMap<u8, ResourceRef>,
}

impl Aliases {
    /// Name something, recording what it is called now.
    pub fn set(&mut self, resource: ResourceRef, label: &str, observed: &str) -> Result<()> {
        let label = label.trim();
        if label.is_empty() || label.chars().count() > MAX_ALIAS_CHARACTERS {
            bail!("a name must be 1 to {MAX_ALIAS_CHARACTERS} characters");
        }
        if label.chars().any(char::is_control) {
            bail!("a name must not contain control characters");
        }
        if self.names.len() >= MAX_ALIASES && !self.names.contains_key(&resource) {
            bail!("at most {MAX_ALIASES} names can be kept");
        }
        self.names.insert(
            resource,
            Alias {
                label: label.to_owned(),
                observed: observed.to_owned(),
            },
        );
        Ok(())
    }

    pub fn clear(&mut self, resource: &ResourceRef) -> bool {
        self.names.remove(resource).is_some()
    }

    /// The name to show, and whether it can be trusted.
    ///
    /// `observed` is what the host calls the resource right now. A caller that
    /// does not know passes what it has; a mismatch suspends rather than
    /// retargets, because a name shown over the wrong resource is worse than
    /// no name at all.
    pub fn resolve(&self, resource: &ResourceRef, observed: &str) -> Option<(&str, AliasState)> {
        let alias = self.names.get(resource)?;
        let state = if alias.observed == observed {
            AliasState::Current
        } else {
            AliasState::Suspended
        };
        Some((alias.label.as_str(), state))
    }

    /// Accept that a suspended alias still belongs to this resource.
    ///
    /// The person looked and said so. Nothing else can: the host offers no way
    /// to tell a rename from a reuse.
    pub fn confirm(&mut self, resource: &ResourceRef, observed: &str) -> bool {
        let Some(alias) = self.names.get_mut(resource) else {
            return false;
        };
        alias.observed = observed.to_owned();
        true
    }

    pub fn favourite(&mut self, resource: ResourceRef) -> Result<bool> {
        if let Some(index) = self.favourites.iter().position(|held| held == &resource) {
            self.favourites.remove(index);
            return Ok(false);
        }
        if self.favourites.len() >= MAX_FAVOURITES {
            bail!("at most {MAX_FAVOURITES} favourites can be kept");
        }
        self.favourites.push(resource);
        Ok(true)
    }

    pub fn is_favourite(&self, resource: &ResourceRef) -> bool {
        self.favourites.contains(resource)
    }

    pub fn favourites(&self) -> &[ResourceRef] {
        &self.favourites
    }

    /// Put something in a numbered slot, or clear one.
    ///
    /// A slot holds one thing: assigning over an occupied slot replaces it,
    /// because that is what pressing a number is for, and the same resource
    /// leaves whatever other slot it was in rather than answering to two.
    pub fn set_slot(&mut self, slot: u8, resource: Option<ResourceRef>) -> Result<()> {
        if !JUMP_SLOTS.contains(&slot) {
            bail!(
                "a jump slot must be between {} and {}",
                JUMP_SLOTS.start(),
                JUMP_SLOTS.end()
            );
        }
        match resource {
            Some(resource) => {
                self.slots.retain(|_, held| held != &resource);
                self.slots.insert(slot, resource);
            }
            None => {
                self.slots.remove(&slot);
            }
        }
        Ok(())
    }

    pub fn slot(&self, slot: u8) -> Option<&ResourceRef> {
        self.slots.get(&slot)
    }

    pub fn slots(&self) -> impl Iterator<Item = (u8, &ResourceRef)> {
        self.slots.iter().map(|(slot, resource)| (*slot, resource))
    }

    /// Every target any name, favourite or slot refers to.
    pub fn targets(&self) -> std::collections::BTreeSet<TargetSession> {
        self.names
            .keys()
            .chain(self.favourites.iter())
            .chain(self.slots.values())
            .map(target_of)
            .collect()
    }

    /// Forget everything about resources on a target that is no longer
    /// configured.
    ///
    /// Removing a target from the configuration is a decision; keeping names
    /// for its panes forever is not. Nothing is forgotten because a host was
    /// merely unreachable — that is not a decision anybody made.
    pub fn forget_target(&mut self, target: &TargetSession) -> bool {
        let before = self.names.len() + self.favourites.len() + self.slots.len();
        self.names.retain(|resource, _| !belongs(resource, target));
        self.favourites
            .retain(|resource| !belongs(resource, target));
        self.slots.retain(|_, resource| !belongs(resource, target));
        before != self.names.len() + self.favourites.len() + self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty() && self.favourites.is_empty() && self.slots.is_empty()
    }
}

fn target_of(resource: &ResourceRef) -> TargetSession {
    match resource {
        ResourceRef::Target(id) => id.clone(),
        ResourceRef::Workspace(id) => id.target_session(),
        ResourceRef::Tab(id) => id.target_session(),
        ResourceRef::Pane(id) => id.target_session(),
        ResourceRef::Agent(id) => id.target_session(),
    }
}

fn belongs(resource: &ResourceRef, target: &TargetSession) -> bool {
    &target_of(resource) == target
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedAliases {
    version: u32,
    names: Vec<(ResourceRef, Alias)>,
    favourites: Vec<ResourceRef>,
    slots: Vec<(u8, ResourceRef)>,
}

impl From<&Aliases> for PersistedAliases {
    fn from(aliases: &Aliases) -> Self {
        Self {
            version: ALIASES_VERSION,
            names: aliases
                .names
                .iter()
                .map(|(resource, alias)| (resource.clone(), alias.clone()))
                .collect(),
            favourites: aliases.favourites.clone(),
            slots: aliases
                .slots
                .iter()
                .map(|(slot, resource)| (*slot, resource.clone()))
                .collect(),
        }
    }
}

impl TryFrom<PersistedAliases> for Aliases {
    type Error = anyhow::Error;

    fn try_from(persisted: PersistedAliases) -> Result<Self> {
        if persisted.version != ALIASES_VERSION {
            bail!("persisted aliases have an unsupported version");
        }
        if persisted.names.len() > MAX_ALIASES || persisted.favourites.len() > MAX_FAVOURITES {
            bail!("persisted aliases hold more than the limits allow");
        }
        Ok(Self {
            names: persisted.names.into_iter().collect(),
            favourites: persisted.favourites,
            slots: persisted
                .slots
                .into_iter()
                .filter(|(slot, _)| JUMP_SLOTS.contains(slot))
                .collect(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct AliasStore {
    path: PathBuf,
}

impl AliasStore {
    pub fn discover() -> Result<Self> {
        let root = if let Some(root) = env::var_os("XDG_STATE_HOME") {
            PathBuf::from(root)
        } else {
            let home: OsString = env::var_os("HOME")
                .context("XDG_STATE_HOME or HOME is required to persist Super-Herdr aliases")?;
            PathBuf::from(home).join(".local/state")
        };
        Ok(Self {
            path: root.join("super-herdr/aliases.json"),
        })
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Result<Aliases> {
        let metadata = match fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Aliases::default());
            }
            Err(error) => return Err(error).context("failed to inspect aliases"),
        };
        if metadata.len() > MAX_ALIASES_BYTES {
            bail!("persisted aliases exceed the size limit");
        }
        let bytes = fs::read(&self.path).context("failed to read aliases")?;
        let persisted: PersistedAliases =
            serde_json::from_slice(&bytes).context("persisted aliases are invalid")?;
        persisted.try_into()
    }

    pub fn save(&self, aliases: &Aliases) -> Result<()> {
        let directory = self
            .path
            .parent()
            .context("the aliases path has no parent directory")?;
        fs::create_dir_all(directory).context("failed to create the aliases directory")?;
        set_directory_permissions(directory)?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".aliases-")
            .tempfile_in(directory)
            .context("failed to create a temporary aliases file")?;
        set_file_permissions(temporary.path())?;
        serde_json::to_writer(&mut temporary, &PersistedAliases::from(aliases))
            .context("failed to encode aliases")?;
        temporary
            .write_all(b"\n")
            .context("failed to finish aliases")?;
        temporary
            .as_file()
            .sync_all()
            .context("failed to synchronize aliases")?;
        temporary
            .persist(&self.path)
            .context("failed to atomically replace aliases")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{AliasState, AliasStore, Aliases, MAX_ALIAS_CHARACTERS, ResourceRef};
    use crate::model::{PaneId, TargetSession, WorkspaceId};

    fn workspace(target: &str, resource: &str) -> ResourceRef {
        ResourceRef::Workspace(WorkspaceId::new(target, "work", resource))
    }

    #[test]
    fn a_name_is_shown_and_never_routed_by() {
        let mut aliases = Aliases::default();
        let here = workspace("host-a", "w1");
        let there = workspace("host-b", "w1");
        aliases.set(here.clone(), "compiler", "w1").unwrap();

        assert_eq!(
            aliases.resolve(&here, "w1"),
            Some(("compiler", AliasState::Current))
        );
        assert_eq!(
            aliases.resolve(&there, "w1"),
            None,
            "two hosts may both call something w1 without sharing a name"
        );
    }

    #[test]
    fn an_id_that_came_back_as_something_else_does_not_inherit_the_name() {
        let mut aliases = Aliases::default();
        let workspace = workspace("host-a", "w1");
        aliases
            .set(workspace.clone(), "compiler", "toolchain")
            .unwrap();

        // The host now calls w1 something else: either it was renamed, or the
        // id was reused. Nothing here can tell which.
        let resolved = aliases.resolve(&workspace, "customer demo");

        assert_eq!(
            resolved,
            Some(("compiler", AliasState::Suspended)),
            "a name shown over the wrong resource is worse than no name at all"
        );
    }

    #[test]
    fn a_person_can_say_a_suspended_name_still_belongs() {
        let mut aliases = Aliases::default();
        let workspace = workspace("host-a", "w1");
        aliases
            .set(workspace.clone(), "compiler", "toolchain")
            .unwrap();

        assert!(aliases.confirm(&workspace, "toolchain v2"));

        assert_eq!(
            aliases.resolve(&workspace, "toolchain v2"),
            Some(("compiler", AliasState::Current))
        );
    }

    #[test]
    fn a_name_is_bounded_and_free_of_control_characters() {
        let mut aliases = Aliases::default();
        let workspace = workspace("host-a", "w1");

        assert!(aliases.set(workspace.clone(), "", "w1").is_err());
        assert!(
            aliases
                .set(
                    workspace.clone(),
                    &"x".repeat(MAX_ALIAS_CHARACTERS + 1),
                    "w1"
                )
                .is_err()
        );
        assert!(aliases.set(workspace.clone(), "two\nlines", "w1").is_err());
        assert!(aliases.set(workspace, "  compiler  ", "w1").is_ok());
    }

    #[test]
    fn a_favourite_toggles_and_a_slot_holds_one_thing() {
        let mut aliases = Aliases::default();
        let first = workspace("host-a", "w1");
        let second = workspace("host-a", "w2");

        assert!(aliases.favourite(first.clone()).unwrap());
        assert!(aliases.is_favourite(&first));
        assert!(!aliases.favourite(first.clone()).unwrap());
        assert!(!aliases.is_favourite(&first));

        aliases.set_slot(1, Some(first.clone())).unwrap();
        aliases.set_slot(2, Some(first.clone())).unwrap();

        assert_eq!(
            aliases.slot(1),
            None,
            "one resource does not answer to two slots"
        );
        assert_eq!(aliases.slot(2), Some(&first));
        aliases.set_slot(2, Some(second.clone())).unwrap();
        assert_eq!(
            aliases.slot(2),
            Some(&second),
            "a slot holds the newest thing put in it"
        );
        aliases.set_slot(2, None).unwrap();
        assert_eq!(aliases.slot(2), None);
        assert!(aliases.set_slot(0, Some(second)).is_err());
    }

    #[test]
    fn removing_a_target_from_the_configuration_forgets_its_names() {
        let mut aliases = Aliases::default();
        let gone = TargetSession::new("host-a", "work");
        aliases
            .set(workspace("host-a", "w1"), "compiler", "w1")
            .unwrap();
        aliases
            .set(
                ResourceRef::Pane(PaneId::new("host-b", "work", "w1:p1")),
                "kept",
                "p1",
            )
            .unwrap();
        aliases.favourite(workspace("host-a", "w2")).unwrap();
        aliases
            .set_slot(1, Some(workspace("host-a", "w2")))
            .unwrap();

        assert!(aliases.forget_target(&gone));

        assert_eq!(aliases.resolve(&workspace("host-a", "w1"), "w1"), None);
        assert!(aliases.slot(1).is_none());
        assert!(aliases.favourites().is_empty());
        assert!(
            aliases
                .resolve(
                    &ResourceRef::Pane(PaneId::new("host-b", "work", "w1:p1")),
                    "p1"
                )
                .is_some(),
            "another host's names are not somebody else's configuration change"
        );
    }

    #[test]
    fn names_favourites_and_slots_survive_a_restart() {
        let directory = tempfile::tempdir().unwrap();
        let store = AliasStore::at(directory.path().join("aliases.json"));
        let mut aliases = Aliases::default();
        let workspace = workspace("host-a", "w1");
        aliases
            .set(workspace.clone(), "compiler", "toolchain")
            .unwrap();
        aliases.favourite(workspace.clone()).unwrap();
        aliases.set_slot(3, Some(workspace.clone())).unwrap();

        store.save(&aliases).unwrap();
        let restored = store.load().unwrap();

        assert_eq!(restored, aliases);
        assert_eq!(
            restored.resolve(&workspace, "toolchain"),
            Some(("compiler", AliasState::Current)),
            "the evidence a reused id is caught by has to survive too"
        );
        assert!(restored.is_favourite(&workspace));
        assert_eq!(restored.slot(3), Some(&workspace));
    }

    #[test]
    fn a_missing_file_is_nothing_named() {
        let directory = tempfile::tempdir().unwrap();

        assert!(
            AliasStore::at(directory.path().join("absent.json"))
                .load()
                .unwrap()
                .is_empty()
        );
    }
}
