use std::{
    fs::{self, Permissions},
    io::{self, BufRead, Read, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::Arc,
    time::Duration,
};

use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use office_protocol::{GetRunWorkContext, RunWorkContext};
use reqwest::{Client, Url, redirect::Policy};
use serde_json::{Value, json};
use tokio::{
    net::UnixListener,
    task::JoinHandle,
    time::{Instant, sleep},
};
use uuid::Uuid;

const REQUEST_LIMIT: usize = office_protocol::MAX_PUBLICATION_BODY;
const RESPONSE_LIMIT: usize = 192 * 1024;
const ACCEPT: &str = "application/json, text/event-stream";
const MCP_PROTOCOL_VERSION: &str = "mcp-protocol-version";

pub(super) struct WorkerSocket {
    listener: UnixListener,
    upstream: Arc<Upstream>,
}

struct Upstream {
    client: Client,
    mcp: Url,
    bearer: String,
}

pub(super) struct Listening(JoinHandle<()>);

impl Drop for Listening {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl WorkerSocket {
    pub fn bind(path: &Path, client: Client, mcp: Url, bearer: String) -> Result<Self, String> {
        let listener =
            UnixListener::bind(path).map_err(|_| "Could not bind the run's private MCP socket")?;
        fs::set_permissions(path, Permissions::from_mode(0o600))
            .map_err(|_| "Could not restrict access to the run's private MCP socket")?;
        Ok(Self {
            listener,
            upstream: Arc::new(Upstream {
                client,
                mcp,
                bearer,
            }),
        })
    }

    pub fn start(self) -> Listening {
        let router = Router::new()
            .route("/mcp", post(forward))
            .layer(DefaultBodyLimit::max(REQUEST_LIMIT))
            .with_state(self.upstream);
        Listening(tokio::spawn(async move {
            if axum::serve(self.listener, router).await.is_err() {
                eprintln!("The run's local MCP socket stopped serving requests");
            }
        }))
    }
}

async fn forward(
    State(upstream): State<Arc<Upstream>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let mut request = upstream
        .client
        .post(upstream.mcp.clone())
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, ACCEPT)
        .bearer_auth(&upstream.bearer)
        .body(bytes);
    if let Some(version) = headers
        .get(MCP_PROTOCOL_VERSION)
        .filter(|version| version.as_bytes().len() <= 32)
    {
        request = request.header(MCP_PROTOCOL_VERSION, version);
    }
    let Ok(response) = request.send().await else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let status = response.status();
    match bounded_response(response).await {
        Ok(body) => (status, [(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Err(_) => StatusCode::BAD_GATEWAY.into_response(),
    }
}

fn socket_client(socket: &Path) -> Result<Client, String> {
    Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        // This client only uses HTTP over the bound Unix socket.
        .tls_certs_only([])
        .unix_socket(socket.to_owned())
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|_| "Could not initialize the worker MCP socket client".into())
}

pub(super) async fn run_work_context(
    socket: &Path,
    cursor: Option<Uuid>,
) -> Result<RunWorkContext, String> {
    let client = socket_client(socket)?;
    let message = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"office_run_work_context","arguments": GetRunWorkContext { inbox_cursor: cursor, limit: None }}});
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let response = client
            .post("http://localhost/mcp")
            .header(header::ACCEPT, ACCEPT)
            .header(MCP_PROTOCOL_VERSION, "2025-11-25")
            .timeout(Duration::from_secs(2))
            .json(&message)
            .send()
            .await;
        if let Ok(response) = response
            && response.status().is_success()
        {
            let bytes = bounded_response(response).await?;
            let mut answer: Value = serde_json::from_slice(&bytes)
                .map_err(|_| "The run MCP work context is not valid JSON")?;
            if answer.pointer("/result/isError") == Some(&Value::Bool(true)) {
                return Err(
                    match answer
                        .pointer("/result/structuredContent/code")
                        .and_then(Value::as_str)
                    {
                        Some("CURSOR_EXPIRED") => {
                            "The run inbox cursor expired while the backend was active".into()
                        }
                        _ => "The run MCP work context was refused".into(),
                    },
                );
            }
            let result = answer
                .pointer_mut("/result/structuredContent")
                .ok_or("The run MCP work context was refused")?
                .take();
            return serde_json::from_value(result).map_err(|_| {
                "The run MCP work context lacks its execution identity or inbox".into()
            });
        }
        if Instant::now() >= deadline {
            return Err("The mandatory run MCP service is unavailable".into());
        }
        sleep(Duration::from_millis(120)).await;
    }
}

#[tokio::main]
pub(super) async fn stdio(socket: &Path) -> Result<(), String> {
    let client = socket_client(socket)?;
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let mut version = "2025-11-25".to_owned();
    loop {
        let mut line = Vec::new();
        let count = (&mut stdin)
            .take(REQUEST_LIMIT as u64 + 1)
            .read_until(b'\n', &mut line)
            .map_err(|_| "Could not read the worker MCP request")?;
        if count == 0 {
            return Ok(());
        }
        if count > REQUEST_LIMIT {
            return Err("Worker MCP request exceeds the byte limit".into());
        }
        let request: Value =
            serde_json::from_slice(&line).map_err(|_| "Invalid worker MCP JSON-RPC request")?;
        let Some(method) = request.get("method").and_then(Value::as_str) else {
            return Err("Invalid worker MCP JSON-RPC request".into());
        };
        let mut outbound = client
            .post("http://localhost/mcp")
            .header(header::ACCEPT, ACCEPT)
            .json(&request);
        // The initial stdio request proposes its own version in the body.
        // The HTTP header belongs to subsequent requests after negotiation.
        if method != "initialize" {
            let requested_version = request
                .pointer("/params/_meta/protocolVersion")
                .and_then(Value::as_str)
                .filter(|value| valid_version(value))
                .unwrap_or(&version);
            outbound = outbound.header(MCP_PROTOCOL_VERSION, requested_version);
        }
        let response = outbound.send().await;
        let Some(id) = request.get("id") else {
            continue;
        };
        let answer = match response {
            Ok(response) if response.status().is_success() => {
                let body = bounded_response(response).await?;
                serde_json::from_slice::<Value>(&body)
                    .map_err(|_| "The local socket returned malformed MCP JSON")?
            }
            Ok(response) => json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,
                "message":format!("Run MCP returned HTTP {}", response.status().as_u16())}}),
            Err(_) => json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,
                "message":"Could not connect to the run's MCP socket"}}),
        };
        if method == "initialize"
            && let Some(protocol) = answer
                .pointer("/result/protocolVersion")
                .and_then(Value::as_str)
            && valid_version(protocol)
        {
            version = protocol.to_owned();
        }
        serde_json::to_writer(&mut stdout, &answer)
            .map_err(|_| "Could not encode the worker MCP response")?;
        stdout
            .write_all(b"\n")
            .and_then(|_| stdout.flush())
            .map_err(|_| "Could not write the worker MCP response")?;
    }
}

fn valid_version(value: &str) -> bool {
    value.len() == 10
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'-')
}

async fn bounded_response(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Could not read the run MCP response")?
    {
        if bytes.len() + chunk.len() > RESPONSE_LIMIT {
            return Err("Run MCP response exceeds the byte limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
