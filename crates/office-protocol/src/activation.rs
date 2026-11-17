use chrono::{DateTime, Utc};
use office_domain::{AgentId, ChannelId, MessageId, RunId, RunnerId, TaskId, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ReviewDecision, Task, TaskResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActivationTrigger {
    Assignment,
    Question,
    ReviewRequest,
    Answer,
}

impl ActivationTrigger {
    pub fn event_type(self) -> &'static str {
        match self {
            Self::Assignment => "task.assignment_requested",
            Self::Question => "message.question",
            Self::ReviewRequest => "task.review_requested",
            Self::Answer => "message.answer",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ManagedActivation {
    pub runner_id: RunnerId,
    pub runtime_template_id: Uuid,
    pub triggers: Vec<ActivationTrigger>,
    pub debounce_seconds: u16,
    pub cooldown_seconds: u16,
    pub max_turns_per_thread: u16,
    pub max_turns_per_hour: u16,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetActivationPolicy {
    pub workspace_id: WorkspaceId,
    pub agent_id: AgentId,
    pub expected_revision: i64,
    pub idempotency_key: Uuid,
    /// Null disables automatic turns for this agent.
    pub managed: Option<ManagedActivation>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetActivationPolicy {
    pub workspace_id: WorkspaceId,
    pub agent_id: AgentId,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ActivationPolicy {
    pub workspace_id: WorkspaceId,
    pub agent_id: AgentId,
    pub revision: i64,
    pub managed: Option<ManagedActivation>,
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct RunActivation {
    pub trigger: ActivationTrigger,
    pub thread_id: Uuid,
    pub sources: Vec<ActivationSource>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActivationSource {
    Message {
        event_id: Uuid,
        channel_id: ChannelId,
        question_id: MessageId,
        message_id: MessageId,
        body: String,
        question_open: bool,
    },
    Task {
        event_id: Uuid,
        task_id: TaskId,
        trigger_revision: i64,
        result_run_id: Option<RunId>,
    },
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnswerActivationQuestion {
    pub question_id: MessageId,
    pub idempotency_key: Uuid,
    pub body: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ActivationReview {
    pub task: Task,
    pub result: TaskResult,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewActivationResult {
    pub idempotency_key: Uuid,
    pub result_sha256: String,
    pub decision: ReviewDecision,
    pub reason: String,
}
