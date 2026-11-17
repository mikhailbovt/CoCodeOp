use std::{
    fs::{DirBuilder, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Stdio,
};

use office_domain::RunId;
use office_protocol::{
    ActivationTrigger, BACKEND_PROTOCOL_VERSION, BackendCapabilities, BackendCost, BackendEvent,
    BackendEventKind as Event, BackendEvidence, BackendState, BackendUsage, CODEX_EXEC_ADAPTER,
    ExecutionTaskBrief, GENERIC_JSONL_ADAPTER, MAX_BACKEND_EVENTS, MAX_BACKEND_TRACE_BYTES,
    OPENCODE_RUN_ADAPTER, ProcessResult, ResultAttachment, ResultReport, RunContextView,
    RunWorkContext, valid_backend_id,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
};
use uuid::Uuid;

use crate::{WorkerAgentOptions, WorkerBackend, inference, worker_mcp};

mod native;
pub(super) use native::backend_executable;

const WORK: &str = "/work";
const TASK: &str = "/task.json";
const RUNNER: &str = "/usr/local/bin/office-runner";
const MCP_NAME: &str = "office";
const MCP_SOCKET: &str = "/run/cocodeop/mcp.sock";
const INFERENCE_SOCKET: &str = "/run/cocodeop/inference.sock";
const MAX_LINE: usize = 256 * 1024;
const MAX_CHILD_OUTPUT: usize = 8 * 1024 * 1024;
const MAX_CHECKPOINTS: u32 = 32;

impl WorkerBackend {
    fn id(self) -> &'static str {
        match self {
            Self::Codex => CODEX_EXEC_ADAPTER,
            Self::Opencode => OPENCODE_RUN_ADAPTER,
            Self::Jsonl => GENERIC_JSONL_ADAPTER,
        }
    }
}

#[tokio::main]
pub(super) async fn run(options: WorkerAgentOptions) -> Result<(), String> {
    if options.capabilities {
        let description = BackendCapabilities {
            can_resume: false,
            can_cancel: true,
            supports_approvals: false,
            supports_usage: true,
            supports_tools: true,
            supports_shared_gateway: true,
            enforces_no_native_source_write: true,
        };
        println!(
            "{}",
            serde_json::to_string(&description)
                .map_err(|_| "Could not encode backend capabilities")?
        );
        return Ok(());
    }
    let root = private_directory(&Path::new(WORK).join("agent"))?;
    let mut trace = Trace::new(&root)?;
    let model = std::env::var("COCODEOP_MODEL").ok();
    let outcome = execute(&options, &root, model.as_deref(), &mut trace).await;
    let failure = match outcome {
        Ok(()) => None,
        Err(reason) => {
            trace.terminal = Some(Event::Failed {
                reason: reason.clone(),
            });
            Some(reason)
        }
    };
    trace.finish(&root, options.backend, model)?;
    failure.map_or(Ok(()), Err)
}

async fn execute(
    options: &WorkerAgentOptions,
    root: &Path,
    model: Option<&str>,
    trace: &mut Trace,
) -> Result<(), String> {
    let task = task_brief()?;
    let work = worker_mcp::run_work_context(Path::new(MCP_SOCKET), None).await?;
    let context = &work.run;
    if task.task_id != context.task.task_id
        || task.checkout_id != context.task.checkout_id
        || (task.checkout_id.is_some() && context.scope_revision.is_none())
    {
        return Err(
            "The mandatory MCP context does not match the assigned task and checkout".into(),
        );
    }
    let mut inbox = WorkInbox::start(root, &work)?;
    let prompt = prompt(options.backend, context);
    let mut inference_listener = None;
    let mut model_url = None;
    if model.is_some() {
        let (url, listener) = inference::start_worker_http(Path::new(INFERENCE_SOCKET)).await?;
        inference_listener = Some(listener);
        model_url = Some(url);
    }
    let mut command = if matches!(options.backend, WorkerBackend::Jsonl) {
        bridge_command(options)?
    } else {
        if options.bridge.is_some() {
            return Err("A native backend requires its packaged executable".into());
        }
        let api =
            std::env::var("COCODEOP_INFERENCE_API").map_err(|_| "The run lacks a model API")?;
        let output_limit = std::env::var("COCODEOP_MODEL_OUTPUT_LIMIT")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| (64..=32_768).contains(value))
            .ok_or("The run lacks a model output limit")?;
        native::command(
            options.backend,
            native::Model {
                name: model.ok_or("No model was installed for the native backend")?,
                url: model_url
                    .as_deref()
                    .ok_or("No inference socket was installed")?,
                api: &api,
                output_limit,
            },
            root,
            &prompt,
            context.worker_instructions.as_ref(),
        )?
    };
    command
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("LC_ALL", "C.UTF-8")
        .env("HOME", private_directory(&root.join("home"))?)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", private_directory(&root.join("data"))?)
        .env("XDG_STATE_HOME", private_directory(&root.join("state"))?)
        .env("XDG_CACHE_HOME", private_directory(&root.join("cache"))?)
        .current_dir(private_directory(&root.join("workspace"))?)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|_| "The installed backend could not start")?;
    let mut input = child.stdin.take();
    let document = if matches!(options.backend, WorkerBackend::Jsonl) {
        serde_json::to_vec(&json!({
            "version": BACKEND_PROTOCOL_VERSION,
            "run": context, "inbox": work.inbox, "instructions": prompt, "mcp_command": [RUNNER, "worker-mcp"],
            "inference": model.zip(model_url.as_deref()).map(|(model, url)| json!({"model":model,"base_url":url})),
        })).map_err(|_| "Could not encode the exact bridge run")?
    } else {
        prompt.into_bytes()
    };
    let stderr = child
        .stderr
        .take()
        .ok_or("The backend produced no stderr stream")?;
    let stderr_file = open_private(&root.join("native.stderr"))?;
    let stderr_drain = tokio::spawn(drain_stderr(stderr, stderr_file));
    let mut stdout = BufReader::new(
        child
            .stdout
            .take()
            .ok_or("The backend produced no JSONL stream")?,
    );
    let backend = options.backend;
    let mut native = native::NativeState::default();
    let send = async {
        if let Some(mut stdin) = input.take() {
            stdin.write_all(&document).await?;
            stdin.write_all(b"\n").await?;
        }
        Ok::<(), io::Error>(())
    };
    let receive = read_events(&mut stdout, backend, trace, &mut native, &mut inbox);
    let parsing = tokio::try_join!(
        async {
            send.await
                .map_err(|_| "Could not send the assigned task to the backend".to_owned())
        },
        receive
    );
    if parsing.is_err() {
        let _ = child.kill().await;
    }
    let status = child
        .wait()
        .await
        .map_err(|_| "Could not verify the backend exit status")?;
    stderr_drain
        .await
        .map_err(|_| "The backend stderr reader failed")??;
    drop(inference_listener);
    trace.exit = status.code().and_then(|code| u8::try_from(code).ok());
    parsing?;
    if let Some(diagnostic) = native.failure.as_ref() {
        save(&root.join("native-error.json"), diagnostic)?;
        return Err(
            "The native client reported a failed turn. Inspect its local diagnostics".into(),
        );
    }
    if !status.success() {
        return Err(format!("The backend exited with {status}"));
    }
    if !matches!(backend, WorkerBackend::Jsonl) {
        if !native.finished || !native.used_context {
            return Err("The native backend did not complete a turn with a successful office_run_context call".into());
        }
        if let Some(request) = native.input.take() {
            if request.run_id != context.run_id {
                return Err("The input request belongs to a different run".into());
            }
            let work = inbox.refresh("checkpoint-1").await?;
            let record = json!({"work":work,"events_sha256_before_checkpoint":hex::encode(trace.digest.clone().finalize())});
            let bytes = serde_json::to_vec(&record)
                .map_err(|_| "Could not serialize the paused checkpoint")?;
            let mut file = open_private(&root.join("checkpoint.json"))?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|_| "Could not save the paused checkpoint")?;
            trace.observe(Event::Checkpoint {
                checkpoint_id: format!("sha256:{}", hex::encode(Sha256::digest(bytes))),
            })?;
            trace.observe(Event::InputRequired {
                question: request.question,
            })?;
        } else {
            trace.terminal = Some(Event::Completed {
                summary: native
                    .into_summary()
                    .ok_or("The native backend returned no final message or action receipt")?,
            });
        }
    }
    if trace.terminal.is_none() {
        return Err("The bridge stopped without a terminal event".into());
    }
    inbox.refresh("finish").await?;
    Ok(())
}

fn bridge_command(options: &WorkerAgentOptions) -> Result<Command, String> {
    let executable = options
        .bridge
        .as_deref()
        .ok_or("Register an absolute JSONL bridge executable")?;
    let resolved = executable
        .canonicalize()
        .map_err(|_| "Could not resolve the installed bridge executable")?;
    if !executable.is_absolute()
        || !resolved.is_file()
        || !(resolved.starts_with("/opt") || resolved.starts_with("/usr/local"))
    {
        return Err(
            "Install the JSONL bridge under /opt or /usr/local in the approved image".into(),
        );
    }
    let mut command = Command::new(resolved);
    command
        .env_clear()
        .args(&options.bridge_arg)
        .stdin(Stdio::piped());
    Ok(command)
}

fn task_brief() -> Result<ExecutionTaskBrief, String> {
    let mut data = Vec::new();
    File::open(TASK)
        .and_then(|file| file.take(64 * 1024 + 1).read_to_end(&mut data))
        .map_err(|_| "Could not read the mounted task brief")?;
    if data.len() > 64 * 1024 {
        return Err("The mounted task brief exceeded its size limit".into());
    }
    serde_json::from_slice(&data).map_err(|_| "The mounted task brief is invalid".into())
}

fn prompt(backend: WorkerBackend, context: &RunContextView) -> String {
    let prefix = match backend {
        WorkerBackend::Codex => "mcp__office__office_",
        WorkerBackend::Opencode => "office_office_",
        WorkerBackend::Jsonl => "office_",
    };
    let tool = format!("{prefix}run_context");
    let continuation = if context.continuation.is_some() {
        " This turn continues after an owner answered a clarification. Read .continuation.answer first. \
         Treat .continuation.question as resolved and use the answer to continue the current task."
    } else {
        ""
    };
    if context
        .activation
        .as_ref()
        .is_some_and(|turn| turn.trigger == ActivationTrigger::Answer)
    {
        return format!(
            "You are the CoCodeOp agent whose addressed question received an answer. First call {tool} with {{}}.{continuation} \
             Read each answer in activation.sources[].body. Summarize what the answer says and any follow-up it requests. \
             Preserve identifiers and reference codes exactly. Finish by returning the summary in text.",
        );
    }
    if context
        .activation
        .as_ref()
        .is_some_and(|turn| turn.trigger == ActivationTrigger::Question)
    {
        return format!(
            "You are the CoCodeOp agent assigned to task {}. First call {tool} with {{}}.{continuation} Read activation.sources. \
             For every source with question_open=true, answer the question's body in the original thread using {prefix}answer_activation_question. \
             Pass question_id, body and a new UUID idempotency_key for each answer. Finish with the actual answer IDs.",
            context.task.task_id.0,
        );
    }
    if context
        .activation
        .as_ref()
        .is_some_and(|turn| turn.trigger == ActivationTrigger::ReviewRequest)
    {
        return format!(
            "You are the CoCodeOp reviewer assigned to task {}. First call {tool} with {{}}.{continuation} \
            Then call {prefix}get_activation_review with {{}}. Compare the submitted result and evidence against the acceptance criteria. \
            Call {prefix}review_activation_result with the exact result_sha256, decision accept or request_changes, a reason and a fresh UUID idempotency_key. Report the recorded decision.",
            context.task.task_id.0
        );
    }
    format!(
        "You are the CoCodeOp worker assigned to task {}. First call the CoCodeOp MCP tool {tool} with {{}} \
         to obtain the task objective, acceptance criteria, active scope revision and required read paths.{continuation} \
         Use the tools registered on the office MCP server for checkout reads, writes and commands. Read allowed files through MCP before editing. \
         For a single UTF-8 file, use {prefix}replace_text_file with the complete new text in content. \
         Publish typed changes with the returned hashes and current scope revision. Omit claim_fence for one bounded publication to obtain \
         a short automatic claim. Use explicit claims for ongoing reservations. The TTL range is 5–120 seconds. \
         On PATH_CLAIMED, use {prefix}request_path_handoff and read handoff notifications with {prefix}run_work_context. \
         Reread the overlapping files after taking over a claim. \
         Use a UUID for each idempotency_key. Copy provided keys exactly and keep an identical payload on retry. \
         Use the approved command tool for checks and cite the actual publication and check receipts. \
         If an effect is blocked, report the exact blocker. Never report an unverified operation as done. \
         Finish with a concise summary of the work and evidence.",
        context.task.task_id.0,
    )
}

async fn read_events(
    reader: &mut BufReader<tokio::process::ChildStdout>,
    backend: WorkerBackend,
    trace: &mut Trace,
    native: &mut native::NativeState,
    inbox: &mut WorkInbox<'_>,
) -> Result<(), String> {
    let mut total = 0;
    let mut line = Vec::new();
    loop {
        line.clear();
        let size = (&mut *reader)
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut line)
            .await
            .map_err(|_| "The backend JSONL stream was interrupted")?;
        if size == 0 {
            return Ok(());
        }
        total += size;
        if size > MAX_LINE || total > MAX_CHILD_OUTPUT {
            return Err("The backend exceeded its JSONL output limit".into());
        }
        if matches!(backend, WorkerBackend::Jsonl) {
            let entry: BackendEvent = serde_json::from_slice(&line)
                .map_err(|_| "The bridge returned an unknown or malformed JSONL event")?;
            if entry.version != BACKEND_PROTOCOL_VERSION {
                return Err("The bridge returned an unsupported JSONL protocol version".into());
            }
            let checkpoint = matches!(entry.event, Event::Checkpoint { .. });
            trace.observe(entry.event)?;
            if checkpoint {
                inbox.checkpoint().await?;
            }
        } else {
            let raw: Value = serde_json::from_slice(&line)
                .map_err(|_| "The native backend returned invalid JSONL")?;
            for event in native.parse(backend, &raw)? {
                trace.observe(event)?;
            }
        }
    }
}

struct WorkInbox<'a> {
    root: &'a Path,
    run: RunId,
    cursor: Uuid,
    checkpoints: u32,
}

impl<'a> WorkInbox<'a> {
    fn start(root: &'a Path, initial: &RunWorkContext) -> Result<Self, String> {
        save(&root.join("inbox-start.json"), initial)?;
        Ok(Self {
            root,
            run: initial.run.run_id,
            cursor: initial.inbox.next_cursor,
            checkpoints: 0,
        })
    }

    async fn checkpoint(&mut self) -> Result<(), String> {
        if self.checkpoints >= MAX_CHECKPOINTS {
            return Err("The bridge exceeded its checkpoint limit".into());
        }
        self.checkpoints += 1;
        let label = format!("checkpoint-{}", self.checkpoints);
        self.refresh(&label).await.map(|_| ())
    }

    async fn refresh(&mut self, label: &str) -> Result<RunWorkContext, String> {
        let work = worker_mcp::run_work_context(Path::new(MCP_SOCKET), Some(self.cursor)).await?;
        if work.run.run_id != self.run {
            return Err("The run MCP work context changed attempts".into());
        }
        save(&self.root.join(format!("inbox-{label}.json")), &work)?;
        self.cursor = work.inbox.next_cursor;
        Ok(work)
    }
}

async fn drain_stderr(mut reader: tokio::process::ChildStderr, file: File) -> Result<(), String> {
    let mut buffer = [0u8; 8192];
    let mut remaining = 32 * 1024;
    let mut file = tokio::fs::File::from_std(file);
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|_| "Could not read backend diagnostics")?;
        if count == 0 {
            return Ok(());
        }
        let retain = count.min(remaining);
        if retain != 0 {
            file.write_all(&buffer[..retain])
                .await
                .map_err(|_| "Could not save backend diagnostics")?;
            remaining -= retain;
        }
    }
}

struct Trace {
    log: File,
    digest: Sha256,
    bytes: usize,
    count: u32,
    exit: Option<u8>,
    session: Option<String>,
    checkpoint: Option<String>,
    terminal: Option<Event>,
    usage: Option<BackendUsage>,
    cost: Option<BackendCost>,
    artifacts: Vec<ResultAttachment>,
}

impl Trace {
    fn new(root: &Path) -> Result<Self, String> {
        private_directory(&root.join("config"))?;
        Ok(Self {
            log: open_private(&root.join("events.jsonl"))?,
            digest: Sha256::new(),
            bytes: 0,
            count: 0,
            exit: None,
            session: None,
            checkpoint: None,
            terminal: None,
            usage: None,
            cost: None,
            artifacts: Vec::new(),
        })
    }

    fn observe(&mut self, event: Event) -> Result<(), String> {
        if self.terminal.is_some() {
            return Err("The bridge emitted an event after its terminal event".into());
        }
        match &event {
            Event::Started { session_id }
                if self.session.is_none() && valid_backend_id(session_id) =>
            {
                self.session = Some(session_id.clone());
            }
            Event::Started { .. } => {
                return Err("The backend session ID is invalid or repeated".into());
            }
            Event::Failed { reason } if valid_text(reason, 4096) => {
                self.terminal = Some(event);
                return Ok(());
            }
            _ if self.session.is_none() => {
                return Err("The bridge omitted its started event".into());
            }
            Event::Completed { summary } if valid_text(summary, 4096) => {
                self.terminal = Some(event);
                return Ok(());
            }
            Event::InputRequired { question }
                if valid_text(question, 3000) && self.checkpoint.is_some() =>
            {
                self.terminal = Some(event);
                return Ok(());
            }
            Event::Checkpoint { checkpoint_id } if valid_backend_id(checkpoint_id) => {
                self.checkpoint = Some(checkpoint_id.clone());
            }
            Event::Usage { usage } => {
                self.usage = Some(
                    self.usage
                        .unwrap_or_default()
                        .checked_add(*usage)
                        .ok_or("Backend usage counters overflowed")?,
                );
            }
            Event::Cost {
                cost: cost @ BackendCost::Reported { .. },
            } if self.cost.is_none() && cost.valid() => {
                self.cost = Some(cost.clone());
            }
            Event::Artifact { attachment }
                if self.artifacts.len() < 4
                    && valid_text(&attachment.name, 96)
                    && attachment.content.len() <= 8192 =>
            {
                self.artifacts.push(attachment.clone());
            }
            Event::Message { text } if valid_text(text, 4096) => {}
            Event::Progress { phase, tool, .. }
                if valid_backend_id(phase) && tool.as_deref().is_none_or(valid_backend_id) => {}
            _ => return Err("The backend emitted an event with invalid fields".into()),
        }
        self.append(event, false)
    }

    fn append(&mut self, event: Event, terminal: bool) -> Result<(), String> {
        let mut data = serde_json::to_vec(&BackendEvent {
            version: BACKEND_PROTOCOL_VERSION,
            event,
        })
        .map_err(|_| "Could not encode normalized backend events")?;
        data.push(b'\n');
        // Keep enough space to record the eventual terminal outcome.
        if !terminal
            && (self.count + 1 >= MAX_BACKEND_EVENTS
                || self.bytes + data.len() > MAX_BACKEND_TRACE_BYTES - MAX_LINE)
        {
            return Err("The backend exceeded its normalized event limit".into());
        }
        self.log
            .write_all(&data)
            .map_err(|_| "Could not record the backend event")?;
        self.digest.update(&data);
        self.count += 1;
        self.bytes += data.len();
        Ok(())
    }

    fn finish(
        mut self,
        root: &Path,
        backend: WorkerBackend,
        model: Option<String>,
    ) -> Result<(), String> {
        let event = self.terminal.take().ok_or("The backend outcome was lost")?;
        let (state, result) = match &event {
            Event::Completed { summary } => (
                BackendState::Completed,
                ProcessResult::Succeeded {
                    summary: summary.clone(),
                    report: (!self.artifacts.is_empty()).then(|| ResultReport {
                        artifacts: std::mem::take(&mut self.artifacts),
                        agent_reported_checks: vec![],
                    }),
                },
            ),
            Event::Failed { reason } => (
                BackendState::Failed,
                ProcessResult::Failed {
                    reason: reason.clone(),
                },
            ),
            Event::InputRequired { question } => (
                BackendState::InputRequired,
                ProcessResult::WaitingInput {
                    question: question.clone(),
                },
            ),
            _ => return Err("The backend outcome is not terminal".into()),
        };
        self.append(event, true)?;
        self.log
            .sync_all()
            .map_err(|_| "Could not commit the backend log")?;
        let evidence = BackendEvidence {
            version: BACKEND_PROTOCOL_VERSION,
            backend: backend.id().into(),
            session_id: self.session,
            model,
            checkpoint_id: self.checkpoint,
            terminal_state: state,
            exit_code: self.exit,
            usage: self.usage,
            cost: self.cost,
            event_count: self.count,
            events_sha256: hex::encode(self.digest.finalize()),
        };
        if !evidence.valid() {
            return Err("The backend evidence failed validation".into());
        }
        save(&root.join("evidence.json"), &evidence)?;
        save(&Path::new(WORK).join("result.json"), &result)
    }
}

fn save(path: &Path, value: &impl serde::Serialize) -> Result<(), String> {
    let mut file = open_private(path)?;
    serde_json::to_writer(&mut file, value).map_err(|_| "Could not serialize the worker record")?;
    file.sync_all()
        .map_err(|_| "Could not sync the worker record".into())
}

fn private_directory(path: &Path) -> Result<PathBuf, String> {
    DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|_| "Could not create isolated backend state")?;
    Ok(path.to_owned())
}

fn open_private(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| "Could not create an exclusive backend record".into())
}

fn valid_text(value: &str, limit: usize) -> bool {
    !value.trim().is_empty() && value.len() <= limit && !value.contains('\0')
}

fn clipped_text(value: &str, limit: usize) -> String {
    value[..value.floor_char_boundary(limit.min(value.len()))]
        .trim()
        .to_owned()
}
