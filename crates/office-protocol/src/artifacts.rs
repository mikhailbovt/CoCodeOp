use chrono::{DateTime, Utc};
use office_domain::{RunId, TaskId, UserId, Visibility, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AclGrant, Recipient};

pub const ARTIFACT_CHUNK_BYTES: usize = 1024 * 1024;
pub const MAX_ARTIFACT_BYTES: i64 = 64 * ARTIFACT_CHUNK_BYTES as i64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ArtifactMime {
    #[serde(rename = "application/octet-stream")]
    Binary,
    #[serde(rename = "application/json")]
    Json,
    #[serde(rename = "application/gzip")]
    Gzip,
    #[serde(rename = "application/zip")]
    Zip,
    #[serde(rename = "text/plain")]
    Text,
}

impl ArtifactMime {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Binary => "application/octet-stream",
            Self::Json => "application/json",
            Self::Gzip => "application/gzip",
            Self::Zip => "application/zip",
            Self::Text => "text/plain",
        }
    }
}

impl std::str::FromStr for ArtifactMime {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "application/octet-stream" => Ok(Self::Binary),
            "application/json" => Ok(Self::Json),
            "application/gzip" => Ok(Self::Gzip),
            "application/zip" => Ok(Self::Zip),
            "text/plain" => Ok(Self::Text),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactProvenance {
    pub task_id: TaskId,
    pub run_id: Option<RunId>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BeginArtifactUpload {
    pub workspace_id: WorkspaceId,
    pub artifact_id: Option<Uuid>,
    pub expected_revision: i64,
    pub idempotency_key: Uuid,
    pub name: String,
    pub mime: ArtifactMime,
    pub size_bytes: i64,
    pub sha256: String,
    pub retention_days: u16,
    pub provenance: Option<ArtifactProvenance>,
    /// Applies when creating a new artifact. Revisions use the original visibility.
    pub visibility: Option<Visibility>,
    #[serde(default)]
    pub grants: Vec<AclGrant>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ArtifactUpload {
    pub workspace_id: WorkspaceId,
    pub artifact_id: Uuid,
    pub upload_id: Uuid,
    pub revision: i64,
    pub chunk_bytes: usize,
    pub chunk_count: i32,
    pub upload_until: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FinishArtifactUpload {
    pub workspace_id: WorkspaceId,
    pub upload_id: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetArtifact {
    pub workspace_id: WorkspaceId,
    pub artifact_id: Uuid,
    pub revision: Option<i64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct Artifact {
    pub workspace_id: WorkspaceId,
    pub artifact_id: Uuid,
    pub revision: i64,
    pub owner_user_id: UserId,
    pub author: Recipient,
    pub name: String,
    pub mime: ArtifactMime,
    pub size_bytes: i64,
    pub sha256: String,
    pub chunk_count: i32,
    pub provenance: Option<ArtifactProvenance>,
    pub finalized_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub available: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ArtifactChunkReceipt {
    pub index: i32,
    pub size_bytes: usize,
    pub sha256: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CleanupArtifacts {
    pub workspace_id: WorkspaceId,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ArtifactCleanup {
    pub abandoned_uploads: i64,
    pub expired_revisions: i64,
    pub freed_bytes: i64,
}

pub fn artifact_chunk_count(size: i64) -> i32 {
    ((size + ARTIFACT_CHUNK_BYTES as i64 - 1) / ARTIFACT_CHUNK_BYTES as i64) as i32
}

pub fn artifact_chunk_length(size: i64, index: i32) -> Option<usize> {
    if !(1..=MAX_ARTIFACT_BYTES).contains(&size)
        || !(0..artifact_chunk_count(size)).contains(&index)
    {
        return None;
    }
    let remaining = size - i64::from(index) * ARTIFACT_CHUNK_BYTES as i64;
    Some(remaining.min(ARTIFACT_CHUNK_BYTES as i64) as usize)
}
