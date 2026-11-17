use std::time::Duration;

use axum::{
    Extension, Json, Router,
    extract::{State, rejection::JsonRejection},
    response::Response,
    routing::post,
};
use office_domain::{BearerDigest, RequestId};
use office_protocol::{CreateManagedWorker, GetManagedWorkerStatus, UpdateManagedWorker};
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

const CREATE: &str = "/v1/workers";
const UPDATE: &str = "/v1/workers/instructions";
const STATUS: &str = "/v1/workers/status";
const LIMIT: Duration = Duration::from_secs(5);

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(CREATE, post(create))
        .route(UPDATE, post(update))
        .route(STATUS, post(status))
}

pub(super) fn agent_route(path: &str) -> bool {
    matches!(path, CREATE | UPDATE | STATUS)
}

#[tool_router(router = workers_tools, vis = "pub(super)")]
impl OfficeMcp {
    #[tool(
        description = "As lead, create a named worker for your own owner and set its persistent work instructions. Use the returned worker identity for team membership and named execution.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_create_managed_worker(
        &self,
        Parameters(input): Parameters<CreateManagedWorker>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                LIMIT,
                self.store
                    .create_managed_worker(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Revise one of your workers' persistent instructions for future attempts. Running attempts retain their bound revision.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_update_managed_worker(
        &self,
        Parameters(input): Parameters<UpdateManagedWorker>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                LIMIT,
                self.store
                    .update_managed_worker(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Find one of your managed workers by exact name or agent ID. Read permitted assigned tasks, queue jobs, runs, and their recorded progress.",
        input_schema = super::model_schema::typed_input::<GetManagedWorkerStatus>(),
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn office_worker_status(
        &self,
        Parameters(input): Parameters<GetManagedWorkerStatus>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                LIMIT,
                self.store
                    .managed_worker_status(current_digest(&context)?, input),
            )
            .await,
        )
    }
}

macro_rules! endpoint {
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
                timeout(LIMIT, state.store.$method(&digest, input)).await,
            )
        }
    };
}

endpoint!(create, CreateManagedWorker, create_managed_worker);
endpoint!(update, UpdateManagedWorker, update_managed_worker);
endpoint!(status, GetManagedWorkerStatus, managed_worker_status);
