use std::time::Duration;

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, State, rejection::JsonRejection},
    http::HeaderMap,
    response::Response,
    routing::{get, post},
};
use office_domain::{BearerDigest, RequestId};
use office_protocol::{
    AdvertiseRunner, AnswerManagedInput, AuthorizeExecutionStart, CancelExecution, ClaimExecution,
    CompleteExecution, GetActivationPolicy, GetExecution, GetExecutionBudget, GetExecutionControl,
    GetManagedInput, GetRunnerProfile, IssueRunCredential, ObserveExecutionStart, QueueExecution,
    ReconcileExecutionStart, RenewExecutionLease, SetActivationPolicy, SetExecutionBudget,
    SetExecutionControl, SetWorkspaceBudget,
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

const SET_BUDGET: &str = "/v1/execution-budget/set";
const GET_BUDGET: &str = "/v1/execution-budget/read";
const SET_CONTROL: &str = "/v1/execution-control/set";
const GET_CONTROL: &str = "/v1/execution-control/read";
const GET_PROFILE: &str = "/v1/runner-profiles/read";
const QUEUE: &str = "/v1/executions/queue";
const GET_EXECUTION: &str = "/v1/executions/read";
const CANCEL_EXECUTION: &str = "/v1/executions/cancel";
const GET_ACTIVATION: &str = "/v1/activation/read";
const SET_ACTIVATION: &str = "/v1/activation/set";
const GET_MANAGED_INPUT: &str = "/v1/executions/input/read";
const ANSWER_MANAGED_INPUT: &str = "/v1/executions/input/answer";

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(SET_BUDGET, post(set_execution_budget))
        .route(GET_BUDGET, post(get_execution_budget))
        .route("/v1/workspace-budget/set", post(set_workspace_budget))
        .route("/v1/workspace-budget/read", post(get_workspace_budget))
        .route(SET_CONTROL, post(set_execution_control))
        .route(GET_CONTROL, post(get_execution_control))
        .route(GET_PROFILE, post(get_runner_profile))
        .route(QUEUE, post(queue_execution))
        .route(GET_EXECUTION, post(get_execution))
        .route(CANCEL_EXECUTION, post(cancel_execution))
        .route(GET_ACTIVATION, post(get_activation_policy))
        .route(SET_ACTIVATION, post(set_activation_policy))
        .route(GET_MANAGED_INPUT, post(get_managed_input))
        .route(ANSWER_MANAGED_INPUT, post(answer_managed_input))
}

async fn set_workspace_budget(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(digest): Extension<BearerDigest>,
    Json(input): Json<SetWorkspaceBudget>,
) -> Response {
    let result = timeout(
        Duration::from_secs(5),
        state.store.set_workspace_budget(&digest, input),
    )
    .await;
    reply(&state, id, result)
}

async fn get_workspace_budget(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(digest): Extension<BearerDigest>,
    Json(input): Json<GetExecutionBudget>,
) -> Response {
    let result = timeout(
        Duration::from_secs(5),
        state.store.get_workspace_budget(&digest, input),
    )
    .await;
    reply(&state, id, result)
}

pub(super) fn agent_route(path: &str) -> bool {
    matches!(
        path,
        QUEUE
            | GET_EXECUTION
            | CANCEL_EXECUTION
            | GET_PROFILE
            | GET_ACTIVATION
            | GET_MANAGED_INPUT
            | ANSWER_MANAGED_INPUT
    )
}

async fn get_managed_input(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetManagedInput>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    reply(
        &state,
        id,
        timeout(
            Duration::from_secs(5),
            state.store.get_managed_input(&actor, body),
        )
        .await,
    )
}

async fn answer_managed_input(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<AnswerManagedInput>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    reply(
        &state,
        id,
        timeout(
            Duration::from_secs(5),
            state.store.answer_managed_input(&actor, body),
        )
        .await,
    )
}

async fn get_activation_policy(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetActivationPolicy>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    reply(
        &state,
        id,
        timeout(
            Duration::from_secs(5),
            state.store.get_activation_policy(&actor, body),
        )
        .await,
    )
}

async fn set_activation_policy(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<SetActivationPolicy>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    reply(
        &state,
        id,
        timeout(
            Duration::from_secs(5),
            state.store.set_activation_policy(&actor, body),
        )
        .await,
    )
}

pub(super) fn runner_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/runner-scheduler/profile",
            get(runner_profile).post(advertise),
        )
        .route("/v1/runner-scheduler/claim", post(claim))
        .route(
            "/v1/runner-scheduler/templates/{template}",
            get(runner_runtime),
        )
        .route("/v1/runner-scheduler/renew", post(renew_lease))
        .route(
            "/v1/runner-scheduler/run-credential",
            post(issue_run_credential),
        )
        .route(
            "/v1/runner-scheduler/start/authorize",
            post(authorize_start),
        )
        .route("/v1/runner-scheduler/start/read", post(read_start))
        .route("/v1/runner-scheduler/start/observe", post(observe_start))
        .route(
            "/v1/runner-scheduler/start/reconcile",
            post(reconcile_start),
        )
        .route(
            "/v1/runner-scheduler/complete",
            post(complete_execution).layer(DefaultBodyLimit::max(160 * 1024)),
        )
}

async fn runner_runtime(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    Path(template): Path<Uuid>,
) -> Response {
    let Some(digest) = digest_from_headers(&headers) else {
        return error_reply(&state, id, IdentityError::Unauthenticated);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.runner_runtime(&digest, template),
    )
    .await;
    reply(&state, id, result)
}

pub(super) fn runner_request<T>(
    headers: &HeaderMap,
    body: Result<Json<T>, JsonRejection>,
) -> Result<(BearerDigest, T), IdentityError> {
    let actor = digest_from_headers(headers).ok_or(IdentityError::Unauthenticated)?;
    let Json(input) = body.map_err(|_| IdentityError::InvalidInput)?;
    Ok((actor, input))
}

async fn complete_execution(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Json<CompleteExecution>, JsonRejection>,
) -> Response {
    match runner_request(&headers, body) {
        Ok((actor, input)) => reply(
            &state,
            id,
            timeout(
                Duration::from_secs(5),
                state.store.complete_execution(&actor, input),
            )
            .await,
        ),
        Err(error) => error_reply(&state, id, error),
    }
}

async fn issue_run_credential(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Json<IssueRunCredential>, JsonRejection>,
) -> Response {
    match runner_request(&headers, body) {
        Ok((actor, input)) => reply(
            &state,
            id,
            timeout(
                Duration::from_secs(5),
                state.store.issue_run_credential(&actor, input),
            )
            .await,
        ),
        Err(error) => error_reply(&state, id, error),
    }
}

async fn authorize_start(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Json<AuthorizeExecutionStart>, JsonRejection>,
) -> Response {
    match runner_request(&headers, body) {
        Ok((actor, input)) => reply(
            &state,
            id,
            timeout(
                Duration::from_secs(5),
                state.store.authorize_execution_start(&actor, input),
            )
            .await,
        ),
        Err(error) => error_reply(&state, id, error),
    }
}

async fn read_start(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Json<RenewExecutionLease>, JsonRejection>,
) -> Response {
    match runner_request(&headers, body) {
        Ok((actor, input)) => reply(
            &state,
            id,
            timeout(
                Duration::from_secs(5),
                state.store.execution_start_effect(&actor, input),
            )
            .await,
        ),
        Err(error) => error_reply(&state, id, error),
    }
}

async fn observe_start(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Json<ObserveExecutionStart>, JsonRejection>,
) -> Response {
    match runner_request(&headers, body) {
        Ok((actor, input)) => reply(
            &state,
            id,
            timeout(
                Duration::from_secs(5),
                state.store.observe_execution_start(&actor, input),
            )
            .await,
        ),
        Err(error) => error_reply(&state, id, error),
    }
}

async fn reconcile_start(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Json<ReconcileExecutionStart>, JsonRejection>,
) -> Response {
    match runner_request(&headers, body) {
        Ok((actor, input)) => reply(
            &state,
            id,
            timeout(
                Duration::from_secs(5),
                state.store.reconcile_execution_start(&actor, input),
            )
            .await,
        ),
        Err(error) => error_reply(&state, id, error),
    }
}

async fn claim(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Json<ClaimExecution>, JsonRejection>,
) -> Response {
    let Some(actor) = digest_from_headers(&headers) else {
        return error_reply(&state, id, IdentityError::Unauthenticated);
    };
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.claim_execution(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn renew_lease(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Json<RenewExecutionLease>, JsonRejection>,
) -> Response {
    let Some(actor) = digest_from_headers(&headers) else {
        return error_reply(&state, id, IdentityError::Unauthenticated);
    };
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.renew_execution_lease(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

#[tool_router(router = scheduler_tools, vis = "pub(super)")]
impl OfficeMcp {
    #[tool(
        description = "Read the question and checkpoint from one paused managed attempt of an authorized task.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_managed_input(
        &self,
        Parameters(input): Parameters<GetManagedInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store
                    .get_managed_input(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Resolve a paused attempt's question and queue a fresh managed attempt with the answer. Requires the accountable owner's authority and the current task revision.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_answer_managed_input(
        &self,
        Parameters(input): Parameters<AnswerManagedInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store
                    .answer_managed_input(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read the revision and configured automatic triggers for an agent you own or represent.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_activation_policy(
        &self,
        Parameters(input): Parameters<GetActivationPolicy>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store
                    .get_activation_policy(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Enable or disable managed turns for an agent you own. Requires the human owner's credential and the policy revision.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_set_activation_policy(
        &self,
        Parameters(input): Parameters<SetActivationPolicy>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store
                    .set_activation_policy(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Queue a managed run for an eligible assigned worker with a reservation from the task owner's budget.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_queue_execution(
        &self,
        Parameters(input): Parameters<QueueExecution>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.queue_execution(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read a queued or cancelled managed execution you can access.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_execution(
        &self,
        Parameters(input): Parameters<GetExecution>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.get_execution(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Cancel a queued managed execution and release its budget reservation.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_cancel_execution(
        &self,
        Parameters(input): Parameters<CancelExecution>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.cancel_execution(actor, input),
            )
            .await,
        )
    }
    #[tool(
        description = "Set the execution budget for your workspace and human account.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_set_execution_budget(
        &self,
        Parameters(input): Parameters<SetExecutionBudget>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.set_execution_budget(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read your own execution budget, reservations and charges.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_execution_budget(
        &self,
        Parameters(input): Parameters<GetExecutionBudget>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.get_execution_budget(actor, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read the latest execution capabilities of a runner registered under your owner account.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_runner_profile(
        &self,
        Parameters(input): Parameters<GetRunnerProfile>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.get_runner_profile(actor, input),
            )
            .await,
        )
    }
}

async fn queue_execution(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<QueueExecution>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.queue_execution(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn get_execution(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetExecution>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.get_execution(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn cancel_execution(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<CancelExecution>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.cancel_execution(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn set_execution_control(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<SetExecutionControl>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.set_execution_control(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn get_execution_control(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetExecutionControl>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.get_execution_control(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn set_execution_budget(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<SetExecutionBudget>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.set_execution_budget(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn get_execution_budget(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetExecutionBudget>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.get_execution_budget(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn get_runner_profile(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<BearerDigest>,
    body: Result<Json<GetRunnerProfile>, JsonRejection>,
) -> Response {
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.get_runner_profile(&actor, input),
    )
    .await;
    reply(&state, id, result)
}

async fn runner_profile(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
) -> Response {
    let Some(actor) = digest_from_headers(&headers) else {
        return error_reply(&state, id, IdentityError::Unauthenticated);
    };
    let result = timeout(Duration::from_secs(5), state.store.runner_profile(&actor)).await;
    reply(&state, id, result)
}

async fn advertise(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    headers: HeaderMap,
    body: Result<Json<AdvertiseRunner>, JsonRejection>,
) -> Response {
    let Some(actor) = digest_from_headers(&headers) else {
        return error_reply(&state, id, IdentityError::Unauthenticated);
    };
    let Ok(Json(input)) = body else {
        return error_reply(&state, id, IdentityError::InvalidInput);
    };
    let result = timeout(
        Duration::from_secs(5),
        state.store.advertise_runner(&actor, input),
    )
    .await;
    reply(&state, id, result)
}
