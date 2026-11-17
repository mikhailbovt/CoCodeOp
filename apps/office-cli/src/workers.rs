use std::{path::PathBuf, process::ExitCode};

use clap::{Args, Subcommand};
use office_domain::{AgentId, TaskId, WorkspaceId};
use office_protocol::{
    CreateManagedWorker, GetManagedWorkerStatus, ManagedWorker, ManagedWorkerSelector,
    ManagedWorkerStatus, UpdateManagedWorker,
};
use reqwest::Url;
use uuid::Uuid;

use crate::{
    identity::{authenticated_post, post_file},
    print_receipt,
};

#[derive(Subcommand)]
pub(crate) enum WorkerCommand {
    /// As lead, create a named managed worker from JSON.
    Create(WorkerFile),
    /// Revise a managed worker's standing instructions from JSON.
    Update(WorkerFile),
    /// Read a managed worker's tasks, jobs and attempts.
    Status {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long, required_unless_present = "name", conflicts_with = "name")]
        agent: Option<Uuid>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        tasks_after: Option<Uuid>,
        #[arg(long)]
        limit: Option<u16>,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args)]
pub(crate) struct WorkerFile {
    #[arg(long)]
    file: PathBuf,
    #[arg(long, default_value = "http://127.0.0.1:8400")]
    url: Url,
    #[arg(long)]
    json: bool,
}

pub(crate) async fn run(command: WorkerCommand) -> ExitCode {
    match command {
        WorkerCommand::Create(input) => print_receipt(
            post_file::<CreateManagedWorker, ManagedWorker>(&input.file, input.url, "/v1/workers")
                .await,
            input.json,
            display,
        ),
        WorkerCommand::Update(input) => print_receipt(
            post_file::<UpdateManagedWorker, ManagedWorker>(
                &input.file,
                input.url,
                "/v1/workers/instructions",
            )
            .await,
            input.json,
            display,
        ),
        WorkerCommand::Status {
            workspace,
            agent,
            name,
            tasks_after,
            limit,
            url,
            json,
        } => {
            let worker = match (agent, name) {
                (Some(id), None) => ManagedWorkerSelector::Id {
                    agent_id: AgentId(id),
                },
                (None, Some(display_name)) => ManagedWorkerSelector::Name { display_name },
                _ => return ExitCode::FAILURE,
            };
            print_receipt(
                authenticated_post::<ManagedWorkerStatus>(
                    url,
                    "/v1/workers/status",
                    &GetManagedWorkerStatus {
                        workspace_id: WorkspaceId(workspace),
                        worker,
                        tasks_after: tasks_after.map(TaskId),
                        limit,
                    },
                )
                .await,
                json,
                |state| {
                    let mut lines = vec![display(&state.worker)];
                    for item in &state.tasks {
                        lines.push(format!(
                            "{:?} · {} · {}",
                            item.state.task.status,
                            item.state.task.task_id.0,
                            item.state.task.objective
                        ));
                        if let Some(job) = &item.job {
                            lines.push(format!("  Job {:?} · {}", job.state, job.job_id));
                        }
                        if let Some(run) = item.state.runs.first() {
                            lines.push(format!(
                                "  Run {:?} · {}{}",
                                run.status,
                                run.run_id.0,
                                run.summary
                                    .as_ref()
                                    .map_or_else(String::new, |message| format!(" · {message}"))
                            ));
                        }
                    }
                    lines.join("\n")
                },
            )
        }
    }
}

fn display(worker: &ManagedWorker) -> String {
    let id = worker
        .worker
        .agent_id
        .map_or_else(String::new, |agent| agent.0.to_string());
    format!(
        "{} ({id}) · instructions revision {}",
        worker.profile.display_name, worker.profile.revision
    )
}
