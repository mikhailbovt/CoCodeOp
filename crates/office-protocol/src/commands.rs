use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use office_domain::{RunId, RunnerId, SourcePath, TaskId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    CheckoutChange, CheckoutContentHead, CheckoutCoordinatorRef, CommandArgument,
    CommandTemplateSpec, GatewayFile, WorkScopeDefinition,
};

pub const MAX_COMMAND_LOG_BYTES: usize = 16 * 1024;
pub const MAX_COMMAND_OUTPUT_BYTES: u64 = 24 * 1024;
pub const MAX_COMMAND_OUTPUT_FILES: usize = 8;

impl CommandTemplateSpec {
    pub fn fingerprint(&self) -> Result<String, serde_json::Error> {
        let mut hash = Sha256::new();
        hash.update(b"cocodeop/checkout-command-template/v1\0");
        hash.update(serde_json::to_vec(self)?);
        Ok(hex::encode(hash.finalize()))
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunCheckoutCommand {
    pub template_id: Uuid,
    pub scope_revision: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_snapshot_id: Option<Uuid>,
    #[serde(default)]
    pub arguments: BTreeMap<String, CommandValue>,
    pub idempotency_key: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum CommandValue {
    Text(String),
    Integer(i32),
}

impl RunCheckoutCommand {
    pub fn arguments_for(&self, spec: &CommandTemplateSpec) -> Option<Vec<String>> {
        let mut consumed = 0;
        let mut argv = Vec::with_capacity(spec.arguments.len());
        for argument in &spec.arguments {
            match argument {
                CommandArgument::Literal { value } => argv.push(value.clone()),
                CommandArgument::Choice { key, values } => {
                    let CommandValue::Text(value) = self.arguments.get(key)? else {
                        return None;
                    };
                    if !values.contains(value) {
                        return None;
                    }
                    argv.push(value.clone());
                    consumed += 1;
                }
                CommandArgument::Integer {
                    key,
                    minimum,
                    maximum,
                } => {
                    let CommandValue::Integer(value) = self.arguments.get(key)? else {
                        return None;
                    };
                    if !(minimum..=maximum).contains(&value) {
                        return None;
                    }
                    argv.push(value.to_string());
                    consumed += 1;
                }
            }
        }
        (consumed == self.arguments.len()).then_some(argv)
    }
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetCheckoutCommand {
    pub command_id: Uuid,
    pub scope_revision: i64,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CheckoutCommand {
    pub command_id: Uuid,
    pub checkout_id: Uuid,
    pub task_id: TaskId,
    pub run_id: RunId,
    pub template_id: Uuid,
    pub host_runner_id: RunnerId,
    pub coordinator_epoch: i64,
    pub scope_revision: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_snapshot_id: Option<Uuid>,
    pub state: CheckoutCommandState,
    pub outcome: Option<CheckoutCommandOutcome>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutCommandState {
    Queued,
    Authorized,
    Running,
    Finished,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutCommandOrder {
    pub command_id: Uuid,
    pub epoch: i64,
    pub template_id: Uuid,
    pub spec: CommandTemplateSpec,
    pub argv: Vec<String>,
    pub scope: WorkScopeDefinition,
    pub head: CheckoutContentHead,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_snapshot_id: Option<Uuid>,
    pub permit_until: DateTime<Utc>,
    pub checked_at: DateTime<Utc>,
    #[serde(default)]
    pub revoked: bool,
    pub previous_snapshot: Option<CommandSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckoutCommandOutcome {
    Exited {
        snapshot: CommandSnapshot,
        exit_code: i32,
        stdout: String,
        stderr: String,
        log_truncated: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        outputs: Vec<CommandOutputFile>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        staged_changes: Vec<CheckoutChange>,
        #[serde(default)]
        files_truncated: bool,
    },
    Timeout {
        snapshot: CommandSnapshot,
    },
    Interrupted,
    NotConfigured,
    SnapshotUnavailable,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandOutputFile {
    pub path: SourcePath,
    pub file: GatewayFile,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SnapshotCheckoutCommand {
    pub coordinator: CheckoutCoordinatorRef,
    pub command_id: Uuid,
    pub snapshot: CommandSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandSnapshot {
    pub checkout_revision: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_snapshot_id: Option<Uuid>,
    pub sha256: String,
    pub file_count: u32,
    pub total_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<CommandEnvironment>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandEnvironment {
    pub image_id: String,
    pub template_sha256: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReportCheckoutCommand {
    pub coordinator: CheckoutCoordinatorRef,
    pub command_id: Uuid,
    pub outcome: CheckoutCommandOutcome,
}
