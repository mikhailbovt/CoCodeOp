use std::{
    fs::File,
    io::{self, Read},
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU16, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, State},
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{StreamExt, stream};
use office_protocol::{BackendCost, BackendUsage, valid_backend_id, valid_capabilities};
use reqwest::{Client, Url, redirect::Policy};
use rustix::{
    fs::{self, FileType, Mode, OFlags},
    process::geteuid,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sse_stream::SseStream;
use tokio::{
    net::{TcpListener, UnixListener},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::transport::{confidential_url, endpoint};

mod tool_aliases;
use tool_aliases::ToolAliases;

const REQUEST_BYTES: usize = 256 * 1024;
const RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MODEL_CATALOG_BYTES: usize = 512 * 1024;

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ProviderApi {
    ChatCompletions,
    Responses,
}

impl ProviderApi {
    fn path(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat/completions",
            Self::Responses => "responses",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProviderProfile {
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub api: ProviderApi,
    pub max_requests: u16,
    pub max_output_tokens: u32,
    pub timeout_seconds: u16,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub flatten_response_tools: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<ProviderPricing>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProviderPricing {
    revision: String,
    /// USD micros per million tokens.
    input: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
}

impl ProviderPricing {
    fn valid(&self) -> bool {
        valid_backend_id(&self.revision)
            && [self.input, self.cache_read, self.cache_write, self.output]
                .into_iter()
                .all(|rate| rate <= 1_000_000_000_000)
    }

    pub fn estimate(&self, usage: BackendUsage) -> Option<BackendCost> {
        let uncached = usage
            .input_tokens
            .checked_sub(usage.cached_input_tokens)?
            .checked_sub(usage.cache_write_input_tokens)?;
        let numerator = u128::from(uncached) * u128::from(self.input)
            + u128::from(usage.cached_input_tokens) * u128::from(self.cache_read)
            + u128::from(usage.cache_write_input_tokens) * u128::from(self.cache_write)
            + u128::from(usage.output_tokens) * u128::from(self.output);
        let cost = BackendCost::Estimated {
            usd_micros: numerator.div_ceil(1_000_000).try_into().ok()?,
            pricing_revision: self.revision.clone(),
        };
        cost.valid().then_some(cost)
    }
}

#[cfg(test)]
mod pricing_tests {
    use super::*;

    #[test]
    fn estimate_charges_each_token_class_once() {
        let pricing = ProviderPricing {
            revision: "owner-model-2026-11".into(),
            input: 2_000_000,
            cache_read: 250_000,
            cache_write: 3_000_000,
            output: 8_000_000,
        };
        let usage = BackendUsage {
            input_tokens: 101,
            output_tokens: 5,
            cached_input_tokens: 31,
            cache_write_input_tokens: 10,
            reasoning_output_tokens: 3,
        };
        assert_eq!(
            pricing.estimate(usage),
            Some(BackendCost::Estimated {
                usd_micros: 198,
                pricing_revision: pricing.revision.clone(),
            })
        );
        assert_eq!(
            pricing.estimate(BackendUsage {
                input_tokens: 39,
                ..usage
            }),
            None
        );
    }
}

impl ProviderProfile {
    pub fn validate(&self) -> Result<Url, String> {
        if !valid_capabilities(std::slice::from_ref(&self.name), 1)
            || !(1..=192).contains(&self.model.len())
            || self.model.chars().any(char::is_control)
            || !(1..=128).contains(&self.max_requests)
            || !(64..=32_768).contains(&self.max_output_tokens)
            || !(5..=600).contains(&self.timeout_seconds)
            || (self.flatten_response_tools && !matches!(self.api, ProviderApi::Responses))
            || self
                .pricing
                .as_ref()
                .is_some_and(|pricing| !pricing.valid())
        {
            return Err(
                "Invalid inference profile. Configure an exact model, protocol and bounded limits"
                    .into(),
            );
        }
        confidential_url(&self.base_url)
    }

    pub fn digest(&self) -> Result<String, String> {
        self.validate()?;
        let bytes =
            serde_json::to_vec(self).map_err(|_| "Could not encode the inference profile")?;
        let mut hash = Sha256::new();
        hash.update(b"cocodeop/inference/v1\0");
        hash.update(bytes);
        Ok(hex::encode(hash.finalize()))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProviderInstallation {
    pub profile: ProviderProfile,
    pub bearer_file: Option<PathBuf>,
}

pub(super) struct ProviderSocket {
    listener: UnixListener,
    upstream: Arc<Upstream>,
}

struct Upstream {
    profile: ProviderProfile,
    base: Url,
    client: Client,
    bearer: Option<String>,
    remaining: AtomicU16,
    closed: CancellationToken,
}

pub(super) struct InferenceListener {
    task: JoinHandle<()>,
    closed: CancellationToken,
}

impl Drop for InferenceListener {
    fn drop(&mut self) {
        self.closed.cancel();
        self.task.abort();
    }
}

impl ProviderSocket {
    pub fn bind(path: &Path, installation: &ProviderInstallation) -> Result<Self, String> {
        let profile = installation.profile.clone();
        let base = profile.validate()?;
        let bearer = installation
            .bearer_file
            .as_deref()
            .map(provider_secret)
            .transpose()?;
        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(u64::from(profile.timeout_seconds)))
            .build()
            .map_err(|_| "Could not configure the inference HTTP client")?;
        let listener = UnixListener::bind(path)
            .map_err(|_| "Could not bind the run's private inference socket")?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| "Could not protect the run's inference socket")?;
        Ok(Self {
            listener,
            upstream: Arc::new(Upstream {
                base,
                client,
                bearer,
                remaining: AtomicU16::new(profile.max_requests),
                profile,
                closed: CancellationToken::new(),
            }),
        })
    }

    pub fn start(self) -> InferenceListener {
        let closed = self.upstream.closed.clone();
        let router = Router::new()
            .route("/v1/models", get(models))
            .route("/v1/chat/completions", post(inference))
            .route("/v1/responses", post(inference))
            .layer(DefaultBodyLimit::max(REQUEST_BYTES))
            .with_state(self.upstream);
        let task = tokio::spawn(async move {
            if axum::serve(self.listener, router).await.is_err() {
                eprintln!("The run's private inference socket stopped");
            }
        });
        InferenceListener { task, closed }
    }
}

async fn inference(State(upstream): State<Arc<Upstream>>, uri: Uri, body: Bytes) -> Response {
    if uri.path().strip_prefix("/v1/") != Some(upstream.profile.api.path()) {
        return failure(
            StatusCode::NOT_FOUND,
            "This route is outside the installed inference profile",
        );
    }
    let Ok(Value::Object(mut request)) = serde_json::from_slice(&body) else {
        return failure(StatusCode::BAD_REQUEST, "Inference requires a JSON object");
    };
    if !validate_request(&upstream.profile, &mut request) {
        return failure(
            StatusCode::BAD_REQUEST,
            "The inference request exceeds the installed profile",
        );
    }
    let Some(tool_aliases) = ToolAliases::prepare(
        upstream.profile.api,
        upstream.profile.flatten_response_tools,
        &mut request,
    ) else {
        return failure(
            StatusCode::BAD_REQUEST,
            "The inference profile accepts bounded client-executed tools and namespaces",
        );
    };
    if !upstream.take_request() {
        return failure(
            StatusCode::TOO_MANY_REQUESTS,
            "The run used its inference request budget",
        );
    }
    let Ok(url) = endpoint(&upstream.base, upstream.profile.api.path()) else {
        return failure(
            StatusCode::BAD_GATEWAY,
            "The configured inference endpoint is unavailable",
        );
    };
    let request = upstream.with_key(upstream.client.post(url).json(&request));
    match upstream.send(request).await {
        Ok(response) => relay_response(response, upstream.closed.clone(), tool_aliases).await,
        Err(status) => failure(
            status,
            match status {
                StatusCode::GATEWAY_TIMEOUT => "The configured inference provider timed out",
                StatusCode::TOO_MANY_REQUESTS => {
                    "The configured inference provider reported a rate limit"
                }
                StatusCode::BAD_REQUEST => {
                    "The configured inference provider rejected the request format"
                }
                _ => "The configured inference provider could not accept this request",
            },
        ),
    }
}

fn validate_request(profile: &ProviderProfile, request: &mut Map<String, Value>) -> bool {
    if request.get("model").and_then(Value::as_str) != Some(profile.model.as_str()) {
        return false;
    }
    let limit_fields: &[&str] = match profile.api {
        ProviderApi::ChatCompletions => &["max_tokens", "max_completion_tokens"],
        ProviderApi::Responses => {
            if ["conversation", "previous_response_id"]
                .iter()
                .any(|key| request.get(*key).is_some_and(|value| !value.is_null()))
            {
                return false;
            }
            request.insert("store".into(), Value::Bool(false));
            &["max_output_tokens"]
        }
    };
    let supplied: Vec<_> = limit_fields
        .iter()
        .filter_map(|key| request.get(*key))
        .filter(|value| !value.is_null())
        .collect();
    if supplied.len() > 1
        || supplied.iter().any(|value| {
            value
                .as_u64()
                .is_none_or(|amount| amount == 0 || amount > u64::from(profile.max_output_tokens))
        })
    {
        return false;
    }
    if supplied.is_empty() {
        request.insert(limit_fields[0].into(), json!(profile.max_output_tokens));
    }
    true
}

async fn models(State(upstream): State<Arc<Upstream>>) -> Response {
    if !upstream.take_request() {
        return failure(
            StatusCode::TOO_MANY_REQUESTS,
            "The run used its inference request budget",
        );
    }
    let Ok(url) = endpoint(&upstream.base, "models") else {
        return failure(
            StatusCode::BAD_GATEWAY,
            "The provider catalog is unavailable",
        );
    };
    let Ok(response) = upstream
        .send(upstream.with_key(upstream.client.get(url)))
        .await
    else {
        return failure(
            StatusCode::BAD_GATEWAY,
            "The provider catalog is unavailable",
        );
    };
    let Ok(bytes) = read_limited(response, &upstream.closed, MODEL_CATALOG_BYTES).await else {
        return failure(
            StatusCode::BAD_GATEWAY,
            "The provider catalog exceeded its limit",
        );
    };
    let Ok(mut body) = serde_json::from_slice::<Value>(&bytes) else {
        return failure(StatusCode::BAD_GATEWAY, "The provider catalog is invalid");
    };
    let Some(rows) = body.get_mut("data").and_then(Value::as_array_mut) else {
        return failure(StatusCode::BAD_GATEWAY, "The provider catalog is invalid");
    };
    rows.retain(|model| {
        model.get("id").and_then(Value::as_str) == Some(upstream.profile.model.as_str())
    });
    axum::Json(body).into_response()
}

impl Upstream {
    fn take_request(&self) -> bool {
        self.remaining
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_sub(1)
            })
            .is_ok()
    }

    fn with_key(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.bearer {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, StatusCode> {
        let response = tokio::select! {
            _ = self.closed.cancelled() => return Err(StatusCode::SERVICE_UNAVAILABLE),
            result = request.send() => result.map_err(|error| {
                if error.is_timeout() {
                    StatusCode::GATEWAY_TIMEOUT
                } else {
                    StatusCode::BAD_GATEWAY
                }
            })?,
        };
        if response.status().is_success() {
            return Ok(response);
        }
        let upstream = response.status();
        eprintln!("Inference provider returned HTTP {}", upstream.as_u16());
        Err(match upstream {
            StatusCode::TOO_MANY_REQUESTS => StatusCode::TOO_MANY_REQUESTS,
            StatusCode::BAD_REQUEST => StatusCode::BAD_REQUEST,
            StatusCode::GATEWAY_TIMEOUT => StatusCode::GATEWAY_TIMEOUT,
            _ => StatusCode::BAD_GATEWAY,
        })
    }
}

async fn relay_response(
    response: reqwest::Response,
    closed: CancellationToken,
    tool_aliases: ToolAliases,
) -> Response {
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    let content_type = match content_type {
        Some(value) if value.starts_with("text/event-stream") => "text/event-stream",
        Some(value) if value.starts_with("application/json") => "application/json",
        _ => {
            return failure(
                StatusCode::BAD_GATEWAY,
                "The inference provider returned an unsupported response",
            );
        }
    };
    if content_type == "application/json" && !tool_aliases.is_empty() {
        let decoded = match read_limited(response, &closed, RESPONSE_BYTES).await {
            Ok(bytes) => serde_json::from_slice::<Value>(&bytes).ok(),
            Err(_) => None,
        };
        let Some(mut decoded) = decoded else {
            return failure(
                StatusCode::BAD_GATEWAY,
                "The provider returned an invalid JSON response",
            );
        };
        tool_aliases.restore_response(&mut decoded);
        return ([(header::CACHE_CONTROL, "no-store")], axum::Json(decoded)).into_response();
    }
    let bytes = stream::unfold(
        (Some(response), RESPONSE_BYTES, closed),
        |(response, remaining, closed)| async move {
            let mut response = response?;
            let chunk = tokio::select! {
                _ = closed.cancelled() => return None,
                result = response.chunk() => result,
            };
            match chunk {
                Ok(Some(bytes)) if bytes.len() <= remaining => {
                    let next = remaining - bytes.len();
                    Some((Ok::<_, io::Error>(bytes), (Some(response), next, closed)))
                }
                Ok(None) => None,
                _ => Some((
                    Err(io::Error::other(
                        "Inference stream interrupted or exceeded its limit",
                    )),
                    (None, 0, closed),
                )),
            }
        },
    );
    let body = if tool_aliases.is_empty() {
        Body::from_stream(bytes)
    } else {
        let frames = SseStream::from_bytes_stream(bytes).map(move |frame| {
            let mut event = frame.map_err(io::Error::other)?;
            if let Some(data) = event.data.as_mut()
                && data != "[DONE]"
            {
                let mut json: Value = serde_json::from_str(data).map_err(io::Error::other)?;
                tool_aliases.restore_response(&mut json);
                *data = serde_json::to_string(&json).map_err(io::Error::other)?;
            }
            Ok::<Bytes, io::Error>(event.into())
        });
        Body::from_stream(frames)
    };
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

async fn read_limited(
    mut response: reqwest::Response,
    closed: &CancellationToken,
    limit: usize,
) -> Result<Vec<u8>, ()> {
    let mut bytes = Vec::new();
    loop {
        let next = tokio::select! {
            _ = closed.cancelled() => return Err(()),
            result = response.chunk() => result.map_err(|_| ())?,
        };
        let Some(chunk) = next else {
            return Ok(bytes);
        };
        if bytes.len() + chunk.len() > limit {
            return Err(());
        }
        bytes.extend_from_slice(&chunk);
    }
}

fn failure(status: StatusCode, message: &'static str) -> Response {
    (
        status,
        axum::Json(json!({"error":{"type":"cocodeop_inference","message":message}})),
    )
        .into_response()
}

pub(super) fn provider_secret(path: &Path) -> Result<String, String> {
    let fd = fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|_| "Could not open the inference credential file")?;
    let stat = fs::fstat(&fd).map_err(|_| "Could not inspect the inference credential file")?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_nlink != 1
        || stat.st_uid != geteuid().as_raw()
        || stat.st_mode & 0o077 != 0
    {
        return Err("The inference credential must be in an owner-private regular file".into());
    }
    let mut value = String::new();
    File::from(fd)
        .take(4097)
        .read_to_string(&mut value)
        .map_err(|_| "Could not read inference credential")?;
    if value.len() > 4096 {
        return Err("The inference credential is too large".into());
    }
    let token = value.trim();
    if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err("Invalid inference credential format".into());
    }
    Ok(token.to_owned())
}

#[tokio::main]
pub(super) async fn worker_http(socket: &Path, listen: SocketAddr) -> Result<(), String> {
    if !listen.ip().is_loopback() {
        return Err("Bind the worker inference bridge only to loopback".into());
    }
    let listener = TcpListener::bind(listen)
        .await
        .map_err(|_| "Could not bind the inference loopback bridge")?;
    axum::serve(listener, worker_router(socket)?)
        .await
        .map_err(|_| "The worker inference bridge stopped".into())
}

pub(super) async fn start_worker_http(
    socket: &Path,
) -> Result<(String, InferenceListener), String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| "Could not bind the agent's inference loopback bridge")?;
    let address = listener
        .local_addr()
        .map_err(|_| "Could not read the agent's inference loopback address")?;
    let routes = worker_router(socket)?;
    let task = tokio::spawn(async move {
        if axum::serve(listener, routes).await.is_err() {
            eprintln!("The agent's inference loopback bridge stopped");
        }
    });
    let handle = InferenceListener {
        task,
        closed: CancellationToken::new(),
    };
    Ok((format!("http://{address}/v1"), handle))
}

fn worker_router(socket: &Path) -> Result<Router, String> {
    let client = Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .tls_certs_only([])
        .unix_socket(socket.to_owned())
        .timeout(Duration::from_secs(620))
        .build()
        .map_err(|_| "Could not configure the worker inference bridge")?;
    Ok(Router::new()
        .route("/v1/models", get(worker_models))
        .route("/v1/chat/completions", post(worker_inference))
        .route("/v1/responses", post(worker_inference))
        .layer(DefaultBodyLimit::max(REQUEST_BYTES))
        .with_state(client))
}

async fn worker_models(State(client): State<Client>) -> Response {
    from_socket(client.get("http://localhost/v1/models")).await
}

async fn worker_inference(State(client): State<Client>, uri: Uri, body: Bytes) -> Response {
    from_socket(
        client
            .post(format!("http://localhost{}", uri.path()))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body),
    )
    .await
}

async fn from_socket(request: reqwest::RequestBuilder) -> Response {
    match request.send().await {
        Ok(response) if response.status().is_success() => {
            relay_response(response, CancellationToken::new(), ToolAliases::default()).await
        }
        Ok(response) => {
            let status = response.status();
            if let Ok(body) = read_limited(response, &CancellationToken::new(), 4096).await
                && serde_json::from_slice::<Value>(&body)
                    .ok()
                    .is_some_and(|value| {
                        value.pointer("/error/type").and_then(Value::as_str)
                            == Some("cocodeop_inference")
                    })
            {
                return (status, [(header::CONTENT_TYPE, "application/json")], body)
                    .into_response();
            }
            failure(status, "The run's inference profile refused the request")
        }
        Err(_) => failure(
            StatusCode::BAD_GATEWAY,
            "The run's inference socket is unavailable",
        ),
    }
}
