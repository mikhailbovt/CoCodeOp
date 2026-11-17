use std::{sync::Arc, time::Duration};

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, StatusCode, header, request::Parts},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use office_domain::{BearerDigest, ErrorCode, MessageId, NoteId, RequestId, TaskId, WorkspaceId};
use office_protocol::{
    AckInbox, ApiError, AttachAgentInstance, GetAgentInstance, GetContext, GetInboxDelivery,
    GetNote, McpIdentity, Outcome, ReadInbox, Reply, UpsertNote,
};
use office_store::{IdentityError, Store};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Implementation, ListResourcesResult, ListToolsResult,
        PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse,
        ReadResourceResult, Resource, ResourceContents, ServerCapabilities, ServerConfig,
    },
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
    },
};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::AppState;

mod artifacts;
mod checkouts;
mod checkpoint_snapshots;
mod claims;
mod communication;
mod coordinator;
mod decisions;
mod delegation;
mod managed_workers;
mod model_schema;
mod mutations;
pub(crate) mod operations_metrics;
mod run_mcp;
mod scheduler;
mod tasks;
mod teams;
mod work_scopes;

pub(crate) fn routes(state: AppState, stop: CancellationToken) -> Router<AppState> {
    let worker_routes = run_mcp::routes(state.clone(), stop.clone());
    let config = StreamableHttpServerConfig::default()
        .enforce_origin_validation()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_max_request_body_bytes(16 * 1024)
        .with_cancellation_token(stop);
    let store = state.store.clone();
    let event_retain_count = state.event_retain_count;
    let service = StreamableHttpService::new(
        move || {
            Ok(OfficeMcp {
                tools: OfficeMcp::tools()
                    + OfficeMcp::artifact_tools()
                    + OfficeMcp::snapshot_tools()
                    + OfficeMcp::workers_tools()
                    + OfficeMcp::communication_tools()
                    + OfficeMcp::task_tools()
                    + OfficeMcp::checkout_tools()
                    + OfficeMcp::claim_tools()
                    + OfficeMcp::decision_tools()
                    + OfficeMcp::scheduler_tools()
                    + OfficeMcp::scope_tools()
                    + OfficeMcp::delegation_tools()
                    + OfficeMcp::team_tools(),
                store: store.clone(),
                event_retain_count,
            })
        },
        Arc::new(NeverSessionManager::default()),
        config,
    );
    Router::new()
        .merge(artifacts::routes())
        .merge(checkpoint_snapshots::routes())
        .merge(managed_workers::routes())
        .merge(checkouts::routes())
        .merge(claims::routes())
        .merge(decisions::routes())
        .merge(scheduler::routes())
        .merge(work_scopes::routes())
        .merge(communication::routes())
        .merge(delegation::routes())
        .merge(tasks::routes())
        .merge(teams::routes())
        .route("/v1/me", get(me))
        .route("/v1/notes", post(mutations::upsert_note))
        .route("/v1/notes/read", post(mutations::read_note))
        .route("/v1/context", post(mutations::actor_context))
        .route("/v1/operations/metrics", post(operations_metrics::read))
        .route("/v1/inbox/read", post(mutations::read_inbox))
        .route("/v1/inbox/ack", post(mutations::ack_inbox))
        .route("/v1/inbox/delivery", post(mutations::inbox_delivery))
        .route("/v1/workspaces", post(mutations::create_workspace))
        .route(
            "/v1/workspaces/{workspace_id}/runners",
            post(mutations::register_runner),
        )
        .route("/v1/credentials", post(mutations::issue_credential))
        .route(
            "/v1/credentials/{credential_id}/revoke",
            post(mutations::revoke_credential),
        )
        .route(
            "/v1/workspaces/{workspace_id}/agents",
            post(mutations::create_agent),
        )
        .route(
            "/v1/workspaces/{workspace_id}/invitations",
            post(mutations::issue_invitation),
        )
        .route_service("/mcp", service)
        .layer(middleware::from_fn_with_state(state, authenticate))
        .merge(checkouts::runner_routes())
        .merge(coordinator::runner_routes())
        .merge(checkpoint_snapshots::runner_routes())
        .merge(work_scopes::runner_routes())
        .merge(scheduler::runner_routes())
        .merge(worker_routes)
        .route("/v1/runners/self", get(mutations::runner_self))
        .route("/v1/invitations/accept", post(mutations::accept_invitation))
        .layer(DefaultBodyLimit::max(16 * 1024))
}

async fn authenticate(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    let digest = digest_from_headers(request.headers());
    let path = request.uri().path();
    let agent_allowed = matches!(
        path,
        "/mcp" | "/v1/context" | "/v1/inbox/read" | "/v1/inbox/ack" | "/v1/inbox/delivery"
    ) || artifacts::agent_route(path)
        || checkpoint_snapshots::agent_route(path)
        || managed_workers::agent_route(path)
        || claims::agent_route(path)
        || communication::question_route(path)
        || decisions::agent_route(path)
        || scheduler::agent_route(path)
        || tasks::result_route(path);
    let actor = timeout(Duration::from_secs(2), async {
        let digest = digest.as_ref().ok_or(IdentityError::Unauthenticated)?;
        if agent_allowed {
            state.store.authenticate_mcp(digest).await
        } else {
            state
                .store
                .authenticate_user(digest)
                .await
                .map(McpIdentity::Human)
        }
    })
    .await;
    match actor {
        Ok(Ok(actor)) => {
            request.headers_mut().remove(header::AUTHORIZATION);
            request.extensions_mut().insert(actor);
            if let Some(digest) = digest {
                request.extensions_mut().insert(digest);
            }
            next.run(request).await
        }
        failure => {
            let id = *request
                .extensions()
                .get::<RequestId>()
                .expect("request context is installed");
            let unauthenticated = matches!(failure, Ok(Err(IdentityError::Unauthenticated)));
            let (status, code, message) = if unauthenticated {
                (
                    StatusCode::UNAUTHORIZED,
                    ErrorCode::Unauthenticated,
                    "Invalid or inactive bearer credential",
                )
            } else {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    ErrorCode::DatabaseUnavailable,
                    "Identity store is unavailable",
                )
            };
            let mut response = (
                status,
                Json(Reply::<()> {
                    request_id: id,
                    timestamp: state.clock.now(),
                    outcome: Outcome::Failure {
                        error: ApiError {
                            code,
                            message: message.into(),
                            retryable: !unauthenticated,
                            current_revision: None,
                        },
                    },
                }),
            )
                .into_response();
            operations_metrics::record(code);
            if unauthenticated {
                response.headers_mut().insert(
                    header::WWW_AUTHENTICATE,
                    axum::http::HeaderValue::from_static("Bearer"),
                );
            }
            response
        }
    }
}

fn digest_from_headers(headers: &HeaderMap) -> Option<BearerDigest> {
    BearerDigest::of(bearer_from_headers(headers)?)
}

fn bearer_from_headers(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let (scheme, token) = values.next()?.to_str().ok()?.split_once(' ')?;
    if values.next().is_some() || !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    Some(token)
}

async fn me(
    State(state): State<AppState>,
    Extension(id): Extension<RequestId>,
    Extension(actor): Extension<McpIdentity>,
) -> Response {
    let McpIdentity::Human(identity) = actor else {
        return mutations::error_reply(&state, id, IdentityError::Forbidden);
    };
    Json(Reply {
        request_id: id,
        timestamp: state.clock.now(),
        outcome: Outcome::Success { result: identity },
    })
    .into_response()
}

struct OfficeMcp {
    tools: ToolRouter<Self>,
    store: Store,
    event_retain_count: u32,
}

#[tool_router(router = tools)]
impl OfficeMcp {
    #[tool(
        description = "Show your authenticated CoCodeOp identity and active workspace memberships. Then call office_get_context for the selected workspace.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_bootstrap(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = current_actor(&context)?;
        let result = serde_json::to_value(actor)
            .map_err(|_| ErrorData::internal_error("Identity could not be encoded", None))?;
        Ok(CallToolResult::structured(result))
    }

    #[tool(
        description = "Get your bounded inbox, active tasks, and role workflow for one workspace. Continue each page with its own cursor.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_context(
        &self,
        Parameters(input): Parameters<GetContext>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                Duration::from_secs(5),
                self.store
                    .actor_context(current_digest(&context)?, input, self.event_retain_count),
            )
            .await,
        )
    }

    #[tool(
        description = "Register an interactive instance for the active agent credential. Return a temporary instance ID.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn office_attach_instance(
        &self,
        Parameters(input): Parameters<AttachAgentInstance>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let digest = instance_credential(&context)?;
        let result = timeout(
            Duration::from_secs(3),
            self.store.attach_instance(digest, input),
        )
        .await;
        tool_reply(result)
    }

    #[tool(
        description = "Get an unexpired instance registered with your active agent credential.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_instance(
        &self,
        Parameters(input): Parameters<GetAgentInstance>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let digest = instance_credential(&context)?;
        let result = timeout(
            Duration::from_secs(3),
            self.store.get_instance(digest, input.instance_id),
        )
        .await;
        tool_reply(result)
    }

    #[tool(
        description = "Create a workspace note with explicit visibility, or revise an existing note with its expected revision.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_upsert_note(
        &self,
        Parameters(input): Parameters<UpsertNote>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let digest = current_digest(&context)?;
        let result = timeout(
            Duration::from_secs(5),
            self.store.upsert_note(digest, input),
        )
        .await;
        tool_reply(result)
    }

    #[tool(
        description = "Read a note or one of its immutable revisions, within the caller's current visibility.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_get_note(
        &self,
        Parameters(input): Parameters<GetNote>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let digest = current_digest(&context)?;
        let result = timeout(Duration::from_secs(3), self.store.get_note(digest, input)).await;
        tool_reply(result)
    }

    #[tool(
        description = "Read the next bounded page of visible workspace events. Keep next_cursor to reconnect.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_read_inbox(
        &self,
        Parameters(input): Parameters<ReadInbox>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let digest = current_digest(&context)?;
        let result = timeout(
            Duration::from_secs(5),
            self.store
                .read_inbox(digest, input, self.event_retain_count),
        )
        .await;
        tool_reply(result)
    }

    #[tool(
        description = "Read the delivery and acknowledgement state of a visible addressed event.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn office_inbox_delivery(
        &self,
        Parameters(input): Parameters<GetInboxDelivery>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        tool_reply(
            timeout(
                Duration::from_secs(3),
                self.store.inbox_delivery(
                    current_digest(&context)?,
                    input,
                    self.event_retain_count,
                ),
            )
            .await,
        )
    }

    #[tool(
        description = "Acknowledge delivery of a visible event for the authenticated reader.",
        annotations(
            idempotent_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn office_ack_inbox(
        &self,
        Parameters(input): Parameters<AckInbox>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let digest = current_digest(&context)?;
        let result = timeout(
            Duration::from_secs(3),
            self.store.ack_inbox(digest, input, self.event_retain_count),
        )
        .await;
        tool_reply(result)
    }
}

fn instance_credential(context: &RequestContext<RoleServer>) -> Result<&BearerDigest, ErrorData> {
    if !matches!(current_actor(context)?, McpIdentity::Agent(_)) {
        return Err(ErrorData::invalid_request(
            "An agent credential is required",
            Some(serde_json::json!({"code": "FORBIDDEN"})),
        ));
    }
    current_digest(context)
}

fn current_digest(context: &RequestContext<RoleServer>) -> Result<&BearerDigest, ErrorData> {
    context
        .extensions
        .get::<Parts>()
        .and_then(|parts| parts.extensions.get::<BearerDigest>())
        .ok_or_else(|| ErrorData::invalid_request("Authentication is required", None))
}

fn tool_reply<T: serde::Serialize>(
    result: Result<Result<T, IdentityError>, tokio::time::error::Elapsed>,
) -> Result<CallToolResult, ErrorData> {
    let code = match result {
        Ok(Ok(value)) => {
            let structured = serde_json::to_value(value)
                .map_err(|_| ErrorData::internal_error("Response could not be encoded", None))?;
            return Ok(CallToolResult::structured(structured));
        }
        Ok(Err(IdentityError::Unauthenticated)) => ErrorCode::Unauthenticated,
        Ok(Err(IdentityError::Forbidden)) => ErrorCode::Forbidden,
        Ok(Err(IdentityError::InvalidName | IdentityError::InvalidInput)) => {
            ErrorCode::InvalidInput
        }
        Ok(Err(IdentityError::IdempotencyConflict)) => ErrorCode::IdempotencyConflict,
        Ok(Err(IdentityError::RevisionConflict)) => ErrorCode::RevisionConflict,
        Ok(Err(IdentityError::RevisionMismatch { current })) => {
            return Ok(CallToolResult::structured_error(serde_json::json!({
                "code": ErrorCode::RevisionConflict, "retryable": false, "current_revision": current,
            })));
        }
        Ok(Err(IdentityError::InvalidTransition)) => ErrorCode::InvalidTransition,
        Ok(Err(IdentityError::DependencyNotReady)) => ErrorCode::DependencyNotReady,
        Ok(Err(IdentityError::DependencyCycle)) => ErrorCode::DependencyCycle,
        Ok(Err(IdentityError::DelegationCycle)) => ErrorCode::DelegationCycle,
        Ok(Err(IdentityError::DelegationLimit)) => ErrorCode::DelegationLimit,
        Ok(Err(IdentityError::DelegationInactive)) => ErrorCode::DelegationInactive,
        Ok(Err(IdentityError::CheckoutRootOverlap)) => ErrorCode::CheckoutRootOverlap,
        Ok(Err(IdentityError::AccessRequestRequired)) => ErrorCode::AccessRequestRequired,
        Ok(Err(IdentityError::PathClaimed)) => ErrorCode::PathClaimed,
        Ok(Err(IdentityError::ClaimFenceStale)) => ErrorCode::ClaimFenceStale,
        Ok(Err(IdentityError::ClaimRenewalLimit)) => ErrorCode::ClaimRenewalLimit,
        Ok(Err(IdentityError::PathHandoffPending)) => ErrorCode::PathHandoffPending,
        Ok(Err(IdentityError::BudgetExhausted)) => ErrorCode::BudgetExhausted,
        Ok(Err(IdentityError::ExecutionPaused)) => ErrorCode::ExecutionPaused,
        Ok(Err(IdentityError::ExecutionCapacityReached)) => ErrorCode::ExecutionCapacityReached,
        Ok(Err(IdentityError::MessageRateLimited)) => ErrorCode::MessageRateLimited,
        Ok(Err(IdentityError::ArtifactQuotaExceeded)) => ErrorCode::ArtifactQuotaExceeded,
        Ok(Err(IdentityError::ArtifactUploadOpen)) => ErrorCode::ArtifactUploadOpen,
        Ok(Err(IdentityError::ArtifactIncomplete)) => ErrorCode::ArtifactIncomplete,
        Ok(Err(IdentityError::ArtifactDigestMismatch)) => ErrorCode::ArtifactDigestMismatch,
        Ok(Err(IdentityError::ArtifactUnavailable)) => ErrorCode::ArtifactUnavailable,
        Ok(Err(IdentityError::RunLeaseStale)) => ErrorCode::RunLeaseStale,
        Ok(Err(IdentityError::CoordinatorStale)) => ErrorCode::CoordinatorStale,
        Ok(Err(IdentityError::CheckoutOffline)) => ErrorCode::CheckoutOffline,
        Ok(Err(IdentityError::PathForbidden)) => ErrorCode::PathForbidden,
        Ok(Err(IdentityError::ScopeStale)) => ErrorCode::ScopeStale,
        Ok(Err(IdentityError::ContentConflict)) => ErrorCode::ContentConflict,
        Ok(Err(IdentityError::ContextStale)) => ErrorCode::ContextStale,
        Ok(Err(IdentityError::CheckoutReconciliationRequired)) => {
            ErrorCode::CheckoutReconciliationRequired
        }
        Ok(Err(IdentityError::PublicationPending)) => ErrorCode::PublicationPending,
        Ok(Err(IdentityError::FileNotFound)) => ErrorCode::FileNotFound,
        Ok(Err(IdentityError::CursorExpired)) => {
            return Ok(CallToolResult::structured_error(serde_json::json!({
                "code": ErrorCode::CursorExpired,
                "retryable": false,
                "message": IdentityError::CursorExpired.to_string(),
            })));
        }
        _ => ErrorCode::DatabaseUnavailable,
    };
    operations_metrics::record(code);
    Ok(CallToolResult::structured_error(serde_json::json!({
        "code": code, "retryable": matches!(code, ErrorCode::DatabaseUnavailable | ErrorCode::CheckoutOffline
            | ErrorCode::PublicationPending | ErrorCode::ExecutionCapacityReached | ErrorCode::MessageRateLimited),
    })))
}

#[tool_handler(router = self.tools)]
impl ServerHandler for OfficeMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("cocodeop", env!("CARGO_PKG_VERSION")))
        .with_instructions("Call office_bootstrap to verify your identity. Use office_get_context for the workspace inbox, your tasks, and workflow instructions. Leads can use office_create_managed_worker and office_worker_status. Read event references before responding.")
    }

    async fn list_tools(
        &self,
        _params: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let actor = current_actor(&context)?;
        let mut tools = self.tools.list_all();
        if matches!(actor, McpIdentity::Human(_)) {
            tools.retain(|tool| {
                !matches!(
                    tool.name.as_ref(),
                    "office_attach_instance" | "office_get_instance"
                )
            });
        } else {
            tools.retain(|tool| tool.name.as_ref() != "office_set_activation_policy");
        }
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn list_resources(
        &self,
        _params: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let actor = current_actor(&context)?;
        let items = match actor {
            McpIdentity::Human(user) => user
                .memberships
                .iter()
                .map(|membership| workspace_resource(membership.workspace_id.0, &membership.name))
                .collect(),
            McpIdentity::Agent(agent) => vec![workspace_resource(
                agent.workspace_id.0,
                &agent.workspace_name,
            )],
        };
        Ok(ListResourcesResult::with_all_items(items))
    }

    async fn read_resource(
        &self,
        params: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let unavailable =
            || ErrorData::resource_not_found("Workspace resource is unavailable", None);
        if let Some((workspace, task)) = task_resource(&params.uri) {
            let digest = current_digest(&context)?;
            let input = office_protocol::GetTask {
                workspace_id: workspace,
                task_id: task,
                runs_after: None,
                run_limit: None,
            };
            let result = timeout(Duration::from_secs(5), self.store.get_task(digest, input)).await;
            return resource_reply(params.uri, result);
        }
        if let Some((workspace, thread)) = thread_resource(&params.uri) {
            let digest = current_digest(&context)?;
            let result = timeout(
                Duration::from_secs(5),
                self.store.read_thread_resource(digest, workspace, thread),
            )
            .await;
            return resource_reply(params.uri, result);
        }
        if let Some(input) = note_resource(&params.uri) {
            let digest = current_digest(&context)?;
            let result = timeout(Duration::from_secs(3), self.store.get_note(digest, input)).await;
            return resource_reply(params.uri, result);
        }
        let actor = current_actor(&context)?;
        let result = match actor {
            McpIdentity::Human(user) => {
                let membership = user.memberships.iter()
                    .find(|membership| workspace_uri(membership.workspace_id.0) == params.uri)
                    .ok_or_else(unavailable)?;
                serde_json::to_string(membership)
                    .map_err(|_| ErrorData::internal_error("Workspace could not be encoded", None))?
            }
            McpIdentity::Agent(agent) if workspace_uri(agent.workspace_id.0) == params.uri => {
                serde_json::json!({"workspace_id": agent.workspace_id, "name": agent.workspace_name, "role": agent.role}).to_string()
            }
            McpIdentity::Agent(_) => return Err(unavailable()),
        };
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(result, params.uri).with_mime_type("application/json"),
        ])
        .into())
    }
}

fn note_resource(uri: &str) -> Option<GetNote> {
    let parts: Vec<_> = uri
        .strip_prefix("office://workspace/")?
        .split('/')
        .collect();
    let [workspace, "notes", id, "revisions", revision] = parts.as_slice() else {
        return None;
    };
    Some(GetNote {
        workspace_id: WorkspaceId(workspace.parse().ok()?),
        note_id: NoteId(id.parse().ok()?),
        revision: Some(revision.parse().ok()?),
    })
}

fn task_resource(uri: &str) -> Option<(WorkspaceId, TaskId)> {
    let (workspace, task) = uri
        .strip_prefix("office://workspace/")?
        .split_once("/tasks/")?;
    Some((
        WorkspaceId(workspace.parse().ok()?),
        TaskId(task.parse().ok()?),
    ))
}

fn thread_resource(uri: &str) -> Option<(WorkspaceId, MessageId)> {
    let parts: Vec<_> = uri
        .strip_prefix("office://workspace/")?
        .split('/')
        .collect();
    let [workspace, "threads", id] = parts.as_slice() else {
        return None;
    };
    Some((
        WorkspaceId(workspace.parse().ok()?),
        MessageId(id.parse().ok()?),
    ))
}

fn resource_reply<T: serde::Serialize>(
    uri: String,
    result: Result<Result<T, IdentityError>, tokio::time::error::Elapsed>,
) -> Result<ReadResourceResponse, ErrorData> {
    let response = match result {
        Ok(Ok(response)) => response,
        Ok(Err(
            IdentityError::Forbidden | IdentityError::InvalidInput | IdentityError::Unauthenticated,
        )) => {
            operations_metrics::record(ErrorCode::Forbidden);
            return Err(ErrorData::resource_not_found(
                "Workspace resource is unavailable",
                None,
            ));
        }
        _ => {
            return Err(ErrorData::internal_error(
                "Resource is temporarily unavailable",
                None,
            ));
        }
    };
    let result = serde_json::to_string(&response)
        .map_err(|_| ErrorData::internal_error("Resource could not be encoded", None))?;
    Ok(ReadResourceResult::new(vec![
        ResourceContents::text(result, uri).with_mime_type("application/json"),
    ])
    .into())
}

fn current_actor(context: &RequestContext<RoleServer>) -> Result<&McpIdentity, ErrorData> {
    context
        .extensions
        .get::<Parts>()
        .and_then(|parts| parts.extensions.get::<McpIdentity>())
        .ok_or_else(|| ErrorData::invalid_request("Authentication is required", None))
}

fn workspace_uri(id: uuid::Uuid) -> String {
    format!("office://workspace/{id}/overview")
}

fn workspace_resource(id: uuid::Uuid, name: &str) -> Resource {
    Resource::new(workspace_uri(id), name).with_mime_type("application/json")
}
