use std::time::Duration;

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::HeaderMap,
    response::Response,
    routing::{get, post},
};
use office_domain::{BearerDigest, RequestId};
use office_protocol::{
    CheckoutEnrollment, CreateProject, GetCheckout, GetProject, InspectCheckoutHost,
    RequestCheckout,
};
use office_store::IdentityError;
use rmcp::{
    ErrorData, RoleServer, handler::server::wrapper::Parameters, model::CallToolResult,
    service::RequestContext, tool, tool_router,
};
use tokio::time::timeout;
use uuid::Uuid;

use super::{
    OfficeMcp, current_digest, digest_from_headers,
    mutations::{error_reply, reply},
    tool_reply,
};
use crate::AppState;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/projects", post(create_project))
        .route("/v1/projects/read", post(get_project))
        .route("/v1/checkouts", post(request_checkout))
        .route("/v1/checkouts/read", post(get_checkout))
}

pub(super) fn runner_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/runner-checkouts/{checkout_id}",
            get(get_runner_checkout),
        )
        .route("/v1/runner-checkouts/enroll", post(enroll_checkout))
        .route(
            "/v1/runner-checkouts/diagnostics",
            post(inspect_checkout_host),
        )
}

#[tool_router(router = checkout_tools, vis = "pub(super)")]
impl OfficeMcp {
    #[tool(
        description = "Create project metadata and set who can read it. Requires its human owner's credential.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_create_project(
        &self,
        Parameters(input): Parameters<CreateProject>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.create_project(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read the named project if it is shared with your principal.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_project(
        &self,
        Parameters(input): Parameters<GetProject>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(timeout(Duration::from_secs(5), self.store.get_project(actor, input)).await)
    }

    #[tool(
        description = "Request one mediated checkout with an owner-approved source manifest on your own runner.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_request_checkout(
        &self,
        Parameters(input): Parameters<RequestCheckout>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.request_checkout(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read the checkout's registration state. The source manifest is restricted to its owner.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_checkout(
        &self,
        Parameters(input): Parameters<GetCheckout>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.get_checkout(actor, input),
            )
            .await,
        )
    }
}

async fn create_project(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<CreateProject>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.create_project(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn get_project(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetProject>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.get_project(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn request_checkout(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<RequestCheckout>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.request_checkout(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn get_checkout(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetCheckout>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.get_checkout(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn get_runner_checkout(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Path(checkout): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let Some(digest) = digest_from_headers(&headers) else {
        return error_reply(&state, id, IdentityError::Unauthenticated);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.get_runner_checkout(&digest, checkout),
    )
    .await;
    reply(&state, id, result)
}

async fn enroll_checkout(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Json<CheckoutEnrollment>, JsonRejection>,
) -> Response {
    let Some(digest) = digest_from_headers(&headers) else {
        return error_reply(&state, id, IdentityError::Unauthenticated);
    };
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.enroll_checkout(&digest, input),
    )
    .await;
    reply(&state, id, result)
}

async fn inspect_checkout_host(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    Json(input): Json<InspectCheckoutHost>,
) -> Response {
    let Some(digest) = digest_from_headers(&headers) else {
        return error_reply(&state, id, IdentityError::Unauthenticated);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.inspect_checkout_host(&digest, input),
    )
    .await;
    reply(&state, id, result)
}
