use crate::model::{PaneId, TabId, TargetSession, WorkspaceId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceAction {
    JumpToPane {
        pane: PaneId,
        label: String,
    },
    OpenTargetManager,
    OpenAgentNavigator,
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
}

impl ResourceAction {
    pub fn mutates_herdr(&self) -> bool {
        !matches!(
            self,
            Self::JumpToPane { .. } | Self::OpenTargetManager | Self::OpenAgentNavigator
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
            | Self::CreateTab { workspace } => Some(workspace.target_session()),
            Self::RenameTab { tab, .. } | Self::CloseTab { tab, .. } => Some(tab.target_session()),
            Self::OpenTargetManager | Self::OpenAgentNavigator => None,
        }
    }

    pub fn palette_label(&self) -> String {
        match self {
            Self::JumpToPane { label, .. } => format!("Jump to {label}"),
            Self::OpenTargetManager => "Manage machines".to_owned(),
            Self::OpenAgentNavigator => "Open agent navigator".to_owned(),
            Self::CreateWorkspace { .. } => "Create workspace".to_owned(),
            Self::RenameWorkspace { current_label, .. } => {
                format!("Rename workspace {current_label:?}")
            }
            Self::CloseWorkspace { label, .. } => format!("Close workspace {label:?}"),
            Self::CreateTab { .. } => "Create tab in selected workspace".to_owned(),
            Self::RenameTab { current_label, .. } => format!("Rename tab {current_label:?}"),
            Self::CloseTab { label, .. } => format!("Close tab {label:?}"),
            Self::SplitPane { direction, .. } => format!("Split pane {}", direction.label()),
            Self::TogglePaneZoom { .. } => "Toggle pane zoom".to_owned(),
            Self::ClosePane { label, .. } => format!("Close pane {label:?}"),
        }
    }

    pub fn palette_scope(&self) -> String {
        self.target_session()
            .map(|target| target.to_string())
            .unwrap_or_else(|| "Super-Herdr".to_owned())
    }

    pub fn search_text(&self) -> String {
        format!("{} {}", self.palette_label(), self.palette_scope())
    }
}

#[cfg(test)]
mod tests {
    use super::{ResourceAction, SplitDirection};
    use crate::model::{PaneId, TargetSession, WorkspaceId};

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
