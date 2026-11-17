use office_domain::{UserId, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetWorkspaceBudget {
    pub workspace_id: WorkspaceId,
    pub expected_revision: i64,
    pub idempotency_key: Uuid,
    pub limit_units: i64,
    pub member_user_ids: Vec<UserId>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceBudget {
    pub workspace_id: WorkspaceId,
    pub revision: i64,
    pub limit_units: i64,
    pub member_user_ids: Vec<UserId>,
    pub reserved_units: i64,
    pub charged_units: i64,
}
