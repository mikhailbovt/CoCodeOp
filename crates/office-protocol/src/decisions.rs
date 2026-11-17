use chrono::{DateTime, Utc};
use office_domain::{UserId, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::Recipient;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProposalOption {
    pub key: String,
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProposalContent {
    pub title: String,
    pub problem: String,
    pub options: Vec<ProposalOption>,
    pub recommendation: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProposalTarget {
    Workspace {
        topic: String,
        required_approvers: Vec<Recipient>,
    },
    SharedArea {
        checkout_id: Uuid,
        area_id: Uuid,
        expected_area_revision: i64,
        expected_generation: i64,
    },
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProposeDecision {
    pub workspace_id: WorkspaceId,
    pub target: ProposalTarget,
    pub content: ProposalContent,
    pub deadline: DateTime<Utc>,
    pub idempotency_key: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviseProposal {
    pub workspace_id: WorkspaceId,
    pub proposal_id: Uuid,
    pub expected_revision: i64,
    pub content: ProposalContent,
    pub deadline: DateTime<Utc>,
    pub idempotency_key: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProposalDecision {
    Approve,
    Reject,
    RequestChanges,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RespondProposal {
    pub workspace_id: WorkspaceId,
    pub proposal_id: Uuid,
    pub expected_revision: i64,
    pub decision: ProposalDecision,
    pub reason: String,
    pub idempotency_key: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetProposal {
    pub workspace_id: WorkspaceId,
    pub proposal_id: Uuid,
    pub revision: Option<i64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetDecision {
    pub workspace_id: WorkspaceId,
    pub decision_id: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetAreaDecision {
    pub workspace_id: WorkspaceId,
    pub checkout_id: Uuid,
    pub area_id: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProposalState {
    Open,
    ChangesRequested,
    Accepted,
    Rejected,
    Superseded,
    Escalated,
    Expired,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ProposalResponse {
    pub responder: Recipient,
    pub decision: ProposalDecision,
    pub reason: String,
    pub recorded_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Proposal {
    pub proposal_id: Uuid,
    pub workspace_id: WorkspaceId,
    pub revision: i64,
    pub author: Recipient,
    pub target: ProposalTarget,
    pub content: ProposalContent,
    pub required_approvers: Vec<Recipient>,
    pub responses: Vec<ProposalResponse>,
    pub deadline: DateTime<Utc>,
    pub state: ProposalState,
    pub escalated_to: Option<UserId>,
    pub decision_id: Option<Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Decision {
    pub decision_id: Uuid,
    pub workspace_id: WorkspaceId,
    pub proposal_id: Uuid,
    pub proposal_revision: i64,
    pub topic: String,
    pub checkout_id: Option<Uuid>,
    pub area_id: Option<Uuid>,
    pub area_revision: Option<i64>,
    pub generation: Option<i64>,
    pub selected_option: ProposalOption,
    pub content_sha256: String,
    pub accepted_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct AreaDecision {
    pub area_id: Uuid,
    pub area_revision: i64,
    pub generation: i64,
    pub decision: Option<Decision>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ProposalReceipt {
    pub proposal: Proposal,
    pub decision: Option<Decision>,
    pub event_cursor: Uuid,
}
