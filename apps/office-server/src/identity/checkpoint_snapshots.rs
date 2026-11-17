use std::time::Duration;

use axum::{
    Extension, Json, Router,
    extract::{State, rejection::JsonRejection},
    http::HeaderMap,
    response::Response,
    routing::post,
};
use office_domain::{BearerDigest, RequestId};
use office_protocol::{
    ApproveCheckoutReview, CheckoutCoordinatorRef, CreateCheckoutSnapshot, GetCheckout,
    GetCheckoutRelease, GetCheckoutReview, GetCheckoutSnapshot, ReportCheckoutRelease,
    ReportCheckoutSnapshot, RequestCheckoutRelease, SetCheckoutReleasePolicy,
};
use office_store::IdentityError;
use rmcp::{
    ErrorData, RoleServer, handler::server::wrapper::Parameters, model::CallToolResult,
    service::RequestContext, tool, tool_router,
};
use tokio::time::timeout;

use super::{
    OfficeMcp, current_digest,
    mutations::{error_reply, reply},
    scheduler::runner_request,
    tool_reply,
};
use crate::AppState;

const HEAD: &str = "/v1/checkouts/head";
const CREATE: &str = "/v1/checkouts/snapshots";
const GET: &str = "/v1/checkouts/snapshots/read";
const REVIEW: &str = "/v1/checkouts/snapshots/review";
const APPROVE: &str = "/v1/checkouts/snapshots/approve-review";
const RELEASE: &str = "/v1/checkouts/releases";
const GET_RELEASE: &str = "/v1/checkouts/releases/read";
const POLICY: &str = "/v1/checkouts/releases/policy";
const GET_POLICY: &str = "/v1/checkouts/releases/read-policy";
const TTL: Duration = Duration::from_secs(5);

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(HEAD, post(head))
        .route(CREATE, post(create))
        .route(GET, post(read))
        .route(REVIEW, post(review))
        .route(APPROVE, post(approve))
        .route(RELEASE, post(release))
        .route(GET_RELEASE, post(get_release))
        .route(POLICY, post(policy))
        .route(GET_POLICY, post(get_policy))
}

pub(super) fn runner_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/runner-checkouts/coordinator/next-snapshot", post(next))
        .route(
            "/v1/runner-checkouts/coordinator/snapshot-receipt",
            post(report),
        )
        .route(
            "/v1/runner-checkouts/coordinator/next-release",
            post(next_release),
        )
        .route(
            "/v1/runner-checkouts/coordinator/release-receipt",
            post(report_release),
        )
}

pub(super) fn agent_route(path: &str) -> bool {
    matches!(
        path,
        HEAD | CREATE | GET | REVIEW | APPROVE | RELEASE | GET_RELEASE | POLICY | GET_POLICY
    )
}

#[tool_router(router = snapshot_tools, vis = "pub(super)")]
impl OfficeMcp {
    #[tool(
        description = "Inspect the checkout's contract test policy and last accepted Git checkpoint.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn office_get_checkout_release_policy(
        &self,
        Parameters(input): Parameters<GetCheckout>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                TTL,
                self.store
                    .get_checkout_release_policy(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Configure the immutable full-source command templates required to accept a checkpoint.",
        annotations(
            idempotent_hint = true,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn office_set_checkout_release_policy(
        &self,
        Parameters(input): Parameters<SetCheckoutReleasePolicy>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                TTL,
                self.store
                    .set_checkout_release_policy(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Request an accepted Git checkpoint for the current sealed source. Every included change needs its bound review and contract checks, and the accepted ref must match the expected commit.",
        annotations(
            idempotent_hint = true,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn office_request_checkout_release(
        &self,
        Parameters(input): Parameters<RequestCheckoutRelease>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                TTL,
                self.store
                    .request_checkout_release(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read the coordinator's recorded outcome of an accepted checkpoint request.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn office_get_checkout_release(
        &self,
        Parameters(input): Parameters<GetCheckoutRelease>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                TTL,
                self.store
                    .get_checkout_release(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read exact result and change set bindings for a sealed checkout, with independently recorded checks on that source tree.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn office_get_checkout_review(
        &self,
        Parameters(input): Parameters<GetCheckoutReview>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                TTL,
                self.store
                    .get_checkout_review(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "As the task's designated reviewer, approve exactly the reviewed checkout snapshot, recorded checks, and task result.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_approve_checkout_review(
        &self,
        Parameters(input): Parameters<ApproveCheckoutReview>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                TTL,
                self.store
                    .approve_checkout_review(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read the current authoritative checkout content revision and source hash as its owner or a lead for that owner.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn office_get_checkout_head(
        &self,
        Parameters(input): Parameters<GetCheckout>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                TTL,
                self.store
                    .get_checkout_head(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Request an immutable snapshot and WIP Git checkpoint from the registered checkout host at the expected content revision.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_create_checkout_snapshot(
        &self,
        Parameters(input): Parameters<CreateCheckoutSnapshot>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                TTL,
                self.store
                    .create_checkout_snapshot(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read snapshot status and immutable source and Git tree hashes as the checkout owner. The response contains metadata.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn office_get_checkout_snapshot(
        &self,
        Parameters(input): Parameters<GetCheckoutSnapshot>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                TTL,
                self.store
                    .get_checkout_snapshot(current_digest(&context)?, input),
            )
            .await,
        )
    }
}

macro_rules! metadata {
    ($handler:ident, $input:ty, $method:ident) => {
        async fn $handler(
            State(state): State<AppState>,
            Extension(id): Extension<RequestId>,
            Extension(digest): Extension<BearerDigest>,
            input: Result<Json<$input>, JsonRejection>,
        ) -> Response {
            let Ok(Json(input)) = input else {
                return error_reply(&state, id, IdentityError::InvalidInput);
            };
            reply(
                &state,
                id,
                timeout(TTL, state.store.$method(&digest, input)).await,
            )
        }
    };
}

metadata!(head, GetCheckout, get_checkout_head);
metadata!(create, CreateCheckoutSnapshot, create_checkout_snapshot);
metadata!(read, GetCheckoutSnapshot, get_checkout_snapshot);
metadata!(review, GetCheckoutReview, get_checkout_review);
metadata!(approve, ApproveCheckoutReview, approve_checkout_review);
metadata!(release, RequestCheckoutRelease, request_checkout_release);
metadata!(get_release, GetCheckoutRelease, get_checkout_release);
metadata!(
    policy,
    SetCheckoutReleasePolicy,
    set_checkout_release_policy
);
metadata!(get_policy, GetCheckout, get_checkout_release_policy);

async fn next_release(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    input: Result<Json<CheckoutCoordinatorRef>, JsonRejection>,
) -> Response {
    match runner_request(&headers, input) {
        Ok((credential, input)) => reply(
            &state,
            id,
            timeout(TTL, state.store.next_checkout_release(&credential, input)).await,
        ),
        Err(error) => error_reply(&state, id, error),
    }
}

async fn report_release(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    input: Result<Json<ReportCheckoutRelease>, JsonRejection>,
) -> Response {
    match runner_request(&headers, input) {
        Ok((credential, input)) => reply(
            &state,
            id,
            timeout(TTL, state.store.report_checkout_release(&credential, input)).await,
        ),
        Err(error) => error_reply(&state, id, error),
    }
}

async fn next(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    input: Result<Json<CheckoutCoordinatorRef>, JsonRejection>,
) -> Response {
    match runner_request(&headers, input) {
        Ok((credential, input)) => reply(
            &state,
            id,
            timeout(TTL, state.store.next_checkout_snapshot(&credential, input)).await,
        ),
        Err(error) => error_reply(&state, id, error),
    }
}

async fn report(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    input: Result<Json<ReportCheckoutSnapshot>, JsonRejection>,
) -> Response {
    match runner_request(&headers, input) {
        Ok((credential, input)) => reply(
            &state,
            id,
            timeout(
                TTL,
                state.store.report_checkout_snapshot(&credential, input),
            )
            .await,
        ),
        Err(error) => error_reply(&state, id, error),
    }
}
