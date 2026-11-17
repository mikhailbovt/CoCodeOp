use std::time::Duration;

use axum::{
    Extension, Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use office_domain::{BearerDigest, CredentialId, ErrorCode, RequestId, WorkspaceId};
use office_protocol::{
    AcceptInvitation, AckInbox, ApiError, CreateAgent, CreateInvitation, CreateRunner,
    CreateWorkspace, GetContext, GetInboxDelivery, GetNote, IssueCredential, Outcome, ReadInbox,
    Reply, RevokeCredential, UpsertNote,
};
use office_store::IdentityError;
use tokio::time::timeout;

use crate::AppState;

pub(super) async fn upsert_note(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<UpsertNote>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.upsert_note(&actor, body),
    )
    .await;
    reply(&state, id, result)
}

pub(super) async fn read_note(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetNote>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(Duration::from_secs(3), state.store.get_note(&actor, body)).await;
    reply(&state, id, result)
}

pub(super) async fn issue_invitation(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    Path(workspace): Path<WorkspaceId>,
    body: Result<Json<CreateInvitation>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(3),
        state.store.invite_user(&actor, workspace, body),
    )
    .await;
    reply(&state, id, result)
}

pub(super) async fn read_inbox(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<ReadInbox>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state
            .store
            .read_inbox(&actor, body, state.event_retain_count),
    )
    .await;
    reply(&state, id, result)
}

pub(super) async fn actor_context(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetContext>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state
            .store
            .actor_context(&actor, body, state.event_retain_count),
    )
    .await;
    reply(&state, id, result)
}

pub(super) async fn ack_inbox(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<AckInbox>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(3),
        state
            .store
            .ack_inbox(&actor, body, state.event_retain_count),
    )
    .await;
    reply(&state, id, result)
}

pub(super) async fn inbox_delivery(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetInboxDelivery>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(3),
        state
            .store
            .inbox_delivery(&actor, body, state.event_retain_count),
    )
    .await;
    reply(&state, id, result)
}

pub(super) async fn accept_invitation(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Json<AcceptInvitation>, JsonRejection>,
) -> Response {
    let Some(credential) = super::digest_from_headers(&headers) else {
        return error_reply(&state, id, IdentityError::Unauthenticated);
    };
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let Some(invitation) = BearerDigest::of(&body.invitation_token) else {
        return error_reply(&state, id, IdentityError::Unauthenticated);
    };
    let result = timeout(
        Duration::from_secs(3),
        state
            .store
            .accept_invitation(&invitation, &credential, body.idempotency_key),
    )
    .await;
    reply(&state, id, result)
}

pub(super) async fn create_workspace(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<CreateWorkspace>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(3),
        state.store.create_workspace(&actor, body),
    )
    .await;
    reply(&state, id, result)
}

pub(super) async fn create_agent(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    Path(workspace): Path<WorkspaceId>,
    body: Result<Json<CreateAgent>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(3),
        state.store.create_agent(&actor, workspace, body),
    )
    .await;
    reply(&state, id, result)
}

pub(super) async fn issue_credential(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<IssueCredential>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(3),
        state.store.issue_credential(&actor, body),
    )
    .await;
    reply(&state, id, result)
}

pub(super) async fn revoke_credential(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    Path(credential): Path<CredentialId>,
    body: Result<Json<RevokeCredential>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(3),
        state.store.revoke_credential(&actor, credential, body),
    )
    .await;
    reply(&state, id, result)
}

pub(super) async fn register_runner(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    Path(workspace): Path<WorkspaceId>,
    body: Result<Json<CreateRunner>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(3),
        state.store.register_runner(&actor, workspace, body),
    )
    .await;
    reply(&state, id, result)
}

pub(super) async fn runner_self(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
) -> Response {
    let Some(digest) = super::digest_from_headers(&headers) else {
        return error_reply(&state, id, IdentityError::Unauthenticated);
    };
    let result = timeout(
        Duration::from_secs(2),
        state.store.authenticate_runner(&digest),
    )
    .await;
    reply(&state, id, result)
}

pub(super) fn reply<T: serde::Serialize>(
    state: &AppState,
    id: RequestId,
    result: Result<Result<T, IdentityError>, tokio::time::error::Elapsed>,
) -> Response {
    match result {
        Ok(Ok(result)) => Json(Reply {
            request_id: id,
            timestamp: state.clock.now(),
            outcome: Outcome::Success { result },
        })
        .into_response(),
        Ok(Err(error)) => error_reply(state, id, error),
        Err(_) => error_reply(state, id, IdentityError::Timeout),
    }
}

pub(super) fn error_reply(state: &AppState, id: RequestId, error: IdentityError) -> Response {
    let (status, code) = match error {
        IdentityError::Unauthenticated => (StatusCode::UNAUTHORIZED, ErrorCode::Unauthenticated),
        IdentityError::Forbidden => (StatusCode::FORBIDDEN, ErrorCode::Forbidden),
        IdentityError::InvalidName | IdentityError::InvalidInput => {
            (StatusCode::BAD_REQUEST, ErrorCode::InvalidInput)
        }
        IdentityError::IdempotencyConflict => {
            (StatusCode::CONFLICT, ErrorCode::IdempotencyConflict)
        }
        IdentityError::RevisionConflict | IdentityError::RevisionMismatch { .. } => {
            (StatusCode::CONFLICT, ErrorCode::RevisionConflict)
        }
        IdentityError::InvalidTransition => (StatusCode::CONFLICT, ErrorCode::InvalidTransition),
        IdentityError::DependencyNotReady => (StatusCode::CONFLICT, ErrorCode::DependencyNotReady),
        IdentityError::DependencyCycle => (StatusCode::CONFLICT, ErrorCode::DependencyCycle),
        IdentityError::DelegationCycle => (StatusCode::CONFLICT, ErrorCode::DelegationCycle),
        IdentityError::DelegationLimit => (StatusCode::CONFLICT, ErrorCode::DelegationLimit),
        IdentityError::DelegationInactive => (StatusCode::FORBIDDEN, ErrorCode::DelegationInactive),
        IdentityError::CheckoutRootOverlap => {
            (StatusCode::CONFLICT, ErrorCode::CheckoutRootOverlap)
        }
        IdentityError::AccessRequestRequired => {
            (StatusCode::FORBIDDEN, ErrorCode::AccessRequestRequired)
        }
        IdentityError::PathClaimed => (StatusCode::CONFLICT, ErrorCode::PathClaimed),
        IdentityError::ClaimFenceStale => (StatusCode::CONFLICT, ErrorCode::ClaimFenceStale),
        IdentityError::ClaimRenewalLimit => (StatusCode::CONFLICT, ErrorCode::ClaimRenewalLimit),
        IdentityError::PathHandoffPending => (StatusCode::CONFLICT, ErrorCode::PathHandoffPending),
        IdentityError::BudgetExhausted => (StatusCode::CONFLICT, ErrorCode::BudgetExhausted),
        IdentityError::ExecutionPaused => (StatusCode::CONFLICT, ErrorCode::ExecutionPaused),
        IdentityError::ExecutionCapacityReached => {
            (StatusCode::CONFLICT, ErrorCode::ExecutionCapacityReached)
        }
        IdentityError::MessageRateLimited => {
            (StatusCode::TOO_MANY_REQUESTS, ErrorCode::MessageRateLimited)
        }
        IdentityError::ArtifactQuotaExceeded => {
            (StatusCode::CONFLICT, ErrorCode::ArtifactQuotaExceeded)
        }
        IdentityError::ArtifactUploadOpen => (StatusCode::CONFLICT, ErrorCode::ArtifactUploadOpen),
        IdentityError::ArtifactIncomplete => (StatusCode::CONFLICT, ErrorCode::ArtifactIncomplete),
        IdentityError::ArtifactDigestMismatch => {
            (StatusCode::CONFLICT, ErrorCode::ArtifactDigestMismatch)
        }
        IdentityError::ArtifactUnavailable => (StatusCode::GONE, ErrorCode::ArtifactUnavailable),
        IdentityError::RunLeaseStale => (StatusCode::CONFLICT, ErrorCode::RunLeaseStale),
        IdentityError::CoordinatorStale => (StatusCode::CONFLICT, ErrorCode::CoordinatorStale),
        IdentityError::CheckoutOffline => {
            (StatusCode::SERVICE_UNAVAILABLE, ErrorCode::CheckoutOffline)
        }
        IdentityError::PathForbidden => (StatusCode::FORBIDDEN, ErrorCode::PathForbidden),
        IdentityError::ScopeStale => (StatusCode::CONFLICT, ErrorCode::ScopeStale),
        IdentityError::ContentConflict => (StatusCode::CONFLICT, ErrorCode::ContentConflict),
        IdentityError::ContextStale => (StatusCode::CONFLICT, ErrorCode::ContextStale),
        IdentityError::CheckoutReconciliationRequired => (
            StatusCode::CONFLICT,
            ErrorCode::CheckoutReconciliationRequired,
        ),
        IdentityError::PublicationPending => {
            (StatusCode::TOO_MANY_REQUESTS, ErrorCode::PublicationPending)
        }
        IdentityError::FileNotFound => (StatusCode::NOT_FOUND, ErrorCode::FileNotFound),
        IdentityError::CursorExpired => (StatusCode::GONE, ErrorCode::CursorExpired),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::DatabaseUnavailable,
        ),
    };
    let current_revision = match error {
        IdentityError::RevisionMismatch { current } => Some(current),
        _ => None,
    };
    super::operations_metrics::record(code);
    let unavailable = status == StatusCode::SERVICE_UNAVAILABLE;
    let retryable = unavailable
        || matches!(
            code,
            ErrorCode::ExecutionCapacityReached | ErrorCode::MessageRateLimited
        );
    let message = if unavailable {
        "Identity store is unavailable".into()
    } else {
        error.to_string()
    };
    (
        status,
        Json(Reply::<()> {
            request_id: id,
            timestamp: state.clock.now(),
            outcome: Outcome::Failure {
                error: ApiError {
                    code,
                    message,
                    retryable,
                    current_revision,
                },
            },
        }),
    )
        .into_response()
}
