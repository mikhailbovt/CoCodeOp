use chrono::{DateTime, Utc};
use office_domain::{RunId, Subject, TaskId, TaskStatus, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ExecutionTerminalReason;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExplainTask {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct TaskExplanation {
    pub task_id: TaskId,
    pub task_revision: i64,
    pub status: TaskStatus,
    pub job_id: Option<Uuid>,
    pub run_id: Option<RunId>,
    pub next_actor: Option<Subject>,
    pub observed_at: DateTime<Utc>,
    pub condition: TaskCondition,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskCondition {
    AwaitingOwner,
    AwaitingReview,
    AwaitingInput,
    AwaitingStopConfirmation,
    Dependencies {
        tasks: Vec<TaskId>,
    },
    WorkScopeRequired {
        checkout_id: Option<Uuid>,
    },
    ExecutionPaused,
    CapacityLimit {
        scope: String,
    },
    RetryDelay {
        earliest: DateTime<Utc>,
    },
    AwaitingCompatibleRunner {
        template_id: Uuid,
    },
    AwaitingDispatch {
        template_id: Uuid,
    },
    Prepare,
    ReadyForAssignment,
    InProgress,
    NeedsAttention,
    Terminal {
        reason: Option<ExecutionTerminalReason>,
    },
}
