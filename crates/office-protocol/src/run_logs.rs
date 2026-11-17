use chrono::{DateTime, Utc};
use office_domain::{RunId, TaskId, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::Run;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadRunLogs {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub run_id: RunId,
    pub after: Option<Uuid>,
    pub limit: Option<u16>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct RunLog {
    pub run: Run,
    pub events: Vec<RunLogEvent>,
    pub next_after: Option<Uuid>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct RunLogEvent {
    pub event_id: Uuid,
    pub event_type: String,
    pub created_at: DateTime<Utc>,
    pub detail: Option<String>,
}
