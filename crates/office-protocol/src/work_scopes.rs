use chrono::{DateTime, Utc};
use office_domain::{PathRule, SourcePath, TaskId, UserId, WorkPermissions, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::Recipient;

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IssueCheckoutGrant {
    pub workspace_id: WorkspaceId,
    pub checkout_id: Uuid,
    pub expected_revision: i64,
    pub idempotency_key: Uuid,
    pub recipient: Recipient,
    pub expires_at: DateTime<Utc>,
    pub permissions: WorkPermissions,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetCheckoutGrant {
    pub workspace_id: WorkspaceId,
    pub grant_id: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RevokeCheckoutGrant {
    pub workspace_id: WorkspaceId,
    pub grant_id: Uuid,
    pub expected_revision: i64,
    pub idempotency_key: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CheckoutGrant {
    pub grant_id: Uuid,
    pub workspace_id: WorkspaceId,
    pub checkout_id: Uuid,
    pub owner_user_id: UserId,
    pub recipient: Recipient,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub permissions: WorkPermissions,
    pub revision: i64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CheckoutGrantReceipt {
    pub grant: CheckoutGrant,
    pub event_cursor: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateCommandTemplate {
    pub workspace_id: WorkspaceId,
    pub checkout_id: Uuid,
    pub expected_revision: i64,
    pub idempotency_key: Uuid,
    pub template: CommandTemplateSpec,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandTemplateSpec {
    pub name: String,
    pub executable: String,
    pub arguments: Vec<CommandArgument>,
    pub source_inputs: Vec<PathRule>,
    pub scratch_outputs: Vec<PathRule>,
    pub timeout_seconds: u32,
    #[serde(default, skip_serializing_if = "disabled")]
    pub writable_staging: bool,
    #[serde(default, skip_serializing_if = "CommandResources::is_empty")]
    pub resources: CommandResources,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandResources {
    #[serde(default)]
    pub build_cache: bool,
    #[serde(default)]
    pub package_cache: bool,
    #[serde(default)]
    pub test_database: bool,
}

impl CommandResources {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

fn disabled(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandArgument {
    Literal {
        value: String,
    },
    Choice {
        key: String,
        values: Vec<String>,
    },
    Integer {
        key: String,
        minimum: i32,
        maximum: i32,
    },
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CommandTemplateReceipt {
    pub template_id: Uuid,
    pub workspace_id: WorkspaceId,
    pub checkout_id: Uuid,
    pub template: CommandTemplateSpec,
    pub event_cursor: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetTaskScope {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub checkout_id: Uuid,
    pub grant_id: Uuid,
    pub worker: Recipient,
    pub expected_task_revision: i64,
    pub expected_scope_revision: Option<i64>,
    pub idempotency_key: Uuid,
    pub expires_at: DateTime<Utc>,
    pub permissions: WorkPermissions,
    /// Omitting this field preserves the previous revision's mandatory dependencies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_read_paths: Option<Vec<SourcePath>>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetWorkScope {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub revision: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkScopeState {
    PendingActivation,
    Active,
    Superseded,
    Rejected,
    RevocationPending,
    Revoked,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkScopeOperation {
    #[default]
    Apply,
    Revoke,
}

impl WorkScopeOperation {
    fn is_apply(&self) -> bool {
        *self == Self::Apply
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RevokeWorkScope {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub expected_scope_revision: i64,
    pub idempotency_key: Uuid,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkScopeDefinition {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub checkout_id: Uuid,
    pub grant_id: Uuid,
    pub grant_revision: i64,
    pub scope_id: Uuid,
    pub revision: i64,
    #[serde(default, skip_serializing_if = "WorkScopeOperation::is_apply")]
    pub operation: WorkScopeOperation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_request_id: Option<Uuid>,
    pub worker: Recipient,
    pub expires_at: DateTime<Utc>,
    pub permissions: WorkPermissions,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_read_paths: Vec<SourcePath>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct WorkScope {
    pub definition: WorkScopeDefinition,
    pub state: WorkScopeState,
    pub policy_sequence: i64,
    pub acknowledged_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct WorkScopeReceipt {
    pub scope: WorkScope,
    pub task_revision: i64,
    pub event_cursor: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ScopeRevocationReceipt {
    pub scope: WorkScope,
    pub event_cursor: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopeDeployment {
    pub scope: WorkScopeDefinition,
    pub policy_sequence: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RootIdentity {
    pub host_fingerprint: String,
    pub directory_id: String,
    pub ancestors: Vec<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct NextScopeDeployment {
    pub checkout_id: Uuid,
    pub root: RootIdentity,
    pub pending: Option<ScopeDeployment>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AcknowledgeScope {
    pub checkout_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinator_epoch: Option<i64>,
    pub policy_sequence: i64,
    pub scope_id: Uuid,
    pub scope_revision: i64,
    pub deployment_sha256: String,
}
