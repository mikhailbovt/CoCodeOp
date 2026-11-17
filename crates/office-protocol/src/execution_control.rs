use chrono::{DateTime, Utc};
use office_domain::{AgentId, TeamId, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionControlScope {
    Workspace,
    Owner,
    Team { team_id: TeamId },
    Agent { agent_id: AgentId },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Running,
    Paused,
    Stopped,
}

impl ExecutionMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecutionLimits {
    /// Includes queued jobs, active attempts, unresolved stops and jobs waiting for input.
    pub max_outstanding: u16,
    /// Includes active attempts and unresolved external effects.
    pub max_concurrent: u16,
}

impl ExecutionLimits {
    pub const fn is_valid(self) -> bool {
        self.max_outstanding <= 4096
            && self.max_concurrent <= 256
            && self.max_concurrent <= self.max_outstanding
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetExecutionControl {
    pub workspace_id: WorkspaceId,
    pub scope: ExecutionControlScope,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetExecutionControl {
    pub workspace_id: WorkspaceId,
    pub scope: ExecutionControlScope,
    pub expected_revision: i64,
    pub mode: ExecutionMode,
    pub limits: ExecutionLimits,
    pub reason: String,
    pub idempotency_key: Uuid,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecutionControl {
    pub workspace_id: WorkspaceId,
    pub scope: ExecutionControlScope,
    pub revision: i64,
    pub mode: ExecutionMode,
    pub limits: ExecutionLimits,
    pub reason: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
}
