use office_domain::{AgentRole, ErrorCode, TaskId, TaskStatus, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{InboxPage, Recipient, RunContextView};

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetContext {
    pub workspace_id: WorkspaceId,
    pub inbox_cursor: Option<Uuid>,
    pub tasks_after: Option<TaskId>,
    pub limit: Option<u16>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ActorContext {
    pub workspace_id: WorkspaceId,
    pub actor: Recipient,
    pub role: Option<AgentRole>,
    pub inbox: Option<InboxPage>,
    pub tasks: ContextTasks,
    pub workflow: WorkflowGuide,
    pub diagnostics: Vec<ContextDiagnostic>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ContextTasks {
    pub items: Vec<ContextTask>,
    pub next_after: Option<TaskId>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ContextTask {
    pub task_id: TaskId,
    pub revision: i64,
    pub status: TaskStatus,
    pub objective: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct WorkflowGuide {
    pub version: i16,
    pub steps: Vec<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ContextDiagnostic {
    pub code: ErrorCode,
    pub next_action: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetRunWorkContext {
    pub inbox_cursor: Option<Uuid>,
    pub limit: Option<u16>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RunWorkContext {
    pub run: RunContextView,
    pub inbox: InboxPage,
}
