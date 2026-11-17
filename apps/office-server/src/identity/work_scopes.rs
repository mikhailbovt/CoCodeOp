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
    AcknowledgeScope, CreateCommandTemplate, DefineArea, GetAccessRequest, GetArea,
    GetCheckoutGrant, GetWorkScope, IssueCheckoutGrant, RequestAccess, RespondAccess,
    RevokeCheckoutGrant, RevokeWorkScope, SetTaskScope,
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
        .route("/v1/command-templates", post(create_command_template))
        .route("/v1/checkout-grants", post(issue_checkout_grant))
        .route("/v1/checkout-grants/read", post(get_checkout_grant))
        .route("/v1/checkout-grants/revoke", post(revoke_checkout_grant))
        .route("/v1/work-scopes", post(set_task_scope))
        .route("/v1/work-scopes/read", post(get_work_scope))
        .route("/v1/work-scopes/revoke", post(revoke_work_scope))
        .route("/v1/checkout-areas", post(define_area))
        .route("/v1/checkout-areas/read", post(get_area))
        .route("/v1/access-requests", post(request_access))
        .route("/v1/access-requests/read", post(get_access_request))
        .route("/v1/access-requests/respond", post(respond_access))
}

pub(super) fn runner_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/runner-checkouts/{checkout_id}/next-scope",
            get(next_scope),
        )
        .route(
            "/v1/runner-checkouts/acknowledge-scope",
            post(acknowledge_scope),
        )
}

#[tool_router(router = scope_tools, vis = "pub(super)")]
impl OfficeMcp {
    #[tool(
        description = "Close a task scope to new operations and queue a revocation for its checkout host. Requires task authority or the human checkout owner.",
        annotations(
            idempotent_hint = true,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn office_revoke_scope(
        &self,
        Parameters(input): Parameters<RevokeWorkScope>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.revoke_work_scope(actor, input),
            )
            .await,
        )
    }
    #[tool(
        description = "Ask for permissions at the current task and scope revisions. Pass permissions as an object with read, write and write_operations. The server determines the exact approvers.",
        input_schema = super::model_schema::typed_input::<RequestAccess>(),
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_request_access(
        &self,
        Parameters(input): Parameters<RequestAccess>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.request_access(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read a visible access request, approvals and host activation status.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_access_request(
        &self,
        Parameters(input): Parameters<GetAccessRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.get_access_request(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Record an authorized approval or rejection, or cancel your request. Full approval queues a host deployment.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_respond_access(
        &self,
        Parameters(input): Parameters<RespondAccess>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.respond_access(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Define or revise ownership and approval policy for an exact file or directory. Requires the human checkout owner.",
        annotations(
            idempotent_hint = true,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn office_define_area(
        &self,
        Parameters(input): Parameters<DefineArea>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(timeout(Duration::from_secs(5), self.store.define_area(actor, input)).await)
    }

    #[tool(
        description = "Read a checkout area policy under its human owner's credential.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_area(
        &self,
        Parameters(input): Parameters<GetArea>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(timeout(Duration::from_secs(5), self.store.get_area(actor, input)).await)
    }

    #[tool(
        description = "Create a pending version of a coding task's permissions, bounded by your owner grant. Supply permissions.read and permissions.write with allow/deny paths. Set permissions.write_operations beside read and write. Supply command_template_ids in permissions when needed.",
        input_schema = super::model_schema::typed_input::<SetTaskScope>(),
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_set_task_scope(
        &self,
        Parameters(input): Parameters<SetTaskScope>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.set_task_scope(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read a visible task's latest work scope version or a specific revision, including whether its host has activated it.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_work_scope(
        &self,
        Parameters(input): Parameters<GetWorkScope>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.get_work_scope(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Register a typed command template for a checkout under its human owner's credential.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_create_command_template(
        &self,
        Parameters(input): Parameters<CreateCommandTemplate>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.create_command_template(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Issue a source and command envelope from the checkout owner to one named lead.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_issue_checkout_grant(
        &self,
        Parameters(input): Parameters<IssueCheckoutGrant>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.issue_checkout_grant(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read an owner-granted checkout envelope visible to your principal.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_checkout_grant(
        &self,
        Parameters(input): Parameters<GetCheckoutGrant>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.get_checkout_grant(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Revoke an envelope as the checkout owner.",
        annotations(
            idempotent_hint = true,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn office_revoke_checkout_grant(
        &self,
        Parameters(input): Parameters<RevokeCheckoutGrant>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.revoke_checkout_grant(actor, input),
            )
            .await,
        )
    }
}

async fn create_command_template(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<CreateCommandTemplate>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.create_command_template(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn issue_checkout_grant(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<IssueCheckoutGrant>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.issue_checkout_grant(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn get_checkout_grant(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetCheckoutGrant>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.get_checkout_grant(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn revoke_checkout_grant(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<RevokeCheckoutGrant>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.revoke_checkout_grant(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn set_task_scope(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<SetTaskScope>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.set_task_scope(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn get_work_scope(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetWorkScope>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.get_work_scope(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn request_access(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<RequestAccess>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.request_access(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn get_access_request(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetAccessRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.get_access_request(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn respond_access(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<RespondAccess>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.respond_access(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn define_area(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<DefineArea>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.define_area(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn revoke_work_scope(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<RevokeWorkScope>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.revoke_work_scope(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn get_area(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetArea>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(Duration::from_secs(5), state.store.get_area(&actor, input)).await;
    reply(&state, id, result)
}

async fn next_scope(
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
        state.store.next_scope_deployment(&digest, checkout),
    )
    .await;
    reply(&state, id, result)
}

async fn acknowledge_scope(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Json<AcknowledgeScope>, JsonRejection>,
) -> Response {
    let Some(digest) = digest_from_headers(&headers) else {
        return error_reply(&state, id, IdentityError::Unauthenticated);
    };
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.acknowledge_scope(&digest, input),
    )
    .await;
    reply(&state, id, result)
}
