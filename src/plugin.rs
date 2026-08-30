//! Herdr plugin actions exposed through the documented socket API.
//!
//! Plugin and action identifiers are server-local just like pane identifiers,
//! so they remain qualified with their target and session until the final API
//! request. The command behind an action deliberately never enters this model:
//! Herdr owns execution, while Super-Herdr needs only enough metadata to let a
//! person choose an action and route it back to the same server.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::time::timeout;

use crate::config::{Target, TransportConfig};
use crate::model::{PaneId, TabId, TargetSession, WorkspaceId};
use crate::transport::{ApiSession, SnapshotError};

const MAX_PLUGIN_ACTIONS: usize = 512;
const MAX_PLUGIN_RUNS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginActionContext {
    Global,
    Workspace,
    Tab,
    Pane,
    Selection,
    /// A context introduced by a newer Herdr. Kept non-global so an older
    /// Super-Herdr hides the action instead of invoking it with the wrong
    /// resource.
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginActionId {
    pub target: TargetSession,
    pub plugin_id: String,
    pub action_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginAction {
    pub id: PluginActionId,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub contexts: Vec<PluginActionContext>,
}

/// A Herdr plugin command qualified with the server that owns its log ID.
/// Command arguments and process output deliberately never enter this model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRunId {
    pub target: TargetSession,
    pub plugin_id: String,
    pub log_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginRunStatus {
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRun {
    pub id: PluginRunId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_id: Option<String>,
    pub status: PluginRunStatus,
}

impl PluginAction {
    pub fn supports(&self, context: PluginActionContext) -> bool {
        if self.contexts.is_empty() {
            return context == PluginActionContext::Global;
        }
        self.contexts.contains(&context)
    }
}

/// The resource a person invoked an action from. Each identifier stays
/// qualified; conversion to server-local strings happens inside `invoke`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "context", rename_all = "snake_case")]
pub enum PluginInvocationTarget {
    Session { target: TargetSession },
    Workspace { workspace: WorkspaceId },
    Tab { tab: TabId },
    Pane { pane: PaneId },
}

impl PluginInvocationTarget {
    pub fn target_session(&self) -> TargetSession {
        match self {
            Self::Session { target } => target.clone(),
            Self::Workspace { workspace } => workspace.target_session(),
            Self::Tab { tab } => tab.target_session(),
            Self::Pane { pane } => pane.target_session(),
        }
    }
}

#[derive(Deserialize)]
struct ListedAction {
    plugin_id: String,
    action_id: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    contexts: Vec<String>,
}

#[derive(Deserialize)]
struct ActionList {
    #[serde(rename = "type")]
    kind: String,
    actions: Vec<ListedAction>,
}

#[derive(Deserialize)]
struct CommandLog {
    log_id: String,
    plugin_id: String,
    #[serde(default)]
    action_id: Option<String>,
    status: PluginRunStatus,
}

#[derive(Deserialize)]
struct CommandLogList {
    #[serde(rename = "type")]
    kind: String,
    logs: Vec<CommandLog>,
}

/// List actions without carrying their command arrays across the daemon
/// boundary. One malformed or oversized registry fails only this target.
pub async fn list(
    target_key: &TargetSession,
    target: &Target,
    transport: &TransportConfig,
    request_timeout: Duration,
) -> Result<Vec<PluginAction>, SnapshotError> {
    timeout(request_timeout, async {
        let mut session = ApiSession::open(target, transport, request_timeout).await?;
        let value = session.request("plugin.action.list", json!({})).await?;
        parse_list(target_key, value)
    })
    .await
    .map_err(|_| SnapshotError::timed_out(request_timeout))?
}

fn parse_list(target: &TargetSession, value: Value) -> Result<Vec<PluginAction>, SnapshotError> {
    let listed: ActionList = serde_json::from_value(value).map_err(|_| {
        SnapshotError::unavailable("Herdr returned an invalid plugin action registry")
    })?;
    if listed.kind != "plugin_action_list" {
        return Err(SnapshotError::unavailable(
            "Herdr returned an unexpected plugin action response",
        ));
    }
    if listed.actions.len() > MAX_PLUGIN_ACTIONS {
        return Err(SnapshotError::unavailable(format!(
            "Herdr returned more than {MAX_PLUGIN_ACTIONS} plugin actions"
        )));
    }
    Ok(listed
        .actions
        .into_iter()
        .map(|action| PluginAction {
            id: PluginActionId {
                target: target.clone(),
                plugin_id: action.plugin_id,
                action_id: action.action_id,
            },
            title: action.title,
            description: action.description,
            contexts: action
                .contexts
                .into_iter()
                .map(|context| match context.as_str() {
                    "global" => PluginActionContext::Global,
                    "workspace" => PluginActionContext::Workspace,
                    "tab" => PluginActionContext::Tab,
                    "pane" => PluginActionContext::Pane,
                    "selection" => PluginActionContext::Selection,
                    _ => PluginActionContext::Unsupported,
                })
                .collect(),
        })
        .collect())
}

/// Invoke one action with a context hydrated from the same Herdr server.
///
/// Herdr fills missing context fields from its own current focus. Hydrating the
/// complete workspace/tab/pane chain here prevents a click on one Super-Herdr
/// pane from accidentally inheriting a different pane that Herdr happens to
/// have focused locally. Selection text is explicitly empty: Super-Herdr does
/// not expose selection-only plugin actions and never forwards terminal text as
/// incidental plugin context.
pub async fn invoke(
    action: &PluginActionId,
    context: &PluginInvocationTarget,
    target: &Target,
    transport: &TransportConfig,
    request_timeout: Duration,
) -> Result<PluginRun, SnapshotError> {
    let context_target = context.target_session();
    if action.target != context_target {
        return Err(SnapshotError::unavailable(
            "plugin action and invocation context belong to different Herdr sessions",
        ));
    }

    timeout(request_timeout, async {
        let mut session = ApiSession::open(target, transport, request_timeout).await?;
        let context = hydrated_context(&mut session, context).await?;
        let result = session
            .request(
                "plugin.action.invoke",
                json!({
                    "plugin_id": action.plugin_id,
                    "action_id": action.action_id,
                    "context": context,
                }),
            )
            .await?;
        if result.get("type").and_then(Value::as_str) != Some("plugin_action_invoked") {
            return Err(SnapshotError::unavailable(
                "Herdr returned an unexpected plugin action result",
            ));
        }
        let log: CommandLog =
            serde_json::from_value(result.get("log").cloned().ok_or_else(|| {
                SnapshotError::unavailable("Herdr returned no plugin action run")
            })?)
            .map_err(|_| {
                SnapshotError::unavailable("Herdr returned an invalid plugin action run")
            })?;
        let run = qualify_run(&action.target, log);
        if run.id.plugin_id != action.plugin_id
            || run.action_id.as_deref() != Some(&action.action_id)
        {
            return Err(SnapshotError::unavailable(
                "Herdr returned a different plugin action run",
            ));
        }
        Ok(run)
    })
    .await
    .map_err(|_| SnapshotError::timed_out(request_timeout))?
}

/// Read one command's lifecycle without returning its command or output.
pub async fn status(
    run: &PluginRunId,
    target: &Target,
    transport: &TransportConfig,
    request_timeout: Duration,
) -> Result<PluginRun, SnapshotError> {
    timeout(request_timeout, async {
        let mut session = ApiSession::open(target, transport, request_timeout).await?;
        let value = session
            .request(
                "plugin.log.list",
                json!({
                    "plugin_id": run.plugin_id,
                    "limit": MAX_PLUGIN_RUNS,
                }),
            )
            .await?;
        let listed: CommandLogList = serde_json::from_value(value)
            .map_err(|_| SnapshotError::unavailable("Herdr returned invalid plugin run state"))?;
        if listed.kind != "plugin_log_list" {
            return Err(SnapshotError::unavailable(
                "Herdr returned an unexpected plugin run response",
            ));
        }
        listed
            .logs
            .into_iter()
            .find(|candidate| {
                candidate.log_id == run.log_id && candidate.plugin_id == run.plugin_id
            })
            .map(|log| qualify_run(&run.target, log))
            .ok_or_else(|| SnapshotError::unavailable("Herdr no longer has that plugin run"))
    })
    .await
    .map_err(|_| SnapshotError::timed_out(request_timeout))?
}

fn qualify_run(target: &TargetSession, log: CommandLog) -> PluginRun {
    PluginRun {
        id: PluginRunId {
            target: target.clone(),
            plugin_id: log.plugin_id,
            log_id: log.log_id,
        },
        action_id: log.action_id,
        status: log.status,
    }
}

async fn hydrated_context(
    session: &mut ApiSession,
    target: &PluginInvocationTarget,
) -> Result<Value, SnapshotError> {
    let requested_pane = match target {
        PluginInvocationTarget::Pane { pane } => Some(pane.server_local_id().to_owned()),
        _ => None,
    };
    let requested_tab = match target {
        PluginInvocationTarget::Tab { tab } => Some(tab.server_local_id().to_owned()),
        _ => None,
    };
    let requested_workspace = match target {
        PluginInvocationTarget::Workspace { workspace } => {
            Some(workspace.server_local_id().to_owned())
        }
        _ => None,
    };

    let mut pane = match (target, requested_pane.as_deref()) {
        (_, Some(pane_id)) => Some(response_object(
            session
                .request("pane.get", json!({ "pane_id": pane_id }))
                .await?,
            "pane_info",
            "pane",
        )?),
        (PluginInvocationTarget::Session { .. }, None) => Some(response_object(
            session.request("pane.current", json!({})).await?,
            "pane_current",
            "pane",
        )?),
        (_, None) => None,
    };
    let mut tab_id = requested_tab.or_else(|| string_field(pane.as_ref(), "tab_id"));
    let mut workspace_id =
        requested_workspace.or_else(|| string_field(pane.as_ref(), "workspace_id"));

    let mut tab = match tab_id.as_deref() {
        Some(tab_id) => Some(response_object(
            session
                .request("tab.get", json!({ "tab_id": tab_id }))
                .await?,
            "tab_info",
            "tab",
        )?),
        None => None,
    };
    workspace_id = workspace_id.or_else(|| string_field(tab.as_ref(), "workspace_id"));

    let workspace = match workspace_id.as_deref() {
        Some(workspace_id) => Some(response_object(
            session
                .request("workspace.get", json!({ "workspace_id": workspace_id }))
                .await?,
            "workspace_info",
            "workspace",
        )?),
        None => None,
    };

    if tab_id.is_none() {
        tab_id = string_field(workspace.as_ref(), "active_tab_id");
        if let Some(active_tab_id) = tab_id.as_deref() {
            tab = Some(response_object(
                session
                    .request("tab.get", json!({ "tab_id": active_tab_id }))
                    .await?,
                "tab_info",
                "tab",
            )?);
        }
    }

    if pane.is_none()
        && let Some(workspace_id) = workspace_id.as_deref()
    {
        let listed = session
            .request("pane.list", json!({ "workspace_id": workspace_id }))
            .await?;
        if listed.get("type").and_then(Value::as_str) != Some("pane_list") {
            return Err(SnapshotError::unavailable(
                "Herdr returned an unexpected pane list while building plugin context",
            ));
        }
        pane = listed
            .get("panes")
            .and_then(Value::as_array)
            .and_then(|panes| {
                panes
                    .iter()
                    .filter(|candidate| {
                        tab_id.as_deref().is_none_or(|tab_id| {
                            candidate.get("tab_id").and_then(Value::as_str) == Some(tab_id)
                        })
                    })
                    .find(|candidate| {
                        candidate
                            .get("focused")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                    })
                    .or_else(|| {
                        panes.iter().find(|candidate| {
                            tab_id.as_deref().is_none_or(|tab_id| {
                                candidate.get("tab_id").and_then(Value::as_str) == Some(tab_id)
                            })
                        })
                    })
                    .and_then(Value::as_object)
                    .cloned()
            });
    }

    ensure_requested(
        "pane",
        requested_pane.as_deref(),
        pane.as_ref()
            .and_then(|value| value.get("pane_id"))
            .and_then(Value::as_str),
    )?;
    ensure_requested("tab", tab_id.as_deref(), string_ref(tab.as_ref(), "tab_id"))?;
    ensure_requested(
        "workspace",
        workspace_id.as_deref(),
        string_ref(workspace.as_ref(), "workspace_id"),
    )?;
    if workspace_id.is_some() && pane.is_none() {
        return Err(SnapshotError::unavailable(
            "Herdr returned no pane in the selected plugin context",
        ));
    }

    let pane_cwd = pane.as_ref().and_then(|pane| {
        pane.get("foreground_cwd")
            .and_then(Value::as_str)
            .or_else(|| pane.get("cwd").and_then(Value::as_str))
    });
    let worktree = workspace
        .as_ref()
        .and_then(|workspace| workspace.get("worktree"))
        .filter(|worktree| !worktree.is_null())
        .cloned();
    let workspace_cwd = worktree
        .as_ref()
        .and_then(|worktree| worktree.get("checkout_path"))
        .and_then(Value::as_str)
        .or(pane_cwd);

    let mut context = Map::new();
    insert_optional(&mut context, "workspace_id", workspace_id.as_deref());
    insert_optional(
        &mut context,
        "workspace_label",
        string_ref(workspace.as_ref(), "label"),
    );
    insert_optional(&mut context, "workspace_cwd", workspace_cwd);
    if let Some(worktree) = worktree {
        context.insert("worktree".to_owned(), worktree);
    }
    insert_optional(&mut context, "tab_id", tab_id.as_deref());
    insert_optional(&mut context, "tab_label", string_ref(tab.as_ref(), "label"));
    insert_optional(
        &mut context,
        "focused_pane_id",
        string_ref(pane.as_ref(), "pane_id"),
    );
    insert_optional(&mut context, "focused_pane_cwd", pane_cwd);
    insert_optional(
        &mut context,
        "focused_pane_agent",
        string_ref(pane.as_ref(), "display_agent").or_else(|| string_ref(pane.as_ref(), "agent")),
    );
    if let Some(status) = pane
        .as_ref()
        .and_then(|pane| pane.get("agent_status"))
        .filter(|status| !status.is_null())
    {
        context.insert("focused_pane_status".to_owned(), status.clone());
    }
    context.insert(
        "invocation_source".to_owned(),
        Value::String("super-herdr".to_owned()),
    );
    context.insert("selected_text".to_owned(), Value::String(String::new()));
    context.insert("clicked_url".to_owned(), Value::String(String::new()));
    context.insert("link_handler_id".to_owned(), Value::String(String::new()));
    Ok(Value::Object(context))
}

fn response_object(
    value: Value,
    expected_type: &str,
    field: &str,
) -> Result<Map<String, Value>, SnapshotError> {
    if value.get("type").and_then(Value::as_str) != Some(expected_type) {
        return Err(SnapshotError::unavailable(format!(
            "Herdr returned an unexpected {expected_type} response"
        )));
    }
    value
        .get(field)
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| {
            SnapshotError::unavailable(format!("Herdr returned invalid {expected_type} data"))
        })
}

fn ensure_requested(
    kind: &str,
    requested: Option<&str>,
    returned: Option<&str>,
) -> Result<(), SnapshotError> {
    if requested.is_some_and(|requested| returned != Some(requested)) {
        return Err(SnapshotError::unavailable(format!(
            "Herdr returned a different {kind} while building plugin context"
        )));
    }
    Ok(())
}

fn string_field(value: Option<&Map<String, Value>>, field: &str) -> Option<String> {
    string_ref(value, field).map(str::to_owned)
}

fn string_ref<'a>(value: Option<&'a Map<String, Value>>, field: &str) -> Option<&'a str> {
    value?.get(field)?.as_str()
}

fn insert_optional(context: &mut Map<String, Value>, field: &str, value: Option<&str>) {
    if let Some(value) = value {
        context.insert(field.to_owned(), Value::String(value.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        MAX_PLUGIN_RUNS, PluginActionContext, PluginActionId, PluginInvocationTarget,
        PluginRunStatus, invoke, parse_list, status,
    };
    use crate::model::{PaneId, TargetSession};

    #[test]
    fn listed_actions_are_qualified_and_commands_do_not_cross_the_boundary() {
        let target = TargetSession::new("build", "work");
        let actions = parse_list(
            &target,
            json!({
                "type": "plugin_action_list",
                "actions": [{
                    "plugin_id": "herdr-workflows",
                    "action_id": "run",
                    "title": "Run workflow",
                    "description": "Choose a workflow",
                    "contexts": ["workspace", "pane"],
                    "platforms": null,
                    "command": ["secret-command", "--never-forward-this"]
                }]
            }),
        )
        .expect("valid registry");

        assert_eq!(actions[0].id.target, target);
        assert!(actions[0].supports(PluginActionContext::Pane));
        let encoded = serde_json::to_string(&actions).expect("actions encode");
        assert!(!encoded.contains("secret-command"));
        assert!(!encoded.contains("never-forward-this"));
    }

    #[test]
    fn an_action_without_contexts_is_global() {
        let actions = parse_list(
            &TargetSession::new("local", "default"),
            json!({
                "type": "plugin_action_list",
                "actions": [{
                    "plugin_id": "one",
                    "action_id": "two",
                    "title": "Three",
                    "command": ["true"]
                }]
            }),
        )
        .expect("valid registry");

        assert!(actions[0].supports(PluginActionContext::Global));
        assert!(!actions[0].supports(PluginActionContext::Selection));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn invocation_hydrates_the_exact_qualified_pane_without_selection_text() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        use crate::config::Config;

        let directory = tempfile::tempdir().expect("temporary directory");
        let socket = directory.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).expect("test socket");
        let server = tokio::spawn(async move {
            for method in [
                "pane.get",
                "tab.get",
                "workspace.get",
                "plugin.action.invoke",
                "plugin.log.list",
            ] {
                let (stream, _) = listener.accept().await.expect("request connection");
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.expect("request line");
                let request: serde_json::Value = serde_json::from_str(&line).expect("JSON request");
                assert_eq!(request["method"], method);
                let id = request["id"].as_str().expect("request id");
                let result = match method {
                    "pane.get" => {
                        assert_eq!(request["params"]["pane_id"], "w2:p3");
                        json!({
                            "type": "pane_info",
                            "pane": {
                                "pane_id": "w2:p3",
                                "workspace_id": "w2",
                                "tab_id": "w2:t1",
                                "foreground_cwd": "/work/repo/subdir",
                                "display_agent": "codex",
                                "agent_status": "working"
                            }
                        })
                    }
                    "tab.get" => {
                        assert_eq!(request["params"]["tab_id"], "w2:t1");
                        json!({
                            "type": "tab_info",
                            "tab": {
                                "tab_id": "w2:t1",
                                "workspace_id": "w2",
                                "label": "implementation"
                            }
                        })
                    }
                    "workspace.get" => {
                        assert_eq!(request["params"]["workspace_id"], "w2");
                        json!({
                            "type": "workspace_info",
                            "workspace": {
                                "workspace_id": "w2",
                                "label": "super-herdr",
                                "active_tab_id": "w2:t1",
                                "worktree": {
                                    "checkout_path": "/work/repo",
                                    "repo_key": "repo",
                                    "repo_name": "repo",
                                    "repo_root": "/work/repo",
                                    "is_linked_worktree": false
                                }
                            }
                        })
                    }
                    "plugin.action.invoke" => {
                        let params = &request["params"];
                        assert_eq!(params["plugin_id"], "herdr-workflows");
                        assert_eq!(params["action_id"], "run");
                        assert_eq!(params["context"]["workspace_id"], "w2");
                        assert_eq!(params["context"]["tab_id"], "w2:t1");
                        assert_eq!(params["context"]["focused_pane_id"], "w2:p3");
                        assert_eq!(params["context"]["workspace_cwd"], "/work/repo");
                        assert_eq!(params["context"]["focused_pane_cwd"], "/work/repo/subdir");
                        assert_eq!(params["context"]["selected_text"], "");
                        assert_eq!(params["context"]["invocation_source"], "super-herdr");
                        json!({
                            "type": "plugin_action_invoked",
                            "log": {
                                "log_id": "log-7",
                                "plugin_id": "herdr-workflows",
                                "action_id": "run",
                                "status": "running",
                                "started_unix_ms": 1,
                                "command": ["secret-command"],
                                "stdout": "terminal contents do not cross",
                                "stderr": null,
                                "error": null
                            }
                        })
                    }
                    "plugin.log.list" => {
                        assert_eq!(request["params"]["plugin_id"], "herdr-workflows");
                        assert_eq!(request["params"]["limit"], MAX_PLUGIN_RUNS);
                        json!({
                            "type": "plugin_log_list",
                            "logs": [{
                                "log_id": "log-7",
                                "plugin_id": "herdr-workflows",
                                "action_id": "run",
                                "status": "succeeded",
                                "started_unix_ms": 1,
                                "finished_unix_ms": 2,
                                "command": ["secret-command"],
                                "stdout": "terminal contents do not cross",
                                "stderr": "nor does stderr",
                                "error": null
                            }]
                        })
                    }
                    _ => unreachable!(),
                };
                reader
                    .get_mut()
                    .write_all(format!("{{\"id\":{id:?},\"result\":{result}}}\n").as_bytes())
                    .await
                    .expect("response");
            }
        });
        let config = Config::parse(&format!(
            "[[targets]]\nname = \"local\"\nsession = \"work\"\nsocket = {:?}\n",
            socket.display().to_string()
        ))
        .expect("test config");
        let target = TargetSession::new("local", "work");

        let run = invoke(
            &PluginActionId {
                target: target.clone(),
                plugin_id: "herdr-workflows".to_owned(),
                action_id: "run".to_owned(),
            },
            &PluginInvocationTarget::Pane {
                pane: PaneId::new("local", "work", "w2:p3"),
            },
            &config.targets[0],
            &config.transport,
            std::time::Duration::from_secs(1),
        )
        .await
        .expect("action invocation");
        assert_eq!(run.id.target, target);
        assert_eq!(run.id.log_id, "log-7");
        assert_eq!(run.status, PluginRunStatus::Running);
        let encoded = serde_json::to_string(&run).expect("run encodes");
        assert!(!encoded.contains("secret-command"));
        assert!(!encoded.contains("terminal contents"));
        let finished = status(
            &run.id,
            &config.targets[0],
            &config.transport,
            std::time::Duration::from_secs(1),
        )
        .await
        .expect("run status");
        assert_eq!(finished.status, PluginRunStatus::Succeeded);
        let encoded = serde_json::to_string(&finished).expect("status encodes");
        assert!(!encoded.contains("secret-command"));
        assert!(!encoded.contains("stderr"));
        server.await.expect("fake Herdr server");
    }
}
