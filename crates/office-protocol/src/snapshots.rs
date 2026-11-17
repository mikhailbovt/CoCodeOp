use chrono::{DateTime, Utc};
use office_domain::{RunnerId, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{CheckoutContentHead, CheckoutCoordinatorRef, SourceBaseline};

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateCheckoutSnapshot {
    pub workspace_id: WorkspaceId,
    pub checkout_id: Uuid,
    pub expected_checkout_revision: i64,
    pub idempotency_key: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetCheckoutSnapshot {
    pub workspace_id: WorkspaceId,
    pub checkout_id: Uuid,
    pub snapshot_id: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutSnapshotState {
    Queued,
    Ready,
    SourceChanged,
    StorageUnavailable,
    Expired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutSnapshotProof {
    pub baseline: SourceBaseline,
    pub git_tree_sha256: String,
    pub git_commit_sha256: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CheckoutSnapshot {
    pub workspace_id: WorkspaceId,
    pub checkout_id: Uuid,
    pub snapshot_id: Uuid,
    pub host_runner_id: RunnerId,
    pub head: CheckoutContentHead,
    pub state: CheckoutSnapshotState,
    pub proof: Option<CheckoutSnapshotProof>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckoutSnapshotOrder {
    pub snapshot_id: Uuid,
    pub checkout_id: Uuid,
    pub head: CheckoutContentHead,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckoutSnapshotOutcome {
    Ready { proof: CheckoutSnapshotProof },
    SourceChanged,
    StorageUnavailable,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportCheckoutSnapshot {
    pub coordinator: CheckoutCoordinatorRef,
    pub snapshot_id: Uuid,
    pub outcome: CheckoutSnapshotOutcome,
}
