use chrono::{DateTime, Utc};
use office_domain::{ErrorCode, InstallationId, RequestId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

mod access;
mod activation;
mod areas;
mod artifacts;
mod backend;
mod checkout_releases;
mod checkout_reviews;
mod checkouts;
mod commands;
mod context;
mod decisions;
mod delegation;
mod doctor;
mod execution_control;
mod gateway;
mod handoffs;
mod identity;
mod inbox;
mod managed_input;
mod managed_workers;
mod messages;
mod notes;
mod operations_metrics;
mod path_claims;
mod path_handoffs;
mod publications;
mod run_logs;
mod scheduler;
mod search;
mod snapshots;
mod task_explanation;
mod task_results;
mod tasks;
mod teams;
mod work_scopes;
mod workspace_budget;

pub use access::{
    AccessApproval, AccessDecision, AccessRequest, AccessRequestReceipt, AccessRequestState,
    AreaRevision, GetAccessRequest, RequestAccess, RespondAccess,
};
pub use activation::{
    ActivationPolicy, ActivationReview, ActivationSource, ActivationTrigger,
    AnswerActivationQuestion, GetActivationPolicy, ManagedActivation, ReviewActivationResult,
    RunActivation, SetActivationPolicy,
};
pub use areas::{Area, AreaPolicy, AreaReceipt, DefineArea, GetArea};
pub use artifacts::{
    ARTIFACT_CHUNK_BYTES, Artifact, ArtifactChunkReceipt, ArtifactCleanup, ArtifactMime,
    ArtifactProvenance, ArtifactUpload, BeginArtifactUpload, CleanupArtifacts,
    FinishArtifactUpload, GetArtifact, MAX_ARTIFACT_BYTES, artifact_chunk_count,
    artifact_chunk_length,
};
pub use backend::{
    BACKEND_PROTOCOL_VERSION, BackendCapabilities, BackendCost, BackendEvent, BackendEventKind,
    BackendEvidence, BackendState, BackendUsage, CODEX_EXEC_ADAPTER, GENERIC_JSONL_ADAPTER,
    MAX_BACKEND_EVENTS, MAX_BACKEND_TRACE_BYTES, OPENCODE_RUN_ADAPTER, packaged_backend,
    valid_backend_id,
};
pub use checkout_releases::{
    CheckoutRelease, CheckoutReleaseOrder, CheckoutReleaseOutcome, CheckoutReleasePolicy,
    CheckoutReleaseState, GetCheckoutRelease, ReportCheckoutRelease, RequestCheckoutRelease,
    SetCheckoutReleasePolicy,
};
pub use checkout_reviews::{
    ApproveCheckoutReview, CheckoutContribution, CheckoutReview, CheckoutReviewContent,
    CheckoutReviewReceipt, GetCheckoutReview, VerifiedCheckoutCheck,
};
pub use checkouts::{
    CheckoutEnrollment, CheckoutHostHealth, CheckoutMode, CheckoutReceipt, CheckoutState,
    CreateProject, GetCheckout, GetProject, InspectCheckoutHost, MAX_ROOT_ANCESTORS,
    MAX_SOURCE_BYTES, MAX_SOURCE_FILES, Project, ProjectCheckout, ProjectReceipt, RequestCheckout,
    RootProof, ScopeDeploymentProof, SourceBaseline, SourceEntry, WriteStrategy,
};
pub use commands::{
    CheckoutCommand, CheckoutCommandOrder, CheckoutCommandOutcome, CheckoutCommandState,
    CommandEnvironment, CommandOutputFile, CommandSnapshot, CommandValue, GetCheckoutCommand,
    MAX_COMMAND_LOG_BYTES, MAX_COMMAND_OUTPUT_BYTES, MAX_COMMAND_OUTPUT_FILES,
    ReportCheckoutCommand, RunCheckoutCommand, SnapshotCheckoutCommand,
};
pub use context::{
    ActorContext, ContextDiagnostic, ContextTask, ContextTasks, GetContext, GetRunWorkContext,
    RunWorkContext, WorkflowGuide,
};
pub use decisions::{
    AreaDecision, Decision, GetAreaDecision, GetDecision, GetProposal, Proposal, ProposalContent,
    ProposalDecision, ProposalOption, ProposalReceipt, ProposalResponse, ProposalState,
    ProposalTarget, ProposeDecision, RespondProposal, ReviseProposal,
};
pub use delegation::{
    CreateRuntimeTemplate, DelegateTask, DelegationGrant, DelegationLimits, DelegationReceipt,
    GetDelegation, IssueDelegation, RevokeDelegation, RuntimeKind, RuntimeTemplate,
};
pub use doctor::{DoctorCheck, DoctorReport, DoctorStatus};
pub use execution_control::{
    ExecutionControl, ExecutionControlScope, ExecutionLimits, ExecutionMode, GetExecutionControl,
    SetExecutionControl,
};
pub use gateway::{
    CheckoutCoordinatorLease, CheckoutCoordinatorRef, FileEncoding, GatewayFile, GatewayOrigin,
    GatewayQuery, GatewayReadOrder, GatewayReadOutcome, GatewayReadReceipt, GatewaySearchMatch,
    GatewaySearchReceipt, GatewayStatusEntry, GatewayStatusReceipt, GetCheckoutStatus,
    MAX_GATEWAY_QUERY_RESULTS, MAX_GATEWAY_READ_BYTES, MAX_GATEWAY_SNIPPET_BYTES,
    MAX_GATEWAY_STATUS_HASH_BYTES, ReadCheckoutFile, ReportGatewayRead, SearchCheckoutCode,
    StartCheckoutCoordinator,
};
pub use handoffs::{
    HandoffDecision, HandoffMutation, HandoffPage, HandoffState, ListHandoffs, RequestHandoff,
    RespondHandoff, TaskHandoff,
};
pub use identity::{
    AcceptInvitation, AgentIdentity, AgentInstance, AttachAgentInstance, CreateAgent,
    CreateInvitation, CreateWorkspace, CredentialSubject, GetAgentInstance, Identity, Invitation,
    IssueCredential, IssuedCredential, McpIdentity, RevokeCredential, RevokedCredential,
    WorkspaceMembership,
};
pub use identity::{CreateRunner, RunnerIdentity};
pub use inbox::{
    AckInbox, GetInboxDelivery, InboxDelivery, InboxDeliveryState, InboxEvent, InboxPage,
    InboxReceipt, ReadInbox,
};
pub use managed_input::{
    AnswerManagedInput, GetManagedInput, ManagedInput, ManagedInputReceipt, ManagedInputState,
    RequestRunInput, RunContinuation, RunInputIntent,
};
pub use managed_workers::{
    CreateManagedWorker, GetManagedWorkerStatus, ManagedWorker, ManagedWorkerSelector,
    ManagedWorkerStatus, ManagedWorkerTask, UpdateManagedWorker, WorkerInstructions,
};
pub use messages::{
    AnswerQuestion, AskQuestion, Channel, ChannelAudience, ChannelPage, CreateChannel, GetQuestion,
    ListChannels, Message, NoteLink, ReadThread, Recipient, SendMessage, ThreadPage,
    ThreadQuestion,
};
pub use notes::{AclGrant, GetNote, NoteChange, NoteRevision, UpsertNote};
pub use operations_metrics::{
    CheckoutMetrics, ExecutionMetrics, InboxMetrics, InstanceRejections, LatencyMetrics,
    OperationsMetrics, ReadOperationsMetrics, UsageMetrics,
};
pub use path_claims::{
    ClaimPaths, ListPathClaims, PathClaim, PathClaimPage, PathClaimReceipt, PathClaimState,
    ReleasePathClaim, RenewPathClaim,
};
pub use path_handoffs::{
    GetPathHandoff, PathHandoff, PathHandoffDecision, PathHandoffReceipt, PathHandoffState,
    RequestPathHandoff, RespondPathHandoff,
};
pub use publications::{
    ApplyCheckoutChanges, CheckoutChange, CheckoutChangeReceipt, CheckoutContent,
    CheckoutContentHead, CheckoutDependency, CheckoutFileDelta, CheckoutPublicationError,
    CheckoutPublicationRecovery, CheckoutPublicationSession, CheckoutWriteOffer,
    CheckoutWriteOrder, CheckoutWriteOutcome, MAX_PUBLICATION_BODY, MAX_PUBLICATION_BYTES,
    MAX_PUBLICATION_OPERATIONS, MAX_READ_DEPENDENCIES, ReplaceCheckoutTextFile,
    ReportCheckoutWrite, is_sha256, valid_checkout_changes,
};
pub use run_logs::{ReadRunLogs, RunLog, RunLogEvent};
pub use scheduler::{
    AdvertiseRunner, AuthorizeExecutionStart, CancelExecution, ClaimExecution, CompleteExecution,
    ExecutionBudget, ExecutionBudgetReceipt, ExecutionCompletionReceipt, ExecutionJob,
    ExecutionLease, ExecutionReceipt, ExecutionStartEffect, ExecutionStartGrant,
    ExecutionStartReceipt, ExecutionState, ExecutionTaskBrief, ExecutionTerminalReason,
    GetExecution, GetExecutionBudget, GetRunnerProfile, IssueRunCredential, ManagedRuntime,
    ObserveExecutionStart, ProcessResult, QueueExecution, ReconcileExecutionStart,
    RenewExecutionLease, RunContextView, RunCredentialReceipt, RunLease, RunnerProfile,
    SetExecutionBudget, StartEffectRef, StartEffectState, StopObservation, valid_capabilities,
};
pub use search::{Search, SearchHit, SearchResult, SearchSource};
pub use snapshots::{
    CheckoutSnapshot, CheckoutSnapshotOrder, CheckoutSnapshotOutcome, CheckoutSnapshotProof,
    CheckoutSnapshotState, CreateCheckoutSnapshot, GetCheckoutSnapshot, ReportCheckoutSnapshot,
};
pub use task_explanation::{ExplainTask, TaskCondition, TaskExplanation};
pub use task_results::{
    AgentReportedCheck, ArtifactFormat, GetTaskResult, ReportedCheckOutcome, ResultArtifact,
    ResultAttachment, ResultBinding, ResultReport, ResultReview, TaskResult, TaskResultContent,
};
pub use tasks::{
    ChangeTask, CreateTask, GetTask, ListTasks, ReviewDecision, ReviewPolicy, Run, Task,
    TaskBlockReason, TaskChange, TaskDependency, TaskExecution, TaskMutation, TaskPage, TaskView,
};
pub use tasks::{ClaimTask, ReviewTask, SubmitTaskResult, TaskCommandTarget};
pub use teams::{
    CreateTeam, GetTeam, ListTeams, SetTeamMember, Team, TeamChange, TeamMember, TeamMutation,
    TeamPage, TeamView, UpdateTeam,
};
pub use teams::{
    DecideTeamInvite, InviteTeamMember, ListTeamInvites, TeamInvitation, TeamInviteDecision,
    TeamInviteMutation, TeamInvitePage, TeamInviteState,
};
pub use work_scopes::{
    AcknowledgeScope, CheckoutGrant, CheckoutGrantReceipt, CommandArgument, CommandResources,
    CommandTemplateReceipt, CommandTemplateSpec, CreateCommandTemplate, GetCheckoutGrant,
    GetWorkScope, IssueCheckoutGrant, NextScopeDeployment, RevokeCheckoutGrant, RevokeWorkScope,
    RootIdentity, ScopeDeployment, ScopeRevocationReceipt, SetTaskScope, WorkScope,
    WorkScopeDefinition, WorkScopeOperation, WorkScopeReceipt, WorkScopeState,
};
pub use workspace_budget::{SetWorkspaceBudget, WorkspaceBudget};

pub const API_VERSION: i16 = 1;
pub const REQUEST_ID_HEADER: &str = "x-request-id";

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct Reply<T> {
    pub request_id: RequestId,
    pub timestamp: DateTime<Utc>,
    #[serde(flatten)]
    pub outcome: Outcome<T>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum Outcome<T> {
    Success { result: T },
    Failure { error: ApiError },
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_revision: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct Health {
    pub version: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct Readiness {
    pub installation_id: InstallationId,
    pub api_version: i16,
    pub schema_version: i64,
}
