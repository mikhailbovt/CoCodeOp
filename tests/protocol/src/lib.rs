mod auth;

use std::{
    io::{self, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use auth::Actor;
use axum::{Router, http::request::Parts, middleware};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Implementation, ListResourcesResult, PaginatedRequestParams,
        ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
        ResourceContents, ServerCapabilities, ServerConfig,
    },
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
    },
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::{NamedTempFile, TempDir};
use tokio_util::sync::CancellationToken;

const MAX_TEXT_BYTES: usize = 1_024;
const MAX_POST_BYTES: usize = 16 * 1024;
const IDENTITY_URI: &str = "office://probe/identity";

#[derive(Clone)]
struct Probe {
    checkout: Arc<TempDir>,
    tools: ToolRouter<Self>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Text {
    #[schemars(length(min = 1, max = 1024))]
    text: String,
}

#[tool_router(router = tools)]
impl Probe {
    #[tool(
        description = "Return the authenticated CoCodeOp probe identity.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn whoami(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor(&context)?;
        let protocol_header = context
            .extensions
            .get::<Parts>()
            .and_then(|parts| parts.headers.get("mcp-protocol-version"))
            .and_then(|header| header.to_str().ok());
        Ok(CallToolResult::structured(json!({
            "actor_id": actor.id(),
            "mcp_protocol_header": protocol_header,
        })))
    }

    #[tool(
        description = "Echo a short string as the authenticated identity.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn echo(
        &self,
        Parameters(input): Parameters<Text>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor(&context)?;
        validate_text(&input.text)?;
        Ok(CallToolResult::structured(json!({
            "actor_id": actor.id(),
            "text": input.text,
        })))
    }

    #[tool(
        description = "Create your assigned file in the disposable shared source directory. The server selects its name. A file that already exists is preserved.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn probe_create_file(
        &self,
        Parameters(input): Parameters<Text>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor(&context)?;
        validate_text(&input.text)?;

        let path = actor.fixture_file();
        let hash = hex::encode(Sha256::digest(input.text.as_bytes()));
        let checkout = self.checkout.clone();
        let created = tokio::task::spawn_blocking(move || {
            create_file(checkout.path(), path, input.text.as_bytes())
        })
        .await
        .map_err(|_| ErrorData::internal_error("Fixture writer failed", None))?;

        match created {
            Ok(()) => Ok(CallToolResult::structured(json!({
                "actor_id": actor.id(),
                "path": path,
                "sha256": hash,
            }))),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                Ok(CallToolResult::structured_error(json!({
                    "code": "CONTENT_CONFLICT",
                    "retryable": false,
                    "message": "The assigned fixture file already exists",
                })))
            }
            Err(_) => Err(ErrorData::internal_error("Fixture write failed", None)),
        }
    }
}

#[tool_handler(router = self.tools)]
impl ServerHandler for Probe {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new(
            "cocodeop-compat-probe",
            env!("CARGO_PKG_VERSION"),
        ))
    }

    async fn list_resources(
        &self,
        _params: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        actor(&context)?;
        let resource = Resource::new(IDENTITY_URI, "Authenticated identity")
            .with_mime_type("application/json");
        Ok(ListResourcesResult::with_all_items(vec![resource]))
    }

    async fn read_resource(
        &self,
        params: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let actor = actor(&context)?;
        if params.uri != IDENTITY_URI {
            return Err(ErrorData::resource_not_found(
                "Unknown probe resource",
                None,
            ));
        }
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(identity(actor).to_string(), IDENTITY_URI)
                .with_mime_type("application/json"),
        ])
        .into())
    }
}

fn actor(context: &RequestContext<RoleServer>) -> Result<Actor, ErrorData> {
    context
        .extensions
        .get::<Parts>()
        .and_then(|parts| parts.extensions.get::<Actor>())
        .copied()
        .ok_or_else(|| ErrorData::invalid_request("Missing authenticated identity", None))
}

fn identity(actor: Actor) -> serde_json::Value {
    json!({"actor_id": actor.id()})
}

fn validate_text(text: &str) -> Result<(), ErrorData> {
    if text.is_empty() || text.len() > MAX_TEXT_BYTES {
        return Err(ErrorData::invalid_params(
            "Expected 1–1024 UTF-8 bytes",
            None,
        ));
    }
    Ok(())
}

fn create_file(root: &Path, file: &str, bytes: &[u8]) -> io::Result<()> {
    let mut staged = NamedTempFile::new_in(root)?;
    staged.write_all(bytes)?;
    staged.as_file().sync_all()?;
    staged.persist_noclobber(root.join(file))?;
    Ok(())
}

/// A local diagnostic server that only writes into its own fresh temporary directory.
pub fn router(
    credentials: PathBuf,
    checkout: Arc<TempDir>,
    listener: SocketAddr,
    stop: CancellationToken,
) -> io::Result<Router> {
    if !listener.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the compatibility probe binds to loopback",
        ));
    }
    let mut hosts = vec![
        listener.to_string(),
        format!("localhost:{}", listener.port()),
    ];
    if listener.port() == 80 {
        hosts.extend([listener.ip().to_string(), "localhost".to_owned()]);
    }
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(hosts)
        .enforce_origin_validation()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_max_request_body_bytes(MAX_POST_BYTES)
        .with_cancellation_token(stop);
    let probe = Probe {
        checkout,
        tools: Probe::tools(),
    };
    let service = StreamableHttpService::new(
        move || Ok(probe.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    Ok(Router::new()
        .route_service("/mcp", service)
        .layer(middleware::from_fn_with_state(
            credentials,
            auth::authenticate,
        ))
        .layer(middleware::from_fn(timeout_response)))
}

async fn timeout_response(
    request: axum::extract::Request,
    next: middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match tokio::time::timeout(Duration::from_secs(5), next.run(request)).await {
        Ok(response) => response,
        Err(_) => (
            axum::http::StatusCode::GATEWAY_TIMEOUT,
            axum::Json(json!({"code": "REQUEST_TIMEOUT", "retryable": false})),
        )
            .into_response(),
    }
}
