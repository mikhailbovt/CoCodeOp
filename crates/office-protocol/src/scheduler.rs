use chrono::{DateTime, Utc};
use office_domain::{AgentId, RunId, RunnerId, SourcePath, TaskId, TeamId, UserId, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ManagedRuntime {
    pub adapter_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_sha256: Option<String>,
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    pub reservation_units: i64,
}

impl ManagedRuntime {
    pub fn is_valid(&self) -> bool {
        valid_capability(&self.adapter_id)
            && self.manifest_sha256.as_ref().is_none_or(|hash| {
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            })
            && valid_capabilities(&self.required_capabilities, 16)
            && (1..=1_000_000_000).contains(&self.reservation_units)
    }
}

pub fn valid_capabilities(capabilities: &[String], limit: usize) -> bool {
    capabilities.len() <= limit
        && capabilities
            .iter()
            .enumerate()
            .all(|(index, label)| valid_capability(label) && !capabilities[..index].contains(label))
}

fn valid_capability(label: &str) -> bool {
    (1..=64).contains(&label.len())
        && label.as_bytes()[0].is_ascii_alphanumeric()
        && label.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdvertiseRunner {
    pub expected_revision: i64,
    pub capacity: u16,
    pub runtime_template_ids: Vec<Uuid>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosted_checkout_ids: Vec<Uuid>,
    pub capabilities: Vec<String>,
    pub ttl_seconds: u16,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetRunnerProfile {
    pub workspace_id: WorkspaceId,
    pub runner_id: RunnerId,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct RunnerProfile {
    pub runner_id: RunnerId,
    pub workspace_id: WorkspaceId,
    pub owner_user_id: UserId,
    pub revision: i64,
    pub capacity: u16,
    pub runtime_template_ids: Vec<Uuid>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosted_checkout_ids: Vec<Uuid>,
    pub capabilities: Vec<String>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetExecutionBudget {
    pub workspace_id: WorkspaceId,
    pub expected_revision: i64,
    pub limit_units: i64,
    pub idempotency_key: Uuid,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetExecutionBudget {
    pub workspace_id: WorkspaceId,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecutionBudget {
    pub budget_id: Uuid,
    pub workspace_id: WorkspaceId,
    pub owner_user_id: UserId,
    pub revision: i64,
    pub limit_units: i64,
    pub charged_units: i64,
    pub reserved_units: i64,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecutionBudgetReceipt {
    pub budget: ExecutionBudget,
    pub event_cursor: Uuid,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueExecution {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub expected_task_revision: i64,
    pub worker_agent_id: AgentId,
    pub runtime_template_id: Uuid,
    pub idempotency_key: Uuid,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetExecution {
    pub workspace_id: WorkspaceId,
    pub job_id: Uuid,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelExecution {
    pub workspace_id: WorkspaceId,
    pub job_id: Uuid,
    pub expected_revision: i64,
    pub reason: String,
    pub idempotency_key: Uuid,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    Queued,
    Leased,
    ReconcileRequired,
    Cancelled,
    DeadLetter,
    Completed,
    WaitingInput,
    Resumed,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionTerminalReason {
    AttemptLimit,
    EligibilityChanged,
    StartInterrupted,
    ExecutionFailed,
    InputExpired,
    EmergencyStopped,
    RestoreReviewRequired,
}

impl ExecutionTerminalReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AttemptLimit => "attempt_limit",
            Self::EligibilityChanged => "eligibility_changed",
            Self::StartInterrupted => "start_interrupted",
            Self::ExecutionFailed => "execution_failed",
            Self::InputExpired => "input_expired",
            Self::EmergencyStopped => "emergency_stopped",
            Self::RestoreReviewRequired => "restore_review_required",
        }
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecutionJob {
    pub job_id: Uuid,
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub task_revision: i64,
    pub owner_user_id: UserId,
    pub worker_agent_id: AgentId,
    pub team_id: Option<TeamId>,
    pub runtime_template_id: Uuid,
    pub checkout_id: Option<Uuid>,
    pub scope_id: Option<Uuid>,
    pub scope_revision: Option<i64>,
    pub revision: i64,
    pub state: ExecutionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_run_id: Option<RunId>,
    #[serde(default)]
    pub managed_attempts: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_not_before: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<ExecutionTerminalReason>,
    pub reserved_units: i64,
    pub enqueued_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecutionReceipt {
    pub job: ExecutionJob,
    pub event_cursor: Uuid,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClaimExecution {
    pub claim_key: Uuid,
    #[serde(default)]
    pub analysis_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_gateway_templates: Option<Vec<Uuid>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenewExecutionLease {
    pub job_id: Uuid,
    pub run_id: RunId,
    pub run_fence: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunLease {
    pub job_id: Uuid,
    pub runner_id: RunnerId,
    pub run_fence: i64,
    pub lease_until: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecutionLease {
    pub run: crate::Run,
    pub managed_runtime: ManagedRuntime,
    pub checkout_id: Option<Uuid>,
    pub scope_id: Option<Uuid>,
    pub scope_revision: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuthorizeExecutionStart {
    pub lease: RenewExecutionLease,
    pub idempotency_key: Uuid,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StartEffectRef {
    pub lease: RenewExecutionLease,
    pub effect_id: Uuid,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObserveExecutionStart {
    pub effect: StartEffectRef,
    pub external_id: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum StopObservation {
    NeverStarted,
    Stopped { external_id: String },
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReconcileExecutionStart {
    pub effect: StartEffectRef,
    pub observation: StopObservation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StartEffectState {
    Authorized,
    Started,
    Unknown,
    NeverStarted,
    Stopped,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecutionStartEffect {
    pub effect_id: Uuid,
    pub runner_id: RunnerId,
    pub approval_sha256: String,
    pub state: StartEffectState,
    pub external_id: Option<String>,
    pub authorized_at: DateTime<Utc>,
    pub confirmed_at: Option<DateTime<Utc>>,
    pub reconciled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecutionStartReceipt {
    pub run: crate::Run,
    pub effect: ExecutionStartEffect,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecutionStartGrant {
    #[serde(flatten)]
    pub execution: ExecutionStartReceipt,
    pub task: ExecutionTaskBrief,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecutionTaskBrief {
    pub task_id: TaskId,
    pub task_revision: i64,
    pub objective: String,
    pub criteria_revision: i64,
    pub acceptance_criteria: Vec<String>,
    pub checkout_id: Option<Uuid>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IssueRunCredential {
    pub effect: StartEffectRef,
    pub credential_digest: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct RunCredentialReceipt {
    pub credential_id: Uuid,
    pub run_id: RunId,
    pub issued_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct RunContextView {
    pub workspace_id: WorkspaceId,
    pub run_id: RunId,
    pub executor: crate::Recipient,
    pub task: ExecutionTaskBrief,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_revision: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_read_paths: Vec<SourcePath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation: Option<crate::RunActivation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<crate::RunContinuation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_instructions: Option<crate::WorkerInstructions>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum ProcessResult {
    Succeeded {
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        report: Option<crate::ResultReport>,
    },
    Failed {
        reason: String,
    },
    WaitingInput {
        question: String,
    },
}

impl ProcessResult {
    pub fn matches_backend_state(&self, terminal: crate::BackendState) -> bool {
        use crate::BackendState;
        match self {
            Self::Succeeded { .. } => terminal == BackendState::Completed,
            Self::WaitingInput { .. } => terminal == BackendState::InputRequired,
            Self::Failed { .. } => terminal == BackendState::Failed,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompleteExecution {
    pub effect: StartEffectRef,
    pub external_id: String,
    pub idempotency_key: Uuid,
    pub exit_code: u8,
    pub result: ProcessResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<crate::BackendEvidence>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecutionCompletionReceipt {
    pub execution: ExecutionStartReceipt,
    pub exit_code: u8,
    pub task_revision: i64,
}
