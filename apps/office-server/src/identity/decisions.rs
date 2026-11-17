use std::time::Duration;

use axum::{
    Extension, Json, Router,
    extract::{State, rejection::JsonRejection},
    response::Response,
    routing::post,
};
use office_domain::{BearerDigest, RequestId};
use office_protocol::{
    GetAreaDecision, GetDecision, GetProposal, ProposeDecision, RespondProposal, ReviseProposal,
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

const OPEN: &str = "/v1/proposals";
const REVISE: &str = "/v1/proposals/revise";
const RESPOND: &str = "/v1/proposals/respond";
const PROPOSAL: &str = "/v1/proposals/read";
const DECISION: &str = "/v1/decisions/read";
const AREA: &str = "/v1/decisions/area";

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(OPEN, post(propose_decision))
        .route(REVISE, post(revise_proposal))
        .route(RESPOND, post(respond_proposal))
        .route(PROPOSAL, post(get_proposal))
        .route(DECISION, post(get_decision))
        .route(AREA, post(get_area_decision))
}

pub(super) fn agent_route(path: &str) -> bool {
    matches!(path, OPEN | REVISE | RESPOND | PROPOSAL | DECISION | AREA)
}

#[tool_router(router = decision_tools, vis = "pub(super)")]
impl OfficeMcp {
    #[tool(
        description = "Create a versioned proposal for exact approvers. Shared area policy determines the approval roster.",
        input_schema = super::model_schema::typed_input::<ProposeDecision>(),
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_propose_decision(
        &self,
        Parameters(input): Parameters<ProposeDecision>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.propose_decision(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Publish a new proposal revision before its deadline. Approvals attach to the revision voters saw.",
        input_schema = super::model_schema::typed_input::<ReviseProposal>(),
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_revise_proposal(
        &self,
        Parameters(input): Parameters<ReviseProposal>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.revise_proposal(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Respond as an exact listed approver. Unanimous approval on the current revision records a decision.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_respond_proposal(
        &self,
        Parameters(input): Parameters<RespondProposal>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.respond_proposal(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read a visible proposal and responses for its latest or specified revision.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_proposal(
        &self,
        Parameters(input): Parameters<GetProposal>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.get_proposal(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read an immutable accepted decision visible to your credential.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_decision(
        &self,
        Parameters(input): Parameters<GetDecision>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.get_decision(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read the current shared area decision and generation as an appointed lead or checkout owner.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_area_decision(
        &self,
        Parameters(input): Parameters<GetAreaDecision>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.get_area_decision(actor, input),
            )
            .await,
        )
    }
}

async fn propose_decision(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<ProposeDecision>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.propose_decision(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn revise_proposal(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<ReviseProposal>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.revise_proposal(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn respond_proposal(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<RespondProposal>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.respond_proposal(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn get_proposal(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetProposal>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.get_proposal(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn get_decision(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetDecision>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.get_decision(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn get_area_decision(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetAreaDecision>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.get_area_decision(&actor, input),
    )
    .await;
    reply(&state, id, result)
}
