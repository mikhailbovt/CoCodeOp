use chrono::{DateTime, Utc};
use office_domain::{PathRule, RunId, TaskId, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::Recipient;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestPathHandoff {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub run_id: RunId,
    pub scope_revision: i64,
    pub paths: Vec<PathRule>,
    pub reason: String,
    pub idempotency_key: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PathHandoffDecision {
    Offer,
    Reject,
    Cancel,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RespondPathHandoff {
    pub workspace_id: WorkspaceId,
    pub handoff_id: Uuid,
    pub expected_revision: i64,
    pub decision: PathHandoffDecision,
    pub idempotency_key: Uuid,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetPathHandoff {
    pub workspace_id: WorkspaceId,
    pub handoff_id: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PathHandoffState {
    Requested,
    Offered,
    Accepted,
    Rejected,
    Cancelled,
    Expired,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PathHandoff {
    pub handoff_id: Uuid,
    pub checkout_id: Uuid,
    pub requester: Recipient,
    pub holder: Recipient,
    pub paths: Vec<PathRule>,
    pub reason: String,
    pub expires_at: DateTime<Utc>,
    pub state: PathHandoffState,
    pub revision: i64,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct PathHandoffReceipt {
    pub handoff: PathHandoff,
    pub event_cursor: Uuid,
}
