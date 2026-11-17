use chrono::{DateTime, Utc};
use office_domain::{
    AgentId, InstanceId, RunId, RunStatus, Subject, TaskId, TaskStatus, TaskTransition, TeamId,
    UserId, Visibility, WorkspaceId,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AclGrant, Recipient, ResultBinding, ResultReport, RunLease};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskExecution {
    #[default]
    Coordination,
    Coding {
        checkout_id: Option<Uuid>,
        work_scope_id: Option<Uuid>,
        work_scope_revision: Option<i64>,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReviewPolicy {
    #[default]
    Accountable,
    ExplicitReviewer {
        reviewer: Recipient,
    },
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateTask {
    pub workspace_id: WorkspaceId,
    pub idempotency_key: Uuid,
    pub objective: String,
    pub acceptance_criteria: Vec<String>,
    pub parent_task_id: Option<TaskId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_id: Option<TeamId>,
    #[serde(default)]
    pub execution: TaskExecution,
    #[serde(default)]
    pub review_policy: ReviewPolicy,
    #[serde(default = "workspace_visibility")]
    pub visibility: Visibility,
    #[serde(default)]
    pub grants: Vec<AclGrant>,
}

fn workspace_visibility() -> Visibility {
    Visibility::Workspace
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Accept,
    RequestChanges,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskChange {
    AddDependency {
        prerequisite_id: TaskId,
    },
    ReviseCriteria {
        expected_criteria_revision: i64,
        acceptance_criteria: Vec<String>,
    },
    ResolveReviewEscalation {
        reason: String,
    },
    Prepare,
    Assign {
        agent_id: AgentId,
    },
    Claim {
        instance_id: Option<InstanceId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        runtime_template_id: Option<Uuid>,
    },
    Progress {
        message: String,
    },
    RequestHelp {
        message: String,
        wait_seconds: Option<u32>,
    },
    Resume,
    Submit {
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        report: Option<ResultReport>,
    },
    Review {
        run_id: RunId,
        decision: ReviewDecision,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<ResultBinding>,
    },
    Fail {
        reason: String,
    },
    Cancel {
        reason: String,
    },
    AcknowledgeCancel {
        run_id: RunId,
    },
}

impl TaskChange {
    pub fn transition(&self) -> TaskTransition {
        match self {
            Self::AddDependency { .. } => TaskTransition::AddDependency,
            Self::ReviseCriteria { .. } => TaskTransition::ReviseCriteria,
            Self::ResolveReviewEscalation { .. } => TaskTransition::ResolveReviewEscalation,
            Self::Prepare => TaskTransition::Prepare,
            Self::Assign { .. } => TaskTransition::Assign,
            Self::Claim { .. } => TaskTransition::Claim,
            Self::Progress { .. } => TaskTransition::Progress,
            Self::RequestHelp { .. } => TaskTransition::RequestHelp,
            Self::Resume => TaskTransition::Resume,
            Self::Submit { .. } => TaskTransition::Submit,
            Self::Review {
                decision: ReviewDecision::Accept,
                ..
            } => TaskTransition::Accept,
            Self::Review {
                decision: ReviewDecision::RequestChanges,
                ..
            } => TaskTransition::RequestChanges,
            Self::Fail { .. } => TaskTransition::Fail,
            Self::Cancel { .. } => TaskTransition::Cancel,
            Self::AcknowledgeCancel { .. } => TaskTransition::AcknowledgeCancel,
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChangeTask {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub idempotency_key: Uuid,
    pub expected_revision: i64,
    pub change: TaskChange,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskCommandTarget {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub idempotency_key: Uuid,
    pub expected_revision: i64,
}

impl TaskCommandTarget {
    pub fn with_change(self, change: TaskChange) -> ChangeTask {
        ChangeTask {
            workspace_id: self.workspace_id,
            task_id: self.task_id,
            idempotency_key: self.idempotency_key,
            expected_revision: self.expected_revision,
            change,
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClaimTask {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub idempotency_key: Uuid,
    pub expected_revision: i64,
    pub instance_id: Option<InstanceId>,
    pub runtime_template_id: Option<Uuid>,
}

impl ClaimTask {
    pub fn into_change(self) -> ChangeTask {
        ChangeTask {
            workspace_id: self.workspace_id,
            task_id: self.task_id,
            idempotency_key: self.idempotency_key,
            expected_revision: self.expected_revision,
            change: TaskChange::Claim {
                instance_id: self.instance_id,
                runtime_template_id: self.runtime_template_id,
            },
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubmitTaskResult {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub idempotency_key: Uuid,
    pub expected_revision: i64,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<ResultReport>,
}

impl SubmitTaskResult {
    pub fn into_change(self) -> ChangeTask {
        ChangeTask {
            workspace_id: self.workspace_id,
            task_id: self.task_id,
            idempotency_key: self.idempotency_key,
            expected_revision: self.expected_revision,
            change: TaskChange::Submit {
                summary: self.summary,
                report: self.report,
            },
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewTask {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub idempotency_key: Uuid,
    pub expected_revision: i64,
    pub run_id: RunId,
    pub decision: ReviewDecision,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<ResultBinding>,
}

impl ReviewTask {
    pub fn into_change(self) -> ChangeTask {
        ChangeTask {
            workspace_id: self.workspace_id,
            task_id: self.task_id,
            idempotency_key: self.idempotency_key,
            expected_revision: self.expected_revision,
            change: TaskChange::Review {
                run_id: self.run_id,
                decision: self.decision,
                reason: self.reason,
                binding: self.binding,
            },
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetTask {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub runs_after: Option<RunId>,
    pub run_limit: Option<u16>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListTasks {
    pub workspace_id: WorkspaceId,
    pub status: Option<TaskStatus>,
    pub after: Option<TaskId>,
    pub limit: Option<u16>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskDependency {
    pub task_id: TaskId,
    pub status: TaskStatus,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Task {
    pub task_id: TaskId,
    pub workspace_id: WorkspaceId,
    pub owner_user_id: UserId,
    pub author: Subject,
    pub accountable: Subject,
    pub assignee: Option<Recipient>,
    pub parent_task_id: Option<TaskId>,
    pub team_id: Option<TeamId>,
    pub delegation_grant_id: Option<Uuid>,
    pub status: TaskStatus,
    pub block_reason: Option<TaskBlockReason>,
    pub revision: i64,
    pub objective: String,
    pub acceptance_criteria: Vec<String>,
    #[serde(default = "first_criteria_revision")]
    pub acceptance_criteria_revision: i64,
    pub execution: TaskExecution,
    pub review_policy: ReviewPolicy,
    pub dependencies: Vec<TaskDependency>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskBlockReason {
    AwaitingOwner,
    ReviewEscalation,
}

fn first_criteria_revision() -> i64 {
    1
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Run {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub workspace_id: WorkspaceId,
    pub attempt_number: i64,
    pub executor: Recipient,
    pub instance_id: Option<InstanceId>,
    pub runtime_template_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<RunLease>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<crate::BackendEvidence>,
    pub status: RunStatus,
    pub summary: Option<String>,
    pub waiting_until: Option<DateTime<Utc>>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TaskView {
    pub task: Task,
    pub runs: Vec<Run>,
    pub next_run_after: Option<RunId>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TaskMutation {
    pub task: Task,
    pub run: Option<Run>,
    pub event_cursor: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TaskPage {
    pub tasks: Vec<Task>,
    pub next_after: Option<TaskId>,
}
