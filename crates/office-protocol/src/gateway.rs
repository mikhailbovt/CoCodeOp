use chrono::{DateTime, Utc};
use office_domain::{PathAccess, RunnerId, SourceManifest, SourcePath, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{RootIdentity, WorkScopeDefinition};

pub const MAX_GATEWAY_READ_BYTES: u64 = 24 * 1024;
pub const MAX_GATEWAY_QUERY_RESULTS: u16 = 24;
pub const MAX_GATEWAY_SNIPPET_BYTES: usize = 320;
pub const MAX_GATEWAY_STATUS_HASH_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StartCheckoutCoordinator {
    pub checkout_id: Uuid,
    pub instance_key: Uuid,
    pub root: RootIdentity,
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutCoordinatorRef {
    pub checkout_id: Uuid,
    pub epoch: i64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CheckoutCoordinatorLease {
    pub workspace_id: WorkspaceId,
    pub checkout_id: Uuid,
    pub runner_id: RunnerId,
    pub epoch: i64,
    pub started_at: DateTime<Utc>,
    pub lease_until: DateTime<Utc>,
    pub ready_at: Option<DateTime<Utc>>,
    pub stopped_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadCheckoutFile {
    pub path: SourcePath,
    pub scope_revision: i64,
    pub idempotency_key: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchCheckoutCode {
    pub needle: String,
    pub under: Option<SourcePath>,
    pub limit: Option<u16>,
    pub scope_revision: i64,
    pub idempotency_key: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetCheckoutStatus {
    pub under: Option<SourcePath>,
    pub after: Option<SourcePath>,
    pub limit: Option<u16>,
    pub scope_revision: i64,
    pub idempotency_key: Uuid,
}

#[derive(PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayQuery {
    File {
        path: SourcePath,
    },
    Search {
        needle: String,
        under: Option<SourcePath>,
        limit: u16,
    },
    Status {
        under: Option<SourcePath>,
        after: Option<SourcePath>,
        limit: u16,
    },
}

impl GatewayQuery {
    pub fn is_valid(&self) -> bool {
        let valid_limit = |limit| (1..=MAX_GATEWAY_QUERY_RESULTS).contains(&limit);
        match self {
            Self::File { .. } => true,
            Self::Search { needle, limit, .. } => {
                (1..=256).contains(&needle.len())
                    && !needle.contains(['\0', '\r', '\n'])
                    && valid_limit(*limit)
            }
            Self::Status { limit, .. } => valid_limit(*limit),
        }
    }

    pub fn path(&self) -> Option<&SourcePath> {
        match self {
            Self::File { path } => Some(path),
            _ => None,
        }
    }

    pub fn admits(&self, manifest: &SourceManifest, read: &PathAccess) -> bool {
        let allowed = |path: &SourcePath| manifest.allows(path) && read.allows(path);
        let subtree = |under: Option<&SourcePath>| {
            under.is_none_or(|path| manifest.descend(path) && read.descend(path))
        };
        match self {
            Self::File { path } => allowed(path),
            Self::Search { under, .. } => subtree(under.as_ref()),
            Self::Status { under, after, .. } => {
                subtree(under.as_ref())
                    && after.as_ref().is_none_or(|path| {
                        allowed(path)
                            && under
                                .as_ref()
                                .is_none_or(|prefix| prefix != path && prefix.contains(path))
                    })
            }
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GatewayReadOrder {
    pub read_id: Uuid,
    pub epoch: i64,
    pub scope: WorkScopeDefinition,
    pub query: GatewayQuery,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReportGatewayRead {
    pub coordinator: CheckoutCoordinatorRef,
    pub read_id: Uuid,
    pub outcome: GatewayReadOutcome,
}

#[derive(PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayReadOutcome {
    Success {
        file: GatewayFile,
    },
    Search {
        matches: Vec<GatewaySearchMatch>,
        truncated: bool,
    },
    Status {
        files: Vec<GatewayStatusEntry>,
        next_after: Option<SourcePath>,
    },
    Forbidden,
    Missing,
    TooLarge,
    Changed,
    Unavailable,
}

#[derive(PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GatewaySearchMatch {
    pub path: SourcePath,
    pub line_number: u32,
    pub file_sha256: String,
    pub snippet: String,
}

#[derive(PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GatewayStatusEntry {
    pub path: SourcePath,
    pub size_bytes: u64,
    pub executable: bool,
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GatewayFile {
    pub sha256: String,
    pub size_bytes: u64,
    pub encoding: FileEncoding,
    pub content: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileEncoding {
    Utf8,
    Base64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GatewayReadReceipt {
    #[serde(flatten)]
    pub origin: GatewayOrigin,
    pub path: SourcePath,
    pub file: GatewayFile,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GatewayOrigin {
    pub read_id: Uuid,
    pub checkout_id: Uuid,
    pub host_runner_id: RunnerId,
    pub coordinator_epoch: i64,
    pub scope_revision: i64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GatewaySearchReceipt {
    #[serde(flatten)]
    pub origin: GatewayOrigin,
    pub matches: Vec<GatewaySearchMatch>,
    pub truncated: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GatewayStatusReceipt {
    #[serde(flatten)]
    pub origin: GatewayOrigin,
    pub files: Vec<GatewayStatusEntry>,
    pub next_after: Option<SourcePath>,
}
