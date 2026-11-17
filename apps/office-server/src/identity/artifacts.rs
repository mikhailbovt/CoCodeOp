use std::time::Duration;

use axum::{
    Extension, Json, Router,
    body::to_bytes,
    extract::{Path, Request, State, rejection::JsonRejection},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use office_domain::{BearerDigest, RequestId, WorkspaceId};
use office_protocol::{
    ARTIFACT_CHUNK_BYTES, BeginArtifactUpload, CleanupArtifacts, FinishArtifactUpload, GetArtifact,
};
use office_store::IdentityError;
use rmcp::{
    ErrorData, RoleServer, handler::server::wrapper::Parameters, model::CallToolResult,
    service::RequestContext, tool, tool_router,
};
use tokio::time::timeout;
use uuid::Uuid;

use super::{
    OfficeMcp, current_digest,
    mutations::{error_reply, reply},
    tool_reply,
};
use crate::AppState;

const BEGIN: &str = "/v1/artifacts/begin";
const FINISH: &str = "/v1/artifacts/finalize";
const READ: &str = "/v1/artifacts/read";
const CLEANUP: &str = "/v1/artifacts/cleanup";
const TRANSFER: Duration = Duration::from_secs(8);

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(BEGIN, post(begin))
        .route(FINISH, post(finish))
        .route(READ, post(metadata))
        .route(CLEANUP, post(cleanup))
        .route(
            "/v1/artifacts/upload/{workspace}/{upload}/{index}",
            put(upload),
        )
        .route(
            "/v1/artifacts/content/{workspace}/{artifact}/{revision}/{index}",
            get(download),
        )
}

pub(super) fn agent_route(path: &str) -> bool {
    matches!(path, BEGIN | FINISH | READ)
        || path.starts_with("/v1/artifacts/upload/")
        || path.starts_with("/v1/artifacts/content/")
}

#[tool_router(router = artifact_tools, vis = "pub(super)")]
impl OfficeMcp {
    #[tool(
        description = "Reserve an immutable artifact revision and declare its SHA-256. Upload the bytes with the CLI or binary HTTP API.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_begin_artifact(
        &self,
        Parameters(input): Parameters<BeginArtifactUpload>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                TRANSFER,
                self.store.begin_artifact(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Publish a complete artifact after the server verifies its exact length and SHA-256.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_finalize_artifact(
        &self,
        Parameters(input): Parameters<FinishArtifactUpload>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                TRANSFER,
                self.store.finish_artifact(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read permitted artifact metadata and its exact revision. Use the CLI or binary HTTP API to get the bytes.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn office_get_artifact(
        &self,
        Parameters(input): Parameters<GetArtifact>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                TRANSFER,
                self.store.get_artifact(current_digest(&context)?, input),
            )
            .await,
        )
    }
}

macro_rules! artifact_json {
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
                timeout(TRANSFER, state.store.$method(&digest, input)).await,
            )
        }
    };
}

artifact_json!(begin, BeginArtifactUpload, begin_artifact);
artifact_json!(finish, FinishArtifactUpload, finish_artifact);
artifact_json!(metadata, GetArtifact, get_artifact);
artifact_json!(cleanup, CleanupArtifacts, cleanup_artifacts);

async fn upload(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(digest): Extension<BearerDigest>,
    Path((workspace, upload, index)): Path<(Uuid, Uuid, i32)>,
    request: Request,
) -> Response {
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|header| header.to_str().ok())
        != Some("application/octet-stream")
    {
        return error_reply(&state, id, IdentityError::InvalidInput);
    }
    let result = timeout(TRANSFER, async {
        let body = to_bytes(request.into_body(), ARTIFACT_CHUNK_BYTES)
            .await
            .map_err(|_| IdentityError::InvalidInput)?;
        state
            .store
            .put_artifact_chunk(&digest, WorkspaceId(workspace), upload, index, &body)
            .await
    })
    .await;
    reply(&state, id, result)
}

async fn download(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(digest): Extension<BearerDigest>,
    Path((workspace, artifact, revision, index)): Path<(Uuid, Uuid, i64, i32)>,
) -> Response {
    let input = GetArtifact {
        workspace_id: WorkspaceId(workspace),
        artifact_id: artifact,
        revision: Some(revision),
    };
    match timeout(
        TRANSFER,
        state.store.read_artifact_chunk(&digest, input, index),
    )
    .await
    {
        Ok(Ok(content)) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/octet-stream"),
                (header::CONTENT_DISPOSITION, "attachment"),
                (header::CACHE_CONTROL, "private, no-store"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            content,
        )
            .into_response(),
        Ok(Err(error)) => error_reply(&state, id, error),
        Err(_) => error_reply(&state, id, IdentityError::Timeout),
    }
}
