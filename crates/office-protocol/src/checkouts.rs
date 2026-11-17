use chrono::{DateTime, Utc};
use office_domain::{
    RunnerId, SourceManifest, SourcePath, TaskId, UserId, Visibility, WorkspaceId,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MAX_SOURCE_FILES: u32 = 8192;
pub const MAX_SOURCE_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_ROOT_ANCESTORS: usize = 64;

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateProject {
    pub workspace_id: WorkspaceId,
    pub idempotency_key: Uuid,
    pub name: String,
    #[serde(default)]
    pub visibility: Visibility,
    #[serde(default)]
    pub grants: Vec<crate::AclGrant>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetProject {
    pub workspace_id: WorkspaceId,
    pub project_id: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct Project {
    pub project_id: Uuid,
    pub workspace_id: WorkspaceId,
    pub owner_user_id: UserId,
    pub name: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProjectReceipt {
    pub project: Project,
    pub event_cursor: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestCheckout {
    pub workspace_id: WorkspaceId,
    pub project_id: Uuid,
    pub host_runner_id: RunnerId,
    pub idempotency_key: Uuid,
    pub manifest: SourceManifest,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetCheckout {
    pub workspace_id: WorkspaceId,
    pub checkout_id: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectCheckoutHost {
    pub checkout_id: Uuid,
    pub scope_task_id: Option<TaskId>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CheckoutHostHealth {
    pub checkout_id: Uuid,
    pub root: crate::RootIdentity,
    pub manifest: SourceManifest,
    pub lease: Option<crate::CheckoutCoordinatorLease>,
    pub state_id: Option<Uuid>,
    pub head: Option<crate::CheckoutContentHead>,
    pub applies_pending: bool,
    pub needs_reconcile: bool,
    pub scope: Option<ScopeDeploymentProof>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ScopeDeploymentProof {
    pub scope_id: Uuid,
    pub revision: i64,
    pub deployment_sha256: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutMode {
    Shared,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WriteStrategy {
    Mediated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutState {
    PendingRegistration,
    Registered,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProjectCheckout {
    pub checkout_id: Uuid,
    pub workspace_id: WorkspaceId,
    pub project_id: Uuid,
    pub host_runner_id: RunnerId,
    pub mode: CheckoutMode,
    pub write_strategy: WriteStrategy,
    pub state: CheckoutState,
    pub revision: i64,
    pub root_ref: Option<Uuid>,
    pub manifest: Option<SourceManifest>,
    pub baseline: Option<SourceBaseline>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CheckoutReceipt {
    pub checkout: ProjectCheckout,
    pub event_cursor: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutEnrollment {
    pub checkout_id: Uuid,
    pub expected_revision: i64,
    pub proof: RootProof,
}

/// Runner-only network payload. The absolute host path never leaves the coordinator.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RootProof {
    pub host_fingerprint: String,
    pub directory_id: String,
    pub ancestors: Vec<String>,
    pub baseline: SourceBaseline,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceBaseline {
    pub manifest_sha256: String,
    pub file_count: u32,
    pub total_bytes: u64,
}

/// Local preview only. Enrollment sends the baseline hash and counts.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SourceEntry {
    pub path: SourcePath,
    pub sha256: String,
    pub size_bytes: u64,
    pub executable: bool,
}
