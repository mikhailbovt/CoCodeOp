use std::{path::PathBuf, process::ExitCode};

use clap::Args;
use office_domain::{RunnerId, WorkspaceId};
use office_protocol::{
    API_VERSION, AgentIdentity, DoctorReport, GetRunnerProfile, Health, Outcome, Readiness, Reply,
    RunnerProfile,
};
use reqwest::{Method, Url};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{identity, request};

const MCP_PROTOCOL: &str = "2025-11-25";
pub(crate) const MCP_ACCEPT: &str = "application/json, text/event-stream";

#[derive(Args)]
pub(crate) struct DoctorOptions {
    #[arg(long, default_value = "http://127.0.0.1:8400")]
    url: Url,
    /// Local agent credential file. Defaults to OFFICE_TOKEN.
    #[arg(long)]
    token_file: Option<PathBuf>,
    /// Expected actor for this client configuration.
    #[arg(long)]
    agent: Option<Uuid>,
    /// Also check that this owner's runner advertises active capacity and templates.
    #[arg(long)]
    runner: Option<Uuid>,
    #[arg(long)]
    json: bool,
}

pub(crate) async fn run(options: DoctorOptions) -> ExitCode {
    let mut report = DoctorReport::default();
    let url = &options.url;
    report.add("network", health(url).await);
    report.add("database", readiness(url).await);
    match authenticated_agent(&options, &mut report).await {
        Some((agent, token)) => match options.runner {
            Some(runner) => report.add(
                "runner",
                runner_profile(url, agent.workspace_id, runner, &token).await,
            ),
            None => report.skip("runner", "Pass --runner to check an advertised profile"),
        },
        None => report.skip("runner", "A verified MCP identity is required"),
    }
    let passed = report.passed();
    if options.json {
        println!(
            "{}",
            serde_json::to_string(&report).expect("doctor report is serializable")
        );
    } else {
        for check in report.checks {
            println!(
                "{:?} · {} · {}",
                check.status, check.component, check.detail
            );
        }
    }
    if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

async fn health(url: &Url) -> Result<String, String> {
    identity::require_confidential(url)?;
    let health: Health = public_result(url, "/healthz").await?;
    let connection = if url.scheme() == "https" {
        "verified HTTPS"
    } else {
        "loopback HTTP"
    };
    Ok(format!("CoCodeOp {} · {connection}", health.version))
}

async fn readiness(url: &Url) -> Result<String, String> {
    identity::require_confidential(url)?;
    let readiness: Readiness = public_result(url, "/readyz").await?;
    if readiness.api_version != API_VERSION || readiness.schema_version < 1 {
        return Err(format!(
            "Unsupported API {} or database schema {}",
            readiness.api_version, readiness.schema_version
        ));
    }
    Ok(format!(
        "API {} · schema {} · installation {}",
        readiness.api_version, readiness.schema_version, readiness.installation_id.0
    ))
}

async fn public_result<T: DeserializeOwned>(url: &Url, path: &str) -> Result<T, String> {
    let response = request(Method::GET, url.clone(), path)?
        .send()
        .await
        .map_err(|_| "Could not establish the server connection")?;
    let status = response.status();
    let body = response
        .json::<Reply<T>>()
        .await
        .map_err(|_| "Invalid CoCodeOp health response")?;
    match body.outcome {
        Outcome::Success { result } if status.is_success() => Ok(result),
        Outcome::Failure { error } => Err(error.message),
        _ => Err(format!("Unexpected HTTP {status}")),
    }
}

async fn authenticated_agent(
    options: &DoctorOptions,
    report: &mut DoctorReport,
) -> Option<(AgentIdentity, String)> {
    let token = match doctor_token(options) {
        Ok(token) => token,
        Err(error) => {
            report.add("mcp_auth", Err(error));
            report.skip("protocol", "A local agent credential is required");
            return None;
        }
    };
    if let Err(error) = negotiate(&options.url, &token).await {
        report.add("protocol", Err(error));
        report.skip(
            "mcp_auth",
            "Initialization must succeed before reading office_bootstrap",
        );
        return None;
    }
    report.add(
        "protocol",
        Ok(format!("MCP {MCP_PROTOCOL} · Streamable HTTP")),
    );
    let agent = bootstrap(&options.url, &token).await.and_then(|agent| {
        if options
            .agent
            .is_some_and(|expected| agent.agent_id.0 != expected)
        {
            return Err("office_bootstrap returned a different agent".into());
        }
        Ok(agent)
    });
    match agent {
        Ok(agent) => {
            report.add(
                "mcp_auth",
                Ok(format!(
                    "Agent {} · workspace {}",
                    agent.agent_id.0, agent.workspace_id.0
                )),
            );
            Some((agent, token))
        }
        Err(error) => {
            report.add("mcp_auth", Err(error));
            None
        }
    }
}

fn doctor_token(options: &DoctorOptions) -> Result<String, String> {
    identity::require_confidential(&options.url)?;
    let Some(path) = &options.token_file else {
        return identity::user_token();
    };
    identity::read_local_token(path)
}

pub(crate) async fn negotiate(url: &Url, token: &str) -> Result<(), String> {
    let result = mcp(
        url,
        token,
        "initialize",
        json!({
            "protocolVersion": MCP_PROTOCOL,
            "capabilities": {},
            "clientInfo": {"name": "cocodeop-doctor", "version": env!("CARGO_PKG_VERSION")},
        }),
    )
    .await?;
    if result["protocolVersion"].as_str() != Some(MCP_PROTOCOL)
        || !result["capabilities"]["tools"].is_object()
    {
        return Err(
            "The server did not negotiate the supported MCP protocol and tool capability".into(),
        );
    }
    let initialized = mcp_request(url, token)?
        .json(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .send()
        .await
        .map_err(|_| "Could not confirm MCP initialization")?;
    if !initialized.status().is_success() {
        return Err(format!(
            "MCP initialization acknowledgement returned HTTP {}",
            initialized.status()
        ));
    }
    Ok(())
}

pub(crate) async fn bootstrap(url: &Url, token: &str) -> Result<AgentIdentity, String> {
    let result = mcp(
        url,
        token,
        "tools/call",
        json!({
            "name": "office_bootstrap", "arguments": {},
        }),
    )
    .await?;
    if result["isError"].as_bool() == Some(true) {
        return Err("office_bootstrap denied this credential".into());
    }
    serde_json::from_value(result["structuredContent"].clone())
        .map_err(|_| "office_bootstrap did not return an agent identity".into())
}

fn mcp_request(url: &Url, token: &str) -> Result<reqwest::RequestBuilder, String> {
    Ok(request(Method::POST, url.clone(), "/mcp")?
        .header("Accept", MCP_ACCEPT)
        .header("MCP-Protocol-Version", MCP_PROTOCOL)
        .bearer_auth(token))
}

async fn mcp(url: &Url, token: &str, method: &str, params: Value) -> Result<Value, String> {
    let response = mcp_request(url, token)?
        .json(&json!({"jsonrpc":"2.0", "id": 1, "method": method, "params": params}))
        .send()
        .await
        .map_err(|_| "Could not contact the MCP endpoint")?;
    if !response.status().is_success() {
        return Err(format!("MCP {method} returned HTTP {}", response.status()));
    }
    let mut reply: Value = response.json().await.map_err(|_| "Invalid MCP response")?;
    if reply["id"].as_u64() != Some(1)
        || reply["jsonrpc"].as_str() != Some("2.0")
        || !reply["result"].is_object()
    {
        return Err(format!("MCP {method} returned no result"));
    }
    Ok(reply["result"].take())
}

async fn runner_profile(
    url: &Url,
    workspace_id: WorkspaceId,
    runner: Uuid,
    token: &str,
) -> Result<String, String> {
    let input = GetRunnerProfile {
        workspace_id,
        runner_id: RunnerId(runner),
    };
    let response = request(Method::POST, url.clone(), "/v1/runner-profiles/read")?
        .bearer_auth(token)
        .json(&input)
        .send()
        .await
        .map_err(|_| "Could not read the advertised runner profile")?;
    let status = response.status();
    let reply = response
        .json::<Reply<RunnerProfile>>()
        .await
        .map_err(|_| "Invalid runner profile response")?;
    let profile = match reply.outcome {
        Outcome::Success { result } if status.is_success() => result,
        Outcome::Failure { error } => return Err(error.message),
        _ => return Err(format!("Unexpected HTTP {status}")),
    };
    if profile.runner_id.0 != runner
        || profile.expires_at <= reply.timestamp
        || profile.capacity == 0
        || profile.runtime_template_ids.is_empty()
    {
        return Err("The runner advertises no active execution capacity and templates".into());
    }
    Ok(format!(
        "Runner {runner} · capacity {} · {} approved templates",
        profile.capacity,
        profile.runtime_template_ids.len()
    ))
}
