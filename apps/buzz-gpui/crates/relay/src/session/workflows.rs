//! The session's workflow capability (`session.workflows`): which channels
//! it may read and command, from the roster.

use super::{Actor, Session};
use crate::discovery::DiscoveryState;
use crate::workflows::Workflows;

impl Session {
    /// Workflow definitions, run history and commands for this community.
    pub fn workflows(&self) -> &Workflows {
        &self.workflows
    }
}

impl Actor {
    /// Hands the capability the channels the viewer may participate in
    /// (`channels.canParticipate`). Only a new discovery revision can
    /// change them.
    pub(super) fn workflows_access(&mut self) {
        let revision = self.discovery.as_ref().map(DiscoveryState::revision);
        if self.workflows_revision == Some(revision) {
            return;
        }
        self.workflows_revision = Some(revision);
        let Some(discovery) = &self.discovery else {
            self.workflows.set_access(Default::default());
            return;
        };
        let access = discovery
            .channels()
            .into_iter()
            .map(|channel| channel.id)
            .filter(|id| discovery.can_participate(id))
            .collect();
        self.workflows.set_access(access);
    }
}
