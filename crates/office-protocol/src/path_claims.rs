use chrono::{DateTime, Utc};
use office_domain::{PathRule, RunId, TaskId, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::Recipient;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClaimPaths {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub run_id: RunId,
    pub scope_revision: i64,
    /// Required for atomic replacement of an existing live set.
    pub expected_fence: Option<i64>,
    /// Accept a reservation addressed to this exact run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_id: Option<Uuid>,
    pub paths: Vec<PathRule>,
    pub ttl_seconds: u16,
    pub idempotency_key: Uuid,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenewPathClaim {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub run_id: RunId,
    pub claim_fence: i64,
    pub ttl_seconds: u16,
    pub idempotency_key: Uuid,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleasePathClaim {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub run_id: RunId,
    pub claim_fence: i64,
    pub idempotency_key: Uuid,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListPathClaims {
    pub workspace_id: WorkspaceId,
    pub checkout_id: Uuid,
    pub task_id: Option<TaskId>,
    pub before_fence: Option<i64>,
    pub limit: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PathClaimState {
    Active,
    Released,
    Expired,
    Fenced,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PathClaim {
    pub claim_id: Uuid,
    pub checkout_id: Uuid,
    pub task_id: TaskId,
    pub run_id: RunId,
    pub scope_id: Uuid,
    pub scope_revision: i64,
    pub worker: Recipient,
    pub claim_fence: i64,
    pub paths: Vec<PathRule>,
    pub started_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub renewals: i16,
    pub state: PathClaimState,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct PathClaimReceipt {
    pub claim: PathClaim,
    pub event_cursor: Uuid,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct PathClaimPage {
    pub claims: Vec<PathClaim>,
    pub next_before_fence: Option<i64>,
}
