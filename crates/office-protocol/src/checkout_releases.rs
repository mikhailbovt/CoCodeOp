use chrono::{DateTime, Utc};
use office_domain::{RunnerId, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{CheckoutContentHead, CheckoutCoordinatorRef};

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetCheckoutReleasePolicy {
    pub workspace_id: WorkspaceId,
    pub checkout_id: Uuid,
    pub expected_revision: i64,
    pub required_template_ids: Vec<Uuid>,
    pub idempotency_key: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CheckoutReleasePolicy {
    pub workspace_id: WorkspaceId,
    pub checkout_id: Uuid,
    pub revision: i64,
    pub required_template_ids: Vec<Uuid>,
    pub accepted_snapshot_id: Option<Uuid>,
    pub accepted_commit_sha256: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestCheckoutRelease {
    pub workspace_id: WorkspaceId,
    pub checkout_id: Uuid,
    pub snapshot_id: Uuid,
    pub expected_policy_revision: i64,
    pub expected_commit_sha256: Option<String>,
    pub acknowledge_external_baseline: bool,
    pub command_ids: Vec<Uuid>,
    pub idempotency_key: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetCheckoutRelease {
    pub workspace_id: WorkspaceId,
    pub checkout_id: Uuid,
    pub release_id: Uuid,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutReleaseState {
    Queued,
    Applying,
    Accepted,
    SourceChanged,
    RefConflict,
    StorageUnavailable,
    Expired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckoutReleaseOutcome {
    Accepted {
        commit_sha256: String,
        tree_sha256: String,
    },
    SourceChanged,
    RefConflict,
    StorageUnavailable,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CheckoutRelease {
    pub workspace_id: WorkspaceId,
    pub checkout_id: Uuid,
    pub release_id: Uuid,
    pub snapshot_id: Uuid,
    pub host_runner_id: RunnerId,
    pub head: CheckoutContentHead,
    pub policy_revision: i64,
    pub authorization_sha256: String,
    pub approved_changes: u32,
    pub command_ids: Vec<Uuid>,
    pub state: CheckoutReleaseState,
    pub outcome: Option<CheckoutReleaseOutcome>,
    pub finalized_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckoutReleaseOrder {
    pub checkout_id: Uuid,
    pub release_id: Uuid,
    pub snapshot_id: Uuid,
    pub head: CheckoutContentHead,
    pub tree_sha256: String,
    pub expected_commit_sha256: Option<String>,
    pub authorization_sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportCheckoutRelease {
    pub coordinator: CheckoutCoordinatorRef,
    pub release_id: Uuid,
    pub outcome: CheckoutReleaseOutcome,
}
