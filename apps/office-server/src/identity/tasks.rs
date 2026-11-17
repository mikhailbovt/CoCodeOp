use std::time::Duration;

use axum::{
    Extension, Json, Router,
    extract::{State, rejection::JsonRejection},
    http::request::Parts,
    response::Response,
    routing::post,
};
use office_domain::{BearerDigest, RequestId};
use office_protocol::{
    ChangeTask, ClaimTask, CreateTask, ExplainTask, GetTask, GetTaskResult, ListHandoffs,
    ListTasks, RequestHandoff, RespondHandoff, ReviewTask, SubmitTaskResult, TaskChange,
    TaskCommandTarget, TaskMutation,
};
use office_store::IdentityError;
use rmcp::{
    ErrorData, RoleServer, handler::server::wrapper::Parameters, model::CallToolResult,
    service::RequestContext, tool, tool_router,
};
use serde::Serialize;
use tokio::time::{error::Elapsed, timeout};

use super::{
    OfficeMcp, current_digest,
    mutations::{error_reply, reply},
    tool_reply,
};
use crate::AppState;

const RESULT_SUBMIT: &str = "/v1/task-results/submit";
const RESULT_REVIEW: &str = "/v1/task-results/review";
const RESULT_GET: &str = "/v1/task-results/read";
const EXPLAIN: &str = "/v1/tasks/explain";
const RUN_LOGS: &str = "/v1/runs/logs";

pub(super) fn result_route(path: &str) -> bool {
    matches!(
        path,
        RESULT_SUBMIT | RESULT_REVIEW | RESULT_GET | EXPLAIN | RUN_LOGS
    )
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/tasks", post(create_task))
        .route("/v1/tasks/read", post(get_task))
        .route(EXPLAIN, post(explain_task))
        .route(RUN_LOGS, post(run_logs))
        .route("/v1/tasks/list", post(list_tasks))
        .route("/v1/tasks/change", post(change_task))
        .route(RESULT_SUBMIT, post(submit_result))
        .route(RESULT_REVIEW, post(review_result))
        .route(RESULT_GET, post(get_result))
        .route("/v1/task-handoffs", post(request_handoff))
        .route("/v1/task-handoffs/respond", post(respond_handoff))
        .route("/v1/task-handoffs/list", post(list_handoffs))
}

#[tool_router(router = task_tools, vis = "pub(super)")]
impl OfficeMcp {
    #[tool(
        description = "Explain the current scheduling or workflow condition of a task you can read, including who should act.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_explain_task(
        &self,
        Parameters(input): Parameters<ExplainTask>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.explain_task(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Offer task responsibility to a named owner or lead with existing read access. Uses the current task revision.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_request_handoff(
        &self,
        Parameters(input): Parameters<RequestHandoff>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.request_handoff(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Accept or decline a task handoff addressed to you or to a lead you own.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_respond_handoff(
        &self,
        Parameters(input): Parameters<RespondHandoff>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.respond_handoff(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "List task handoffs visible to the current principal.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_list_handoffs(
        &self,
        Parameters(input): Parameters<ListHandoffs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.list_handoffs(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Create a draft task with an objective, acceptance criteria and review policy.",
        input_schema = super::model_schema::typed_input::<CreateTask>(),
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_create_task(
        &self,
        Parameters(input): Parameters<CreateTask>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        let result = timeout(Duration::from_secs(5), self.store.create_task(actor, input)).await;
        mutation_reply(&context, result)
    }

    #[tool(
        description = "Read the task and a page of its actual execution attempts, most recent first.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_task(
        &self,
        Parameters(input): Parameters<GetTask>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(timeout(Duration::from_secs(5), self.store.get_task(actor, input)).await)
    }

    #[tool(
        description = "List visible tasks, optionally filtering by state.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_list_tasks(
        &self,
        Parameters(input): Parameters<ListTasks>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(timeout(Duration::from_secs(5), self.store.list_tasks(actor, input)).await)
    }

    #[tool(
        description = "Apply a typed task command at the expected revision. Agent claims use an attached agent instance ID.",
        annotations(
            idempotent_hint = true,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    async fn office_change_task(
        &self,
        Parameters(input): Parameters<ChangeTask>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        self.change(input, &context).await
    }

    #[tool(
        description = "Make a draft or returned task ready after all prerequisites are accepted.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_prepare_task(
        &self,
        Parameters(target): Parameters<TaskCommandTarget>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        self.change(target.with_change(TaskChange::Prepare), &context)
            .await
    }

    #[tool(
        description = "Claim a ready or assigned task. Agent callers provide their current instance ID.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_claim_task(
        &self,
        Parameters(input): Parameters<ClaimTask>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        self.change(input.into_change(), &context).await
    }

    #[tool(
        description = "Submit the current interactive attempt for review and record its result.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_submit_result(
        &self,
        Parameters(input): Parameters<SubmitTaskResult>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        self.change(input.into_change(), &context).await
    }

    #[tool(
        description = "Accept the current attempt or request changes. For an attached report, copy the exact binding returned by office_get_task_result.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_review_task(
        &self,
        Parameters(input): Parameters<ReviewTask>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        self.change(input.into_change(), &context).await
    }

    #[tool(
        description = "Read the immutable result, reported checks, artifact digests, and review for the latest or a specific task result revision.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_task_result(
        &self,
        Parameters(input): Parameters<GetTaskResult>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.get_task_result(actor, input),
            )
            .await,
        )
    }
}

impl OfficeMcp {
    async fn change(
        &self,
        input: ChangeTask,
        context: &RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(context)?;
        let result = timeout(Duration::from_secs(5), self.store.change_task(actor, input)).await;
        mutation_reply(context, result)
    }
}

#[derive(Serialize)]
struct TracedMutation {
    request_id: RequestId,
    #[serde(flatten)]
    mutation: TaskMutation,
}

fn mutation_reply(
    context: &RequestContext<RoleServer>,
    result: Result<Result<TaskMutation, IdentityError>, Elapsed>,
) -> Result<CallToolResult, ErrorData> {
    let id = context
        .extensions
        .get::<Parts>()
        .and_then(|parts| parts.extensions.get::<RequestId>())
        .copied()
        .ok_or_else(|| ErrorData::internal_error("Request trace is unavailable", None))?;
    tool_reply(result.map(|result| {
        result.map(|mutation| TracedMutation {
            request_id: id,
            mutation,
        })
    }))
}

async fn submit_result(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<SubmitTaskResult>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.change_task(&actor, input.into_change()),
    )
    .await;
    reply(&state, id, result)
}

async fn review_result(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<ReviewTask>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.change_task(&actor, input.into_change()),
    )
    .await;
    reply(&state, id, result)
}

async fn get_result(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetTaskResult>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.get_task_result(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn create_task(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<CreateTask>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.create_task(&actor, body),
    )
    .await;
    reply(&state, id, result)
}

async fn get_task(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetTask>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(Duration::from_secs(5), state.store.get_task(&actor, body)).await;
    reply(&state, id, result)
}

async fn explain_task(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<ExplainTask>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.explain_task(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn run_logs(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<office_protocol::ReadRunLogs>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.read_run_logs(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn list_tasks(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<ListTasks>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(Duration::from_secs(5), state.store.list_tasks(&actor, body)).await;
    reply(&state, id, result)
}

async fn change_task(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<ChangeTask>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.change_task(&actor, body),
    )
    .await;
    reply(&state, id, result)
}

async fn request_handoff(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<RequestHandoff>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.request_handoff(&actor, body),
    )
    .await;
    reply(&state, id, result)
}

async fn respond_handoff(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<RespondHandoff>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.respond_handoff(&actor, body),
    )
    .await;
    reply(&state, id, result)
}

async fn list_handoffs(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<ListHandoffs>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.list_handoffs(&actor, body),
    )
    .await;
    reply(&state, id, result)
}
