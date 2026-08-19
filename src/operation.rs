//! Operations a client asks the daemon to perform on a Herdr server.
//!
//! This is deliberately not [`crate::resource_action::ResourceAction`]. That
//! type is a frontend intent: "rename this workspace" means *open a prompt*,
//! and "close this tab" means *ask for confirmation*. Prompting and confirming
//! are the client's job, and a daemon that received those intents would either
//! have to grow a UI or guess what the person meant.
//!
//! An `Operation` is what remains once the client has finished asking: fully
//! resolved, qualified, and executable exactly once. A rename carries the new
//! label; a close carries only the identity of something the person has already
//! agreed to lose.

use serde::{Deserialize, Serialize};

use crate::model::{PaneId, TabId, TargetSession, WorkspaceId};
use crate::resource_action::SplitDirection;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum Operation {
    CreateWorkspace {
        target: TargetSession,
        label: String,
    },
    RenameWorkspace {
        workspace: WorkspaceId,
        label: String,
    },
    CloseWorkspace {
        workspace: WorkspaceId,
    },
    /// Relocate every tab of one workspace into another of the same session.
    /// Herdr moves live panes, so nothing restarts.
    MoveWorkspace {
        workspace: WorkspaceId,
        destination: WorkspaceId,
    },
    /// Rebuild a workspace's structure on another session with fresh shells.
    /// Never silently substituted for a move.
    RecreateWorkspace {
        workspace: WorkspaceId,
        destination: TargetSession,
        label: Option<String>,
    },
    CreateTab {
        workspace: WorkspaceId,
    },
    RenameTab {
        tab: TabId,
        label: String,
    },
    CloseTab {
        tab: TabId,
    },
    SplitPane {
        pane: PaneId,
        direction: SplitDirection,
    },
    TogglePaneZoom {
        pane: PaneId,
    },
    ClosePane {
        pane: PaneId,
    },
}

impl Operation {
    /// Every operation names exactly one session. Unlike a frontend intent,
    /// there is no such thing as an operation that acts on nothing.
    pub fn target_session(&self) -> TargetSession {
        match self {
            Self::CreateWorkspace { target, .. } => target.clone(),
            Self::RenameWorkspace { workspace, .. }
            | Self::CloseWorkspace { workspace }
            | Self::MoveWorkspace { workspace, .. }
            | Self::RecreateWorkspace { workspace, .. }
            | Self::CreateTab { workspace } => workspace.target_session(),
            Self::RenameTab { tab, .. } | Self::CloseTab { tab } => tab.target_session(),
            Self::SplitPane { pane, .. }
            | Self::TogglePaneZoom { pane }
            | Self::ClosePane { pane } => pane.target_session(),
        }
    }

    /// The session an operation reads from and the session it writes to differ
    /// only for a cross-session recreation, which is why that one is not a move.
    pub fn destination_session(&self) -> TargetSession {
        match self {
            Self::RecreateWorkspace { destination, .. } => destination.clone(),
            other => other.target_session(),
        }
    }

    pub fn is_destructive(&self) -> bool {
        matches!(
            self,
            Self::CloseWorkspace { .. } | Self::CloseTab { .. } | Self::ClosePane { .. }
        )
    }

    /// The documented Herdr CLI invocation, with the server-local identifier
    /// extracted at this final step and never before. Multi-request operations
    /// return `None`: they are replayed through the private API socket instead
    /// of a single command.
    pub fn herdr_args(&self) -> Option<Vec<String>> {
        let args = match self {
            Self::CreateWorkspace { label, .. } => vec![
                "workspace".to_owned(),
                "create".to_owned(),
                "--label".to_owned(),
                label.clone(),
                "--focus".to_owned(),
            ],
            Self::RenameWorkspace { workspace, label } => vec![
                "workspace".to_owned(),
                "rename".to_owned(),
                workspace.server_local_id().to_owned(),
                label.clone(),
            ],
            Self::CloseWorkspace { workspace } => vec![
                "workspace".to_owned(),
                "close".to_owned(),
                workspace.server_local_id().to_owned(),
            ],
            Self::MoveWorkspace { .. } | Self::RecreateWorkspace { .. } => return None,
            Self::CreateTab { workspace } => vec![
                "tab".to_owned(),
                "create".to_owned(),
                "--workspace".to_owned(),
                workspace.server_local_id().to_owned(),
                "--focus".to_owned(),
            ],
            Self::RenameTab { tab, label } => vec![
                "tab".to_owned(),
                "rename".to_owned(),
                tab.server_local_id().to_owned(),
                label.clone(),
            ],
            Self::CloseTab { tab } => vec![
                "tab".to_owned(),
                "close".to_owned(),
                tab.server_local_id().to_owned(),
            ],
            Self::SplitPane { pane, direction } => vec![
                "pane".to_owned(),
                "split".to_owned(),
                pane.server_local_id().to_owned(),
                "--direction".to_owned(),
                direction.cli_value().to_owned(),
                "--focus".to_owned(),
            ],
            Self::TogglePaneZoom { pane } => vec![
                "pane".to_owned(),
                "zoom".to_owned(),
                pane.server_local_id().to_owned(),
                "--toggle".to_owned(),
            ],
            Self::ClosePane { pane } => vec![
                "pane".to_owned(),
                "close".to_owned(),
                pane.server_local_id().to_owned(),
            ],
        };
        Some(args)
    }

    pub fn description(&self) -> String {
        match self {
            Self::CreateWorkspace { target, label } => {
                format!("create workspace {label:?} on {target}")
            }
            Self::RenameWorkspace { workspace, label } => {
                format!(
                    "rename workspace to {label:?} on {}",
                    workspace.target_session()
                )
            }
            Self::CloseWorkspace { workspace } => format!("close workspace {workspace}"),
            Self::MoveWorkspace {
                workspace,
                destination,
            } => format!("move workspace {workspace} into {destination}"),
            Self::RecreateWorkspace {
                workspace,
                destination,
                ..
            } => format!("recreate workspace {workspace} on {destination} (new shells)"),
            Self::CreateTab { workspace } => format!("create tab in {workspace}"),
            Self::RenameTab { tab, label } => {
                format!("rename tab to {label:?} on {}", tab.target_session())
            }
            Self::CloseTab { tab } => format!("close tab {tab}"),
            Self::SplitPane { pane, direction } => {
                format!("split pane {} on {pane}", direction.label())
            }
            Self::TogglePaneZoom { pane } => format!("toggle zoom on pane {pane}"),
            Self::ClosePane { pane } => format!("close pane {pane}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Operation;
    use crate::model::{PaneId, TabId, TargetSession, WorkspaceId};
    use crate::resource_action::SplitDirection;

    #[test]
    fn only_the_final_argument_list_holds_a_server_local_id() {
        let operation = Operation::SplitPane {
            pane: PaneId::new("development", "work", "w1:p1"),
            direction: SplitDirection::Down,
        };

        assert_eq!(
            operation.target_session(),
            TargetSession::new("development", "work")
        );
        assert_eq!(
            operation.herdr_args().expect("a single command"),
            vec!["pane", "split", "w1:p1", "--direction", "down", "--focus"]
        );
    }

    #[test]
    fn a_rename_carries_the_new_label_because_prompting_belongs_to_the_client() {
        let operation = Operation::RenameTab {
            tab: TabId::new("build", "toolchains", "t3"),
            label: "release".to_owned(),
        };

        assert_eq!(
            operation.herdr_args().expect("a single command"),
            vec!["tab", "rename", "t3", "release"]
        );
        assert!(!operation.is_destructive());
    }

    #[test]
    fn multi_request_operations_have_no_single_command() {
        let workspace = WorkspaceId::new("build", "toolchains", "w2");
        let move_operation = Operation::MoveWorkspace {
            workspace: workspace.clone(),
            destination: WorkspaceId::new("build", "toolchains", "w5"),
        };
        let recreate = Operation::RecreateWorkspace {
            workspace,
            destination: TargetSession::new("development", "work"),
            label: Some("compiler".to_owned()),
        };

        assert!(move_operation.herdr_args().is_none());
        assert!(recreate.herdr_args().is_none());
        assert_eq!(
            move_operation.destination_session(),
            TargetSession::new("build", "toolchains")
        );
        assert_eq!(
            recreate.destination_session(),
            TargetSession::new("development", "work")
        );
    }

    #[test]
    fn closing_is_the_only_destructive_family() {
        for operation in [
            Operation::CloseWorkspace {
                workspace: WorkspaceId::new("build", "toolchains", "w2"),
            },
            Operation::CloseTab {
                tab: TabId::new("build", "toolchains", "t1"),
            },
            Operation::ClosePane {
                pane: PaneId::new("build", "toolchains", "w2:p1"),
            },
        ] {
            assert!(operation.is_destructive(), "{operation:?}");
        }
        assert!(
            !Operation::CreateTab {
                workspace: WorkspaceId::new("build", "toolchains", "w2"),
            }
            .is_destructive()
        );
    }
}
