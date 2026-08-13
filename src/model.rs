use std::fmt;
use std::marker::PhantomData;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TargetSession {
    pub target: String,
    pub session: String,
}

impl TargetSession {
    pub fn new(target: &str, session: &str) -> Self {
        Self {
            target: target.to_owned(),
            session: session.to_owned(),
        }
    }
}

impl fmt::Display for TargetSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.target, self.session)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Workspace {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tab {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Pane {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Agent {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Terminal {}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct QualifiedId<Resource> {
    pub target: String,
    pub session: String,
    pub resource: String,
    #[serde(skip)]
    resource_kind: PhantomData<Resource>,
}

pub type WorkspaceId = QualifiedId<Workspace>;
pub type TabId = QualifiedId<Tab>;
pub type PaneId = QualifiedId<Pane>;
pub type AgentId = QualifiedId<Agent>;
pub type TerminalId = QualifiedId<Terminal>;

impl<Resource> QualifiedId<Resource> {
    pub fn new(target: &str, session: &str, resource: &str) -> Self {
        Self {
            target: target.to_owned(),
            session: session.to_owned(),
            resource: resource.to_owned(),
            resource_kind: PhantomData,
        }
    }

    pub fn target_session(&self) -> TargetSession {
        TargetSession::new(&self.target, &self.session)
    }

    pub fn server_local_id(&self) -> &str {
        &self.resource
    }
}

impl<Resource> fmt::Display for QualifiedId<Resource> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}/{}/{}",
            self.target, self.session, self.resource
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{PaneId, TargetSession, WorkspaceId};

    #[test]
    fn qualifies_server_local_ids() {
        let left = WorkspaceId::new("host-a", "dev", "w1");
        let right = WorkspaceId::new("host-b", "dev", "w1");

        assert_ne!(left, right);
        assert_eq!(left.to_string(), "host-a/dev/w1");
        assert_eq!(left.target_session(), TargetSession::new("host-a", "dev"));
    }

    #[test]
    fn exposes_only_the_server_local_part_for_routing() {
        let pane = PaneId::new("host-a", "dev", "w1:p1");

        assert_eq!(pane.server_local_id(), "w1:p1");
    }
}
