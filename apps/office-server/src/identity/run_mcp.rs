use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    extract::{Request, State},
    http::{header, request::Parts},
    middleware::{self, Next},
    response::Response,
};
use office_domain::{BearerDigest, RequestId};
use office_protocol::{
    ActivationTrigger, AnswerActivationQuestion, ApplyCheckoutChanges, ClaimPaths,
    GetAccessRequest, GetCheckoutCommand, GetCheckoutStatus, GetPathHandoff, GetRunWorkContext,
    MAX_PUBLICATION_BODY, ReadCheckoutFile, ReleasePathClaim, RenewPathClaim,
    ReplaceCheckoutTextFile, RequestAccess, RequestPathHandoff, RequestRunInput,
    RespondPathHandoff, ReviewActivationResult, RunCheckoutCommand, RunContextView,
    SearchCheckoutCode,
};
use office_store::{IdentityError, Store};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Implementation, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerConfig,
    },
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
    },
};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use super::{bearer_from_headers, current_digest, mutations::error_reply, tool_reply};
use crate::AppState;

pub(super) fn routes(state: AppState, stop: CancellationToken) -> Router<AppState> {
    let store = state.store.clone();
    let event_retain_count = state.event_retain_count;
    let config = StreamableHttpServerConfig::default()
        .enforce_origin_validation()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_max_request_body_bytes(MAX_PUBLICATION_BODY)
        .with_cancellation_token(stop);
    let service = StreamableHttpService::new(
        move || {
            Ok(RunMcp {
                tools: RunMcp::tools(),
                store: store.clone(),
                event_retain_count,
            })
        },
        Arc::new(NeverSessionManager::default()),
        config,
    );
    Router::new()
        .route_service("/run/mcp", service)
        .layer(middleware::from_fn_with_state(state, authenticate))
}

async fn authenticate(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    let id = *request
        .extensions()
        .get::<RequestId>()
        .expect("request context is installed");
    let Some(digest) = bearer_from_headers(request.headers()).and_then(BearerDigest::of_run) else {
        return error_reply(&state, id, IdentityError::Unauthenticated);
    };
    match timeout(Duration::from_secs(3), state.store.run_context(&digest)).await {
        Ok(Ok(worker)) => {
            request.headers_mut().remove(header::AUTHORIZATION);
            request.extensions_mut().insert(Arc::new(worker));
            request.extensions_mut().insert(digest);
            next.run(request).await
        }
        Ok(Err(error)) => error_reply(&state, id, error),
        Err(_) => error_reply(&state, id, IdentityError::Timeout),
    }
}

struct RunMcp {
    tools: ToolRouter<Self>,
    store: Store,
    event_retain_count: u32,
}

#[tool_router(router = tools)]
impl RunMcp {
    #[tool(
        description = "Read the task and criteria assigned to this active execution.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_run_context(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = serde_json::to_value(worker(&context)?)
            .map_err(|_| ErrorData::internal_error("Run context could not be encoded", None))?;
        Ok(CallToolResult::structured(result))
    }

    #[tool(
        description = "Read the current run and a bounded inbox page for its task. Continue with inbox.next_cursor as inbox_cursor.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_run_work_context(
        &self,
        Parameters(input): Parameters<GetRunWorkContext>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.run_work_context(
                    current_digest(&context)?,
                    input,
                    self.event_retain_count,
                ),
            )
            .await,
        )
    }

    #[tool(
        description = "Ask the accountable lead a clarification for this run. After the request is recorded, finish the turn so the runner can checkpoint and release the slot. Provide a UUID idempotency_key and one question.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_request_run_input(
        &self,
        Parameters(input): Parameters<RequestRunInput>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store
                    .request_run_input(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Answer an open question included in this run's activation references. Pass the exact question_id, a fresh UUID idempotency_key and the answer body.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_answer_activation_question(
        &self,
        Parameters(input): Parameters<AnswerActivationQuestion>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store
                    .answer_activation_question(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read the task and exact submitted result bound to this review activation.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_activation_review(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.get_activation_review(current_digest(&context)?),
            )
            .await,
        )
    }

    #[tool(
        description = "Submit accept or request_changes for the bound result. Copy result_sha256 from office_get_activation_review and include a reason and UUID idempotency_key.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_review_activation_result(
        &self,
        Parameters(input): Parameters<ReviewActivationResult>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store
                    .review_activation_result(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Request a new WorkScope for this running task. Include the current task and scope revisions. Pass permissions as an object with read, write and write_operations. Only the designated approvers can approve it.",
        input_schema = super::model_schema::typed_input::<RequestAccess>(),
        annotations(idempotent_hint = true, open_world_hint = false)
    )]
    async fn office_request_access(
        &self,
        Parameters(input): Parameters<RequestAccess>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store
                    .request_run_access(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read the latest decision on a request for this task. Read office_run_context again after approval to get the installed scope revision.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_access_request(
        &self,
        Parameters(input): Parameters<GetAccessRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store
                    .get_run_access_request(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read one allowed file from the checkout host. Supply the relative path, current scope_revision and a UUID idempotency_key. The result includes the host and file.sha256.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn office_read_file(
        &self,
        Parameters(input): Parameters<ReadCheckoutFile>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let credential = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(8),
                self.store.read_checkout_file(credential, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Search authorized checkout files for one literal string, optionally within a directory. The result contains file hashes and line numbers.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn office_search_code(
        &self,
        Parameters(input): Parameters<SearchCheckoutCode>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let credential = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(8),
                self.store.search_checkout_code(credential, input),
            )
            .await,
        )
    }

    #[tool(
        description = "List authorized files and their current hashes from the checkout host. Pass next_after as after to read another page.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn office_checkout_status(
        &self,
        Parameters(input): Parameters<GetCheckoutStatus>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let credential = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(8),
                self.store.checkout_status(credential, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Publish a bounded array of create, replace, delete or rename operations. For a text replacement set kind to replace, path to the relative file path, before_sha256 to the hash from office_read_file, and file to an object with encoding utf8 and content containing the complete text. Include the current scope revision, the base checkout revision, required read_dependencies as path and sha256 pairs, and a UUID idempotency_key. Omit claim_fence for a short automatic path claim.",
        input_schema = super::model_schema::typed_input::<ApplyCheckoutChanges>(),
        annotations(
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn office_apply_changes(
        &self,
        Parameters(input): Parameters<ApplyCheckoutChanges>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let credential = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(9),
                self.store.apply_checkout_changes(credential, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Replace one allowed UTF-8 file. Set path to its relative path, before_sha256 to the read hash and content to the complete text. Include both revisions and a UUID idempotency_key. For a required read dependency pass dependency_path and dependency_sha256 from the read. For several dependencies use office_apply_changes. Omit claim_fence for a short automatic claim.",
        input_schema = super::model_schema::typed_input::<ReplaceCheckoutTextFile>(),
        annotations(destructive_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn office_replace_text_file(
        &self,
        Parameters(input): Parameters<ReplaceCheckoutTextFile>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let input = input.try_into().map_err(|()| {
            ErrorData::invalid_params("Provide both dependency_path and dependency_sha256", None)
        })?;
        self.office_apply_changes(Parameters(input), context).await
    }

    #[tool(
        description = "Reserve all requested paths for this exact running attempt. Each entry in paths uses kind file or directory and a relative path. Use 5–120 seconds for ttl_seconds and a UUID idempotency_key. Supply expected_fence to replace the current set.",
        input_schema = super::model_schema::typed_input::<ClaimPaths>(),
    )]
    async fn office_claim_paths(
        &self,
        Parameters(input): Parameters<ClaimPaths>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let credential = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.claim_run_paths(credential, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Extend the active claim for this running attempt, within its current lease and scope."
    )]
    async fn office_renew_claim(
        &self,
        Parameters(input): Parameters<RenewPathClaim>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let credential = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.renew_run_path_claim(credential, input),
            )
            .await,
        )
    }

    #[tool(description = "Release the active claim owned by this running attempt.")]
    async fn office_release_claim(
        &self,
        Parameters(input): Parameters<ReleasePathClaim>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let credential = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.release_run_path_claim(credential, input),
            )
            .await,
        )
    }

    #[tool(
        description = "After PATH_CLAIMED, ask the current holder to hand off the overlapping paths to this run. Pass the current scope revision and exact list of paths.",
        input_schema = super::model_schema::typed_input::<RequestPathHandoff>(),
        annotations(idempotent_hint = true, open_world_hint = false)
    )]
    async fn office_request_path_handoff(
        &self,
        Parameters(input): Parameters<RequestPathHandoff>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store
                    .request_run_path_handoff(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Read a path handoff for this run. Find incoming handoff IDs in office_run_work_context.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_path_handoff(
        &self,
        Parameters(input): Parameters<GetPathHandoff>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store
                    .get_run_path_handoff(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Respond to a path handoff for this run. The holder can offer or reject. The requester can cancel. After an offer the requester uses office_claim_paths with the same paths and handoff_id, then rereads the files.",
        annotations(idempotent_hint = true, open_world_hint = false)
    )]
    async fn office_respond_path_handoff(
        &self,
        Parameters(input): Parameters<RespondPathHandoff>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store
                    .respond_run_path_handoff(current_digest(&context)?, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Schedule an approved command on the checkout host. Arguments must match the template. Use office_get_command with the returned command_id to track the result.",
        annotations(idempotent_hint = true, open_world_hint = false)
    )]
    async fn office_run_command(
        &self,
        Parameters(input): Parameters<RunCheckoutCommand>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let credential = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(8),
                self.store.run_checkout_command(credential, input),
            )
            .await,
        )
    }

    #[tool(
        description = "Get the state or bounded outcome of a command for this running attempt. Polling a queued command rechecks the current scope before dispatch.",
        annotations(idempotent_hint = true, open_world_hint = false)
    )]
    async fn office_get_command(
        &self,
        Parameters(input): Parameters<GetCheckoutCommand>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let credential = current_digest(&context)?;
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store.get_checkout_command(credential, input),
            )
            .await,
        )
    }
}

#[tool_handler(router = self.tools)]
impl ServerHandler for RunMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("cocodeop-run", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_tools(
        &self,
        _parameters: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let run = worker(&context)?;
        let mut tools = self.tools.list_all();
        tools.retain(|tool| match tool.name.as_ref() {
            "office_run_context" | "office_run_work_context" | "office_request_run_input" => true,
            "office_answer_activation_question" => run
                .activation
                .as_ref()
                .is_some_and(|turn| turn.trigger == ActivationTrigger::Question),
            "office_get_activation_review" | "office_review_activation_result" => run
                .activation
                .as_ref()
                .is_some_and(|turn| turn.trigger == ActivationTrigger::ReviewRequest),
            _ => run.scope_revision.is_some(),
        });
        Ok(ListToolsResult::with_all_items(tools))
    }
}

fn worker(context: &RequestContext<RoleServer>) -> Result<&RunContextView, ErrorData> {
    context
        .extensions
        .get::<Parts>()
        .and_then(|parts| parts.extensions.get::<Arc<RunContextView>>())
        .map(Arc::as_ref)
        .ok_or_else(|| ErrorData::invalid_request("An active run credential is required", None))
}
