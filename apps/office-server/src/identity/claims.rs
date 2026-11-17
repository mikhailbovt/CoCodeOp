use std::time::Duration;

use axum::{
    Extension, Json, Router,
    extract::{State, rejection::JsonRejection},
    response::Response,
    routing::post,
};
use office_domain::{BearerDigest, RequestId};
use office_protocol::{
    ClaimPaths, GetPathHandoff, ListPathClaims, ReleasePathClaim, RenewPathClaim,
    RequestPathHandoff, RespondPathHandoff,
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
    tool_reply,
};
use crate::AppState;

const ACQUIRE: &str = "/v1/path-claims/acquire";
const RENEW: &str = "/v1/path-claims/renew";
const RELEASE: &str = "/v1/path-claims/release";
const LIST: &str = "/v1/path-claims/list";
const REQUEST_HANDOFF: &str = "/v1/path-handoffs/request";
const RESPOND_HANDOFF: &str = "/v1/path-handoffs/respond";
const GET_HANDOFF: &str = "/v1/path-handoffs/read";

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(ACQUIRE, post(acquire))
        .route(RENEW, post(renew))
        .route(RELEASE, post(release))
        .route(LIST, post(list))
        .route(REQUEST_HANDOFF, post(request_handoff))
        .route(RESPOND_HANDOFF, post(respond_handoff))
        .route(GET_HANDOFF, post(get_handoff))
}

pub(super) fn agent_route(path: &str) -> bool {
    matches!(
        path,
        ACQUIRE | RENEW | RELEASE | LIST | REQUEST_HANDOFF | RESPOND_HANDOFF | GET_HANDOFF
    )
}

#[tool_router(router = claim_tools, vis = "pub(super)")]
impl OfficeMcp {
    #[tool(
        description = "Claim all exact files or directory prefixes atomically for your active run and WorkScope. To replace an existing set, provide its fence. PATH_CLAIMED leaves that set intact.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_claim_paths(
        &self,
        Parameters(input): Parameters<ClaimPaths>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(timeout(Duration::from_secs(5), self.store.claim_paths(actor, input)).await)
    }

    #[tool(
        description = "Extend one active path claim within its renewal and lifetime limits. Supply the current fence.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_renew_claim(
        &self,
        Parameters(input): Parameters<RenewPathClaim>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.renew_path_claim(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Release your entire path claim set by its current fence. This leaves the task attempt itself active.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_release_claim(
        &self,
        Parameters(input): Parameters<ReleasePathClaim>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.release_path_claim(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read path claim history visible through your WorkScope permissions.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_list_claims(
        &self,
        Parameters(input): Parameters<ListPathClaims>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.list_path_claims(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Ask the holder to hand off paths covered by your WorkScope and by that holder's claim. Release any claim you hold before requesting.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_request_path_handoff(
        &self,
        Parameters(input): Parameters<RequestPathHandoff>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.request_path_handoff(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "As the exact holder, reject a path handoff or release your full claim and reserve the requested paths for its requester. The requester can cancel.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_respond_path_handoff(
        &self,
        Parameters(input): Parameters<RespondPathHandoff>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.respond_path_handoff(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read an addressed path handoff. Accept an offer using its exact paths and handoff_id in office_claim_paths.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_path_handoff(
        &self,
        Parameters(input): Parameters<GetPathHandoff>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.get_path_handoff(actor, input),
            )
            .await,
        )
    }
}

async fn request_handoff(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<RequestPathHandoff>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.request_path_handoff(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn respond_handoff(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<RespondPathHandoff>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.respond_path_handoff(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn get_handoff(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetPathHandoff>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.get_path_handoff(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn acquire(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<ClaimPaths>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.claim_paths(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn renew(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<RenewPathClaim>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.renew_path_claim(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn release(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<ReleasePathClaim>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.release_path_claim(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn list(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<ListPathClaims>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.list_path_claims(&actor, input),
    )
    .await;
    reply(&state, id, result)
}
