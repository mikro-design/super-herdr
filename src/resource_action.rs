use serde::{Deserialize, Serialize};

use crate::agent_marks::AgentMarkRequest;
use crate::model::{AgentId, PaneId, TabId, TargetSession, WorkspaceId};
use crate::plugin::{PluginAction, PluginInvocationTarget};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    Right,
    Down,
}

impl SplitDirection {
    pub fn cli_value(self) -> &'static str {
        match self {
            Self::Right => "right",
            Self::Down => "down",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Right => "right",
            Self::Down => "down",
        }
    }

    /// Herdr reports and accepts the same split names in its documented layout
    /// export and `pane.move` destination.
    pub fn from_api_value(value: &str) -> Option<Self> {
        match value {
            "right" => Some(Self::Right),
            "down" => Some(Self::Down),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceAction {
    JumpToPane {
        pane: PaneId,
        label: String,
    },
    OpenTargetManager,
    OpenAgentNavigator,
    OpenAttentionCenter,
    CreateWorkspace {
        target: TargetSession,
    },
    RenameWorkspace {
        workspace: WorkspaceId,
        current_label: String,
    },
    CloseWorkspace {
        workspace: WorkspaceId,
        label: String,
    },
    /// Move every tab of one workspace into another workspace of the same
    /// session. Herdr relocates live panes, so no process is restarted.
    MoveWorkspace {
        workspace: WorkspaceId,
        destination: WorkspaceId,
        label: String,
        destination_label: String,
    },
    /// Rebuild a workspace's tab and split structure on another session with
    /// fresh shells. Herdr cannot move live panes across sessions, so this is
    /// offered as a recreation and never as a move.
    RecreateWorkspace {
        workspace: WorkspaceId,
        destination: TargetSession,
        label: String,
    },
    CreateTab {
        workspace: WorkspaceId,
    },
    RenameTab {
        tab: TabId,
        current_label: String,
    },
    CloseTab {
        tab: TabId,
        label: String,
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
        label: String,
    },
    InvokePluginAction {
        action: PluginAction,
        context: PluginInvocationTarget,
    },
    /// Pin, mute, or snooze one agent in Super-Herdr's own inbox.
    ///
    /// Listed among the Herdr actions because that is where a person looks for
    /// something to do to an agent, and deliberately not one of them: it
    /// changes nothing on the host. See [`ResourceAction::mutates_herdr`].
    MarkAgent {
        agent: AgentId,
        mark: AgentMarkRequest,
        label: String,
    },
}

impl ResourceAction {
    pub fn mutates_herdr(&self) -> bool {
        !matches!(
            self,
            Self::JumpToPane { .. }
                | Self::OpenTargetManager
                | Self::OpenAgentNavigator
                | Self::OpenAttentionCenter
                | Self::MarkAgent { .. }
        )
    }

    pub fn is_destructive(&self) -> bool {
        matches!(
            self,
            Self::CloseWorkspace { .. } | Self::CloseTab { .. } | Self::ClosePane { .. }
        )
    }

    pub fn target_session(&self) -> Option<TargetSession> {
        match self {
            Self::JumpToPane { pane, .. }
            | Self::SplitPane { pane, .. }
            | Self::TogglePaneZoom { pane }
            | Self::ClosePane { pane, .. } => Some(pane.target_session()),
            Self::CreateWorkspace { target } => Some(target.clone()),
            Self::RenameWorkspace { workspace, .. }
            | Self::CloseWorkspace { workspace, .. }
            | Self::MoveWorkspace { workspace, .. }
            | Self::RecreateWorkspace { workspace, .. }
            | Self::CreateTab { workspace } => Some(workspace.target_session()),
            Self::RenameTab { tab, .. } | Self::CloseTab { tab, .. } => Some(tab.target_session()),
            Self::InvokePluginAction { action, .. } => Some(action.id.target.clone()),
            Self::MarkAgent { agent, .. } => Some(agent.target_session()),
            Self::OpenTargetManager | Self::OpenAgentNavigator | Self::OpenAttentionCenter => None,
        }
    }

    pub fn palette_label(&self) -> String {
        match self {
            Self::JumpToPane { label, .. } => format!("Jump to {label}"),
            Self::OpenTargetManager => "Manage machines".to_owned(),
            Self::OpenAgentNavigator => "Open agent navigator".to_owned(),
            Self::OpenAttentionCenter => "Open attention history".to_owned(),
            Self::CreateWorkspace { .. } => "Create workspace".to_owned(),
            Self::RenameWorkspace { current_label, .. } => {
                format!("Rename workspace {current_label:?}")
            }
            Self::CloseWorkspace { label, .. } => format!("Close workspace {label:?}"),
            Self::MoveWorkspace {
                label,
                destination_label,
                ..
            } => format!("Move workspace {label:?} into {destination_label:?}"),
            Self::RecreateWorkspace {
                label, destination, ..
            } => format!("Recreate workspace {label:?} on {destination} (new shells)"),
            Self::CreateTab { .. } => "Create tab".to_owned(),
            Self::RenameTab { current_label, .. } => format!("Rename tab {current_label:?}"),
            Self::CloseTab { label, .. } => format!("Close tab {label:?}"),
            Self::SplitPane { direction, .. } => format!("Split pane {}", direction.label()),
            Self::TogglePaneZoom { .. } => "Toggle pane zoom".to_owned(),
            Self::ClosePane { label, .. } => format!("Close pane {label:?}"),
            Self::InvokePluginAction { action, .. } => {
                format!("Plugin: {}", action.title)
            }
            Self::MarkAgent { mark, label, .. } => match mark {
                AgentMarkRequest::Pin { pinned: true } => format!("Pin agent {label:?}"),
                AgentMarkRequest::Pin { pinned: false } => format!("Unpin agent {label:?}"),
                AgentMarkRequest::Mute { muted: true } => format!("Mute agent {label:?}"),
                AgentMarkRequest::Mute { muted: false } => format!("Unmute agent {label:?}"),
                AgentMarkRequest::Snooze {
                    minutes: Some(minutes),
                } => format!("Snooze agent {label:?} for {minutes} minutes"),
                AgentMarkRequest::Snooze { minutes: None } => format!("Wake agent {label:?}"),
            },
        }
    }

    pub fn palette_scope(&self) -> String {
        self.target_session()
            .map(|target| target.to_string())
            .unwrap_or_else(|| "Super-Herdr".to_owned())
    }

    pub fn search_text(&self) -> String {
        let description = match self {
            Self::InvokePluginAction { action, .. } => {
                action.description.as_deref().unwrap_or_default()
            }
            _ => "",
        };
        format!(
            "{} {} {description}",
            self.palette_label(),
            self.palette_scope()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{ResourceAction, SplitDirection};
    use crate::agent_marks::AgentMarkRequest;
    use crate::model::{AgentId, PaneId, TargetSession, WorkspaceId};

    #[test]
    fn actions_keep_server_local_ids_qualified() {
        let action = ResourceAction::CloseWorkspace {
            workspace: WorkspaceId::new("build", "toolchains", "w2"),
            label: "compiler".to_owned(),
        };

        assert_eq!(
            action.target_session(),
            Some(TargetSession::new("build", "toolchains"))
        );
        assert_eq!(action.palette_scope(), "build/toolchains");
        assert!(action.mutates_herdr());
        assert!(action.is_destructive());
    }

    #[test]
    fn a_workspace_move_stays_qualified_and_is_not_a_close() {
        let action = ResourceAction::MoveWorkspace {
            workspace: WorkspaceId::new("build", "toolchains", "w2"),
            destination: WorkspaceId::new("build", "toolchains", "w5"),
            label: "compiler".to_owned(),
            destination_label: "archive".to_owned(),
        };

        assert_eq!(
            action.target_session(),
            Some(TargetSession::new("build", "toolchains"))
        );
        assert!(action.mutates_herdr());
        assert!(!action.is_destructive());
        assert_eq!(
            action.palette_label(),
            "Move workspace \"compiler\" into \"archive\""
        );
    }

    #[test]
    fn a_mark_is_qualified_and_never_a_herdr_mutation() {
        let action = ResourceAction::MarkAgent {
            agent: AgentId::new("build", "toolchains", "w2:p1"),
            mark: AgentMarkRequest::Snooze { minutes: Some(60) },
            label: "reviewer".to_owned(),
        };

        assert_eq!(
            action.target_session(),
            Some(TargetSession::new("build", "toolchains"))
        );
        assert!(
            !action.mutates_herdr(),
            "pinning, muting and snoozing are this inbox's opinions, not the host's"
        );
        assert!(!action.is_destructive());
        assert_eq!(
            action.palette_label(),
            "Snooze agent \"reviewer\" for 60 minutes"
        );
        assert!(action.search_text().contains("build/toolchains"));
    }

    #[test]
    fn split_direction_has_a_documented_cli_value() {
        let action = ResourceAction::SplitPane {
            pane: PaneId::new("development", "work", "w1:p1"),
            direction: SplitDirection::Down,
        };

        assert_eq!(SplitDirection::Down.cli_value(), "down");
        assert!(action.search_text().contains("development/work"));
    }
}
