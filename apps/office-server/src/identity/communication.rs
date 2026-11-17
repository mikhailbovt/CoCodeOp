use std::time::Duration;

use axum::{
    Extension, Json, Router,
    extract::{State, rejection::JsonRejection},
    response::Response,
    routing::post,
};
use office_domain::{BearerDigest, RequestId};
use office_protocol::{
    AnswerQuestion, AskQuestion, CreateChannel, GetQuestion, ListChannels, ReadThread, Search,
    SendMessage,
};
use office_store::IdentityError;
use rmcp::{
    ErrorData, RoleServer, handler::server::wrapper::Parameters, model::CallToolResult,
    service::RequestContext, tool, tool_router,
};
use tokio::time::timeout;

use crate::AppState;

use super::{
    OfficeMcp, current_digest,
    mutations::{error_reply, reply},
    tool_reply,
};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/channels", post(create_channel))
        .route("/v1/channels/list", post(list_channels))
        .route("/v1/messages", post(send_message))
        .route("/v1/messages/thread", post(read_thread))
        .route("/v1/messages/questions", post(ask_question))
        .route("/v1/messages/questions/read", post(get_question))
        .route("/v1/messages/questions/answer", post(answer_question))
        .route("/v1/search", post(search))
}

pub(super) fn question_route(path: &str) -> bool {
    matches!(
        path,
        "/v1/messages/questions" | "/v1/messages/questions/read" | "/v1/messages/questions/answer"
    )
}

#[tool_router(router = communication_tools, vis = "pub(super)")]
impl OfficeMcp {
    #[tool(
        description = "Create a workspace channel, a team channel or a direct conversation with a specific principal.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_create_channel(
        &self,
        Parameters(input): Parameters<CreateChannel>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.create_channel(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "List channels and direct conversations visible to the current identity.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_list_channels(
        &self,
        Parameters(input): Parameters<ListChannels>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.list_channels(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Post or reply in a channel. Reuse the same idempotency key only to retry the same message.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_send_message(
        &self,
        Parameters(input): Parameters<SendMessage>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.send_message(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Ask one exact recipient an actionable question in a channel they can read. Returns the question and addressed event IDs.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_ask_question(
        &self,
        Parameters(input): Parameters<AskQuestion>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.ask_question(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read a question you asked or received, including the linked answer and its event if available.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_question(
        &self,
        Parameters(input): Parameters<GetQuestion>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                Duration::from_secs(3),
                self.store.get_question(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Answer one open question addressed to your exact identity. The answer is linked to its question and delivered to the asker.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_answer_question(
        &self,
        Parameters(input): Parameters<AnswerQuestion>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.answer_question(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read a thread or list the root messages of a channel. Use next_after for the next page.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_read_thread(
        &self,
        Parameters(input): Parameters<ReadThread>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(timeout(Duration::from_secs(5), self.store.read_thread(actor, input)).await)
    }

    #[tool(
        description = "Search visible workspace messages and current note revisions by text.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_search(
        &self,
        Parameters(input): Parameters<Search>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(timeout(Duration::from_secs(5), self.store.search(actor, input)).await)
    }
}

async fn create_channel(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<CreateChannel>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.create_channel(&actor, body),
    )
    .await;
    reply(&state, id, result)
}

async fn list_channels(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<ListChannels>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.list_channels(&actor, body),
    )
    .await;
    reply(&state, id, result)
}

async fn send_message(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<SendMessage>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.send_message(&actor, body),
    )
    .await;
    reply(&state, id, result)
}

async fn ask_question(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<AskQuestion>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    reply(
        &state,
        id,
        timeout(
            Duration::from_secs(5),
            state.store.ask_question(&actor, body),
        )
        .await,
    )
}

async fn get_question(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetQuestion>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    reply(
        &state,
        id,
        timeout(
            Duration::from_secs(3),
            state.store.get_question(&actor, body),
        )
        .await,
    )
}

async fn answer_question(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<AnswerQuestion>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    reply(
        &state,
        id,
        timeout(
            Duration::from_secs(5),
            state.store.answer_question(&actor, body),
        )
        .await,
    )
}

async fn read_thread(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<ReadThread>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.read_thread(&actor, body),
    )
    .await;
    reply(&state, id, result)
}

async fn search(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<Search>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(Duration::from_secs(5), state.store.search(&actor, body)).await;
    reply(&state, id, result)
}
