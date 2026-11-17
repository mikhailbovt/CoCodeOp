use chrono::{DateTime, Utc};
use office_domain::{AgentId, GrantRight, NoteId, Subject, UserId, Visibility, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AclGrant {
    pub user_id: UserId,
    pub agent_id: Option<AgentId>,
    pub right: GrantRight,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum NoteChange {
    Create {
        #[serde(default)]
        visibility: Visibility,
        #[serde(default)]
        grants: Vec<AclGrant>,
    },
    Revise {
        note_id: NoteId,
        expected_revision: i64,
    },
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpsertNote {
    pub workspace_id: WorkspaceId,
    pub idempotency_key: Uuid,
    pub title: String,
    pub body: String,
    pub change: NoteChange,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetNote {
    pub workspace_id: WorkspaceId,
    pub note_id: NoteId,
    pub revision: Option<i64>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NoteRevision {
    pub workspace_id: WorkspaceId,
    pub note_id: NoteId,
    pub revision: i64,
    pub visibility: Visibility,
    pub owner_user_id: UserId,
    pub author: Subject,
    pub editor: Subject,
    pub title: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}
