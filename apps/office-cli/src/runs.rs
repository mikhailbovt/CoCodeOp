use std::process::ExitCode;

use clap::{Args, Subcommand};
use office_domain::{RunId, TaskId, WorkspaceId};
use office_protocol::{ReadRunLogs, RunLog};
use reqwest::Url;
use uuid::Uuid;

use crate::{identity::authenticated_post, print_receipt};

#[derive(Subcommand)]
pub(crate) enum RunCommand {
    /// Read the durable coordination events and evidence for an attempt.
    Logs(LogOptions),
}

#[derive(Args)]
pub(crate) struct LogOptions {
    #[arg(long)]
    workspace: Uuid,
    #[arg(long)]
    task: Uuid,
    #[arg(long)]
    run: Uuid,
    #[arg(long)]
    after: Option<Uuid>,
    #[arg(long)]
    limit: Option<u16>,
    #[arg(long, default_value = "http://127.0.0.1:8400")]
    url: Url,
    #[arg(long)]
    json: bool,
}

pub(crate) async fn run(command: RunCommand) -> ExitCode {
    let RunCommand::Logs(options) = command;
    let report = authenticated_post::<RunLog>(
        options.url,
        "/v1/runs/logs",
        &ReadRunLogs {
            workspace_id: WorkspaceId(options.workspace),
            task_id: TaskId(options.task),
            run_id: RunId(options.run),
            after: options.after,
            limit: options.limit,
        },
    )
    .await;
    print_receipt(report, options.json, display_log)
}

fn display_log(log: &RunLog) -> String {
    let mut lines = vec![format!(
        "Run {} · {} · attempt {}",
        log.run.run_id.0,
        log.run.status.as_str(),
        log.run.attempt_number
    )];
    if let Some(summary) = &log.run.summary {
        lines.push(summary.clone());
    }
    for entry in &log.events {
        let detail = entry.detail.as_deref().unwrap_or_default();
        lines.push(format!(
            "{} · {} {detail}",
            entry.created_at, entry.event_type
        ));
    }
    if let Some(cursor) = log.next_after {
        lines.push(format!("Continue with --after {cursor}"));
    }
    lines.join("\n")
}
