use chrono::{DateTime, Utc};
use office_domain::{AgentId, TaskId, TeamId, UserId, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ManagedRuntime;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    Interactive,
    Managed,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateRuntimeTemplate {
    pub workspace_id: WorkspaceId,
    pub idempotency_key: Uuid,
    pub name: String,
    pub kind: RuntimeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed: Option<ManagedRuntime>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RuntimeTemplate {
    pub template_id: Uuid,
    pub workspace_id: WorkspaceId,
    pub owner_user_id: UserId,
    pub name: String,
    pub kind: RuntimeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed: Option<ManagedRuntime>,
    pub created_at: DateTime<Utc>,
    pub event_cursor: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DelegationLimits {
    pub remaining_depth: u8,
    pub expires_at: DateTime<Utc>,
    pub max_attempts: u16,
    pub max_subtasks: u16,
    pub runtime_template_ids: Vec<Uuid>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IssueDelegation {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub expected_revision: i64,
    pub idempotency_key: Uuid,
    pub lead_agent_id: AgentId,
    pub limits: DelegationLimits,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DelegateTask {
    pub workspace_id: WorkspaceId,
    pub grant_id: Uuid,
    pub expected_revision: i64,
    pub idempotency_key: Uuid,
    pub worker_agent_id: AgentId,
    pub objective: String,
    pub acceptance_criteria: Vec<String>,
    pub limits: DelegationLimits,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetDelegation {
    pub workspace_id: WorkspaceId,
    pub grant_id: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RevokeDelegation {
    pub workspace_id: WorkspaceId,
    pub grant_id: Uuid,
    pub expected_revision: i64,
    pub idempotency_key: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DelegationGrant {
    pub grant_id: Uuid,
    pub parent_grant_id: Option<Uuid>,
    pub task_id: TaskId,
    pub workspace_id: WorkspaceId,
    pub team_id: TeamId,
    pub owner_user_id: UserId,
    pub issuer_agent_id: Option<AgentId>,
    pub grantee_agent_id: AgentId,
    pub limits: DelegationLimits,
    pub attempts_used: u16,
    pub subtasks_used: u16,
    pub revision: i64,
    pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DelegationReceipt {
    pub grant: DelegationGrant,
    pub task_revision: Option<i64>,
    pub event_cursor: Uuid,
}
