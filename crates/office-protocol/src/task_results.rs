use chrono::{DateTime, Utc};
use office_domain::{RunId, TaskId, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Recipient, ReviewDecision};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactFormat {
    Text,
    Json,
    Diff,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResultAttachment {
    pub name: String,
    pub format: ArtifactFormat,
    pub content: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReportedCheckOutcome {
    Passed,
    Failed,
    Skipped,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentReportedCheck {
    pub name: String,
    pub outcome: ReportedCheckOutcome,
    pub notes: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResultReport {
    #[serde(default)]
    pub artifacts: Vec<ResultAttachment>,
    #[serde(default)]
    pub agent_reported_checks: Vec<AgentReportedCheck>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ResultArtifact {
    pub name: String,
    pub format: ArtifactFormat,
    pub content: String,
    pub content_sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct TaskResultContent {
    pub summary: String,
    pub criteria_revision: i64,
    pub acceptance_criteria: Vec<String>,
    pub artifacts: Vec<ResultArtifact>,
    pub agent_reported_checks: Vec<AgentReportedCheck>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResultBinding {
    pub result_revision: i64,
    pub result_sha256: String,
    pub criteria_revision: i64,
    pub criteria_sha256: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ResultReview {
    pub reviewer: Recipient,
    pub decision: ReviewDecision,
    pub reason: String,
    pub reviewed_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct TaskResult {
    pub result_id: Uuid,
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub run_id: RunId,
    pub author: Recipient,
    pub binding: ResultBinding,
    pub content: TaskResultContent,
    pub review: Option<ResultReview>,
    pub submitted_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetTaskResult {
    pub workspace_id: WorkspaceId,
    pub task_id: TaskId,
    pub revision: Option<i64>,
}
