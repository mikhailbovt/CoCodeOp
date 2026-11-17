use reqwest::header::CONTENT_TYPE;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::{connection::Connection, doctor, server_client};

const MAX_FRAME: usize = 16 * 1024;
const MAX_REPLY: usize = 4 * 1024 * 1024;

pub(crate) async fn serve() -> Result<(), String> {
    let connection = Connection::read()?;
    let endpoint = connection.endpoint()?;
    let token = connection.bearer()?;
    doctor::negotiate(&endpoint, &token).await?;
    let agent = doctor::bootstrap(&endpoint, &token).await?;
    if agent.agent_id.0 != connection.agent_id() {
        return Err("The credential selects a different CoCodeOp agent. Run office connect".into());
    }
    let client = server_client()?;
    let mut input = BufReader::new(tokio::io::stdin());
    let mut output = tokio::io::stdout();
    let mut line = Vec::with_capacity(4096);
    let mut protocol_version = None;
    loop {
        line.clear();
        let count = (&mut input)
            .take((MAX_FRAME + 2) as u64)
            .read_until(b'\n', &mut line)
            .await
            .map_err(|_| "Could not read an MCP frame")?;
        if count == 0 {
            return Ok(());
        }
        if line.ends_with(b"\n") {
            line.pop();
        }
        if line.ends_with(b"\r") {
            line.pop();
        }
        if line.len() > MAX_FRAME {
            write_response(
                &mut output,
                &rpc_error(&Value::Null, -32600, "MCP frame exceeds the request limit"),
            )
            .await?;
            return Err("MCP input exceeds the CoCodeOp request limit".into());
        }
        let call: Value = match serde_json::from_slice(&line) {
            Ok(call) => call,
            Err(_) => {
                write_response(
                    &mut output,
                    &rpc_error(&Value::Null, -32700, "Invalid JSON"),
                )
                .await?;
                continue;
            }
        };
        let Some(method) = call["method"].as_str().filter(|_| call["jsonrpc"] == "2.0") else {
            write_response(
                &mut output,
                &rpc_error(&Value::Null, -32600, "Invalid MCP request"),
            )
            .await?;
            continue;
        };
        let id = call.get("id");
        if id.is_some_and(|id| !id.is_string() && !id.is_i64() && !id.is_u64()) {
            write_response(
                &mut output,
                &rpc_error(&Value::Null, -32600, "Invalid MCP request ID"),
            )
            .await?;
            continue;
        }
        let mut request = client
            .post(endpoint.clone())
            .bearer_auth(&token)
            .header("Accept", doctor::MCP_ACCEPT)
            .header(CONTENT_TYPE, "application/json")
            .body(line.clone());
        if let Some(version) = &protocol_version {
            request = request.header("MCP-Protocol-Version", version);
        }
        match forward(request, id).await {
            Ok(Some(result)) => {
                if method == "initialize" {
                    protocol_version = result["result"]["protocolVersion"]
                        .as_str()
                        .and_then(|value| reqwest::header::HeaderValue::from_str(value).ok());
                }
                write_response(&mut output, &result).await?;
            }
            Ok(None) => {}
            Err(problem) => match id {
                Some(id) => write_response(&mut output, &rpc_error(id, -32001, &problem)).await?,
                None => eprintln!("{problem}"),
            },
        }
    }
}

async fn forward(
    request: reqwest::RequestBuilder,
    id: Option<&Value>,
) -> Result<Option<Value>, String> {
    let mut response = request
        .send()
        .await
        .map_err(|_| "Could not reach CoCodeOp MCP")?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("CoCodeOp MCP returned HTTP {status}"));
    }
    let Some(id) = id else {
        return Ok(None);
    };
    let json_response = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|header| header.to_str().ok())
        .is_some_and(|mime| mime.split(';').next() == Some("application/json"));
    if !json_response
        || response
            .content_length()
            .is_some_and(|size| size > MAX_REPLY as u64)
    {
        return Err("Unexpected CoCodeOp MCP response format or size".into());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Incomplete MCP response")?
    {
        if chunk.len() > MAX_REPLY - body.len() {
            return Err("CoCodeOp MCP response exceeds 4 MiB".into());
        }
        body.extend_from_slice(&chunk);
    }
    let result: Value =
        serde_json::from_slice(&body).map_err(|_| "Invalid CoCodeOp MCP response")?;
    if &result["id"] != id
        || result["jsonrpc"] != "2.0"
        || result.get("result").is_some() == result.get("error").is_some()
    {
        return Err("Unexpected CoCodeOp MCP response envelope".into());
    }
    Ok(Some(result))
}

async fn write_response(output: &mut tokio::io::Stdout, response: &Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(response).expect("valid JSON response");
    bytes.push(b'\n');
    output
        .write_all(&bytes)
        .await
        .map_err(|_| "Could not write the MCP response")?;
    output
        .flush()
        .await
        .map_err(|_| "Could not flush the MCP response".into())
}

fn rpc_error(id: &Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0", "id": id, "error": {"code": code, "message": message}})
}
