use chrono::{DateTime, Utc};
use office_domain::{RunId, TaskId, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{CheckoutContentHead, CommandEnvironment, Recipient, ResultBinding};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutContribution {
    pub change_id: Uuid,
    pub checkout_revision: i64,
    pub task_id: TaskId,
    pub run_id: RunId,
    pub author: Recipient,
    pub delta_sha256: String,
}

/// Summary for a recorded command. The full log remains under the command's read scope.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerifiedCheckoutCheck {
    pub command_id: Uuid,
    pub template_id: Uuid,
    pub run_id: RunId,
    pub source_snapshot_id: Uuid,
    pub source_sha256: String,
    pub environment: CommandEnvironment,
    pub exit_code: i32,
    pub logs_sha256: String,
    pub log_truncated: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetCheckoutReview {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub snapshot_id: Uuid,
    pub command_ids: Vec<Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutReviewContent {
    pub task_id: TaskId,
    pub task_revision: i64,
    pub result_run_id: RunId,
    pub result: ResultBinding,
    pub checkout_id: Uuid,
    pub snapshot_id: Uuid,
    pub head: CheckoutContentHead,
    pub git_tree_sha256: String,
    pub contributions: Vec<CheckoutContribution>,
    pub checks: Vec<VerifiedCheckoutCheck>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CheckoutReview {
    pub revision_sha256: String,
    pub content: CheckoutReviewContent,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApproveCheckoutReview {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub snapshot_id: Uuid,
    pub command_ids: Vec<Uuid>,
    pub revision_sha256: String,
    pub reason: String,
    pub idempotency_key: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CheckoutReviewReceipt {
    pub review: CheckoutReview,
    pub reviewer: Recipient,
    pub approved_at: DateTime<Utc>,
}
