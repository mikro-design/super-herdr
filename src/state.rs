use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::Value;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, sleep_until};

use crate::config::{Config, Target, TransportConfig};
use crate::model::{PaneId, TabId, TargetSession, TerminalId, WorkspaceId};
use crate::transport::{SnapshotError, SnapshotErrorKind, SnapshotTransport, TransportSnapshot};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceCounts {
    pub workspaces: usize,
    pub tabs: usize,
    pub panes: usize,
    pub agents: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NormalizedSnapshot {
    pub server_version: Option<String>,
    pub protocol: Option<u64>,
    pub revision: Option<u64>,
    pub capabilities: BTreeSet<String>,
    pub counts: ResourceCounts,
    pub focused_workspace: Option<WorkspaceId>,
    pub focused_tab: Option<TabId>,
    pub focused_pane: Option<PaneId>,
    pub workspaces: BTreeMap<WorkspaceId, WorkspaceState>,
    pub tabs: BTreeMap<TabId, TabState>,
    pub panes: BTreeMap<PaneId, PaneState>,
    pub layouts: BTreeMap<TabId, LayoutState>,
    pub agents: BTreeMap<PaneId, AgentState>,
    pub terminals: BTreeSet<TerminalId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceState {
    pub id: WorkspaceId,
    pub active_tab: Option<TabId>,
    pub label: Option<String>,
    pub number: Option<u64>,
    pub focused: bool,
    pub agent_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabState {
    pub id: TabId,
    pub workspace: Option<WorkspaceId>,
    pub label: Option<String>,
    pub number: Option<u64>,
    pub focused: bool,
    pub agent_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneState {
    pub id: PaneId,
    pub workspace: Option<WorkspaceId>,
    pub tab: Option<TabId>,
    pub terminal: Option<TerminalId>,
    pub label: Option<String>,
    pub focused: bool,
    pub agent: Option<String>,
    pub agent_status: Option<String>,
    pub revision: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LayoutRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutPane {
    pub pane: PaneId,
    pub focused: bool,
    pub rect: LayoutRect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutState {
    pub workspace: WorkspaceId,
    pub tab: TabId,
    pub zoomed: bool,
    pub area: LayoutRect,
    pub focused_pane: PaneId,
    pub panes: Vec<LayoutPane>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentState {
    pub pane: PaneId,
    pub name: Option<String>,
    pub agent: Option<String>,
    pub status: Option<String>,
    pub interactive_ready: Option<bool>,
    pub revision: Option<u64>,
}

impl NormalizedSnapshot {
    pub fn from_value(target: &TargetSession, snapshot: &Value) -> Self {
        Self {
            server_version: snapshot
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_owned),
            protocol: snapshot.get("protocol").and_then(Value::as_u64),
            revision: snapshot.get("revision").and_then(Value::as_u64),
            capabilities: capability_names(snapshot.get("capabilities")),
            counts: ResourceCounts {
                workspaces: array_len(snapshot, "workspaces"),
                tabs: array_len(snapshot, "tabs"),
                panes: array_len(snapshot, "panes"),
                agents: array_len(snapshot, "agents"),
            },
            focused_workspace: qualified_id(snapshot, "focused_workspace_id", target),
            focused_tab: qualified_id(snapshot, "focused_tab_id", target),
            focused_pane: qualified_id(snapshot, "focused_pane_id", target),
            workspaces: workspace_states(snapshot, target),
            tabs: tab_states(snapshot, target),
            panes: pane_states(snapshot, target),
            layouts: layout_states(snapshot, target),
            agents: agent_states(snapshot, target),
            terminals: qualified_ids(snapshot, "panes", "terminal_id", target),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetConnectionState {
    Connecting,
    Live,
    Backoff { attempt: u32 },
    Incompatible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetUpdateMode {
    Polling,
    Events,
}

#[derive(Debug, Clone)]
pub struct TargetRuntimeState {
    pub key: TargetSession,
    pub endpoint: String,
    pub connection: TargetConnectionState,
    pub update_mode: TargetUpdateMode,
    pub event_error: Option<String>,
    pub connection_generation: u64,
    pub selected_herdr_bin: Option<String>,
    pub snapshot: Option<Arc<NormalizedSnapshot>>,
    pub last_error: Option<String>,
    pub last_success: Option<SystemTime>,
    pub retry_at: Option<SystemTime>,
}

impl TargetRuntimeState {
    fn new(target: &Target) -> Self {
        Self {
            key: target_key(target),
            endpoint: target.endpoint().to_owned(),
            connection: TargetConnectionState::Connecting,
            update_mode: TargetUpdateMode::Polling,
            event_error: None,
            connection_generation: 0,
            selected_herdr_bin: None,
            snapshot: None,
            last_error: None,
            last_success: None,
            retry_at: None,
        }
    }

    pub fn is_stale(&self) -> bool {
        self.snapshot.is_some() && self.connection != TargetConnectionState::Live
    }

    pub fn accepts_generation(&self, generation: u64) -> bool {
        self.connection == TargetConnectionState::Live && self.connection_generation == generation
    }
}

#[derive(Debug, Clone, Default)]
pub struct FederationState {
    pub revision: u64,
    pub targets: BTreeMap<TargetSession, TargetRuntimeState>,
}

#[derive(Debug, Clone)]
pub struct SupervisorOptions {
    pub command_timeout: Duration,
    pub refresh_interval: Duration,
    pub initial_backoff: Duration,
    pub maximum_backoff: Duration,
}

impl SupervisorOptions {
    pub fn from_config(config: &Config) -> Self {
        Self {
            command_timeout: Duration::from_secs(config.transport.command_timeout_seconds),
            refresh_interval: Duration::from_secs(5),
            initial_backoff: Duration::from_secs(1),
            maximum_backoff: Duration::from_secs(30),
        }
    }
}

pub struct FederationStore {
    receiver: watch::Receiver<FederationState>,
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl FederationStore {
    pub fn start<T>(config: Config, transport: Arc<T>, options: SupervisorOptions) -> Self
    where
        T: SnapshotTransport,
    {
        let initial_state = FederationState {
            revision: 0,
            targets: config
                .targets
                .iter()
                .map(|target| (target_key(target), TargetRuntimeState::new(target)))
                .collect(),
        };
        let (updates, receiver) = watch::channel(initial_state);
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let mut tasks = Vec::with_capacity(config.targets.len());

        for target in config.targets {
            tasks.push(tokio::spawn(supervise_target(
                target,
                config.transport.clone(),
                Arc::clone(&transport),
                options.clone(),
                updates.clone(),
                shutdown_receiver.clone(),
            )));
        }

        Self {
            receiver,
            shutdown,
            tasks,
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<FederationState> {
        self.receiver.clone()
    }

    pub async fn shutdown(mut self) {
        self.shutdown.send_replace(true);
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
    }
}

impl Drop for FederationStore {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn supervise_target<T>(
    target: Target,
    transport_config: TransportConfig,
    transport: Arc<T>,
    options: SupervisorOptions,
    updates: watch::Sender<FederationState>,
    mut shutdown: watch::Receiver<bool>,
) where
    T: SnapshotTransport,
{
    let key = target_key(&target);
    let mut connection_generation = 0_u64;
    let mut connected = false;
    let mut failed_attempts = 0_u32;
    let mut change_stream: Option<Box<dyn crate::transport::ChangeStream>> = None;
    let mut subscribed_pane_ids = Vec::new();

    loop {
        let result = tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
                continue;
            }
            result = transport.snapshot(&target, &transport_config, options.command_timeout) => result,
        };

        let mut event_pane_ids = Vec::new();
        let snapshot_succeeded = result.is_ok();
        let delay = match result {
            Ok(selection) => {
                event_pane_ids = snapshot_pane_ids(&selection.snapshot);
                if !connected {
                    connection_generation = connection_generation.saturating_add(1);
                }
                connected = true;
                failed_attempts = 0;
                record_success(&updates, &key, connection_generation, selection);
                options.refresh_interval
            }
            Err(error) if error.kind == SnapshotErrorKind::Incompatible => {
                record_incompatible(&updates, &key, error);
                wait_for_shutdown(&mut shutdown).await;
                return;
            }
            Err(error) => {
                change_stream = None;
                subscribed_pane_ids.clear();
                record_update_mode(&updates, &key, TargetUpdateMode::Polling, None);
                connected = false;
                failed_attempts = failed_attempts.saturating_add(1);
                let delay = retry_delay(&options, failed_attempts);
                record_backoff(&updates, &key, failed_attempts, delay, error);
                delay
            }
        };

        if snapshot_succeeded && target.socket.is_some() {
            let refresh_deadline = Instant::now() + delay;
            if change_stream.is_none() || subscribed_pane_ids != event_pane_ids {
                change_stream = None;
                let opened = tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            return;
                        }
                        continue;
                    }
                    () = sleep_until(refresh_deadline) => continue,
                    result = transport.open_change_stream(
                        &target,
                        &transport_config,
                        options.command_timeout,
                        &event_pane_ids,
                    ) => result,
                };
                match opened {
                    Ok(opened) => {
                        change_stream = Some(opened);
                        subscribed_pane_ids.clone_from(&event_pane_ids);
                        record_update_mode(&updates, &key, TargetUpdateMode::Events, None);
                    }
                    Err(error) => {
                        record_update_mode(
                            &updates,
                            &key,
                            TargetUpdateMode::Polling,
                            Some(error.message),
                        );
                    }
                }
            }
            let Some(changes) = change_stream.as_mut() else {
                if wait_for_deadline_or_shutdown(refresh_deadline, &mut shutdown).await {
                    return;
                }
                continue;
            };
            let event_result = tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                    continue;
                }
                () = sleep_until(refresh_deadline) => continue,
                result = changes.next() => result,
            };
            let Err(error) = event_result else {
                continue;
            };
            change_stream = None;
            subscribed_pane_ids.clear();
            record_update_mode(
                &updates,
                &key,
                TargetUpdateMode::Polling,
                Some(error.message),
            );
            if wait_for_deadline_or_shutdown(refresh_deadline, &mut shutdown).await {
                return;
            }
            continue;
        }
        if wait_for_delay_or_shutdown(delay, &mut shutdown).await {
            return;
        }
    }
}

async fn wait_for_deadline_or_shutdown(
    deadline: Instant,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    tokio::select! {
        changed = shutdown.changed() => changed.is_err() || *shutdown.borrow(),
        () = sleep_until(deadline) => false,
    }
}

fn snapshot_pane_ids(snapshot: &Value) -> Vec<String> {
    collection(snapshot, "panes")
        .filter_map(|pane| pane.get("pane_id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

async fn wait_for_delay_or_shutdown(delay: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        changed = shutdown.changed() => changed.is_err() || *shutdown.borrow(),
        () = sleep(delay) => false,
    }
}

fn record_success(
    updates: &watch::Sender<FederationState>,
    key: &TargetSession,
    connection_generation: u64,
    selection: TransportSnapshot,
) {
    let snapshot = Arc::new(NormalizedSnapshot::from_value(key, &selection.snapshot));
    updates.send_modify(|state| {
        let Some(target) = state.targets.get_mut(key) else {
            return;
        };
        target.connection = TargetConnectionState::Live;
        target.connection_generation = connection_generation;
        target.selected_herdr_bin = Some(selection.herdr_bin);
        target.snapshot = Some(snapshot);
        target.last_error = None;
        target.last_success = Some(SystemTime::now());
        target.retry_at = None;
        state.revision = state.revision.saturating_add(1);
    });
}

fn record_backoff(
    updates: &watch::Sender<FederationState>,
    key: &TargetSession,
    failed_attempts: u32,
    delay: Duration,
    error: SnapshotError,
) {
    updates.send_modify(|state| {
        let Some(target) = state.targets.get_mut(key) else {
            return;
        };
        target.connection = TargetConnectionState::Backoff {
            attempt: failed_attempts,
        };
        target.last_error = Some(error.message);
        target.retry_at = SystemTime::now().checked_add(delay);
        state.revision = state.revision.saturating_add(1);
    });
}

fn record_incompatible(
    updates: &watch::Sender<FederationState>,
    key: &TargetSession,
    error: SnapshotError,
) {
    updates.send_modify(|state| {
        let Some(target) = state.targets.get_mut(key) else {
            return;
        };
        target.connection = TargetConnectionState::Incompatible;
        target.last_error = Some(error.message);
        target.retry_at = None;
        state.revision = state.revision.saturating_add(1);
    });
}

fn record_update_mode(
    updates: &watch::Sender<FederationState>,
    key: &TargetSession,
    update_mode: TargetUpdateMode,
    event_error: Option<String>,
) {
    updates.send_modify(|state| {
        let Some(target) = state.targets.get_mut(key) else {
            return;
        };
        if target.update_mode == update_mode && target.event_error == event_error {
            return;
        }
        target.update_mode = update_mode;
        target.event_error = event_error;
        state.revision = state.revision.saturating_add(1);
    });
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    while !*shutdown.borrow() && shutdown.changed().await.is_ok() {}
}

fn retry_delay(options: &SupervisorOptions, failed_attempts: u32) -> Duration {
    let exponent = failed_attempts.saturating_sub(1).min(31);
    options
        .initial_backoff
        .saturating_mul(1_u32 << exponent)
        .min(options.maximum_backoff)
}

fn target_key(target: &Target) -> TargetSession {
    TargetSession::new(&target.name, target.session_name())
}

fn array_len(value: &Value, key: &str) -> usize {
    value.get(key).and_then(Value::as_array).map_or(0, Vec::len)
}

fn qualified_ids<Resource>(
    value: &Value,
    collection: &str,
    id_key: &str,
    target: &TargetSession,
) -> BTreeSet<crate::model::QualifiedId<Resource>>
where
    Resource: Ord,
{
    value
        .get(collection)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get(id_key).and_then(Value::as_str))
        .map(|resource| crate::model::QualifiedId::new(&target.target, &target.session, resource))
        .collect()
}

fn qualified_id<Resource>(
    value: &Value,
    id_key: &str,
    target: &TargetSession,
) -> Option<crate::model::QualifiedId<Resource>> {
    value
        .get(id_key)
        .and_then(Value::as_str)
        .map(|resource| qualified(target, resource))
}

fn workspace_states(
    snapshot: &Value,
    target: &TargetSession,
) -> BTreeMap<WorkspaceId, WorkspaceState> {
    collection(snapshot, "workspaces")
        .filter_map(|item| {
            let id = item
                .get("workspace_id")
                .and_then(Value::as_str)
                .map(|resource| qualified(target, resource))?;
            Some((
                id.clone(),
                WorkspaceState {
                    id,
                    active_tab: item
                        .get("active_tab_id")
                        .and_then(Value::as_str)
                        .map(|resource| qualified(target, resource)),
                    label: optional_string(item, "label"),
                    number: optional_u64(item, "number"),
                    focused: optional_bool(item, "focused").unwrap_or(false),
                    agent_status: optional_string(item, "agent_status"),
                },
            ))
        })
        .collect()
}

fn tab_states(snapshot: &Value, target: &TargetSession) -> BTreeMap<TabId, TabState> {
    collection(snapshot, "tabs")
        .filter_map(|item| {
            let id = item
                .get("tab_id")
                .and_then(Value::as_str)
                .map(|resource| qualified(target, resource))?;
            Some((
                id.clone(),
                TabState {
                    id,
                    workspace: item
                        .get("workspace_id")
                        .and_then(Value::as_str)
                        .map(|resource| qualified(target, resource)),
                    label: optional_string(item, "label"),
                    number: optional_u64(item, "number"),
                    focused: optional_bool(item, "focused").unwrap_or(false),
                    agent_status: optional_string(item, "agent_status"),
                },
            ))
        })
        .collect()
}

fn pane_states(snapshot: &Value, target: &TargetSession) -> BTreeMap<PaneId, PaneState> {
    collection(snapshot, "panes")
        .filter_map(|item| {
            let id = item
                .get("pane_id")
                .and_then(Value::as_str)
                .map(|resource| qualified(target, resource))?;
            Some((
                id.clone(),
                PaneState {
                    id,
                    workspace: item
                        .get("workspace_id")
                        .and_then(Value::as_str)
                        .map(|resource| qualified(target, resource)),
                    tab: item
                        .get("tab_id")
                        .and_then(Value::as_str)
                        .map(|resource| qualified(target, resource)),
                    terminal: item
                        .get("terminal_id")
                        .and_then(Value::as_str)
                        .map(|resource| qualified(target, resource)),
                    label: optional_string(item, "label"),
                    focused: optional_bool(item, "focused").unwrap_or(false),
                    agent: optional_string(item, "agent"),
                    agent_status: optional_string(item, "agent_status"),
                    revision: optional_u64(item, "revision"),
                },
            ))
        })
        .collect()
}

fn layout_states(snapshot: &Value, target: &TargetSession) -> BTreeMap<TabId, LayoutState> {
    collection(snapshot, "layouts")
        .filter_map(|item| {
            let workspace = item
                .get("workspace_id")
                .and_then(Value::as_str)
                .map(|resource| qualified(target, resource))?;
            let tab = item
                .get("tab_id")
                .and_then(Value::as_str)
                .map(|resource| qualified(target, resource))?;
            let focused_pane = item
                .get("focused_pane_id")
                .and_then(Value::as_str)
                .map(|resource| qualified(target, resource))?;
            let area = layout_rect(item.get("area")?)?;
            let panes = collection(item, "panes")
                .filter_map(|pane| {
                    Some(LayoutPane {
                        pane: pane
                            .get("pane_id")
                            .and_then(Value::as_str)
                            .map(|resource| qualified(target, resource))?,
                        focused: optional_bool(pane, "focused").unwrap_or(false),
                        rect: layout_rect(pane.get("rect")?)?,
                    })
                })
                .collect();
            Some((
                tab.clone(),
                LayoutState {
                    workspace,
                    tab,
                    zoomed: optional_bool(item, "zoomed").unwrap_or(false),
                    area,
                    focused_pane,
                    panes,
                },
            ))
        })
        .collect()
}

fn layout_rect(value: &Value) -> Option<LayoutRect> {
    Some(LayoutRect {
        x: u16::try_from(value.get("x")?.as_u64()?).ok()?,
        y: u16::try_from(value.get("y")?.as_u64()?).ok()?,
        width: u16::try_from(value.get("width")?.as_u64()?).ok()?,
        height: u16::try_from(value.get("height")?.as_u64()?).ok()?,
    })
}

fn agent_states(snapshot: &Value, target: &TargetSession) -> BTreeMap<PaneId, AgentState> {
    collection(snapshot, "agents")
        .filter_map(|item| {
            let pane = item
                .get("pane_id")
                .and_then(Value::as_str)
                .map(|resource| qualified(target, resource))?;
            Some((
                pane.clone(),
                AgentState {
                    pane,
                    name: optional_string(item, "name"),
                    agent: optional_string(item, "agent"),
                    status: optional_string(item, "agent_status"),
                    interactive_ready: optional_bool(item, "interactive_ready"),
                    revision: optional_u64(item, "revision"),
                },
            ))
        })
        .collect()
}

fn collection<'a>(value: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn qualified<Resource>(
    target: &TargetSession,
    resource: &str,
) -> crate::model::QualifiedId<Resource> {
    crate::model::QualifiedId::new(&target.target, &target.session, resource)
}

fn optional_string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn optional_u64(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

fn optional_bool(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(Value::as_bool)
}

fn capability_names(value: Option<&Value>) -> BTreeSet<String> {
    match value {
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(Value::Object(values)) => values
            .iter()
            .filter(|(_, value)| value.as_bool().unwrap_or(!value.is_null()))
            .map(|(name, _)| name.clone())
            .collect(),
        _ => BTreeSet::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::json;
    use tokio::sync::{Mutex, mpsc};
    use tokio::time::timeout;

    use super::{
        FederationState, FederationStore, NormalizedSnapshot, SupervisorOptions,
        TargetConnectionState,
    };
    use crate::config::{Config, Target, TransportConfig};
    use crate::model::{PaneId, TargetSession};
    use crate::transport::{SnapshotError, SnapshotFuture, SnapshotTransport, TransportSnapshot};

    type ScriptReceiver =
        Arc<Mutex<mpsc::UnboundedReceiver<Result<TransportSnapshot, SnapshotError>>>>;

    struct FakeTransport {
        scripts: BTreeMap<TargetSession, ScriptReceiver>,
        events: BTreeMap<TargetSession, Arc<Mutex<mpsc::UnboundedReceiver<()>>>>,
    }

    impl SnapshotTransport for FakeTransport {
        fn snapshot<'a>(
            &'a self,
            target: &'a Target,
            _config: &'a TransportConfig,
            _command_timeout: Duration,
        ) -> SnapshotFuture<'a> {
            let script = self
                .scripts
                .get(&TargetSession::new(&target.name, target.session_name()))
                .cloned();
            Box::pin(async move {
                let script = script.ok_or_else(|| SnapshotError::unavailable("missing script"))?;
                script
                    .lock()
                    .await
                    .recv()
                    .await
                    .unwrap_or_else(|| Err(SnapshotError::unavailable("script exhausted")))
            })
        }

        fn open_change_stream<'a>(
            &'a self,
            target: &'a Target,
            _config: &'a TransportConfig,
            _connect_timeout: Duration,
            _pane_ids: &'a [String],
        ) -> crate::transport::OpenChangeStreamFuture<'a> {
            let events = self
                .events
                .get(&TargetSession::new(&target.name, target.session_name()))
                .cloned();
            Box::pin(async move {
                let events = events.ok_or_else(|| SnapshotError::unavailable("no event script"))?;
                Ok(Box::new(FakeChangeStream { events })
                    as Box<dyn crate::transport::ChangeStream>)
            })
        }
    }

    struct FakeChangeStream {
        events: Arc<Mutex<mpsc::UnboundedReceiver<()>>>,
    }

    impl crate::transport::ChangeStream for FakeChangeStream {
        fn next(&mut self) -> crate::transport::ChangeFuture<'_> {
            Box::pin(async move {
                self.events
                    .lock()
                    .await
                    .recv()
                    .await
                    .ok_or_else(|| SnapshotError::unavailable("event script exhausted"))
            })
        }
    }

    fn snapshot(pane: &str) -> TransportSnapshot {
        TransportSnapshot {
            herdr_bin: "herdr-test".to_owned(),
            snapshot: json!({
                "version": "test",
                "protocol": 17,
                "revision": 1,
                "capabilities": {"events": true, "terminal": false},
                "workspaces": [{"workspace_id": "w1"}],
                "tabs": [{"tab_id": "w1:t1", "workspace_id": "w1"}],
                "panes": [{"pane_id": pane, "workspace_id": "w1", "tab_id": "w1:t1", "terminal_id": "term-1"}],
                "layouts": [{
                    "workspace_id": "w1",
                    "tab_id": "w1:t1",
                    "zoomed": false,
                    "area": {"x": 0, "y": 1, "width": 100, "height": 20},
                    "focused_pane_id": pane,
                    "panes": [{
                        "pane_id": pane,
                        "focused": true,
                        "rect": {"x": 0, "y": 1, "width": 100, "height": 20}
                    }],
                    "splits": []
                }],
                "agents": [{"pane_id": pane, "agent_status": "working"}]
            }),
        }
    }

    async fn wait_for(
        receiver: &mut tokio::sync::watch::Receiver<FederationState>,
        predicate: impl Fn(&FederationState) -> bool,
    ) -> FederationState {
        timeout(Duration::from_secs(1), async {
            loop {
                let current = receiver.borrow().clone();
                if predicate(&current) {
                    return current;
                }
                receiver.changed().await.unwrap();
            }
        })
        .await
        .expect("federation state did not reach the expected condition")
    }

    #[test]
    fn normalizes_capabilities_and_qualified_resource_ids() {
        let key = TargetSession::new("ws01", "dev");
        let normalized = NormalizedSnapshot::from_value(&key, &snapshot("w1:p1").snapshot);

        assert!(normalized.capabilities.contains("events"));
        assert!(!normalized.capabilities.contains("terminal"));
        assert!(
            normalized
                .panes
                .contains_key(&PaneId::new("ws01", "dev", "w1:p1"))
        );
        assert_eq!(
            normalized
                .agents
                .get(&PaneId::new("ws01", "dev", "w1:p1"))
                .and_then(|agent| agent.status.as_deref()),
            Some("working")
        );
        assert_eq!(normalized.counts.panes, 1);
        let layout = normalized
            .layouts
            .get(&crate::model::TabId::new("ws01", "dev", "w1:t1"))
            .unwrap();
        assert_eq!(layout.area.width, 100);
        assert_eq!(layout.panes[0].pane, PaneId::new("ws01", "dev", "w1:p1"));
    }

    #[tokio::test]
    async fn isolates_targets_and_rejects_an_obsolete_generation_after_reconnect() {
        let config = Config::parse(
            r#"
                [[targets]]
                name = "ws01"
                session = "dev"

                [[targets]]
                name = "ws02"
                session = "dev"
            "#,
        )
        .unwrap();
        let first_key = TargetSession::new("ws01", "dev");
        let second_key = TargetSession::new("ws02", "dev");
        let (first_tx, first_rx) = mpsc::unbounded_channel();
        let (second_tx, second_rx) = mpsc::unbounded_channel();
        let transport = Arc::new(FakeTransport {
            scripts: BTreeMap::from([
                (first_key.clone(), Arc::new(Mutex::new(first_rx))),
                (second_key.clone(), Arc::new(Mutex::new(second_rx))),
            ]),
            events: BTreeMap::new(),
        });
        let options = SupervisorOptions {
            command_timeout: Duration::from_secs(1),
            refresh_interval: Duration::from_millis(1),
            initial_backoff: Duration::from_millis(1),
            maximum_backoff: Duration::from_millis(4),
        };

        first_tx.send(Ok(snapshot("w1:p1"))).unwrap();
        second_tx.send(Ok(snapshot("w1:p1"))).unwrap();
        let store = FederationStore::start(config, transport, options);
        let mut receiver = store.subscribe();

        let initial = wait_for(&mut receiver, |state| {
            state.targets.values().all(|target| {
                target.connection == TargetConnectionState::Live
                    && target.connection_generation == 1
            })
        })
        .await;
        assert!(
            initial.targets[&first_key]
                .snapshot
                .as_ref()
                .unwrap()
                .panes
                .contains_key(&PaneId::new("ws01", "dev", "w1:p1"))
        );
        assert!(
            initial.targets[&second_key]
                .snapshot
                .as_ref()
                .unwrap()
                .panes
                .contains_key(&PaneId::new("ws02", "dev", "w1:p1"))
        );

        first_tx
            .send(Err(SnapshotError::unavailable("target unavailable")))
            .unwrap();
        let degraded = wait_for(&mut receiver, |state| {
            matches!(
                state.targets[&first_key].connection,
                TargetConnectionState::Backoff { .. }
            )
        })
        .await;
        assert!(degraded.targets[&first_key].is_stale());
        assert_eq!(
            degraded.targets[&second_key].connection,
            TargetConnectionState::Live
        );

        second_tx.send(Ok(snapshot("w1:p2"))).unwrap();
        let second_updated = wait_for(&mut receiver, |state| {
            state.targets[&second_key]
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| {
                    snapshot
                        .panes
                        .contains_key(&PaneId::new("ws02", "dev", "w1:p2"))
                })
        })
        .await;
        assert!(second_updated.targets[&first_key].is_stale());

        first_tx.send(Ok(snapshot("w1:p3"))).unwrap();
        let reconnected = wait_for(&mut receiver, |state| {
            state.targets[&first_key].connection == TargetConnectionState::Live
                && state.targets[&first_key].connection_generation == 2
        })
        .await;
        assert!(!reconnected.targets[&first_key].accepts_generation(1));
        assert!(reconnected.targets[&first_key].accepts_generation(2));
        assert_eq!(
            reconnected.targets[&second_key].connection,
            TargetConnectionState::Live
        );

        store.shutdown().await;
    }

    #[tokio::test]
    async fn event_signal_triggers_an_immediate_authoritative_snapshot() {
        let config = Config::parse(
            r#"
                [[targets]]
                name = "ws01"
                session = "dev"
                socket = "/tmp/fake-herdr.sock"
            "#,
        )
        .unwrap();
        let key = TargetSession::new("ws01", "dev");
        let (snapshot_tx, snapshot_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let transport = Arc::new(FakeTransport {
            scripts: BTreeMap::from([(key.clone(), Arc::new(Mutex::new(snapshot_rx)))]),
            events: BTreeMap::from([(key.clone(), Arc::new(Mutex::new(event_rx)))]),
        });
        let options = SupervisorOptions {
            command_timeout: Duration::from_secs(1),
            refresh_interval: Duration::from_secs(60),
            initial_backoff: Duration::from_millis(1),
            maximum_backoff: Duration::from_millis(4),
        };

        snapshot_tx.send(Ok(snapshot("w1:p1"))).unwrap();
        let store = FederationStore::start(config, transport, options);
        let mut receiver = store.subscribe();
        wait_for(&mut receiver, |state| {
            state.targets[&key]
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| {
                    snapshot
                        .panes
                        .contains_key(&PaneId::new("ws01", "dev", "w1:p1"))
                })
        })
        .await;

        snapshot_tx.send(Ok(snapshot("w1:p2"))).unwrap();
        event_tx.send(()).unwrap();
        let updated = wait_for(&mut receiver, |state| {
            state.targets[&key]
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| {
                    snapshot
                        .panes
                        .contains_key(&PaneId::new("ws01", "dev", "w1:p2"))
                })
        })
        .await;

        assert_eq!(updated.targets[&key].connection_generation, 1);
        assert_eq!(
            updated.targets[&key].update_mode,
            super::TargetUpdateMode::Events
        );
        store.shutdown().await;
    }
}
