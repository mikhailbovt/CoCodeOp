use std::{collections::BTreeSet, path::Path, process::Stdio};

use office_protocol::{
    BackendEventKind as Event, BackendUsage, RunInputIntent, WorkerInstructions,
};
use serde_json::{Value, json};
use tokio::process::Command;
use uuid::Uuid;

use crate::WorkerBackend;

use super::{MCP_NAME, RUNNER, open_private};

const CODEX: &str = "/opt/cocodeop/codex/bin/codex";
const OPENCODE: &str = "/opt/cocodeop/opencode";

pub(crate) fn backend_executable(backend: &str) -> Option<&'static str> {
    match backend {
        "codex" => Some(CODEX),
        "opencode" => Some(OPENCODE),
        _ => None,
    }
}

pub(super) struct Model<'a> {
    pub name: &'a str,
    pub url: &'a str,
    pub api: &'a str,
    pub output_limit: u32,
}

pub(super) fn command(
    backend: WorkerBackend,
    model: Model<'_>,
    root: &Path,
    prompt: &str,
    instructions: Option<&WorkerInstructions>,
) -> Result<Command, String> {
    match backend {
        WorkerBackend::Codex if model.api == "responses" => codex(model, root, instructions),
        WorkerBackend::Opencode if model.api == "chat_completions" => {
            opencode(model, root, prompt, instructions)
        }
        _ => Err("Install a provider profile with the API required by this backend".into()),
    }
}

fn context_window(output_limit: u32) -> u32 {
    16_384.max(output_limit + 8192)
}

fn codex(
    model: Model<'_>,
    root: &Path,
    instructions: Option<&WorkerInstructions>,
) -> Result<Command, String> {
    let mut cmd = Command::new(CODEX);
    cmd.env_clear()
        .env("CODEX_HOME", root.join("config"))
        .args([
            "--ask-for-approval",
            "never",
            "exec",
            "--json",
            "--ephemeral",
            "--sandbox",
            "read-only",
            "--ignore-user-config",
            "--ignore-rules",
            "--skip-git-repo-check",
            "--model",
            model.name,
        ]);
    let office = format!("mcp_servers.{MCP_NAME}");
    for (key, value) in [
        ("model_provider", json!("cocodeop")),
        ("model_providers.cocodeop.name", json!("CoCodeOp")),
        ("model_providers.cocodeop.base_url", json!(model.url)),
        ("model_providers.cocodeop.wire_api", json!("responses")),
        ("model_providers.cocodeop.request_max_retries", json!(0)),
        ("model_providers.cocodeop.stream_max_retries", json!(0)),
        (
            "model_context_window",
            json!(context_window(model.output_limit)),
        ),
        ("model_reasoning_effort", json!("low")),
        ("check_for_update_on_startup", json!(false)),
        ("web_search", json!("disabled")),
        ("features.shell_tool", json!(false)),
        ("features.shell_snapshot", json!(false)),
        ("features.apps", json!(false)),
        ("features.goals", json!(false)),
        ("features.plugins", json!(false)),
        ("agents.enabled", json!(false)),
        (&format!("{office}.command"), json!(RUNNER)),
        (&format!("{office}.args"), json!(["worker-mcp"])),
        (&format!("{office}.required"), json!(true)),
        (
            &format!("{office}.default_tools_approval_mode"),
            json!("approve"),
        ),
    ] {
        // These scalar JSON strings and arrays are valid TOML override values.
        cmd.arg("-c").arg(format!("{key}={value}"));
    }
    if let Some(profile) = instructions {
        let value = json!(personal_instructions(profile));
        cmd.arg("-c").arg(format!("developer_instructions={value}"));
    }
    cmd.stdin(Stdio::piped()).arg("-");
    Ok(cmd)
}

fn opencode(
    model: Model<'_>,
    root: &Path,
    prompt: &str,
    instructions: Option<&WorkerInstructions>,
) -> Result<Command, String> {
    // OpenCode qualifies external MCP tool names with the configured server name.
    let name = format!("cocodeop/{}", model.name);
    let mut config = json!({
        "share": "disabled",
        "snapshot": false,
        "enabled_providers": ["cocodeop"],
        "model": name,
        "small_model": name,
        "permission": {"*": "deny", format!("{MCP_NAME}_*"): "allow"},
        "provider": {"cocodeop": {
            "npm": "@ai-sdk/openai-compatible",
            "name": "CoCodeOp",
            "options": {"baseURL": model.url},
            "models": {model.name: {"name": model.name, "tool_call": true,
                "limit": {"context": context_window(model.output_limit), "output": model.output_limit}}},
        }},
        "mcp": {MCP_NAME: {"type": "local", "command": [RUNNER, "worker-mcp"],
            "enabled": true, "timeout": 10_000}},
    });
    if let Some(profile) = instructions {
        config["agent"] = json!({"cocodeop-worker": {
            "description": "Owner-managed CoCodeOp worker", "mode": "primary",
            "prompt": personal_instructions(profile),
        }});
    }
    let path = root.join("config").join("opencode.json");
    serde_json::to_writer(open_private(&path)?, &config)
        .map_err(|_| "Could not write the isolated OpenCode configuration")?;
    let mut cmd = Command::new(OPENCODE);
    cmd.env_clear()
        .stdin(Stdio::null())
        .env("OPENCODE_CONFIG", path)
        .env("OPENCODE_DISABLE_AUTOUPDATE", "true")
        .env("OPENCODE_DISABLE_MODELS_FETCH", "true")
        .env("OPENCODE_DISABLE_LSP_DOWNLOAD", "true")
        .env("OPENCODE_DISABLE_PROJECT_CONFIG", "true")
        .env("OPENCODE_DISABLE_EXTERNAL_SKILLS", "true")
        .env("OPENCODE_DISABLE_DEFAULT_PLUGINS", "true")
        .args(["run", "--format", "json", "--model", &name]);
    if instructions.is_some() {
        cmd.args(["--agent", "cocodeop-worker"]);
    }
    cmd.args(["--", prompt]);
    Ok(cmd)
}

fn personal_instructions(profile: &WorkerInstructions) -> String {
    format!(
        "Your CoCodeOp worker name is {}. Follow the lead's work instructions within the current task and its authorized capabilities.\n\n{}",
        profile.display_name, profile.instructions,
    )
}

#[derive(Default)]
pub(super) struct NativeState {
    pub session: Option<String>,
    last_message: Option<String>,
    publications: BTreeSet<Uuid>,
    answers: BTreeSet<Uuid>,
    reviews: BTreeSet<Uuid>,
    pub input: Option<RunInputIntent>,
    pub finished: bool,
    pub used_context: bool,
    pub failure: Option<String>,
}

impl NativeState {
    pub fn parse(&mut self, backend: WorkerBackend, raw: &Value) -> Result<Vec<Event>, String> {
        match backend {
            WorkerBackend::Codex => self.codex_event(raw),
            WorkerBackend::Opencode => self.opencode_event(raw),
            WorkerBackend::Jsonl => Err("Expected a native agent backend".into()),
        }
    }

    fn codex_event(&mut self, raw: &Value) -> Result<Vec<Event>, String> {
        let mut events = Vec::new();
        match field(raw, "type")? {
            "thread.started" => self.note_session(field(raw, "thread_id")?, &mut events)?,
            "turn.started" => events.push(progress("turn_started")),
            "turn.completed" => {
                let usage = serde_json::from_value::<BackendUsage>(
                    raw.get("usage")
                        .cloned()
                        .ok_or("Codex omitted turn usage")?,
                )
                .map_err(|_| "Codex returned invalid turn usage")?;
                events.push(Event::Usage { usage });
                self.finished = true;
            }
            "turn.failed" | "error" => self.record_failure(raw, "Codex reported a failed turn"),
            "item.completed" => {
                let item = raw.get("item").ok_or("Codex omitted the completed item")?;
                match field(item, "type")? {
                    "agent_message" => self.message(field(item, "text")?, &mut events),
                    "mcp_tool_call" => {
                        if field(item, "server")? != MCP_NAME {
                            return Err("Codex called an unregistered MCP server".into());
                        }
                        let succeeded = field(item, "status")? == "completed"
                            && item.get("error").is_none_or(Value::is_null)
                            && !item
                                .pointer("/result/isError")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                        let tool = field(item, "tool")?;
                        let output = item
                            .pointer("/result/structured_content")
                            .or_else(|| item.pointer("/result/structuredContent"));
                        events.push(self.tool(tool, succeeded, output)?);
                    }
                    // Codex reports non-fatal configuration and model notices as error items.
                    "error" => events.push(progress("backend_notice")),
                    _ => {}
                }
            }
            "item.started" | "item.updated" => {}
            _ => return Err("Unrecognized Codex event type".into()),
        }
        Ok(events)
    }

    fn opencode_event(&mut self, raw: &Value) -> Result<Vec<Event>, String> {
        let mut events = Vec::new();
        let kind = field(raw, "type")?;
        if kind == "error" {
            self.record_failure(raw, "OpenCode reported a failed turn");
            return Ok(events);
        }
        self.note_session(field(raw, "sessionID")?, &mut events)?;
        let part = raw.get("part").ok_or("OpenCode omitted the event part")?;
        match kind {
            "step_start" => {
                self.finished = false;
                self.last_message = None;
                events.push(progress("turn_started"));
            }
            "step_finish" => {
                let tokens = part.get("tokens").ok_or("OpenCode omitted step usage")?;
                let usage = opencode_usage(tokens)?;
                events.push(Event::Usage { usage });
                self.finished = matches!(field(part, "reason")?, "stop" | "end_turn");
            }
            "text" => self.message(field(part, "text")?, &mut events),
            "tool_use" => {
                let tool = field(part, "tool")?;
                let tool = tool
                    .strip_prefix("office_")
                    .filter(|name| name.starts_with("office_"))
                    .ok_or("OpenCode used a tool outside its office MCP permissions")?;
                let succeeded =
                    part.pointer("/state/status").and_then(Value::as_str) == Some("completed");
                let output = part
                    .pointer("/state/output")
                    .and_then(Value::as_str)
                    .and_then(|output| serde_json::from_str::<Value>(output).ok());
                events.push(self.tool(tool, succeeded, output.as_ref())?);
            }
            "reasoning" => {}
            _ => return Err("Unrecognized OpenCode event type".into()),
        }
        Ok(events)
    }

    fn note_session(&mut self, id: &str, events: &mut Vec<Event>) -> Result<(), String> {
        match &self.session {
            Some(previous) if previous != id => {
                Err("The backend changed its session ID during the run".into())
            }
            Some(_) => Ok(()),
            None => {
                self.session = Some(id.into());
                events.push(Event::Started {
                    session_id: id.into(),
                });
                Ok(())
            }
        }
    }

    fn message(&mut self, text: &str, events: &mut Vec<Event>) {
        if !text.trim().is_empty() {
            let text = super::clipped_text(text, 4096);
            self.last_message = Some(text.clone());
            events.push(Event::Message { text });
        }
    }

    fn tool(
        &mut self,
        name: &str,
        successful: bool,
        output: Option<&Value>,
    ) -> Result<Event, String> {
        self.last_message = None;
        self.used_context |= name == "office_run_context" && successful;
        if successful {
            let receipt = match name {
                "office_apply_changes" | "office_replace_text_file" => {
                    Some((&mut self.publications, "/change_id"))
                }
                "office_answer_activation_question" => Some((&mut self.answers, "/answer_id")),
                "office_review_activation_result" => Some((&mut self.reviews, "/task/task_id")),
                _ => None,
            };
            if let Some((receipts, field)) = receipt
                && receipts.len() < 16
                && let Some(id) = output
                    .and_then(|value| value.pointer(field))
                    .and_then(Value::as_str)
                    .and_then(|id| Uuid::parse_str(id).ok())
            {
                receipts.insert(id);
            }
            if name == "office_request_run_input" {
                let requested: RunInputIntent = serde_json::from_value(
                    output
                        .cloned()
                        .ok_or("The backend omitted the recorded input request")?,
                )
                .map_err(|_| "The backend returned an invalid input request receipt")?;
                if !super::valid_text(&requested.question, 3000)
                    || self.input.as_ref().is_some_and(|other| *other != requested)
                {
                    return Err(
                        "The backend's recorded input request changed during one turn".into(),
                    );
                }
                self.input = Some(requested);
            }
        }
        Ok(Event::Progress {
            phase: "mcp_call".into(),
            tool: Some(name.into()),
            successful: Some(successful),
        })
    }

    pub fn into_summary(self) -> Option<String> {
        let summaries = [
            (
                self.publications,
                "Published CoCodeOp changes. Recorded change IDs",
            ),
            (self.answers, "Recorded CoCodeOp answer IDs"),
            (self.reviews, "Recorded CoCodeOp reviews for task IDs"),
        ];
        let receipts: Vec<_> = summaries
            .into_iter()
            .filter(|(ids, _)| !ids.is_empty())
            .map(|(ids, label)| {
                format!(
                    "{label} {}.",
                    ids.into_iter()
                        .map(|id| id.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .collect();
        if receipts.is_empty() {
            return self.last_message;
        }
        let receipt = receipts.join("\n");
        match self.last_message {
            Some(message) => {
                let message = super::clipped_text(&message, 4096 - receipt.len() - 2);
                Some(format!("{message}\n\n{receipt}"))
            }
            None => Some(receipt),
        }
    }

    fn record_failure(&mut self, raw: &Value, fallback: &str) {
        let message = raw
            .pointer("/error/message")
            .or_else(|| raw.pointer("/error/data/message"))
            .or_else(|| raw.get("message"))
            .and_then(Value::as_str)
            .unwrap_or(fallback);
        self.failure = Some(super::clipped_text(message, 4096));
    }
}

fn field<'a>(value: &'a Value, field: &str) -> Result<&'a str, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Backend JSONL omitted a required {field} string"))
}

fn progress(phase: &str) -> Event {
    Event::Progress {
        phase: phase.into(),
        tool: None,
        successful: None,
    }
}

fn opencode_usage(tokens: &Value) -> Result<BackendUsage, String> {
    let count = |key| tokens.get(key).and_then(Value::as_u64);
    let cache = |key| tokens.pointer(key).and_then(Value::as_u64).unwrap_or(0);
    let cached_input_tokens = cache("/cache/read");
    let cache_write_input_tokens = cache("/cache/write");
    let reasoning_output_tokens = count("reasoning").unwrap_or(0);
    let input_tokens = count("input")
        .and_then(|input| input.checked_add(cached_input_tokens))
        .and_then(|input| input.checked_add(cache_write_input_tokens))
        .ok_or("OpenCode returned invalid input usage")?;
    let output_tokens = count("output")
        .and_then(|output| output.checked_add(reasoning_output_tokens))
        .ok_or("OpenCode returned invalid output usage")?;
    Ok(BackendUsage {
        input_tokens,
        output_tokens,
        cached_input_tokens,
        cache_write_input_tokens,
        reasoning_output_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opencode_input_includes_both_cache_counts_and_output_includes_reasoning() {
        let usage = opencode_usage(&json!({
            "input": 80, "output": 20, "reasoning": 17, "cache": {"read": 500, "write": 10}
        }))
        .unwrap();
        assert_eq!(usage.input_tokens, 590);
        assert_eq!(usage.output_tokens, 37);
        assert_eq!(usage.cached_input_tokens, 500);
        assert_eq!(usage.cache_write_input_tokens, 10);
    }

    #[test]
    fn opencode_uses_the_publication_receipt_if_it_finishes_without_final_text() {
        let id = Uuid::new_v4();
        let events = [
            json!({"type":"text","part":{"text":"I will edit the file."}}),
            json!({"type":"tool_use","part":{"tool":"office_office_apply_changes",
                "state":{"status":"completed","output":json!({"change_id":id}).to_string()}}}),
            json!({"type":"step_start","part":{}}),
            json!({"type":"step_finish","part":{"reason":"stop","tokens":{"input":1,"output":0}}}),
        ];
        let mut state = NativeState::default();
        for mut event in events {
            event["sessionID"] = json!("ses_correct");
            state.parse(WorkerBackend::Opencode, &event).unwrap();
        }
        assert!(state.finished);
        let summary = state.into_summary().unwrap();
        assert!(summary.contains(&id.to_string()));
        assert!(!summary.contains("I will"));
    }
}
