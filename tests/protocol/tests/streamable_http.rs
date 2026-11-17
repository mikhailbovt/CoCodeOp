use std::{
    fs,
    io::Write,
    net::{Ipv4Addr, SocketAddr},
    path::Path,
    sync::Arc,
    time::Duration,
};

use reqwest::{Client, Response, StatusCode, header};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::{NamedTempFile, TempDir};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

const TOKEN_A: &str = "53eb602f3ff6093e516da7c0f9669c9fe76f1e44fe4edb349b3283dcc180f237";
const TOKEN_B: &str = "4cd98ac178ff03d8e7beefd71883dd11ba39172fd880c91cb5c472c2fb2b7b26";
const INVALID_TOKEN: &str = "7e54e6b103e067cbf82e8f03208cf0c73d0ef285bb5078986a7a7dd2b8144d76";
const LEGACY: &str = "2025-11-25";
const CURRENT: &str = "2026-07-28";

struct Server {
    stop: CancellationToken,
    task: JoinHandle<std::io::Result<()>>,
    address: SocketAddr,
    checkout: Arc<TempDir>,
}

impl Server {
    async fn start(credentials: &Path, port: u16) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let stop = CancellationToken::new();
        let checkout = Arc::new(tempfile::tempdir().unwrap());
        let router = office_compat_probe::router(
            credentials.to_owned(),
            checkout.clone(),
            address,
            stop.clone(),
        )
        .unwrap();
        let shutdown = stop.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
        });
        Self {
            stop,
            task,
            address,
            checkout,
        }
    }

    async fn close(self) {
        self.stop.cancel();
        tokio::time::timeout(Duration::from_secs(8), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

fn write_credentials(dir: &Path, a: Option<&str>, b: Option<&str>) {
    let hash = |token: &str| hex::encode(Sha256::digest(token.as_bytes()));
    let mut staged = NamedTempFile::new_in(dir).unwrap();
    write!(
        staged,
        "{}",
        json!({"lead_a": a.map(hash), "lead_b": b.map(hash)})
    )
    .unwrap();
    staged.persist(dir.join("credentials.json")).unwrap();
}

struct ClientAt {
    http: Client,
    url: String,
    token: &'static str,
}

impl ClientAt {
    fn new(address: SocketAddr, token: &'static str) -> Self {
        Self {
            http: Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(8))
                .build()
                .unwrap(),
            url: format!("http://{address}/mcp"),
            token,
        }
    }

    fn request(&self) -> reqwest::RequestBuilder {
        self.http
            .post(&self.url)
            .bearer_auth(self.token)
            .header(header::ACCEPT, "application/json, text/event-stream")
    }

    async fn post(&self, protocol: &str, method: &str, mut params: Value) -> Response {
        let mut request = self.request().header("mcp-protocol-version", protocol);
        if protocol == CURRENT {
            request = request.header("mcp-method", method);
            let identifier = if method == "resources/read" {
                "uri"
            } else {
                "name"
            };
            if let Some(name) = params.get(identifier).and_then(Value::as_str) {
                request = request.header("mcp-name", name);
            }
            params["_meta"] = json!({
                "io.modelcontextprotocol/protocolVersion": CURRENT,
                "io.modelcontextprotocol/clientInfo": {"name": "probe-wire-test", "version": "1"},
                "io.modelcontextprotocol/clientCapabilities": {},
            });
        }
        request.json(&rpc(method, params)).send().await.unwrap()
    }

    async fn call(&self, protocol: &str, name: &str, arguments: Value) -> Value {
        result(
            self.post(
                protocol,
                "tools/call",
                json!({"name": name, "arguments": arguments}),
            )
            .await,
        )
        .await
    }

    async fn initialize(&self) {
        let response = self
            .post(
                LEGACY,
                "initialize",
                json!({
                    "protocolVersion": LEGACY,
                    "clientInfo": {"name": "lead-b", "version": "1"},
                    "capabilities": {},
                }),
            )
            .await;
        assert!(!response.headers().contains_key("mcp-session-id"));
        assert_eq!(result(response).await["protocolVersion"], LEGACY);
    }
}

fn rpc(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
}

async fn result(response: Response) -> Value {
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["id"], 1, "{value}");
    assert!(value.get("result").is_some(), "{value}");
    value["result"].clone()
}

#[tokio::test]
async fn simultaneous_credentials_revoke_and_reconnect_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = dir.path().join("credentials.json");
    write_credentials(dir.path(), Some(TOKEN_A), Some(TOKEN_B));
    let server = Server::start(&credentials, 0).await;
    let a = ClientAt::new(server.address, TOKEN_A);
    let b = ClientAt::new(server.address, TOKEN_B);

    tokio::join!(a.initialize(), b.initialize());
    let (aa, bb) = tokio::join!(
        a.call(LEGACY, "whoami", json!({})),
        b.call(LEGACY, "whoami", json!({})),
    );
    let binding = |value: Value| value["structuredContent"]["actor_id"].clone();
    assert_eq!(binding(aa), "lead-a");
    assert_eq!(binding(bb), "lead-b");
    let invalid = ClientAt::new(server.address, INVALID_TOKEN);
    let response = invalid.post(LEGACY, "tools/list", json!({})).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(response.headers()[header::WWW_AUTHENTICATE], "Bearer");
    let response = a
        .http
        .post(&a.url)
        .json(&rpc("ping", json!({})))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    write_credentials(dir.path(), None, Some(TOKEN_B));
    let revoked = a
        .post(
            LEGACY,
            "resources/read",
            json!({"uri": "office://probe/identity"}),
        )
        .await;
    assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(binding(b.call(LEGACY, "whoami", json!({})).await), "lead-b");

    let port = server.address.port();
    server.close().await;
    write_credentials(dir.path(), Some(TOKEN_A), Some(TOKEN_B));
    let restarted = Server::start(&credentials, port).await;
    let (aa, bb) = tokio::join!(
        a.call(LEGACY, "whoami", json!({})),
        b.call(LEGACY, "whoami", json!({})),
    );
    assert_eq!(binding(aa), "lead-a");
    assert_eq!(binding(bb), "lead-b");
    restarted.close().await;
}

#[tokio::test]
async fn current_protocol_discovery_schemas_and_resources() {
    let dir = tempfile::tempdir().unwrap();
    write_credentials(dir.path(), Some(TOKEN_A), Some(TOKEN_B));
    let server = Server::start(&dir.path().join("credentials.json"), 0).await;
    let a = ClientAt::new(server.address, TOKEN_A);
    let discovery = result(a.post(CURRENT, "server/discover", json!({})).await).await;
    assert!(
        discovery["supportedVersions"]
            .as_array()
            .unwrap()
            .contains(&json!(CURRENT))
    );
    assert!(discovery["capabilities"].get("tools").is_some());
    let listed = result(a.post(CURRENT, "tools/list", json!({})).await).await;
    let tools = listed["tools"].as_array().unwrap();
    let echo = tools.iter().find(|tool| tool["name"] == "echo").unwrap();
    assert_eq!(echo["inputSchema"]["required"], json!(["text"]));
    assert_eq!(echo["inputSchema"]["additionalProperties"], false);
    assert_eq!(echo["annotations"]["readOnlyHint"], true);
    let create = tools
        .iter()
        .find(|tool| tool["name"] == "probe_create_file")
        .unwrap();
    assert_eq!(create["annotations"]["readOnlyHint"], false);
    assert_eq!(create["annotations"]["destructiveHint"], false);
    let echoed = a.call(CURRENT, "echo", json!({"text": "CoCodeOp"})).await;
    assert_eq!(
        echoed["structuredContent"],
        json!({"actor_id": "lead-a", "text": "CoCodeOp"})
    );

    let listed = result(a.post(CURRENT, "resources/list", json!({})).await).await;
    let uri = &listed["resources"][0]["uri"];
    assert_eq!(uri, "office://probe/identity");
    let identity = result(a.post(CURRENT, "resources/read", json!({"uri": uri})).await).await;
    let content: Value =
        serde_json::from_str(identity["contents"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(content["actor_id"], "lead-a");
    server.close().await;
}

#[tokio::test]
async fn file_creation_is_bound_to_actor_and_never_clobbers() {
    let dir = tempfile::tempdir().unwrap();
    write_credentials(dir.path(), Some(TOKEN_A), Some(TOKEN_B));
    let server = Server::start(&dir.path().join("credentials.json"), 0).await;
    let a = ClientAt::new(server.address, TOKEN_A);
    let b = ClientAt::new(server.address, TOKEN_B);
    let (aa, bb) = tokio::join!(
        a.call(LEGACY, "probe_create_file", json!({"text": "first"})),
        b.call(LEGACY, "probe_create_file", json!({"text": "second"})),
    );
    assert_eq!(aa["structuredContent"]["path"], "lead-a.txt");
    assert_eq!(bb["structuredContent"]["path"], "lead-b.txt");
    let conflict = a
        .call(LEGACY, "probe_create_file", json!({"text": "overwrite"}))
        .await;
    assert_eq!(conflict["isError"], true);
    assert_eq!(conflict["structuredContent"]["code"], "CONTENT_CONFLICT");
    let bad = a.post(LEGACY, "tools/call", json!({"name": "probe_create_file", "arguments": {"text": "spoof", "actor_id": "lead-b"}})).await;
    let bad: Value = bad.json().await.unwrap();
    assert!(
        bad.get("error").is_some() || bad["result"]["isError"] == true,
        "{bad}"
    );

    let root = server.checkout.path();
    assert_eq!(
        fs::read_to_string(root.join("lead-a.txt")).unwrap(),
        "first"
    );
    assert_eq!(
        fs::read_to_string(root.join("lead-b.txt")).unwrap(),
        "second"
    );
    assert_eq!(fs::read_dir(root).unwrap().count(), 2);
    server.close().await;
}

#[tokio::test]
async fn rejects_untrusted_web_origin_oversized_body_and_invalid_credential_store() {
    let dir = tempfile::tempdir().unwrap();
    write_credentials(dir.path(), Some(TOKEN_A), Some(TOKEN_B));
    let credentials = dir.path().join("credentials.json");
    let server = Server::start(&credentials, 0).await;
    let a = ClientAt::new(server.address, TOKEN_A);
    let response = a
        .request()
        .header(header::ORIGIN, "https://untrusted.invalid")
        .json(&rpc("ping", json!({})))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = a
        .request()
        .header(header::HOST, "untrusted.invalid")
        .json(&rpc("ping", json!({})))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = a
        .post(
            LEGACY,
            "tools/call",
            json!({"name": "echo", "arguments": {"text": "x".repeat(16 * 1024)}}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let response = a.post("9999-01-01", "ping", json!({})).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    write_credentials(dir.path(), Some(TOKEN_A), Some(TOKEN_A));
    let response = a.post(LEGACY, "tools/list", json!({})).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    fs::remove_file(&credentials).unwrap();
    let response = a.post(LEGACY, "tools/list", json!({})).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    server.close().await;
}

#[tokio::test]
async fn limits_time_spent_receiving_a_request_body() {
    let dir = tempfile::tempdir().unwrap();
    write_credentials(dir.path(), Some(TOKEN_A), None);
    let server = Server::start(&dir.path().join("credentials.json"), 0).await;
    let mut stream = TcpStream::connect(server.address).await.unwrap();
    let partial = format!(
        "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {TOKEN_A}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: 1024\r\nConnection: close\r\n\r\n{{",
        server.address,
    );
    stream.write_all(partial.as_bytes()).await.unwrap();
    let mut response = [0; 512];
    let size = tokio::time::timeout(Duration::from_secs(8), stream.read(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&response[..size]).starts_with("HTTP/1.1 504"));
    drop(stream);
    server.close().await;
}
