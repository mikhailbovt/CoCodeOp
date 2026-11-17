use chrono::{DateTime, Utc};
use office_domain::WorkspaceId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadOperationsMetrics {
    pub workspace_id: WorkspaceId,
    pub window_seconds: u32,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct OperationsMetrics {
    pub workspace_id: WorkspaceId,
    pub observed_at: DateTime<Utc>,
    pub since: DateTime<Utc>,
    pub executions: ExecutionMetrics,
    pub usage: UsageMetrics,
    pub checkout: CheckoutMetrics,
    pub inbox: InboxMetrics,
    pub instance_rejections: Option<InstanceRejections>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecutionMetrics {
    pub queued_jobs: u64,
    pub running_attempts: u64,
    pub waiting_input_attempts: u64,
    pub blocked_tasks: u64,
    pub reconciling_jobs: u64,
    pub oldest_dispatch_wait_ms: Option<u64>,
    pub first_dispatch: LatencyMetrics,
    pub retries: u64,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct LatencyMetrics {
    pub samples: u64,
    pub p95_ms: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct UsageMetrics {
    pub finished_managed_attempts: u64,
    pub measured_attempts: u64,
    pub input_tokens: String,
    pub output_tokens: String,
    pub cached_input_tokens: String,
    pub reasoning_output_tokens: String,
    pub reported_cost_attempts: u64,
    pub reported_usd_micros: String,
    pub estimated_cost_attempts: u64,
    pub estimated_usd_micros: String,
    pub unknown_cost_attempts: u64,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct CheckoutMetrics {
    pub reconciling_checkouts: u64,
    pub newest_snapshot_age_ms: Option<u64>,
    pub publication: LatencyMetrics,
    pub claim_handoff: LatencyMetrics,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct InboxMetrics {
    /// Addressed, visible human inbox events that remain within event retention.
    pub pending_addressed: u64,
    pub oldest_pending_ms: Option<u64>,
}

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct InstanceRejections {
    pub uptime_seconds: u64,
    pub denied_actions: u64,
    pub scope_denials: u64,
    pub stale_fences: u64,
    pub content_conflicts: u64,
}
