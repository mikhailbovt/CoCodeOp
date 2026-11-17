use chrono::{DateTime, Utc};
use office_domain::{RunId, TaskId, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ExecutionJob, Recipient};

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetManagedInput {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub run_id: RunId,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnswerManagedInput {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub run_id: RunId,
    pub expected_task_revision: i64,
    pub idempotency_key: Uuid,
    pub answer: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ManagedInputState {
    Waiting,
    Answered,
    Expired,
    Cancelled,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ManagedInput {
    pub task_id: TaskId,
    pub run_id: RunId,
    pub requester: Recipient,
    pub checkpoint_id: String,
    pub question: String,
    pub state: ManagedInputState,
    pub expires_at: DateTime<Utc>,
    pub answer: Option<String>,
    pub respondent: Option<Recipient>,
    pub resumed_job_id: Option<Uuid>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ManagedInputReceipt {
    pub input: ManagedInput,
    pub job: ExecutionJob,
    pub task_revision: i64,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct RunContinuation {
    pub run_id: RunId,
    pub checkpoint_id: String,
    pub question: String,
    pub answer: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestRunInput {
    pub idempotency_key: Uuid,
    pub question: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunInputIntent {
    pub run_id: RunId,
    pub question: String,
}
