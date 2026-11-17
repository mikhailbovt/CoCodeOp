use chrono::{DateTime, Utc};
use office_domain::{RunId, TaskId, WorkPermissions, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Recipient, WorkScope};

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestAccess {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub expected_task_revision: i64,
    pub expected_scope_revision: i64,
    pub idempotency_key: Uuid,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<Uuid>,
    pub expires_at: DateTime<Utc>,
    pub scope_expires_at: DateTime<Utc>,
    pub permissions: WorkPermissions,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetAccessRequest {
    pub workspace_id: WorkspaceId,
    pub access_request_id: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AccessDecision {
    Approve,
    Reject,
    Cancel,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RespondAccess {
    pub workspace_id: WorkspaceId,
    pub access_request_id: Uuid,
    pub expected_revision: i64,
    pub idempotency_key: Uuid,
    pub decision: AccessDecision,
    pub as_approver: Option<Recipient>,
    pub replacement_grant_id: Option<Uuid>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AccessRequestState {
    Requested,
    ApprovedPendingApply,
    Approved,
    Rejected,
    Cancelled,
    Expired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AreaRevision {
    pub area_id: Uuid,
    pub revision: i64,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccessApproval {
    pub approver: Recipient,
    pub decided_by: Recipient,
    pub decided_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AccessRequest {
    pub workspace_id: WorkspaceId,
    pub access_request_id: Uuid,
    pub task_id: TaskId,
    pub checkout_id: Uuid,
    pub run_id: Option<RunId>,
    pub scope_id: Uuid,
    pub base_scope_revision: i64,
    pub requester: Recipient,
    pub worker: Recipient,
    pub lead: Recipient,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<Uuid>,
    pub permissions: WorkPermissions,
    pub state: AccessRequestState,
    pub revision: i64,
    pub expires_at: DateTime<Utc>,
    pub scope_expires_at: DateTime<Utc>,
    pub area_revisions: Vec<AreaRevision>,
    pub approvers: Vec<Recipient>,
    pub approvals: Vec<AccessApproval>,
    pub needs_owner_grant: bool,
    pub effective_grant_id: Uuid,
    pub issued_scope_revision: Option<i64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AccessRequestReceipt {
    pub request: AccessRequest,
    pub work_scope: Option<WorkScope>,
    pub event_cursor: Uuid,
}
