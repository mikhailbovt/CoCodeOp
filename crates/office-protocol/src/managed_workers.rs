use chrono::{DateTime, Utc};
use office_domain::{AgentId, TaskId, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ExecutionJob, Recipient, TaskView};

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateManagedWorker {
    pub workspace_id: WorkspaceId,
    pub display_name: String,
    #[serde(default)]
    pub instructions: String,
    pub idempotency_key: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateManagedWorker {
    pub workspace_id: WorkspaceId,
    pub agent_id: AgentId,
    pub expected_revision: i64,
    pub instructions: String,
    pub idempotency_key: Uuid,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkerInstructions {
    pub revision: i64,
    pub display_name: String,
    pub instructions: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ManagedWorker {
    pub workspace_id: WorkspaceId,
    pub worker: Recipient,
    pub lead: Recipient,
    pub profile: WorkerInstructions,
    pub updated_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(tag = "by", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedWorkerSelector {
    Id { agent_id: AgentId },
    Name { display_name: String },
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetManagedWorkerStatus {
    pub workspace_id: WorkspaceId,
    pub worker: ManagedWorkerSelector,
    pub tasks_after: Option<TaskId>,
    pub limit: Option<u16>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ManagedWorkerTask {
    pub state: TaskView,
    pub job: Option<ExecutionJob>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ManagedWorkerStatus {
    pub worker: ManagedWorker,
    pub tasks: Vec<ManagedWorkerTask>,
    pub next_tasks_after: Option<TaskId>,
}
