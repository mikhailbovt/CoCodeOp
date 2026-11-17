use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

mod checkout;
mod identity;
mod policy;
mod task;
mod team;
mod work_scope;

pub use checkout::{InvalidSourcePath, PathRule, SourceManifest, SourcePath};
pub use identity::{
    AgentId, AgentRole, BearerDigest, ChannelId, CredentialId, InstanceId, InvitationId, MessageId,
    NoteId, RunnerId, TeamId, UserId, WorkspaceId, WorkspaceRole,
};
pub use policy::{
    Action, Actor, DataAccess, DataGrant, GrantRight, Resource, Subject, TaskAuthority,
    TeamAuthority, Visibility, WorkspaceAccess, authorize,
};
pub use task::{RunId, RunStatus, TaskId, TaskStatus, TaskTransition};
pub use team::{TeamRole, TeamState};
pub use work_scope::{FileAction, PathAccess, WorkPermissions, WriteOperation};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct InstallationId(pub Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct RequestId(pub Uuid);

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    DatabaseUnavailable,
    IncompatibleSchema,
    Unauthenticated,
    Forbidden,
    InvalidInput,
    IdempotencyConflict,
    RevisionConflict,
    CursorExpired,
    InvalidTransition,
    DependencyNotReady,
    DependencyCycle,
    DelegationCycle,
    DelegationLimit,
    DelegationInactive,
    CheckoutRootOverlap,
    AccessRequestRequired,
    PathClaimed,
    ClaimFenceStale,
    ClaimRenewalLimit,
    PathHandoffPending,
    BudgetExhausted,
    ExecutionPaused,
    ExecutionCapacityReached,
    MessageRateLimited,
    ArtifactQuotaExceeded,
    ArtifactUploadOpen,
    ArtifactIncomplete,
    ArtifactDigestMismatch,
    ArtifactUnavailable,
    RunLeaseStale,
    CoordinatorStale,
    CheckoutOffline,
    PathForbidden,
    ScopeStale,
    ContentConflict,
    ContextStale,
    CheckoutReconciliationRequired,
    PublicationPending,
    FileNotFound,
    NotFound,
}
