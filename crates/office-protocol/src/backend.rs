use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{ResultAttachment, is_sha256, valid_capabilities};

pub const BACKEND_PROTOCOL_VERSION: u16 = 1;
pub const MAX_BACKEND_EVENTS: u32 = 4096;
pub const MAX_BACKEND_TRACE_BYTES: usize = 512 * 1024;
pub const CODEX_EXEC_ADAPTER: &str = "codex-exec.v1";
pub const OPENCODE_RUN_ADAPTER: &str = "opencode-run.v1";
pub const GENERIC_JSONL_ADAPTER: &str = "generic-jsonl.v1";

pub fn packaged_backend(adapter: &str) -> Option<&'static str> {
    match adapter {
        CODEX_EXEC_ADAPTER => Some("codex"),
        OPENCODE_RUN_ADAPTER => Some("opencode"),
        GENERIC_JSONL_ADAPTER => Some("jsonl"),
        _ => None,
    }
}

/// One line on an approved bridge's stdout. Additional fields are forward-compatible.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct BackendEvent {
    pub version: u16,
    #[serde(flatten)]
    pub event: BackendEventKind,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum BackendEventKind {
    Started {
        session_id: String,
    },
    Progress {
        phase: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        successful: Option<bool>,
    },
    Message {
        text: String,
    },
    InputRequired {
        question: String,
    },
    Artifact {
        attachment: ResultAttachment,
    },
    Usage {
        usage: BackendUsage,
    },
    Cost {
        cost: BackendCost,
    },
    Checkpoint {
        checkpoint_id: String,
    },
    Completed {
        summary: String,
    },
    Failed {
        reason: String,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BackendUsage {
    /// Total input, including tokens read from and written to the provider cache.
    pub input_tokens: u64,
    /// Total output, including reasoning tokens when the backend reports them.
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_write_input_tokens: u64,
    #[serde(default)]
    pub reasoning_output_tokens: u64,
}

impl BackendUsage {
    pub fn checked_add(self, next: Self) -> Option<Self> {
        Some(Self {
            input_tokens: self.input_tokens.checked_add(next.input_tokens)?,
            output_tokens: self.output_tokens.checked_add(next.output_tokens)?,
            cached_input_tokens: self
                .cached_input_tokens
                .checked_add(next.cached_input_tokens)?,
            cache_write_input_tokens: self
                .cache_write_input_tokens
                .checked_add(next.cache_write_input_tokens)?,
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .checked_add(next.reasoning_output_tokens)?,
        })
    }
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "quality", rename_all = "snake_case", deny_unknown_fields)]
pub enum BackendCost {
    Reported {
        usd_micros: u64,
        meter: String,
    },
    Estimated {
        usd_micros: u64,
        pricing_revision: String,
    },
}

impl BackendCost {
    pub fn valid(&self) -> bool {
        let (amount, origin) = match self {
            Self::Reported { usd_micros, meter } => (*usd_micros, meter),
            Self::Estimated {
                usd_micros,
                pricing_revision,
            } => (*usd_micros, pricing_revision),
        };
        amount <= i64::MAX as u64 && valid_backend_id(origin)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackendState {
    Completed,
    Failed,
    InputRequired,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BackendCapabilities {
    pub can_resume: bool,
    pub can_cancel: bool,
    pub supports_approvals: bool,
    pub supports_usage: bool,
    pub supports_tools: bool,
    pub supports_shared_gateway: bool,
    pub enforces_no_native_source_write: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BackendEvidence {
    pub version: u16,
    pub backend: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub checkpoint_id: Option<String>,
    pub terminal_state: BackendState,
    pub exit_code: Option<u8>,
    pub usage: Option<BackendUsage>,
    /// Absence means the provider cost is unknown, including subscription runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<BackendCost>,
    pub event_count: u32,
    pub events_sha256: String,
}

impl BackendEvidence {
    pub fn valid(&self) -> bool {
        self.version == BACKEND_PROTOCOL_VERSION
            && valid_capabilities(std::slice::from_ref(&self.backend), 1)
            && self.session_id.as_deref().is_none_or(valid_backend_id)
            && self.checkpoint_id.as_deref().is_none_or(valid_backend_id)
            && self.model.as_ref().is_none_or(|model| {
                (1..=192).contains(&model.len()) && !model.chars().any(char::is_control)
            })
            && (1..=MAX_BACKEND_EVENTS).contains(&self.event_count)
            && self.cost.as_ref().is_none_or(BackendCost::valid)
            && is_sha256(&self.events_sha256)
            && (self.terminal_state != BackendState::Completed
                || (self.session_id.is_some() && self.exit_code == Some(0)))
            && (self.terminal_state != BackendState::InputRequired
                || (self.session_id.is_some()
                    && self
                        .checkpoint_id
                        .as_deref()
                        .and_then(|id| id.strip_prefix("sha256:"))
                        .is_some_and(is_sha256)
                    && self.exit_code == Some(0)))
    }
}

pub fn valid_backend_id(value: &str) -> bool {
    (1..=160).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}
