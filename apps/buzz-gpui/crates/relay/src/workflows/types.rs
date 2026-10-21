//! The workflow capability's contract (`features/workflows/types.ts`).

use crate::outbox::Delivery;
use serde_json::Value;

/// Canonical signed coordinate, bound to the owning community session.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorkflowReference {
    pub id: String,
    pub owner: String,
    pub channel_id: String,
}

/// Configuration intent, not proof of runtime presence, enabled state or
/// authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowDefinition {
    pub id: String,
    pub owner: String,
    pub channel_id: String,
    /// The signed event ID.
    pub revision: String,
    pub created_at: u64,
    pub yaml: String,
}

impl WorkflowDefinition {
    pub fn reference(&self) -> WorkflowReference {
        WorkflowReference {
            id: self.id.clone(),
            owner: self.owner.clone(),
            channel_id: self.channel_id.clone(),
        }
    }

    /// Same owner, channel and UUID.
    pub fn names(&self, reference: &WorkflowReference) -> bool {
        self.id == reference.id
            && self.owner == reference.owner
            && self.channel_id == reference.channel_id
    }
}

/// A bounded configuration snapshot, not a complete runtime inventory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkflowDefinitions {
    pub items: Vec<WorkflowDefinition>,
    pub partial: bool,
    /// Channels whose definition read stopped at the result limit.
    pub partial_channel_ids: Vec<String>,
}

/// The relay's run cursor, kept verbatim, fractional seconds included.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RunCursor {
    pub before: String,
    pub before_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Pending,
    Running,
    WaitingApproval,
    Completed,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "waiting_approval" => Self::WaitingApproval,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    /// The status with underscores as spaces, as the run list shows it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::WaitingApproval => "waiting approval",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowRun {
    pub id: String,
    pub workflow_id: String,
    pub status: RunStatus,
    pub current_step: u64,
    pub trace: Vec<Value>,
    pub started_at: Option<u64>,
    pub completed_at: Option<u64>,
    pub created_at: u64,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunPage {
    pub runs: Vec<WorkflowRun>,
    pub next: Option<RunCursor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Save,
    Delete,
    Trigger,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Save => "save",
            Self::Delete => "delete",
            Self::Trigger => "trigger",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Pending,
    Succeeded,
    Rejected,
    Unknown,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Succeeded => "succeeded",
            Self::Rejected => "rejected",
            Self::Unknown => "unknown",
        }
    }
}

/// Delivery evidence and domain outcome are deliberately separate. No
/// secret here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowOperation {
    pub event_id: String,
    pub workflow: WorkflowReference,
    pub action: Action,
    /// Unix seconds from the signed command.
    pub created_at: u64,
    pub delivery: Delivery,
    pub outcome: Outcome,
    pub error: Option<String>,
    pub run_id: Option<String>,
    /// A one-time webhook secret awaits [`super::Workflows::take_webhook_secret`];
    /// the value itself is never here.
    pub secret_held: bool,
}

/// Host availability, not per-row permission; the relay stays
/// authoritative.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Availability {
    pub definitions: bool,
    pub history: bool,
    pub save: bool,
    pub trigger: bool,
    pub delete: bool,
}

pub fn delivery_label(delivery: Delivery) -> &'static str {
    match delivery {
        Delivery::Sending => "sending",
        Delivery::Accepted => "accepted",
        Delivery::Unknown => "unknown",
        Delivery::Failed => "failed",
        Delivery::Seen => "seen",
    }
}
