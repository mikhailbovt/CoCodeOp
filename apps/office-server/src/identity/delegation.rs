use std::time::Duration;

use axum::{
    Extension, Json, Router,
    extract::{State, rejection::JsonRejection},
    response::Response,
    routing::post,
};
use office_domain::{BearerDigest, RequestId};
use office_protocol::{
    CreateRuntimeTemplate, DelegateTask, GetDelegation, IssueDelegation, RevokeDelegation,
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

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/runtime-templates", post(create_runtime_template))
        .route("/v1/delegations", post(issue_delegation))
        .route("/v1/delegations/delegate", post(delegate_task))
        .route("/v1/delegations/read", post(get_delegation))
        .route("/v1/delegations/revoke", post(revoke_delegation))
}

#[tool_router(router = delegation_tools, vis = "pub(super)")]
impl OfficeMcp {
    #[tool(
        description = "Register an owner-approved interactive or managed runtime template.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_create_runtime_template(
        &self,
        Parameters(input): Parameters<CreateRuntimeTemplate>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.create_runtime_template(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "As the human owner, issue a delegation envelope to your team's lead for a task.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_issue_delegation(
        &self,
        Parameters(input): Parameters<IssueDelegation>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.issue_delegation(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Create a subtask and a strictly narrower grant for a named worker in your team.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_delegate_task(
        &self,
        Parameters(input): Parameters<DelegateTask>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.delegate_task(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read a visible grant, its remaining limits and funded usage.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_delegation(
        &self,
        Parameters(input): Parameters<GetDelegation>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.get_delegation(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Revoke a grant and descendant authority as its human owner or issuing agent.",
        annotations(
            idempotent_hint = true,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn office_revoke_delegation(
        &self,
        Parameters(input): Parameters<RevokeDelegation>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.revoke_delegation(actor, input),
            )
            .await,
        )
    }
}

async fn create_runtime_template(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<CreateRuntimeTemplate>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.create_runtime_template(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn issue_delegation(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<IssueDelegation>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.issue_delegation(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn delegate_task(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<DelegateTask>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.delegate_task(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn get_delegation(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetDelegation>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.get_delegation(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn revoke_delegation(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<RevokeDelegation>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.revoke_delegation(&actor, input),
    )
    .await;
    reply(&state, id, result)
}
