use chrono::{DateTime, Utc};
use office_domain::WorkspaceId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadInbox {
    pub workspace_id: WorkspaceId,
    pub cursor: Option<Uuid>,
    pub limit: Option<u16>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AckInbox {
    pub workspace_id: WorkspaceId,
    pub event_id: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct InboxEvent {
    pub event_id: Uuid,
    pub event_type: String,
    pub payload: Value,
    pub created_at: DateTime<Utc>,
    pub acknowledged_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct InboxPage {
    pub events: Vec<InboxEvent>,
    pub next_cursor: Uuid,
    pub has_more: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct InboxReceipt {
    pub event_id: Uuid,
    pub acknowledged_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetInboxDelivery {
    pub workspace_id: WorkspaceId,
    pub event_id: Uuid,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InboxDeliveryState {
    WaitingForAgent,
    Delivered,
    Acknowledged,
}

impl InboxDeliveryState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WaitingForAgent => "waiting_for_agent",
            Self::Delivered => "delivered",
            Self::Acknowledged => "acknowledged",
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct InboxDelivery {
    pub event_id: Uuid,
    pub state: InboxDeliveryState,
    pub delivered_at: Option<DateTime<Utc>>,
    pub acknowledged_at: Option<DateTime<Utc>>,
}
