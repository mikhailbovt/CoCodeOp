use chrono::{DateTime, Utc};
use office_domain::{AgentId, ChannelId, MessageId, NoteId, Subject, TeamId, UserId, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Recipient {
    pub user_id: UserId,
    pub agent_id: Option<AgentId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChannelAudience {
    Workspace,
    Team { team_id: TeamId },
    Direct { recipient: Recipient },
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateChannel {
    pub workspace_id: WorkspaceId,
    pub idempotency_key: Uuid,
    pub name: String,
    pub audience: ChannelAudience,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Channel {
    pub workspace_id: WorkspaceId,
    pub channel_id: ChannelId,
    pub name: String,
    pub audience: ChannelAudience,
    pub author: Subject,
    pub created_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListChannels {
    pub workspace_id: WorkspaceId,
    pub after: Option<ChannelId>,
    pub limit: Option<u16>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ChannelPage {
    pub channels: Vec<Channel>,
    pub next_after: Option<ChannelId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoteLink {
    pub note_id: NoteId,
    pub revision: i64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendMessage {
    pub workspace_id: WorkspaceId,
    pub channel_id: ChannelId,
    pub idempotency_key: Uuid,
    pub body: String,
    pub reply_to: Option<MessageId>,
    #[serde(default)]
    pub mentions: Vec<Recipient>,
    #[serde(default)]
    pub notes: Vec<NoteLink>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Message {
    pub workspace_id: WorkspaceId,
    pub channel_id: ChannelId,
    pub message_id: MessageId,
    pub thread_id: MessageId,
    pub reply_to: Option<MessageId>,
    pub author: Subject,
    pub body: String,
    pub mentions: Vec<Recipient>,
    pub notes: Vec<NoteLink>,
    pub created_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadThread {
    pub workspace_id: WorkspaceId,
    pub channel_id: ChannelId,
    pub thread_id: Option<MessageId>,
    pub after: Option<MessageId>,
    pub limit: Option<u16>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ThreadPage {
    pub messages: Vec<Message>,
    pub next_after: Option<MessageId>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AskQuestion {
    pub workspace_id: WorkspaceId,
    pub channel_id: ChannelId,
    pub recipient_user_id: UserId,
    pub recipient_agent_id: Option<AgentId>,
    pub idempotency_key: Uuid,
    pub body: String,
    pub reply_to: Option<MessageId>,
    pub expires_in_seconds: Option<u32>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnswerQuestion {
    pub workspace_id: WorkspaceId,
    pub question_id: MessageId,
    pub idempotency_key: Uuid,
    pub body: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetQuestion {
    pub workspace_id: WorkspaceId,
    pub question_id: MessageId,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ThreadQuestion {
    pub workspace_id: WorkspaceId,
    pub channel_id: ChannelId,
    pub question_id: MessageId,
    pub question_event_id: Uuid,
    pub asker: Recipient,
    pub recipient: Recipient,
    pub expires_at: DateTime<Utc>,
    pub answer_id: Option<MessageId>,
    pub answer_event_id: Option<Uuid>,
}
