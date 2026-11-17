use std::collections::BTreeSet;

use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use office_domain::{
    FileAction, PathRule, RunId, RunnerId, SourceManifest, SourcePath, TaskId, WorkPermissions,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{CheckoutCoordinatorRef, FileEncoding, Recipient, WorkScopeDefinition};

pub const MAX_PUBLICATION_BYTES: usize = 64 * 1024;
pub const MAX_PUBLICATION_BODY: usize = 128 * 1024;
pub const MAX_PUBLICATION_OPERATIONS: usize = 8;
pub const MAX_READ_DEPENDENCIES: usize = 16;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyCheckoutChanges {
    pub scope_revision: i64,
    pub base_checkout_revision: i64,
    pub claim_fence: Option<i64>,
    pub idempotency_key: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compensates_change_id: Option<Uuid>,
    pub operations: Vec<CheckoutChange>,
    #[serde(default)]
    pub read_dependencies: Vec<CheckoutDependency>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplaceCheckoutTextFile {
    pub scope_revision: i64,
    pub base_checkout_revision: i64,
    pub claim_fence: Option<i64>,
    pub idempotency_key: Uuid,
    pub path: SourcePath,
    pub before_sha256: String,
    pub content: String,
    pub dependency_path: Option<SourcePath>,
    pub dependency_sha256: Option<String>,
}

impl TryFrom<ReplaceCheckoutTextFile> for ApplyCheckoutChanges {
    type Error = ();

    fn try_from(input: ReplaceCheckoutTextFile) -> Result<Self, Self::Error> {
        let read_dependencies = match (input.dependency_path, input.dependency_sha256) {
            (Some(path), Some(sha256)) => vec![CheckoutDependency { path, sha256 }],
            (None, None) => Vec::new(),
            _ => return Err(()),
        };
        Ok(Self {
            scope_revision: input.scope_revision,
            base_checkout_revision: input.base_checkout_revision,
            claim_fence: input.claim_fence,
            idempotency_key: input.idempotency_key,
            compensates_change_id: None,
            operations: vec![CheckoutChange::Replace {
                path: input.path,
                before_sha256: input.before_sha256,
                file: CheckoutContent {
                    encoding: FileEncoding::Utf8,
                    content: input.content,
                },
                executable: None,
            }],
            read_dependencies,
        })
    }
}

impl ApplyCheckoutChanges {
    pub fn is_valid(&self) -> bool {
        if self.scope_revision < 1
            || self.base_checkout_revision < 0
            || self.claim_fence.is_some_and(|fence| fence < 1)
            || !(1..=MAX_PUBLICATION_OPERATIONS).contains(&self.operations.len())
            || self.read_dependencies.len() > MAX_READ_DEPENDENCIES
        {
            return false;
        }
        if !valid_checkout_changes(&self.operations, MAX_PUBLICATION_BYTES) {
            return false;
        }
        let mut dependencies = BTreeSet::new();
        self.read_dependencies
            .iter()
            .all(|dep| is_sha256(&dep.sha256) && dependencies.insert(&dep.path))
    }

    pub fn claims(&self) -> Vec<PathRule> {
        self.operations
            .iter()
            .flat_map(CheckoutChange::paths)
            .flatten()
            .map(|path| PathRule::File { path: path.clone() })
            .collect()
    }

    pub fn is_permitted(&self, manifest: &SourceManifest, scope: &WorkScopeDefinition) -> bool {
        self.operations
            .iter()
            .all(|op| op.is_permitted(manifest, &scope.permissions))
            && self.read_dependencies.iter().all(|dep| {
                scope
                    .permissions
                    .permits(manifest, FileAction::Read(&dep.path))
            })
    }

    pub fn includes_scope_dependencies(&self, scope: &WorkScopeDefinition) -> bool {
        scope.required_read_paths.iter().all(|path| {
            self.read_dependencies
                .iter()
                .any(|dependency| &dependency.path == path)
        })
    }
}

pub fn valid_checkout_changes(operations: &[CheckoutChange], max_bytes: usize) -> bool {
    let mut total = 0;
    let mut touched: BTreeSet<&SourcePath> = BTreeSet::new();
    for operation in operations {
        if operation
            .before_sha256()
            .is_some_and(|hash| !is_sha256(hash))
        {
            return false;
        }
        if let Some(content) = operation.content() {
            let Some(bytes) = content.bytes() else {
                return false;
            };
            total += bytes.len();
            if total > max_bytes {
                return false;
            }
        }
        for path in operation.paths().into_iter().flatten() {
            if touched
                .iter()
                .any(|seen| seen.contains(path) || path.contains(seen))
            {
                return false;
            }
            touched.insert(path);
        }
    }
    true
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckoutChange {
    Create {
        path: SourcePath,
        file: CheckoutContent,
        #[serde(default)]
        executable: bool,
    },
    Replace {
        path: SourcePath,
        before_sha256: String,
        file: CheckoutContent,
        executable: Option<bool>,
    },
    Delete {
        path: SourcePath,
        before_sha256: String,
    },
    Rename {
        source: SourcePath,
        target: SourcePath,
        before_sha256: String,
    },
}

impl CheckoutChange {
    pub fn paths(&self) -> [Option<&SourcePath>; 2] {
        match self {
            Self::Create { path, .. } | Self::Replace { path, .. } | Self::Delete { path, .. } => {
                [Some(path), None]
            }
            Self::Rename { source, target, .. } => [Some(source), Some(target)],
        }
    }

    pub fn before_sha256(&self) -> Option<&str> {
        match self {
            Self::Create { .. } => None,
            Self::Replace { before_sha256, .. }
            | Self::Delete { before_sha256, .. }
            | Self::Rename { before_sha256, .. } => Some(before_sha256),
        }
    }

    pub fn content(&self) -> Option<&CheckoutContent> {
        match self {
            Self::Create { file, .. } | Self::Replace { file, .. } => Some(file),
            Self::Delete { .. } | Self::Rename { .. } => None,
        }
    }

    fn is_permitted(&self, manifest: &SourceManifest, permissions: &WorkPermissions) -> bool {
        let permits = |action| permissions.permits(manifest, action);
        match self {
            Self::Create {
                path, executable, ..
            } => {
                permits(FileAction::Create(path))
                    && (!executable || permits(FileAction::ChangeExecutable(path)))
            }
            Self::Replace {
                path, executable, ..
            } => {
                permits(FileAction::Replace(path))
                    && (executable.is_none() || permits(FileAction::ChangeExecutable(path)))
            }
            Self::Delete { path, .. } => permits(FileAction::Delete(path)),
            Self::Rename { source, target, .. } => permits(FileAction::Rename { source, target }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutContent {
    pub encoding: FileEncoding,
    pub content: String,
}

impl CheckoutContent {
    pub fn bytes(&self) -> Option<Vec<u8>> {
        let bytes = match self.encoding {
            FileEncoding::Utf8 if self.content.len() <= MAX_PUBLICATION_BYTES => {
                self.content.as_bytes().to_vec()
            }
            FileEncoding::Base64 if self.content.len() <= MAX_PUBLICATION_BYTES.div_ceil(3) * 4 => {
                STANDARD.decode(&self.content).ok()?
            }
            _ => return None,
        };
        (bytes.len() <= MAX_PUBLICATION_BYTES).then_some(bytes)
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutDependency {
    pub path: SourcePath,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutFileDelta {
    pub path: SourcePath,
    pub before_sha256: Option<String>,
    pub after_sha256: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct CheckoutChangeReceipt {
    pub change_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compensates_change_id: Option<Uuid>,
    pub checkout_id: Uuid,
    pub task_id: TaskId,
    pub run_id: RunId,
    pub author: Recipient,
    pub host_runner_id: RunnerId,
    pub coordinator_epoch: i64,
    pub scope_revision: i64,
    pub run_fence: i64,
    pub claim_fence: i64,
    pub checkout_revision: i64,
    pub files: Vec<CheckoutFileDelta>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_dependencies: Vec<CheckoutDependency>,
    pub event_cursor: Uuid,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutContentHead {
    pub checkout_revision: i64,
    pub source_sha256: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutWriteOrder {
    pub change_id: Uuid,
    pub epoch: i64,
    pub scope: WorkScopeDefinition,
    pub previous: CheckoutContentHead,
    pub permit_until: DateTime<Utc>,
    pub input: ApplyCheckoutChanges,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CheckoutWriteOffer {
    pub order: CheckoutWriteOrder,
    pub checked_at: DateTime<Utc>,
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutPublicationSession {
    pub coordinator: CheckoutCoordinatorRef,
    pub state_id: Option<Uuid>,
    #[serde(default)]
    pub reconcile: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct CheckoutPublicationRecovery {
    pub head: Option<CheckoutContentHead>,
    pub outstanding: Option<CheckoutWriteOrder>,
    #[serde(default)]
    pub blocked: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReportCheckoutWrite {
    pub session: CheckoutPublicationSession,
    pub change_id: Option<Uuid>,
    pub outcome: CheckoutWriteOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckoutWriteOutcome {
    Applied {
        head: CheckoutContentHead,
        files: Vec<CheckoutFileDelta>,
    },
    Rejected {
        reason: CheckoutPublicationError,
    },
    ReconcileRequired,
    Restored {
        head: CheckoutContentHead,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutPublicationError {
    Forbidden,
    ContentConflict,
    ContextStale,
    TooLarge,
    InterruptedBeforeStart,
}

pub fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}
