//! Relocate a whole Herdr workspace, either as a live move or as an explicit
//! recreation on another session.
//!
//! Herdr's documented API moves live panes inside one server: `layout.export`
//! describes a tab's split tree, and `pane.move` relocates one pane without
//! restarting its process. A move turns an exported tree into the ordered
//! `pane.move` requests that rebuild the same tree in the destination workspace.
//!
//! Panes belong to one server process and protocol 19 has no cross-session
//! transfer, so nothing can be moved between sessions. Crossing that boundary is
//! offered only as a recreation, which rebuilds the structure with fresh shells
//! and says so; it is never presented as a move.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::{Target, TransportConfig};
use crate::resource_action::SplitDirection;
use crate::transport::ApiSession;

/// Bounded recursion guard for an exported layout tree.
const MAX_LAYOUT_DEPTH: usize = 64;

/// One `pane.move` request that re-creates a split around an already placed
/// pane. `target_pane` names the source-side pane; execution translates it to
/// the identifier Herdr assigned after that pane moved.
#[derive(Debug, Clone, PartialEq)]
pub struct SplitMove {
    pub pane: String,
    pub target_pane: String,
    pub direction: SplitDirection,
    pub ratio: Option<f64>,
}

/// A tab's exported layout reduced to the one pane that opens the destination
/// tab plus the ordered splits that rebuild every remaining pane around it.
#[derive(Debug, Clone, PartialEq)]
pub struct TabMovePlan {
    pub anchor_pane: String,
    pub splits: Vec<SplitMove>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkspaceMoveSummary {
    pub tabs: usize,
    pub panes: usize,
    pub source_closed: bool,
}

/// Reduce one exported layout tree to a move plan.
///
/// The plan is emitted top-down so that every split acts on a region holding
/// exactly one pane: the destination tab starts as the tree's leftmost pane,
/// each split places the second subtree's leftmost pane against the first
/// subtree's leftmost pane, and both subtrees are then rebuilt inside the
/// regions that split created.
pub fn plan_tab_move(root: &Value) -> Result<TabMovePlan, String> {
    let anchor_pane = leftmost_pane(root, 0)?;
    let mut splits = Vec::new();
    collect_splits(root, 0, &mut splits)?;
    Ok(TabMovePlan {
        anchor_pane,
        splits,
    })
}

fn leftmost_pane(node: &Value, depth: usize) -> Result<String, String> {
    if depth > MAX_LAYOUT_DEPTH {
        return Err("exported layout is nested too deeply".to_owned());
    }
    match node.get("type").and_then(Value::as_str) {
        Some("pane") => node
            .get("pane_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "exported layout pane has no identifier".to_owned()),
        Some("split") => {
            let first = node
                .get("first")
                .ok_or_else(|| "exported layout split has no first child".to_owned())?;
            leftmost_pane(first, depth + 1)
        }
        _ => Err("exported layout contained an unsupported node".to_owned()),
    }
}

fn collect_splits(node: &Value, depth: usize, splits: &mut Vec<SplitMove>) -> Result<(), String> {
    if depth > MAX_LAYOUT_DEPTH {
        return Err("exported layout is nested too deeply".to_owned());
    }
    match node.get("type").and_then(Value::as_str) {
        Some("pane") => Ok(()),
        Some("split") => {
            let first = node
                .get("first")
                .ok_or_else(|| "exported layout split has no first child".to_owned())?;
            let second = node
                .get("second")
                .ok_or_else(|| "exported layout split has no second child".to_owned())?;
            let direction = node
                .get("direction")
                .and_then(Value::as_str)
                .and_then(SplitDirection::from_api_value)
                .ok_or_else(|| "exported layout split has an unsupported direction".to_owned())?;
            splits.push(SplitMove {
                pane: leftmost_pane(second, depth + 1)?,
                target_pane: leftmost_pane(first, depth + 1)?,
                direction,
                ratio: node.get("ratio").and_then(Value::as_f64),
            });
            collect_splits(first, depth + 1, splits)?;
            collect_splits(second, depth + 1, splits)
        }
        _ => Err("exported layout contained an unsupported node".to_owned()),
    }
}

/// One source tab as Herdr exported it: its label and its split tree.
#[derive(Debug, Clone, PartialEq)]
struct ExportedTab {
    label: Option<String>,
    root: Value,
}

/// Read every tab of one workspace and its split tree before anything is
/// changed, so a move or a recreation works from one consistent description.
async fn export_workspace_tabs(
    session: &mut ApiSession,
    workspace: &str,
) -> Result<Vec<ExportedTab>, String> {
    let listed = session
        .request("tab.list", json!({ "workspace_id": workspace }))
        .await
        .map_err(|error| error.message)?;
    let tabs = listed
        .get("tabs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if tabs.is_empty() {
        return Err("the source workspace has no tabs".to_owned());
    }

    let mut exported = Vec::with_capacity(tabs.len());
    for tab in tabs {
        let tab_id = tab
            .get("tab_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "Herdr listed a tab without an identifier".to_owned())?;
        let label = tab
            .get("label")
            .and_then(Value::as_str)
            .filter(|label| !label.is_empty())
            .map(str::to_owned);
        let layout = session
            .request("layout.export", json!({ "tab_id": tab_id }))
            .await
            .map_err(|error| error.message)?;
        let root = layout
            .get("layout")
            .and_then(|layout| layout.get("root"))
            .cloned()
            .ok_or_else(|| "Herdr exported a tab without a layout".to_owned())?;
        exported.push(ExportedTab { label, root });
    }
    Ok(exported)
}

/// Move every tab of `source_workspace` into `destination_workspace` on one
/// Herdr session. Both identifiers are server-local; the caller keeps them
/// qualified. Panes keep their processes and scrollback, so a failure part way
/// through leaves the already moved tabs in the destination and the rest in the
/// source instead of destroying anything.
pub async fn move_workspace(
    target: &Target,
    config: &TransportConfig,
    source_workspace: &str,
    destination_workspace: &str,
    request_timeout: Duration,
) -> Result<WorkspaceMoveSummary, String> {
    if source_workspace == destination_workspace {
        return Err("a workspace cannot be moved into itself".to_owned());
    }
    let mut session = ApiSession::open(target, config, request_timeout)
        .await
        .map_err(|error| error.message)?;
    let exported = export_workspace_tabs(&mut session, source_workspace).await?;

    let mut summary = WorkspaceMoveSummary::default();
    let mut moved_ids: BTreeMap<String, String> = BTreeMap::new();
    for tab in &exported {
        let label = tab
            .label
            .as_deref()
            .map_or(Value::Null, |label| Value::String(label.to_owned()));
        let plan = plan_tab_move(&tab.root)?;

        let opened = session
            .request(
                "pane.move",
                json!({
                    "pane_id": plan.anchor_pane,
                    "destination": {
                        "type": "new_tab",
                        "workspace_id": destination_workspace,
                        "label": label,
                    },
                }),
            )
            .await
            .map_err(|error| error.message)?;
        record_move(&plan.anchor_pane, &opened, &mut moved_ids, &mut summary);
        let created_tab = opened
            .get("move_result")
            .and_then(|result| result.get("created_tab"))
            .and_then(|tab| tab.get("tab_id"))
            .and_then(Value::as_str)
            .ok_or_else(|| "Herdr did not report the created destination tab".to_owned())?
            .to_owned();
        summary.tabs = summary.tabs.saturating_add(1);

        for split in &plan.splits {
            let target_pane = moved_ids
                .get(&split.target_pane)
                .cloned()
                .unwrap_or_else(|| split.target_pane.clone());
            let placed = session
                .request(
                    "pane.move",
                    json!({
                        "pane_id": split.pane,
                        "destination": {
                            "type": "tab",
                            "tab_id": created_tab,
                            "split": split.direction.cli_value(),
                            "target_pane_id": target_pane,
                            "ratio": split.ratio,
                        },
                    }),
                )
                .await
                .map_err(|error| error.message)?;
            record_move(&split.pane, &placed, &mut moved_ids, &mut summary);
        }
    }
    Ok(summary)
}

/// Bound on how many panes one recreation may start on the destination, so a
/// mistaken destination cannot launch an unbounded number of shells.
const MAX_RECREATED_PANES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRecreateSummary {
    pub workspace: String,
    pub tabs: usize,
    pub panes: usize,
}

/// Recreate a workspace on another Herdr session, which may be on another host.
///
/// Herdr cannot transfer live panes between sessions, so this is explicitly not
/// a move: it starts fresh shells. Only the tab and split structure, the split
/// ratios, the pane labels, and the recorded working directories cross the
/// boundary. Pane commands and environment are deliberately not replayed—
/// recreating a workspace must not run a program or carry a secret onto another
/// machine. The source workspace keeps running and is never closed here.
pub async fn recreate_workspace(
    source: &Target,
    destination: &Target,
    config: &TransportConfig,
    source_workspace: &str,
    label: Option<&str>,
    request_timeout: Duration,
) -> Result<WorkspaceRecreateSummary, String> {
    let mut source_session = ApiSession::open(source, config, request_timeout)
        .await
        .map_err(|error| error.message)?;
    let exported = export_workspace_tabs(&mut source_session, source_workspace).await?;

    let (layouts, panes) = recreatable_layouts(&exported)?;

    let mut destination_session = ApiSession::open(destination, config, request_timeout)
        .await
        .map_err(|error| error.message)?;
    let created = destination_session
        .request(
            "workspace.create",
            json!({ "label": label, "focus": false }),
        )
        .await
        .map_err(|error| error.message)?;
    let workspace = created
        .get("workspace")
        .and_then(|workspace| workspace.get("workspace_id"))
        .and_then(Value::as_str)
        .ok_or_else(|| "Herdr did not report the created destination workspace".to_owned())?
        .to_owned();
    let first_tab = created
        .get("tab")
        .and_then(|tab| tab.get("tab_id"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    let total = layouts.len();
    let mut tabs = 0_usize;
    for (index, tab) in layouts.into_iter().enumerate() {
        let RecreatableTab { label, root } = tab;
        let params = match first_tab.as_deref() {
            // The created workspace already owns one empty tab; the first
            // exported layout is applied into it instead of beside it.
            Some(tab_id) if index == 0 => json!({
                "tab_id": tab_id,
                "tab_label": label,
                "root": root,
                "focus": false,
            }),
            _ => json!({
                "workspace_id": workspace,
                "tab_label": label,
                "root": root,
                "focus": false,
            }),
        };
        destination_session
            .request("layout.apply", params)
            .await
            .map_err(|error| {
                format!(
                    "{}; workspace {workspace} on the destination holds {tabs} of {total} recreated tab(s)",
                    error.message
                )
            })?;
        tabs = tabs.saturating_add(1);
    }
    Ok(WorkspaceRecreateSummary {
        workspace,
        tabs,
        panes,
    })
}

/// One destination tab: its label and the sanitized layout to apply there.
#[derive(Debug, Clone, PartialEq)]
struct RecreatableTab {
    label: Option<String>,
    root: Value,
}

/// Reduce every exported tab to a recreatable layout, refusing a workspace that
/// would start more shells than the documented bound allows.
fn recreatable_layouts(exported: &[ExportedTab]) -> Result<(Vec<RecreatableTab>, usize), String> {
    let mut panes = 0_usize;
    let mut layouts = Vec::with_capacity(exported.len());
    for tab in exported {
        let (root, tab_panes) = recreatable_layout(&tab.root, 0)?;
        panes = panes.saturating_add(tab_panes);
        if panes > MAX_RECREATED_PANES {
            return Err(format!(
                "recreating this workspace would start more than {MAX_RECREATED_PANES} panes"
            ));
        }
        layouts.push(RecreatableTab {
            label: tab.label.clone(),
            root,
        });
    }
    Ok((layouts, panes))
}

/// Reduce an exported tree to what may be recreated on another server: the
/// structure, the ratios, the labels, and the working directories. Identifiers
/// belong to the source server, and commands and environment are dropped rather
/// than replayed.
fn recreatable_layout(node: &Value, depth: usize) -> Result<(Value, usize), String> {
    if depth > MAX_LAYOUT_DEPTH {
        return Err("exported layout is nested too deeply".to_owned());
    }
    match node.get("type").and_then(Value::as_str) {
        Some("pane") => {
            let mut pane = serde_json::Map::new();
            pane.insert("type".to_owned(), Value::String("pane".to_owned()));
            if let Some(cwd) = node
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|cwd| !cwd.is_empty())
            {
                pane.insert("cwd".to_owned(), Value::String(cwd.to_owned()));
            }
            if let Some(label) = node
                .get("label")
                .and_then(Value::as_str)
                .filter(|label| !label.is_empty())
            {
                pane.insert("label".to_owned(), Value::String(label.to_owned()));
            }
            Ok((Value::Object(pane), 1))
        }
        Some("split") => {
            let direction = node
                .get("direction")
                .and_then(Value::as_str)
                .and_then(SplitDirection::from_api_value)
                .ok_or_else(|| "exported layout split has an unsupported direction".to_owned())?;
            let first = node
                .get("first")
                .ok_or_else(|| "exported layout split has no first child".to_owned())?;
            let second = node
                .get("second")
                .ok_or_else(|| "exported layout split has no second child".to_owned())?;
            let (first, first_panes) = recreatable_layout(first, depth + 1)?;
            let (second, second_panes) = recreatable_layout(second, depth + 1)?;
            Ok((
                json!({
                    "type": "split",
                    "direction": direction.cli_value(),
                    "ratio": node.get("ratio").and_then(Value::as_f64).unwrap_or(0.5),
                    "first": first,
                    "second": second,
                }),
                first_panes.saturating_add(second_panes),
            ))
        }
        _ => Err("exported layout contained an unsupported node".to_owned()),
    }
}

/// Herdr re-qualifies a pane identifier when the pane changes workspace, so the
/// next split in the same tab must target the identifier the move returned.
fn record_move(
    source_pane: &str,
    response: &Value,
    moved_ids: &mut BTreeMap<String, String>,
    summary: &mut WorkspaceMoveSummary,
) {
    let result = response.get("move_result");
    if let Some(moved) = result
        .and_then(|result| result.get("pane"))
        .and_then(|pane| pane.get("pane_id"))
        .and_then(Value::as_str)
    {
        moved_ids.insert(source_pane.to_owned(), moved.to_owned());
    }
    if result
        .and_then(|result| result.get("closed_workspace_id"))
        .and_then(Value::as_str)
        .is_some()
    {
        summary.source_closed = true;
    }
    summary.panes = summary.panes.saturating_add(1);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use tokio::task::JoinHandle;

    use super::{ExportedTab, plan_tab_move, record_move, recreatable_layout, recreatable_layouts};
    use crate::resource_action::SplitDirection;

    /// A Herdr-shaped API server: one request per connection, answered in the
    /// scripted order, recording what it was asked for.
    fn serve(
        listener: UnixListener,
        responses: Vec<serde_json::Value>,
    ) -> JoinHandle<Vec<(String, serde_json::Value)>> {
        tokio::spawn(async move {
            let mut requests = Vec::with_capacity(responses.len());
            for result in responses {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                assert!(reader.read_line(&mut line).await.unwrap() > 0);
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                let id = request["id"].as_str().unwrap().to_owned();
                requests.push((
                    request["method"].as_str().unwrap().to_owned(),
                    request["params"].clone(),
                ));
                reader
                    .get_mut()
                    .write_all(format!("{{\"id\":{id:?},\"result\":{result}}}\n").as_bytes())
                    .await
                    .unwrap();
            }
            requests
        })
    }

    #[test]
    fn a_single_pane_tab_needs_no_splits() {
        let plan = plan_tab_move(&json!({"type": "pane", "pane_id": "w1:p1"})).unwrap();

        assert_eq!(plan.anchor_pane, "w1:p1");
        assert!(plan.splits.is_empty());
    }

    #[test]
    fn a_nested_layout_rebuilds_every_region_top_down() {
        let plan = plan_tab_move(&json!({
            "type": "split",
            "direction": "right",
            "ratio": 0.5,
            "first": {
                "type": "split",
                "direction": "down",
                "ratio": 0.25,
                "first": {"type": "pane", "pane_id": "w1:p1"},
                "second": {"type": "pane", "pane_id": "w1:p2"},
            },
            "second": {"type": "pane", "pane_id": "w1:p3"},
        }))
        .unwrap();

        assert_eq!(plan.anchor_pane, "w1:p1");
        let splits = plan
            .splits
            .iter()
            .map(|split| {
                (
                    split.pane.as_str(),
                    split.target_pane.as_str(),
                    split.direction,
                    split.ratio,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            splits,
            vec![
                ("w1:p3", "w1:p1", SplitDirection::Right, Some(0.5)),
                ("w1:p2", "w1:p1", SplitDirection::Down, Some(0.25)),
            ]
        );
    }

    #[test]
    fn an_unsupported_layout_node_is_rejected() {
        let error = plan_tab_move(&json!({"type": "grid", "panes": []})).unwrap_err();
        assert!(error.contains("unsupported node"));

        let error = plan_tab_move(&json!({
            "type": "split",
            "direction": "diagonal",
            "ratio": 0.5,
            "first": {"type": "pane", "pane_id": "w1:p1"},
            "second": {"type": "pane", "pane_id": "w1:p2"},
        }))
        .unwrap_err();
        assert!(error.contains("unsupported direction"));
    }

    #[tokio::test]
    async fn a_moved_workspace_rebuilds_each_tab_against_re_qualified_panes() {
        use super::move_workspace;
        use crate::config::Config;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = serve(
            listener,
            vec![
                json!({
                    "type": "tab_list",
                    "tabs": [{"tab_id": "w1:t1", "label": "build"}],
                }),
                json!({
                    "type": "layout_export",
                    "layout": {"root": {
                        "type": "split",
                        "direction": "right",
                        "ratio": 0.4,
                        "first": {"type": "pane", "pane_id": "w1:p1"},
                        "second": {"type": "pane", "pane_id": "w1:p2"},
                    }},
                }),
                json!({
                    "type": "pane_move",
                    "move_result": {
                        "pane": {"pane_id": "w9:p1"},
                        "created_tab": {"tab_id": "w9:t4"},
                    },
                }),
                json!({
                    "type": "pane_move",
                    "move_result": {
                        "pane": {"pane_id": "w9:p2"},
                        "closed_workspace_id": "w1",
                    },
                }),
            ],
        );

        let config = Config::parse(&format!(
            "[[targets]]\nname = \"local\"\nsocket = {:?}\n",
            socket.display().to_string()
        ))
        .unwrap();
        let summary = move_workspace(
            &config.targets[0],
            &config.transport,
            "w1",
            "w9",
            Duration::from_secs(5),
        )
        .await
        .unwrap();

        assert_eq!(summary.tabs, 1);
        assert_eq!(summary.panes, 2);
        assert!(summary.source_closed);

        let requests = server.await.unwrap();
        let methods = requests
            .iter()
            .map(|(method, _)| method.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            methods,
            vec!["tab.list", "layout.export", "pane.move", "pane.move"]
        );
        assert_eq!(requests[0].1["workspace_id"], "w1");
        assert_eq!(requests[1].1["tab_id"], "w1:t1");
        assert_eq!(
            requests[2].1,
            json!({
                "pane_id": "w1:p1",
                "destination": {
                    "type": "new_tab",
                    "workspace_id": "w9",
                    "label": "build",
                },
            })
        );
        assert_eq!(
            requests[3].1,
            json!({
                "pane_id": "w1:p2",
                "destination": {
                    "type": "tab",
                    "tab_id": "w9:t4",
                    "split": "right",
                    "target_pane_id": "w9:p1",
                    "ratio": 0.4,
                },
            })
        );
    }

    #[test]
    fn a_recreated_layout_keeps_structure_and_drops_commands_and_environment() {
        let (root, panes) = recreatable_layout(
            &json!({
                "type": "split",
                "direction": "down",
                "first": {
                    "type": "pane",
                    "pane_id": "w1:p1",
                    "cwd": "/src",
                    "label": "editor",
                    "command": ["cargo", "watch"],
                    "env": {"TOKEN": "secret"},
                },
                "second": {"type": "pane", "pane_id": "w1:p2", "cwd": ""},
            }),
            0,
        )
        .unwrap();

        assert_eq!(panes, 2);
        assert_eq!(
            root,
            json!({
                "type": "split",
                "direction": "down",
                // A split without a recorded ratio still satisfies the
                // documented apply request.
                "ratio": 0.5,
                "first": {"type": "pane", "cwd": "/src", "label": "editor"},
                "second": {"type": "pane"},
            })
        );
        let serialized = root.to_string();
        assert!(!serialized.contains("secret"));
        assert!(!serialized.contains("cargo"));
        assert!(!serialized.contains("w1:p1"));
    }

    #[test]
    fn recreation_refuses_to_start_an_unbounded_number_of_shells() {
        let single = ExportedTab {
            label: None,
            root: json!({"type": "pane", "pane_id": "w1:p1"}),
        };
        let tabs = vec![single; super::MAX_RECREATED_PANES];
        let (layouts, panes) = recreatable_layouts(&tabs).unwrap();
        assert_eq!(layouts.len(), super::MAX_RECREATED_PANES);
        assert_eq!(panes, super::MAX_RECREATED_PANES);

        let mut crowded = tabs;
        crowded.push(ExportedTab {
            label: None,
            root: json!({"type": "pane", "pane_id": "w1:p2"}),
        });
        let error = recreatable_layouts(&crowded).unwrap_err();
        assert!(error.contains("more than"));
    }

    #[tokio::test]
    async fn a_recreated_workspace_is_rebuilt_on_the_destination_session() {
        use super::recreate_workspace;
        use crate::config::Config;

        let directory = tempfile::tempdir().unwrap();
        let source_socket = directory.path().join("source.sock");
        let destination_socket = directory.path().join("destination.sock");
        let source = serve(
            UnixListener::bind(&source_socket).unwrap(),
            vec![
                json!({
                    "type": "tab_list",
                    "tabs": [
                        {"tab_id": "w1:t1", "label": "build"},
                        {"tab_id": "w1:t2", "label": ""},
                    ],
                }),
                json!({
                    "type": "layout_export",
                    "layout": {"root": {
                        "type": "split",
                        "direction": "right",
                        "ratio": 0.4,
                        "first": {
                            "type": "pane",
                            "pane_id": "w1:p1",
                            "cwd": "/src",
                            "command": ["cargo", "watch"],
                        },
                        "second": {"type": "pane", "pane_id": "w1:p2"},
                    }},
                }),
                json!({
                    "type": "layout_export",
                    "layout": {"root": {"type": "pane", "pane_id": "w1:p3", "cwd": "/tmp"}},
                }),
            ],
        );
        let destination = serve(
            UnixListener::bind(&destination_socket).unwrap(),
            vec![
                json!({
                    "type": "workspace_created",
                    "workspace": {"workspace_id": "w5"},
                    "tab": {"tab_id": "w5:t1"},
                    "root_pane": {"pane_id": "w5:p1"},
                }),
                json!({"type": "layout_apply", "layout": {"workspace_id": "w5", "tab_id": "w5:t1"}}),
                json!({"type": "layout_apply", "layout": {"workspace_id": "w5", "tab_id": "w5:t2"}}),
            ],
        );

        let config = Config::parse(&format!(
            "[[targets]]\nname = \"source\"\nsocket = {:?}\n\n[[targets]]\nname = \"destination\"\nsocket = {:?}\n",
            source_socket.display().to_string(),
            destination_socket.display().to_string()
        ))
        .unwrap();
        let summary = recreate_workspace(
            &config.targets[0],
            &config.targets[1],
            &config.transport,
            "w1",
            Some("simulator"),
            Duration::from_secs(5),
        )
        .await
        .unwrap();

        assert_eq!(summary.workspace, "w5");
        assert_eq!(summary.tabs, 2);
        assert_eq!(summary.panes, 3);

        let read = source.await.unwrap();
        assert_eq!(
            read.iter()
                .map(|(method, _)| method.as_str())
                .collect::<Vec<_>>(),
            vec!["tab.list", "layout.export", "layout.export"]
        );
        assert_eq!(read[0].1["workspace_id"], "w1");

        let written = destination.await.unwrap();
        assert_eq!(
            written
                .iter()
                .map(|(method, _)| method.as_str())
                .collect::<Vec<_>>(),
            vec!["workspace.create", "layout.apply", "layout.apply"]
        );
        assert_eq!(written[0].1, json!({"label": "simulator", "focus": false}));
        // The first exported tab fills the tab the new workspace already owns.
        assert_eq!(
            written[1].1,
            json!({
                "tab_id": "w5:t1",
                "tab_label": "build",
                "focus": false,
                "root": {
                    "type": "split",
                    "direction": "right",
                    "ratio": 0.4,
                    "first": {"type": "pane", "cwd": "/src"},
                    "second": {"type": "pane"},
                },
            })
        );
        assert_eq!(
            written[2].1,
            json!({
                "workspace_id": "w5",
                "tab_label": null,
                "focus": false,
                "root": {"type": "pane", "cwd": "/tmp"},
            })
        );
    }

    #[test]
    fn a_move_response_re_qualifies_the_pane_and_reports_an_emptied_workspace() {
        let mut moved = std::collections::BTreeMap::new();
        let mut summary = super::WorkspaceMoveSummary::default();

        record_move(
            "w1:p1",
            &json!({"move_result": {"pane": {"pane_id": "w2:p7"}}}),
            &mut moved,
            &mut summary,
        );
        record_move(
            "w1:p2",
            &json!({"move_result": {
                "pane": {"pane_id": "w2:p8"},
                "closed_workspace_id": "w1",
            }}),
            &mut moved,
            &mut summary,
        );

        assert_eq!(moved.get("w1:p1").map(String::as_str), Some("w2:p7"));
        assert_eq!(moved.get("w1:p2").map(String::as_str), Some("w2:p8"));
        assert_eq!(summary.panes, 2);
        assert!(summary.source_closed);
    }
}
