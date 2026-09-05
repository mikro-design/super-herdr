//! What is installed where, across every host, and what differs.
//!
//! Herdr installs plugins per host. Once somebody runs agents on three
//! machines, "which of them has the review plugin, and is it the same one?"
//! stops being answerable by looking, and the usual answer — install it
//! everywhere again and hope — is how versions drift apart quietly.
//!
//! Two things make this harder than diffing lists, and both are decisions
//! rather than details:
//!
//! * **A plugin id is server-local.** Herdr's `plugin_id` identifies a plugin
//!   *on one host*. Two hosts can name unrelated plugins the same thing, and
//!   the same plugin can be installed under different ids. Matching on it would
//!   report drift between things that were never the same plugin, and silence
//!   between two copies of one. So identity here is the *source* — where it was
//!   installed from — and the id stays qualified by the target it came from.
//! * **A plugin with no source has no identity to compare.** A linked local
//!   plugin exists only on the host it was linked on. It is reported as local
//!   to that host and never matched against anything, because a name is not
//!   evidence that two directories hold the same code.
//!
//! Nothing here installs anything. It produces a lockfile of what somebody has
//! decided they want and a plan of the commands that would get there — Herdr's
//! own `herdr plugin install` commands, because Herdr has an installer and
//! reimplementing it would be a second thing to keep correct.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::model::TargetSession;

/// How many plugins one host may report before the answer is refused.
pub const MAX_PLUGINS_PER_TARGET: usize = 256;

/// Where a plugin came from, which is the only identity it has across hosts.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PluginSource {
    /// Herdr's own word for the kind of source. Carried rather than
    /// interpreted: a kind this does not recognise is still an identity, and
    /// two plugins of different kinds are different plugins.
    pub kind: String,
    pub owner: String,
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
}

impl std::fmt::Display for PluginSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.owner, self.repo)?;
        if let Some(subdir) = &self.subdir {
            write!(formatter, "/{subdir}")?;
        }
        Ok(())
    }
}

impl PluginSource {
    /// Where somebody reads about this plugin.
    ///
    /// Derived from the source rather than looked up, because Herdr documents
    /// no marketplace or catalogue to search: `herdr plugin install` takes
    /// `owner/repo`, so `owner/repo` is what a link can be built from. A source
    /// of a kind this cannot address gets no link rather than a guessed one.
    pub fn detail_url(&self) -> Option<String> {
        (self.kind == "github").then(|| format!("https://github.com/{}/{}", self.owner, self.repo))
    }

    /// What `herdr plugin install` would be given.
    pub fn install_argument(&self) -> String {
        self.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledPlugin {
    /// Server-local. Never compared across hosts; carried so a person can find
    /// the thing again on the host it is on.
    pub plugin_id: String,
    pub name: String,
    pub version: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PluginSource>,
    /// What was asked for — a tag, a branch, a commit — and what it became.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// What every reachable host reported, and what the others said instead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inventory {
    pub installed: BTreeMap<TargetSession, Vec<InstalledPlugin>>,
    /// One line per target that could not answer. Kept beside the answers
    /// rather than replacing them: a fleet report where one unreachable host
    /// hides the other five is a report nobody can act on.
    pub errors: BTreeMap<TargetSession, String>,
}

/// A difference worth a person's attention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Drift {
    /// Installed somewhere and not somewhere else.
    Missing {
        source: PluginSource,
        present_on: Vec<TargetSession>,
        absent_from: Vec<TargetSession>,
    },
    /// Installed everywhere, at different versions.
    Version {
        source: PluginSource,
        versions: BTreeMap<String, Vec<TargetSession>>,
    },
    /// Same version, different commit. Worth saying separately: a version that
    /// did not change while the code did is the drift somebody would otherwise
    /// never look for.
    Commit {
        source: PluginSource,
        commits: BTreeMap<String, Vec<TargetSession>>,
    },
    /// Present but switched off somewhere.
    Disabled {
        source: PluginSource,
        disabled_on: Vec<TargetSession>,
    },
    /// A plugin with no source, which exists only where it was linked. Not a
    /// difference to reconcile — there is nothing to compare it to — but worth
    /// naming so it is not mistaken for a gap.
    LocalOnly {
        target: TargetSession,
        plugin_id: String,
        name: String,
    },
}

/// One plugin somebody wants, pinned to something that will not move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedPlugin {
    pub source: PluginSource,
    /// What to ask for. The resolved commit when there is one, because a tag
    /// can be moved and a branch certainly will: a lockfile that pinned a
    /// branch would reproduce whatever that branch says later, which is the
    /// opposite of what it is for.
    pub reference: String,
    pub version: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lockfile {
    pub plugins: Vec<LockedPlugin>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanAction {
    Install,
    Update,
}

/// One command somebody would run, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanStep {
    pub target: TargetSession,
    pub source: PluginSource,
    pub action: PlanAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub to: String,
    /// Herdr's own command. Not run here, and not reimplemented: Herdr has an
    /// installer, and a second one would be a second thing to keep correct.
    pub command: String,
}

impl Inventory {
    pub fn targets(&self) -> impl Iterator<Item = &TargetSession> {
        self.installed.keys()
    }

    /// Everything installed from one source, by the target it is on.
    fn by_source(&self) -> BTreeMap<PluginSource, BTreeMap<TargetSession, &InstalledPlugin>> {
        let mut grouped: BTreeMap<PluginSource, BTreeMap<TargetSession, &InstalledPlugin>> =
            BTreeMap::new();
        for (target, plugins) in &self.installed {
            for plugin in plugins {
                let Some(source) = plugin.source.clone() else {
                    continue;
                };
                grouped
                    .entry(source)
                    .or_default()
                    .insert(target.clone(), plugin);
            }
        }
        grouped
    }

    /// What differs across the hosts that answered.
    ///
    /// Only across those: a host that could not be reached is not evidence
    /// that it is missing anything, and reporting it as a gap would send
    /// somebody to install a plugin that is already there.
    pub fn drift(&self) -> Vec<Drift> {
        let answered = self.installed.keys().cloned().collect::<BTreeSet<_>>();
        let mut drift = Vec::new();
        for (source, holders) in self.by_source() {
            let absent_from = answered
                .iter()
                .filter(|target| !holders.contains_key(*target))
                .cloned()
                .collect::<Vec<_>>();
            if !absent_from.is_empty() {
                drift.push(Drift::Missing {
                    source: source.clone(),
                    present_on: holders.keys().cloned().collect(),
                    absent_from,
                });
            }

            let mut versions: BTreeMap<String, Vec<TargetSession>> = BTreeMap::new();
            let mut commits: BTreeMap<String, Vec<TargetSession>> = BTreeMap::new();
            let mut disabled_on = Vec::new();
            for (target, plugin) in &holders {
                versions
                    .entry(plugin.version.clone())
                    .or_default()
                    .push(target.clone());
                if let Some(commit) = &plugin.resolved_commit {
                    commits
                        .entry(commit.clone())
                        .or_default()
                        .push(target.clone());
                }
                if !plugin.enabled {
                    disabled_on.push(target.clone());
                }
            }
            if versions.len() > 1 {
                drift.push(Drift::Version {
                    source: source.clone(),
                    versions,
                });
            } else if commits.len() > 1 {
                // Only when the versions agree. A version difference already
                // explains a commit difference, and saying both would be one
                // disagreement reported twice.
                drift.push(Drift::Commit {
                    source: source.clone(),
                    commits,
                });
            }
            if !disabled_on.is_empty() {
                drift.push(Drift::Disabled {
                    source,
                    disabled_on,
                });
            }
        }

        for (target, plugins) in &self.installed {
            for plugin in plugins.iter().filter(|plugin| plugin.source.is_none()) {
                drift.push(Drift::LocalOnly {
                    target: target.clone(),
                    plugin_id: plugin.plugin_id.clone(),
                    name: plugin.name.clone(),
                });
            }
        }
        drift
    }

    /// Write down what one host has, as the set to reproduce elsewhere.
    ///
    /// A host is named rather than merged from all of them: "what everybody
    /// has" is not a decision anybody made, and picking the newest of each
    /// would invent a combination nobody has ever run.
    pub fn lockfile(&self, from: &TargetSession) -> Option<Lockfile> {
        let plugins = self.installed.get(from)?;
        Some(Lockfile {
            plugins: plugins
                .iter()
                .filter_map(|plugin| {
                    let source = plugin.source.clone()?;
                    // The commit when there is one: a tag can be moved and a
                    // branch certainly will.
                    let reference = plugin
                        .resolved_commit
                        .clone()
                        .or_else(|| plugin.requested_ref.clone())?;
                    Some(LockedPlugin {
                        source,
                        reference,
                        version: plugin.version.clone(),
                    })
                })
                .collect(),
        })
    }

    /// What it would take to bring the named targets to this lockfile.
    ///
    /// Produced whole and printed before anything runs. A host already holding
    /// the pinned reference produces no step at all, so a plan that is empty
    /// says the fleet already agrees rather than saying nothing happened.
    pub fn plan(&self, lockfile: &Lockfile, targets: &[TargetSession]) -> Vec<PlanStep> {
        let mut steps = Vec::new();
        for target in targets {
            let Some(installed) = self.installed.get(target) else {
                // Not reached, so not planned for. Guessing at what an
                // unreachable host holds is how a plan installs over something.
                continue;
            };
            for locked in &lockfile.plugins {
                let held = installed
                    .iter()
                    .find(|plugin| plugin.source.as_ref() == Some(&locked.source));
                let at = held.and_then(|plugin| {
                    plugin
                        .resolved_commit
                        .clone()
                        .or_else(|| plugin.requested_ref.clone())
                });
                if at.as_deref() == Some(locked.reference.as_str()) {
                    continue;
                }
                steps.push(PlanStep {
                    target: target.clone(),
                    source: locked.source.clone(),
                    action: if held.is_some() {
                        PlanAction::Update
                    } else {
                        PlanAction::Install
                    },
                    from: held.map(|plugin| plugin.version.clone()),
                    to: locked.reference.clone(),
                    command: format!(
                        "herdr plugin install {} --ref {} -y",
                        locked.source.install_argument(),
                        locked.reference
                    ),
                });
            }
        }
        steps
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Drift, InstalledPlugin, Inventory, LockedPlugin, Lockfile, PlanAction, PluginSource,
    };
    use crate::model::TargetSession;

    fn source(owner: &str, repo: &str) -> PluginSource {
        PluginSource {
            kind: "github".to_owned(),
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            subdir: None,
        }
    }

    fn plugin(id: &str, version: &str, source: Option<PluginSource>) -> InstalledPlugin {
        InstalledPlugin {
            plugin_id: id.to_owned(),
            name: id.to_owned(),
            version: version.to_owned(),
            enabled: true,
            source,
            requested_ref: Some("v1".to_owned()),
            resolved_commit: Some("a".repeat(40)),
            warnings: Vec::new(),
        }
    }

    fn inventory(entries: Vec<(&str, Vec<InstalledPlugin>)>) -> Inventory {
        let mut held = Inventory::default();
        for (target, plugins) in entries {
            held.installed
                .insert(TargetSession::new(target, "work"), plugins);
        }
        held
    }

    #[test]
    fn two_hosts_naming_different_plugins_the_same_thing_are_not_one_plugin() {
        let held = inventory(vec![
            (
                "host-a",
                vec![plugin("review", "1.0", Some(source("alice", "review")))],
            ),
            (
                "host-b",
                vec![plugin("review", "1.0", Some(source("bob", "review")))],
            ),
        ]);

        let drift = held.drift();

        // Each is missing from the other host, and neither is a version
        // difference: they were never the same plugin.
        assert_eq!(drift.len(), 2);
        assert!(drift.iter().all(|one| matches!(one, Drift::Missing { .. })));
        assert!(
            !drift.iter().any(|one| matches!(one, Drift::Version { .. })),
            "a shared plugin_id is not evidence that two hosts hold the same code"
        );
    }

    #[test]
    fn one_plugin_installed_under_different_ids_is_still_one_plugin() {
        let held = inventory(vec![
            (
                "host-a",
                vec![plugin("review", "1.0", Some(source("alice", "review")))],
            ),
            (
                "host-b",
                vec![plugin(
                    "code-review",
                    "1.0",
                    Some(source("alice", "review")),
                )],
            ),
        ]);

        assert!(
            held.drift().is_empty(),
            "identity is where it came from, not what the host called it"
        );
    }

    #[test]
    fn a_version_difference_is_reported_once_and_not_twice() {
        let mut newer = plugin("review", "2.0", Some(source("alice", "review")));
        newer.resolved_commit = Some("b".repeat(40));
        let held = inventory(vec![
            (
                "host-a",
                vec![plugin("review", "1.0", Some(source("alice", "review")))],
            ),
            ("host-b", vec![newer]),
        ]);

        let drift = held.drift();

        assert_eq!(drift.len(), 1);
        assert!(matches!(drift[0], Drift::Version { .. }));
    }

    #[test]
    fn the_same_version_built_from_different_commits_is_its_own_finding() {
        let mut moved = plugin("review", "1.0", Some(source("alice", "review")));
        moved.resolved_commit = Some("b".repeat(40));
        let held = inventory(vec![
            (
                "host-a",
                vec![plugin("review", "1.0", Some(source("alice", "review")))],
            ),
            ("host-b", vec![moved]),
        ]);

        let drift = held.drift();

        assert!(
            matches!(drift.as_slice(), [Drift::Commit { .. }]),
            "a version that did not change while the code did is the drift nobody looks for"
        );
    }

    #[test]
    fn a_linked_local_plugin_is_named_and_never_matched() {
        let held = inventory(vec![
            ("host-a", vec![plugin("scratch", "0.1", None)]),
            ("host-b", vec![plugin("scratch", "0.1", None)]),
        ]);

        let drift = held.drift();

        assert_eq!(drift.len(), 2);
        assert!(
            drift
                .iter()
                .all(|one| matches!(one, Drift::LocalOnly { .. }))
        );
        assert!(
            !drift.iter().any(|one| matches!(one, Drift::Missing { .. })),
            "a name is not evidence that two directories hold the same code"
        );
    }

    #[test]
    fn a_host_that_could_not_answer_is_not_reported_as_missing_anything() {
        let mut held = inventory(vec![(
            "host-a",
            vec![plugin("review", "1.0", Some(source("alice", "review")))],
        )]);
        held.errors.insert(
            TargetSession::new("host-b", "work"),
            "connection refused".to_owned(),
        );

        assert!(
            held.drift().is_empty(),
            "an unreachable host is not evidence that it is missing anything"
        );
    }

    #[test]
    fn a_plugin_switched_off_somewhere_is_worth_saying() {
        let mut off = plugin("review", "1.0", Some(source("alice", "review")));
        off.enabled = false;
        let held = inventory(vec![
            (
                "host-a",
                vec![plugin("review", "1.0", Some(source("alice", "review")))],
            ),
            ("host-b", vec![off]),
        ]);

        assert!(matches!(
            held.drift().as_slice(),
            [Drift::Disabled { disabled_on, .. }] if disabled_on.len() == 1
        ));
    }

    #[test]
    fn a_lockfile_pins_the_commit_rather_than_the_tag_that_moved() {
        let held = inventory(vec![(
            "host-a",
            vec![plugin("review", "1.0", Some(source("alice", "review")))],
        )]);

        let lockfile = held
            .lockfile(&TargetSession::new("host-a", "work"))
            .unwrap();

        assert_eq!(lockfile.plugins.len(), 1);
        assert_eq!(
            lockfile.plugins[0].reference,
            "a".repeat(40),
            "a lockfile that pinned a branch would reproduce whatever it says later"
        );
    }

    #[test]
    fn a_plan_names_herdr_own_command_and_skips_what_already_agrees() {
        let held = inventory(vec![
            (
                "host-a",
                vec![plugin("review", "1.0", Some(source("alice", "review")))],
            ),
            ("host-b", Vec::new()),
        ]);
        let lockfile = Lockfile {
            plugins: vec![LockedPlugin {
                source: source("alice", "review"),
                reference: "a".repeat(40),
                version: "1.0".to_owned(),
            }],
        };

        let plan = held.plan(
            &lockfile,
            &[
                TargetSession::new("host-a", "work"),
                TargetSession::new("host-b", "work"),
            ],
        );

        assert_eq!(
            plan.len(),
            1,
            "a host already holding the pin needs no step"
        );
        assert_eq!(plan[0].target, TargetSession::new("host-b", "work"));
        assert_eq!(plan[0].action, PlanAction::Install);
        assert_eq!(
            plan[0].command,
            format!(
                "herdr plugin install alice/review --ref {} -y",
                "a".repeat(40)
            )
        );
    }

    #[test]
    fn a_plan_skips_a_target_that_never_answered() {
        let held = inventory(vec![(
            "host-a",
            vec![plugin("review", "1.0", Some(source("alice", "review")))],
        )]);
        let lockfile = held
            .lockfile(&TargetSession::new("host-a", "work"))
            .unwrap();

        let plan = held.plan(&lockfile, &[TargetSession::new("host-gone", "work")]);

        assert!(
            plan.is_empty(),
            "guessing at what an unreachable host holds is how a plan installs over something"
        );
    }

    #[test]
    fn a_source_addresses_its_own_documentation_and_installer() {
        let with_subdir = PluginSource {
            subdir: Some("plugins/review".to_owned()),
            ..source("alice", "tools")
        };

        assert_eq!(with_subdir.to_string(), "alice/tools/plugins/review");
        assert_eq!(
            with_subdir.detail_url().unwrap(),
            "https://github.com/alice/tools"
        );
        assert_eq!(
            PluginSource {
                kind: "path".to_owned(),
                ..source("alice", "tools")
            }
            .detail_url(),
            None,
            "a source this cannot address gets no link rather than a guessed one"
        );
    }
}
