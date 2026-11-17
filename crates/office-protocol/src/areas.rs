use office_domain::{PathRule, UserId, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::Recipient;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AreaPolicy {
    Owned { lead: Recipient },
    Shared { approvers: Vec<Recipient> },
}

impl AreaPolicy {
    pub fn approvers(&self) -> &[Recipient] {
        match self {
            Self::Owned { lead } => std::slice::from_ref(lead),
            Self::Shared { approvers } => approvers,
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DefineArea {
    pub workspace_id: WorkspaceId,
    pub checkout_id: Uuid,
    pub expected_checkout_revision: i64,
    pub idempotency_key: Uuid,
    pub area_id: Option<Uuid>,
    pub expected_area_revision: Option<i64>,
    pub name: String,
    pub paths: PathRule,
    pub policy: AreaPolicy,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetArea {
    pub workspace_id: WorkspaceId,
    pub area_id: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct Area {
    pub workspace_id: WorkspaceId,
    pub checkout_id: Uuid,
    pub area_id: Uuid,
    pub owner_user_id: UserId,
    pub revision: i64,
    pub name: String,
    pub paths: PathRule,
    pub policy: AreaPolicy,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AreaReceipt {
    pub area: Area,
    pub event_cursor: Uuid,
}
