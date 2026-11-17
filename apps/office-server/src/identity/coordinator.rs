use std::time::Duration;

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Query, State, rejection::JsonRejection},
    http::HeaderMap,
    response::Response,
    routing::post,
};
use office_domain::RequestId;
use office_protocol::{
    CheckoutCoordinatorRef, CheckoutPublicationSession, ReportCheckoutCommand, ReportCheckoutWrite,
    ReportGatewayRead, SnapshotCheckoutCommand, StartCheckoutCoordinator,
};
use serde::Deserialize;
use tokio::time::timeout;

use super::{
    mutations::{error_reply, reply},
    scheduler::runner_request,
};
use crate::AppState;

pub(super) fn runner_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/runner-checkouts/coordinator/start", post(start))
        .route("/v1/runner-checkouts/coordinator/renew", post(renew))
        .route("/v1/runner-checkouts/coordinator/stop", post(stop))
        .route(
            "/v1/runner-checkouts/coordinator/next-file-read",
            post(next_file_read),
        )
        .route(
            "/v1/runner-checkouts/coordinator/file-read-receipt",
            post(file_read_receipt).layer(DefaultBodyLimit::max(160 * 1024)),
        )
        .route(
            "/v1/runner-checkouts/coordinator/recover-publications",
            post(recover_publications),
        )
        .route(
            "/v1/runner-checkouts/coordinator/next-publication",
            post(next_publication),
        )
        .route(
            "/v1/runner-checkouts/coordinator/publication-receipt",
            post(publication_receipt),
        )
        .route(
            "/v1/runner-checkouts/coordinator/next-command",
            post(next_command),
        )
        .route(
            "/v1/runner-checkouts/coordinator/command-snapshot",
            post(command_snapshot),
        )
        .route(
            "/v1/runner-checkouts/coordinator/command-receipt",
            post(command_receipt).layer(DefaultBodyLimit::max(160 * 1024)),
        )
}

async fn next_command(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Json<CheckoutCoordinatorRef>, JsonRejection>,
) -> Response {
    match runner_request(&headers, body) {
        Ok((actor, input)) => reply(
            &state,
            id,
            timeout(
                Duration::from_secs(5),
                state.store.next_checkout_command(&actor, input),
            )
            .await,
        ),
        Err(error) => error_reply(&state, id, error),
    }
}

async fn command_snapshot(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Json<SnapshotCheckoutCommand>, JsonRejection>,
) -> Response {
    match runner_request(&headers, body) {
        Ok((actor, input)) => reply(
            &state,
            id,
            timeout(
                Duration::from_secs(5),
                state.store.record_checkout_command_snapshot(&actor, input),
            )
            .await,
        ),
        Err(error) => error_reply(&state, id, error),
    }
}

async fn command_receipt(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Json<ReportCheckoutCommand>, JsonRejection>,
) -> Response {
    match runner_request(&headers, body) {
        Ok((actor, input)) => reply(
            &state,
            id,
            timeout(
                Duration::from_secs(5),
                state.store.report_checkout_command(&actor, input),
            )
            .await,
        ),
        Err(error) => error_reply(&state, id, error),
    }
}

async fn recover_publications(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Json<CheckoutPublicationSession>, JsonRejection>,
) -> Response {
    match runner_request(&headers, body) {
        Ok((actor, input)) => reply(
            &state,
            id,
            timeout(
                Duration::from_secs(5),
                state.store.recover_checkout_publications(&actor, input),
            )
            .await,
        ),
        Err(error) => error_reply(&state, id, error),
    }
}

#[derive(Deserialize)]
struct PublicationPoll {
    #[serde(default)]
    wait: bool,
}

async fn next_publication(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Query(poll): Query<PublicationPoll>,
    headers: HeaderMap,
    body: Result<Json<CheckoutPublicationSession>, JsonRejection>,
) -> Response {
    match runner_request(&headers, body) {
        Ok((actor, input)) => reply(
            &state,
            id,
            timeout(
                Duration::from_secs(5),
                state
                    .store
                    .next_checkout_publication(&actor, input, poll.wait),
            )
            .await,
        ),
        Err(error) => error_reply(&state, id, error),
    }
}

async fn publication_receipt(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Json<ReportCheckoutWrite>, JsonRejection>,
) -> Response {
    match runner_request(&headers, body) {
        Ok((actor, input)) => reply(
            &state,
            id,
            timeout(
                Duration::from_secs(5),
                state.store.report_checkout_publication(&actor, input),
            )
            .await,
        ),
        Err(error) => error_reply(&state, id, error),
    }
}

async fn next_file_read(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Json<CheckoutCoordinatorRef>, JsonRejection>,
) -> Response {
    match runner_request(&headers, body) {
        Ok((actor, input)) => reply(
            &state,
            id,
            timeout(
                Duration::from_secs(5),
                state.store.next_checkout_read(&actor, input),
            )
            .await,
        ),
        Err(error) => error_reply(&state, id, error),
    }
}

async fn file_read_receipt(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Json<ReportGatewayRead>, JsonRejection>,
) -> Response {
    match runner_request(&headers, body) {
        Ok((actor, input)) => reply(
            &state,
            id,
            timeout(
                Duration::from_secs(5),
                state.store.report_checkout_read(&actor, input),
            )
            .await,
        ),
        Err(error) => error_reply(&state, id, error),
    }
}

async fn start(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Json<StartCheckoutCoordinator>, JsonRejection>,
) -> Response {
    match runner_request(&headers, body) {
        Ok((actor, input)) => reply(
            &state,
            id,
            timeout(
                Duration::from_secs(5),
                state.store.start_checkout_coordinator(&actor, input),
            )
            .await,
        ),
        Err(error) => error_reply(&state, id, error),
    }
}

async fn renew(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Json<CheckoutCoordinatorRef>, JsonRejection>,
) -> Response {
    match runner_request(&headers, body) {
        Ok((actor, input)) => reply(
            &state,
            id,
            timeout(
                Duration::from_secs(5),
                state.store.renew_checkout_coordinator(&actor, input),
            )
            .await,
        ),
        Err(error) => error_reply(&state, id, error),
    }
}

async fn stop(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Json<CheckoutCoordinatorRef>, JsonRejection>,
) -> Response {
    match runner_request(&headers, body) {
        Ok((actor, input)) => reply(
            &state,
            id,
            timeout(
                Duration::from_secs(5),
                state.store.stop_checkout_coordinator(&actor, input),
            )
            .await,
        ),
        Err(error) => error_reply(&state, id, error),
    }
}
