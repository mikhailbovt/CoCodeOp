use chrono::{DateTime, Utc};
use office_domain::{TaskId, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::Recipient;

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestHandoff {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub expected_revision: i64,
    pub idempotency_key: Uuid,
    pub recipient: Recipient,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HandoffDecision {
    Accept,
    Decline,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RespondHandoff {
    pub workspace_id: WorkspaceId,
    pub handoff_id: Uuid,
    pub idempotency_key: Uuid,
    pub decision: HandoffDecision,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListHandoffs {
    pub workspace_id: WorkspaceId,
    pub after: Option<Uuid>,
    pub limit: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HandoffState {
    Pending,
    Accepted,
    Declined,
    Expired,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TaskHandoff {
    pub handoff_id: Uuid,
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub expected_revision: i64,
    pub sender: Recipient,
    pub recipient: Recipient,
    pub state: HandoffState,
    pub reason: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HandoffMutation {
    pub handoff: TaskHandoff,
    pub transferred_task_revision: Option<i64>,
    pub event_cursor: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HandoffPage {
    pub handoffs: Vec<TaskHandoff>,
    pub next_after: Option<Uuid>,
}
