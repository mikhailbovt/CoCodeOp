use office_domain::{ChannelId, MessageId, NoteId, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Search {
    pub workspace_id: WorkspaceId,
    pub query: String,
    pub limit: Option<u16>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SearchSource {
    Note {
        note_id: NoteId,
        revision: i64,
    },
    Message {
        channel_id: ChannelId,
        thread_id: MessageId,
        message_id: MessageId,
    },
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SearchHit {
    pub source: SearchSource,
    pub title: Option<String>,
    pub excerpt: String,
    pub score: f32,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    pub truncated: bool,
}
