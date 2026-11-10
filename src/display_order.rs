//! A custom display order for sessions, workspaces and panes, kept by this
//! Super-Herdr and shown identically to every client attached to it.
//!
//! Herdr reports these in its own order (server-local ids, sorted), and that
//! is fine for Herdr's own purposes. It is not how the person running five
//! targets wants their sidebar arranged. This module is purely that
//! arrangement: it is never sent to Herdr, never changes anything Herdr
//! reports, and a daemon that has never heard of it still works exactly as
//! before.
//!
//! It is daemon-owned rather than per-device, unlike `ui_state.rs`'s pane
//! selection: a person who drags a workspace into place on their phone wants
//! their terminal to agree, not to keep its own separate arrangement.
//!
//! Three levels, each a sibling list under one parent: the sessions under a
//! target, the workspaces under a session, the panes under a workspace (panes
//! are already flattened across tabs for display, so this is the level that
//! matters, not the tab in between). [`OrderScope`] names which list; the
//! stored value is the order of ids within it.
//!
//! A stored order can go stale: a session closes, a workspace is recreated
//! under a new id, a pane splits into two. Nothing here tries to detect a
//! rename — ids are server-local and a reused one is a different resource, the
//! same rule `aliases.rs` follows. [`DisplayOrders::merged`] is what actually
//! decides what to show: ids still live, in the order they were put in,
//! followed by anything live that was never ordered. [`DisplayOrders::compact_against`]
//! prunes what is stored to match, so a daemon that runs for months does not
//! accumulate ids for sessions nobody has seen in weeks.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::attention::{set_directory_permissions, set_file_permissions};
use crate::model::{TargetSession, WorkspaceId};
use crate::state::FederationState;

const DISPLAY_ORDER_VERSION: u32 = 1;
const MAX_DISPLAY_ORDER_BYTES: u64 = 256 * 1024;
pub const MAX_ORDER_SCOPES: usize = 4096;
pub const MAX_ORDER_ITEMS_PER_SCOPE: usize = 512;
pub const MAX_ORDER_ID_CHARACTERS: usize = 256;

/// Which sibling list a stored order applies to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OrderScope {
    SessionsInTarget {
        target: String,
    },
    WorkspacesInSession {
        target: String,
        session: String,
    },
    PanesInWorkspace {
        target: String,
        session: String,
        workspace: String,
    },
}

impl OrderScope {
    pub fn target(&self) -> &str {
        match self {
            Self::SessionsInTarget { target }
            | Self::WorkspacesInSession { target, .. }
            | Self::PanesInWorkspace { target, .. } => target,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DisplayOrders {
    orders: BTreeMap<OrderScope, Vec<String>>,
}

impl DisplayOrders {
    /// Replace the stored order for one scope.
    ///
    /// An empty order is the same as never having set one: it is removed
    /// rather than kept as a pointless empty entry. Returns whether the
    /// stored state actually changed, so a caller knows whether there is
    /// anything worth persisting or broadcasting.
    pub fn set(&mut self, scope: OrderScope, order: Vec<String>) -> Result<bool> {
        if order.len() > MAX_ORDER_ITEMS_PER_SCOPE {
            bail!("at most {MAX_ORDER_ITEMS_PER_SCOPE} items can be ordered at once");
        }
        for id in &order {
            if id.is_empty() || id.chars().count() > MAX_ORDER_ID_CHARACTERS {
                bail!("an ordered id must be 1 to {MAX_ORDER_ID_CHARACTERS} characters");
            }
        }
        if has_duplicate(&order) {
            bail!("an order cannot repeat the same id twice");
        }
        if order.is_empty() {
            return Ok(self.orders.remove(&scope).is_some());
        }
        if self.orders.len() >= MAX_ORDER_SCOPES && !self.orders.contains_key(&scope) {
            bail!("at most {MAX_ORDER_SCOPES} custom orders can be kept");
        }
        let changed = self.orders.get(&scope) != Some(&order);
        self.orders.insert(scope, order);
        Ok(changed)
    }

    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }

    /// Every session named by a stored scope — either directly
    /// (`WorkspacesInSession`/`PanesInWorkspace`'s own key) or as one of the
    /// ids listed inside a `SessionsInTarget` order. For pruning sessions a
    /// configuration reload no longer reports, the same way `Aliases::targets`
    /// finds what to check `forget_target` against.
    pub fn sessions(&self) -> std::collections::BTreeSet<TargetSession> {
        let mut sessions = std::collections::BTreeSet::new();
        for (scope, order) in &self.orders {
            match scope {
                OrderScope::SessionsInTarget { target } => {
                    sessions.extend(
                        order
                            .iter()
                            .map(|session| TargetSession::new(target, session)),
                    );
                }
                OrderScope::WorkspacesInSession { target, session }
                | OrderScope::PanesInWorkspace {
                    target, session, ..
                } => {
                    sessions.insert(TargetSession::new(target, session));
                }
            }
        }
        sessions
    }

    /// Forget every order that belongs to a target no longer configured.
    ///
    /// Mirrors `Aliases::forget_target`: removing a target is a decision, and
    /// the order its sessions and workspaces were in does not survive that
    /// decision. It is not forgotten because the host was briefly
    /// unreachable — that is not a decision anybody made, and is instead
    /// handled by `compact_against` leaving an order alone whenever a
    /// snapshot simply has not arrived yet.
    pub fn forget_target(&mut self, target: &str) -> bool {
        let before = self.orders.len();
        self.orders.retain(|scope, _| scope.target() != target);
        before != self.orders.len()
    }

    /// Forget a session's own workspace order, its panes' orders, and its
    /// entry in its target's session order.
    pub fn forget_session(&mut self, session: &TargetSession) -> bool {
        let mut changed = self
            .orders
            .remove(&OrderScope::WorkspacesInSession {
                target: session.target.clone(),
                session: session.session.clone(),
            })
            .is_some();

        let before = self.orders.len();
        self.orders.retain(|scope, _| {
            !matches!(
                scope,
                OrderScope::PanesInWorkspace { target, session: s, .. }
                    if target == &session.target && s == &session.session
            )
        });
        changed |= before != self.orders.len();

        changed |= self.remove_from_parent(
            &OrderScope::SessionsInTarget {
                target: session.target.clone(),
            },
            &session.session,
        );
        changed
    }

    /// Forget a workspace's own pane order, and its entry in its session's
    /// workspace order.
    pub fn forget_workspace(&mut self, workspace: &WorkspaceId) -> bool {
        let mut changed = self
            .orders
            .remove(&OrderScope::PanesInWorkspace {
                target: workspace.target.clone(),
                session: workspace.session.clone(),
                workspace: workspace.resource.clone(),
            })
            .is_some();

        changed |= self.remove_from_parent(
            &OrderScope::WorkspacesInSession {
                target: workspace.target.clone(),
                session: workspace.session.clone(),
            },
            &workspace.resource,
        );
        changed
    }

    fn remove_from_parent(&mut self, parent: &OrderScope, id: &str) -> bool {
        let Some(order) = self.orders.get_mut(parent) else {
            return false;
        };
        let before = order.len();
        order.retain(|held| held != id);
        if order.len() == before {
            return false;
        }
        if order.is_empty() {
            self.orders.remove(parent);
        }
        true
    }

    /// Prune every stored order against what is actually live right now.
    ///
    /// Only scopes that already hold a stored order are touched, so this
    /// stays cheap on a federation with many targets even though it runs on
    /// every federation update. A session or workspace whose snapshot has
    /// not arrived yet is left untouched — that is "not loaded", not "gone".
    pub fn compact_against(&mut self, state: &FederationState) -> bool {
        let scopes: Vec<OrderScope> = self.orders.keys().cloned().collect();
        let mut changed = false;
        for scope in scopes {
            changed |= self.compact_scope(&scope, state);
        }
        changed
    }

    fn compact_scope(&mut self, scope: &OrderScope, state: &FederationState) -> bool {
        match scope {
            OrderScope::SessionsInTarget { target } => {
                let live: Vec<String> = state
                    .targets
                    .keys()
                    .filter(|key| &key.target == target)
                    .map(|key| key.session.clone())
                    .collect();
                if live.is_empty() {
                    return self.orders.remove(scope).is_some();
                }
                self.retain_live(scope, &live)
            }
            OrderScope::WorkspacesInSession { target, session } => {
                let key = TargetSession::new(target, session);
                let Some(runtime) = state.targets.get(&key) else {
                    return self.orders.remove(scope).is_some();
                };
                let Some(snapshot) = runtime.snapshot.as_ref() else {
                    return false;
                };
                let live: Vec<String> = snapshot
                    .workspaces
                    .keys()
                    .map(|id| id.resource.clone())
                    .collect();
                self.retain_live(scope, &live)
            }
            OrderScope::PanesInWorkspace {
                target,
                session,
                workspace,
            } => {
                let key = TargetSession::new(target, session);
                let Some(runtime) = state.targets.get(&key) else {
                    return self.orders.remove(scope).is_some();
                };
                let Some(snapshot) = runtime.snapshot.as_ref() else {
                    return false;
                };
                let workspace_id = WorkspaceId::new(target, session, workspace);
                if !snapshot.workspaces.contains_key(&workspace_id) {
                    return self.orders.remove(scope).is_some();
                }
                let live: Vec<String> = snapshot
                    .panes
                    .values()
                    .filter(|pane| pane.workspace.as_ref() == Some(&workspace_id))
                    .map(|pane| pane.id.resource.clone())
                    .collect();
                self.retain_live(scope, &live)
            }
        }
    }

    fn retain_live(&mut self, scope: &OrderScope, live: &[String]) -> bool {
        let Some(order) = self.orders.get_mut(scope) else {
            return false;
        };
        let before = order.len();
        order.retain(|id| live.contains(id));
        let changed = order.len() != before;
        if order.is_empty() {
            self.orders.remove(scope);
        }
        changed
    }

    /// What to show for one scope: stored ids still live, in stored order,
    /// then anything live that was never ordered, in its own order.
    ///
    /// Run on the daemon before every broadcast, and again on each client
    /// before every paint against its own freshest snapshot — a broadcast can
    /// be a tick stale by the time a workspace or pane it never mentioned
    /// appears.
    pub fn merged(&self, scope: &OrderScope, live_natural: &[String]) -> Vec<String> {
        let stored = self.orders.get(scope).map(Vec::as_slice).unwrap_or(&[]);
        let mut result: Vec<String> = stored
            .iter()
            .filter(|id| live_natural.contains(*id))
            .cloned()
            .collect();
        for id in live_natural {
            if !result.contains(id) {
                result.push(id.clone());
            }
        }
        result
    }

    /// Every stored scope, merged against `state` for broadcasting.
    ///
    /// A scope whose session/workspace has not loaded a snapshot yet is sent
    /// as stored, unmerged — reconnecting is not "gone", and the sidebar
    /// should not flash empty while it catches up.
    pub fn snapshot_entries(&self, state: &FederationState) -> Vec<DisplayOrderEntry> {
        self.orders
            .iter()
            .map(|(scope, stored)| {
                let order = match live_siblings(scope, state) {
                    Some(live) => self.merged(scope, &live),
                    None => stored.clone(),
                };
                DisplayOrderEntry {
                    scope: scope.clone(),
                    order,
                }
            })
            .collect()
    }
}

/// The live sibling ids for a scope, or `None` if the scope's parent
/// session/workspace has no snapshot yet (not "gone", just not loaded).
fn live_siblings(scope: &OrderScope, state: &FederationState) -> Option<Vec<String>> {
    match scope {
        OrderScope::SessionsInTarget { target } => Some(
            state
                .targets
                .keys()
                .filter(|key| &key.target == target)
                .map(|key| key.session.clone())
                .collect(),
        ),
        OrderScope::WorkspacesInSession { target, session } => {
            let key = TargetSession::new(target, session);
            let snapshot = state.targets.get(&key)?.snapshot.as_ref()?;
            Some(
                snapshot
                    .workspaces
                    .keys()
                    .map(|id| id.resource.clone())
                    .collect(),
            )
        }
        OrderScope::PanesInWorkspace {
            target,
            session,
            workspace,
        } => {
            let key = TargetSession::new(target, session);
            let snapshot = state.targets.get(&key)?.snapshot.as_ref()?;
            let workspace_id = WorkspaceId::new(target, session, workspace);
            if !snapshot.workspaces.contains_key(&workspace_id) {
                return None;
            }
            Some(
                snapshot
                    .panes
                    .values()
                    .filter(|pane| pane.workspace.as_ref() == Some(&workspace_id))
                    .map(|pane| pane.id.resource.clone())
                    .collect(),
            )
        }
    }
}

/// One scope's order, as carried over the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayOrderEntry {
    pub scope: OrderScope,
    pub order: Vec<String>,
}

fn has_duplicate(order: &[String]) -> bool {
    let mut seen = std::collections::BTreeSet::new();
    !order.iter().all(|id| seen.insert(id))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedEntry {
    scope: OrderScope,
    order: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedDisplayOrders {
    version: u32,
    entries: Vec<PersistedEntry>,
}

impl From<&DisplayOrders> for PersistedDisplayOrders {
    fn from(orders: &DisplayOrders) -> Self {
        Self {
            version: DISPLAY_ORDER_VERSION,
            entries: orders
                .orders
                .iter()
                .map(|(scope, order)| PersistedEntry {
                    scope: scope.clone(),
                    order: order.clone(),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DisplayOrderStore {
    path: PathBuf,
}

impl DisplayOrderStore {
    pub fn discover() -> Result<Self> {
        let root = if let Some(root) = env::var_os("XDG_STATE_HOME") {
            PathBuf::from(root)
        } else {
            let home: OsString = env::var_os("HOME").context(
                "XDG_STATE_HOME or HOME is required to persist Super-Herdr's display order",
            )?;
            PathBuf::from(home).join(".local/state")
        };
        Ok(Self {
            path: root.join("super-herdr/display-order.json"),
        })
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Load what was stored, dropping any entry that is on its own too big,
    /// too long, or duplicated — rather than failing the whole file over one
    /// target's corrupt order, which per-target failure isolation (AGENTS.md)
    /// does not allow.
    pub fn load(&self) -> Result<DisplayOrders> {
        let metadata = match fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(DisplayOrders::default());
            }
            Err(error) => return Err(error).context("failed to inspect display order"),
        };
        if metadata.len() > MAX_DISPLAY_ORDER_BYTES {
            bail!("persisted display order exceeds the size limit");
        }
        let bytes = fs::read(&self.path).context("failed to read display order")?;
        let persisted: PersistedDisplayOrders =
            serde_json::from_slice(&bytes).context("persisted display order is invalid")?;
        if persisted.version != DISPLAY_ORDER_VERSION {
            bail!("persisted display order has an unsupported version");
        }

        let mut orders = DisplayOrders::default();
        for entry in persisted.entries {
            if orders.orders.len() >= MAX_ORDER_SCOPES {
                break;
            }
            if entry.order.len() > MAX_ORDER_ITEMS_PER_SCOPE {
                continue;
            }
            if entry
                .order
                .iter()
                .any(|id| id.is_empty() || id.chars().count() > MAX_ORDER_ID_CHARACTERS)
            {
                continue;
            }
            if has_duplicate(&entry.order) {
                continue;
            }
            if entry.order.is_empty() {
                continue;
            }
            orders.orders.insert(entry.scope, entry.order);
        }
        Ok(orders)
    }

    pub fn save(&self, orders: &DisplayOrders) -> Result<()> {
        let directory = self
            .path
            .parent()
            .context("the display order path has no parent directory")?;
        fs::create_dir_all(directory).context("failed to create the display order directory")?;
        set_directory_permissions(directory)?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".display-order-")
            .tempfile_in(directory)
            .context("failed to create a temporary display order file")?;
        set_file_permissions(temporary.path())?;
        serde_json::to_writer(&mut temporary, &PersistedDisplayOrders::from(orders))
            .context("failed to encode display order")?;
        temporary
            .write_all(b"\n")
            .context("failed to finish display order")?;
        temporary
            .as_file()
            .sync_all()
            .context("failed to synchronize display order")?;
        temporary
            .persist(&self.path)
            .context("failed to atomically replace display order")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::{DisplayOrderStore, DisplayOrders, MAX_ORDER_ID_CHARACTERS, OrderScope};
    use crate::model::{TargetSession, WorkspaceId};
    use crate::state::{
        FederationState, NormalizedSnapshot, TargetConnectionState, TargetRuntimeState,
        TargetUpdateMode,
    };

    fn sessions(target: &str) -> OrderScope {
        OrderScope::SessionsInTarget {
            target: target.to_owned(),
        }
    }

    fn workspaces(target: &str, session: &str) -> OrderScope {
        OrderScope::WorkspacesInSession {
            target: target.to_owned(),
            session: session.to_owned(),
        }
    }

    fn panes(target: &str, session: &str, workspace: &str) -> OrderScope {
        OrderScope::PanesInWorkspace {
            target: target.to_owned(),
            session: session.to_owned(),
            workspace: workspace.to_owned(),
        }
    }

    fn state_with_workspaces_and_panes(
        key: TargetSession,
        workspaces: &[&str],
        panes: &[(&str, &str)],
    ) -> FederationState {
        let snapshot_json = json!({
            "workspaces": workspaces
                .iter()
                .map(|id| json!({"workspace_id": id}))
                .collect::<Vec<_>>(),
            "panes": panes
                .iter()
                .map(|(id, workspace)| json!({"pane_id": id, "workspace_id": workspace}))
                .collect::<Vec<_>>(),
        });
        let snapshot = NormalizedSnapshot::from_value(&key, &snapshot_json);
        let mut state = FederationState::default();
        state.targets.insert(
            key.clone(),
            TargetRuntimeState {
                key,
                endpoint: "test".to_owned(),
                connection: TargetConnectionState::Live,
                update_mode: TargetUpdateMode::Events,
                event_error: None,
                connection_generation: 1,
                selected_herdr_bin: Some("herdr".to_owned()),
                roots: Vec::new(),
                preview_ports: Vec::new(),
                snapshot: Some(Arc::new(snapshot)),
                last_error: None,
                last_success: None,
                retry_at: None,
            },
        );
        state
    }

    #[test]
    fn an_order_is_bounded_and_free_of_duplicates() {
        let mut orders = DisplayOrders::default();
        let scope = sessions("host-a");

        assert!(
            orders
                .set(scope.clone(), vec!["a".to_owned(), "a".to_owned()])
                .is_err()
        );
        assert!(
            orders
                .set(scope.clone(), vec!["x".repeat(MAX_ORDER_ID_CHARACTERS + 1)])
                .is_err()
        );
        assert!(orders.set(scope.clone(), vec!["".to_owned()]).is_err());
        assert!(
            orders
                .set(scope, vec!["a".to_owned(), "b".to_owned()])
                .is_ok()
        );
    }

    #[test]
    fn setting_an_empty_order_clears_it() {
        let mut orders = DisplayOrders::default();
        let scope = sessions("host-a");
        assert!(orders.set(scope.clone(), vec!["a".to_owned()]).unwrap());
        assert!(!orders.is_empty());

        assert!(orders.set(scope, Vec::new()).unwrap());
        assert!(orders.is_empty());
    }

    #[test]
    fn merged_keeps_stored_order_and_appends_new_ids() {
        let mut orders = DisplayOrders::default();
        let scope = workspaces("host-a", "work");
        orders
            .set(scope.clone(), vec!["w2".to_owned(), "w1".to_owned()])
            .unwrap();

        let live = vec!["w1".to_owned(), "w2".to_owned(), "w3".to_owned()];
        assert_eq!(
            orders.merged(&scope, &live),
            vec!["w2".to_owned(), "w1".to_owned(), "w3".to_owned()],
            "stored order wins for ids still live; a new id is appended"
        );
    }

    #[test]
    fn merged_drops_ids_no_longer_live() {
        let mut orders = DisplayOrders::default();
        let scope = workspaces("host-a", "work");
        orders
            .set(scope.clone(), vec!["w1".to_owned(), "gone".to_owned()])
            .unwrap();

        let live = vec!["w1".to_owned()];
        assert_eq!(orders.merged(&scope, &live), vec!["w1".to_owned()]);
    }

    #[test]
    fn forgetting_a_target_drops_every_scope_under_it() {
        let mut orders = DisplayOrders::default();
        orders
            .set(sessions("host-a"), vec!["work".to_owned()])
            .unwrap();
        orders
            .set(workspaces("host-a", "work"), vec!["w1".to_owned()])
            .unwrap();
        orders
            .set(panes("host-a", "work", "w1"), vec!["w1:p1".to_owned()])
            .unwrap();
        orders
            .set(sessions("host-b"), vec!["other".to_owned()])
            .unwrap();

        assert!(orders.forget_target("host-a"));

        assert!(
            orders
                .merged(&sessions("host-a"), &["work".to_owned()])
                .is_empty()
                || orders.merged(&sessions("host-a"), &[]).is_empty()
        );
        assert_eq!(
            orders.merged(&sessions("host-b"), &["other".to_owned()]),
            vec!["other".to_owned()],
            "another target's order is not somebody else's configuration change"
        );
    }

    #[test]
    fn forgetting_a_session_removes_its_entry_from_the_target_order() {
        let mut orders = DisplayOrders::default();
        orders
            .set(
                sessions("host-a"),
                vec!["work".to_owned(), "lab".to_owned()],
            )
            .unwrap();
        orders
            .set(workspaces("host-a", "work"), vec!["w1".to_owned()])
            .unwrap();
        orders
            .set(panes("host-a", "work", "w1"), vec!["w1:p1".to_owned()])
            .unwrap();

        let gone = TargetSession::new("host-a", "work");
        assert!(orders.forget_session(&gone));

        assert_eq!(
            orders.merged(&sessions("host-a"), &["lab".to_owned()]),
            vec!["lab".to_owned()],
            "the closed session's own entry is gone from its target's order"
        );
        assert!(orders.merged(&workspaces("host-a", "work"), &[]).is_empty());
        assert!(
            orders
                .merged(&panes("host-a", "work", "w1"), &[])
                .is_empty()
        );
    }

    #[test]
    fn forgetting_a_workspace_removes_its_entry_from_the_session_order() {
        let mut orders = DisplayOrders::default();
        orders
            .set(
                workspaces("host-a", "work"),
                vec!["w1".to_owned(), "w2".to_owned()],
            )
            .unwrap();
        orders
            .set(panes("host-a", "work", "w1"), vec!["w1:p1".to_owned()])
            .unwrap();

        let gone = WorkspaceId::new("host-a", "work", "w1");
        assert!(orders.forget_workspace(&gone));

        assert_eq!(
            orders.merged(&workspaces("host-a", "work"), &["w2".to_owned()]),
            vec!["w2".to_owned()]
        );
        assert!(
            orders
                .merged(&panes("host-a", "work", "w1"), &[])
                .is_empty()
        );
    }

    #[test]
    fn compacting_drops_ids_that_disappeared_from_a_live_snapshot() {
        let mut orders = DisplayOrders::default();
        orders
            .set(
                workspaces("host-a", "work"),
                vec!["w1".to_owned(), "w2".to_owned()],
            )
            .unwrap();
        orders
            .set(
                panes("host-a", "work", "w1"),
                vec!["w1:p1".to_owned(), "w1:p2".to_owned()],
            )
            .unwrap();

        let state = state_with_workspaces_and_panes(
            TargetSession::new("host-a", "work"),
            &["w1"],
            &[("w1:p1", "w1")],
        );

        assert!(orders.compact_against(&state));
        assert_eq!(
            orders.merged(&workspaces("host-a", "work"), &["w1".to_owned()]),
            vec!["w1".to_owned()],
            "w2 no longer exists in the live snapshot"
        );
        assert_eq!(
            orders.merged(&panes("host-a", "work", "w1"), &["w1:p1".to_owned()]),
            vec!["w1:p1".to_owned()],
            "w1:p2 no longer exists in the live snapshot"
        );
    }

    #[test]
    fn compacting_removes_a_scope_whose_session_is_entirely_gone() {
        let mut orders = DisplayOrders::default();
        orders
            .set(workspaces("host-a", "work"), vec!["w1".to_owned()])
            .unwrap();

        let state = FederationState::default();
        assert!(orders.compact_against(&state));
        assert!(orders.is_empty());
    }

    #[test]
    fn compacting_leaves_an_order_alone_while_a_session_has_not_loaded_yet() {
        let mut orders = DisplayOrders::default();
        orders
            .set(workspaces("host-a", "work"), vec!["w1".to_owned()])
            .unwrap();

        let key = TargetSession::new("host-a", "work");
        let mut state = FederationState::default();
        state.targets.insert(
            key.clone(),
            TargetRuntimeState {
                key,
                endpoint: "test".to_owned(),
                connection: TargetConnectionState::Connecting,
                update_mode: TargetUpdateMode::Polling,
                event_error: None,
                connection_generation: 0,
                selected_herdr_bin: None,
                roots: Vec::new(),
                preview_ports: Vec::new(),
                snapshot: None,
                last_error: None,
                last_success: None,
                retry_at: None,
            },
        );

        assert!(!orders.compact_against(&state));
        assert_eq!(
            orders.merged(&workspaces("host-a", "work"), &["w1".to_owned()]),
            vec!["w1".to_owned()]
        );
    }

    #[test]
    fn snapshot_entries_merges_against_live_state_and_leaves_unloaded_scopes_as_stored() {
        let mut orders = DisplayOrders::default();
        orders
            .set(
                workspaces("host-a", "work"),
                vec!["w2".to_owned(), "w1".to_owned()],
            )
            .unwrap();
        orders
            .set(workspaces("host-b", "lab"), vec!["w9".to_owned()])
            .unwrap();

        let state = state_with_workspaces_and_panes(
            TargetSession::new("host-a", "work"),
            &["w1", "w2", "w3"],
            &[],
        );

        let entries = orders.snapshot_entries(&state);
        let host_a = entries
            .iter()
            .find(|entry| entry.scope == workspaces("host-a", "work"))
            .unwrap();
        assert_eq!(
            host_a.order,
            vec!["w2".to_owned(), "w1".to_owned(), "w3".to_owned()],
            "broadcast carries the already-merged order"
        );

        let host_b = entries
            .iter()
            .find(|entry| entry.scope == workspaces("host-b", "lab"))
            .unwrap();
        assert_eq!(
            host_b.order,
            vec!["w9".to_owned()],
            "a scope with no snapshot yet is sent as stored, not wiped"
        );
    }

    #[test]
    fn display_order_survives_a_restart() {
        let directory = tempfile::tempdir().unwrap();
        let store = DisplayOrderStore::at(directory.path().join("display-order.json"));
        let mut orders = DisplayOrders::default();
        orders
            .set(
                workspaces("host-a", "work"),
                vec!["w2".to_owned(), "w1".to_owned()],
            )
            .unwrap();

        store.save(&orders).unwrap();
        let restored = store.load().unwrap();

        assert_eq!(restored, orders);
    }

    #[test]
    fn a_missing_file_is_no_custom_order() {
        let directory = tempfile::tempdir().unwrap();

        assert!(
            DisplayOrderStore::at(directory.path().join("absent.json"))
                .load()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_corrupt_entry_does_not_sink_every_other_targets_order() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("display-order.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "version": 1,
                "entries": [
                    {
                        "scope": {"kind": "sessions_in_target", "target": "host-a"},
                        "order": ["work", "work"]
                    },
                    {
                        "scope": {"kind": "sessions_in_target", "target": "host-b"},
                        "order": ["other"]
                    }
                ]
            })
            .to_string(),
        )
        .unwrap();

        let restored = DisplayOrderStore::at(path).load().unwrap();

        assert!(
            restored.merged(&sessions("host-a"), &[]).is_empty(),
            "the duplicated entry for host-a is dropped on its own"
        );
        assert_eq!(
            restored.merged(&sessions("host-b"), &["other".to_owned()]),
            vec!["other".to_owned()],
            "host-b's entry is unaffected by host-a's corruption"
        );
    }
}
