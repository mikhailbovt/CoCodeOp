use std::{path::PathBuf, process::ExitCode};

use clap::{Args, Subcommand};
use office_domain::{RunId, TaskId, TaskStatus, WorkspaceId};
use office_protocol::{
    ChangeTask, CreateTask, ExplainTask, GetTask, GetTaskResult, HandoffMutation, HandoffPage,
    ListHandoffs, ListTasks, RequestHandoff, RespondHandoff, ReviewTask, SubmitTaskResult,
    TaskCondition, TaskExplanation, TaskHandoff, TaskMutation, TaskPage, TaskResult, TaskView,
};
use reqwest::Url;
use uuid::Uuid;

use crate::{
    identity::{authenticated_post, post_file},
    print_receipt,
};

#[derive(Args)]
pub(crate) struct TaskFile {
    #[arg(long)]
    file: PathBuf,
    #[arg(long, default_value = "http://127.0.0.1:8400")]
    url: Url,
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
pub(crate) enum TaskCommand {
    /// Explain what is blocking a task and who should act.
    Explain {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        id: Uuid,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Create a draft from JSON.
    Create(TaskFile),
    /// Apply a typed task command from JSON.
    Change(TaskFile),
    /// Submit your current attempt and optional attachments.
    Submit(TaskFile),
    /// Review the exact current result or return it for changes.
    Review(TaskFile),
    /// Read the latest or a specific immutable result revision.
    Result {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        id: Uuid,
        #[arg(long)]
        revision: Option<i64>,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Offer task responsibility to an owner or lead with read access.
    RequestHandoff(TaskFile),
    /// Accept or decline a task handoff.
    RespondHandoff(TaskFile),
    /// List incoming and sent handoffs.
    Handoffs {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        after: Option<Uuid>,
        #[arg(long)]
        limit: Option<u16>,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Inspect a task and its attempt history.
    Get {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        id: Uuid,
        #[arg(long)]
        runs_after: Option<Uuid>,
        #[arg(long)]
        run_limit: Option<u16>,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// List visible tasks in the workspace.
    List {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        status: Option<TaskStatus>,
        #[arg(long)]
        after: Option<Uuid>,
        #[arg(long)]
        limit: Option<u16>,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
}

pub(crate) async fn run(command: TaskCommand) -> ExitCode {
    match command {
        TaskCommand::Explain {
            workspace,
            id,
            url,
            json,
        } => print_receipt(
            authenticated_post::<TaskExplanation>(
                url,
                "/v1/tasks/explain",
                &ExplainTask {
                    workspace_id: WorkspaceId(workspace),
                    task_id: TaskId(id),
                },
            )
            .await,
            json,
            display_explanation,
        ),
        TaskCommand::Submit(input) => print_receipt(
            post_file::<SubmitTaskResult, TaskMutation>(
                &input.file,
                input.url,
                "/v1/task-results/submit",
            )
            .await,
            input.json,
            display_mutation,
        ),
        TaskCommand::Review(input) => print_receipt(
            post_file::<ReviewTask, TaskMutation>(
                &input.file,
                input.url,
                "/v1/task-results/review",
            )
            .await,
            input.json,
            display_mutation,
        ),
        TaskCommand::Result {
            workspace,
            id,
            revision,
            url,
            json,
        } => {
            let input = GetTaskResult {
                workspace_id: WorkspaceId(workspace),
                task_id: TaskId(id),
                revision,
            };
            let output =
                authenticated_post::<TaskResult>(url, "/v1/task-results/read", &input).await;
            print_receipt(output, json, |result| {
                format!(
                    "Result {} revision {} sha256 {}",
                    result.result_id, result.binding.result_revision, result.binding.result_sha256
                )
            })
        }
        TaskCommand::RequestHandoff(input) => print_receipt(
            post_file::<RequestHandoff, HandoffMutation>(
                &input.file,
                input.url,
                "/v1/task-handoffs",
            )
            .await,
            input.json,
            |receipt| display_handoff(&receipt.handoff),
        ),
        TaskCommand::RespondHandoff(input) => print_receipt(
            post_file::<RespondHandoff, HandoffMutation>(
                &input.file,
                input.url,
                "/v1/task-handoffs/respond",
            )
            .await,
            input.json,
            |receipt| display_handoff(&receipt.handoff),
        ),
        TaskCommand::Handoffs {
            workspace,
            after,
            limit,
            url,
            json,
        } => print_receipt(
            authenticated_post::<HandoffPage>(
                url,
                "/v1/task-handoffs/list",
                &ListHandoffs {
                    workspace_id: WorkspaceId(workspace),
                    after,
                    limit,
                },
            )
            .await,
            json,
            |page| {
                let mut lines: Vec<_> = page.handoffs.iter().map(display_handoff).collect();
                if let Some(after) = page.next_after {
                    lines.push(format!("Next page after {after}"));
                }
                lines.join("\n")
            },
        ),
        TaskCommand::Create(input) => print_receipt(
            post_file::<CreateTask, TaskMutation>(&input.file, input.url, "/v1/tasks").await,
            input.json,
            display_mutation,
        ),
        TaskCommand::Change(input) => print_receipt(
            post_file::<ChangeTask, TaskMutation>(&input.file, input.url, "/v1/tasks/change").await,
            input.json,
            display_mutation,
        ),
        TaskCommand::Get {
            workspace,
            id,
            runs_after,
            run_limit,
            url,
            json,
        } => print_receipt(
            authenticated_post::<TaskView>(
                url,
                "/v1/tasks/read",
                &GetTask {
                    workspace_id: WorkspaceId(workspace),
                    task_id: TaskId(id),
                    runs_after: runs_after.map(RunId),
                    run_limit,
                },
            )
            .await,
            json,
            |view| {
                format!(
                    "{} · {} · revision {}\n{}\nAttempts in page {}",
                    view.task.task_id.0,
                    view.task.status.as_str(),
                    view.task.revision,
                    view.task.objective,
                    view.runs.len(),
                )
            },
        ),
        TaskCommand::List {
            workspace,
            status,
            after,
            limit,
            url,
            json,
        } => print_receipt(
            authenticated_post::<TaskPage>(
                url,
                "/v1/tasks/list",
                &ListTasks {
                    workspace_id: WorkspaceId(workspace),
                    status,
                    after: after.map(TaskId),
                    limit,
                },
            )
            .await,
            json,
            |page| {
                let mut lines: Vec<_> = page
                    .tasks
                    .iter()
                    .map(|task| {
                        format!(
                            "{} · {} · {}",
                            task.task_id.0,
                            task.status.as_str(),
                            task.objective
                        )
                    })
                    .collect();
                if let Some(after) = page.next_after {
                    lines.push(format!("Next page after {}", after.0));
                }
                lines.join("\n")
            },
        ),
    }
}

fn display_explanation(report: &TaskExplanation) -> String {
    use TaskCondition as Wait;
    let detail = match &report.condition {
        Wait::AwaitingOwner => "Waiting for the owner".into(),
        Wait::AwaitingReview => "Waiting for a review".into(),
        Wait::AwaitingInput => "Waiting for an answer".into(),
        Wait::AwaitingStopConfirmation => {
            "Waiting for confirmation that the process stopped".into()
        }
        Wait::Dependencies { tasks } => format!("Waiting for {} prerequisites", tasks.len()),
        Wait::WorkScopeRequired { .. } => "Waiting for an approved checkout scope".into(),
        Wait::ExecutionPaused => "Execution admission is paused".into(),
        Wait::CapacityLimit { scope } => format!("Waiting for a slot at the {scope} limit"),
        Wait::RetryDelay { earliest } => format!("A retry is eligible at {earliest}"),
        Wait::AwaitingCompatibleRunner { template_id } => {
            format!("Waiting for a runner with template {template_id}")
        }
        Wait::AwaitingDispatch { template_id } => {
            format!("Waiting for dispatch on template {template_id}")
        }
        Wait::Prepare => "Prepare the draft for assignment".into(),
        Wait::ReadyForAssignment => "Ready to assign or claim".into(),
        Wait::InProgress => "The attempt is in progress".into(),
        Wait::NeedsAttention => "Check the task's latest activity".into(),
        Wait::Terminal {
            reason: Some(reason),
        } => format!("Execution ended · {reason:?}"),
        Wait::Terminal { reason: None } => "The task ended".into(),
    };
    let mut output = format!(
        "Task {} · {} · {detail}",
        report.task_id.0,
        report.status.as_str()
    );
    if let Some(actor) = report.next_actor {
        let identity = match actor {
            office_domain::Subject::Human { user_id } => format!("owner {}", user_id.0),
            office_domain::Subject::Agent { agent_id } => format!("agent {}", agent_id.0),
        };
        output.push_str(&format!("\nNext actor · {identity}"));
    }
    output
}

fn display_mutation(mutation: &TaskMutation) -> String {
    format!(
        "{} · {} · revision {}",
        mutation.task.task_id.0,
        mutation.task.status.as_str(),
        mutation.task.revision
    )
}

fn display_handoff(handoff: &TaskHandoff) -> String {
    format!(
        "{} · {:?} · {}",
        handoff.handoff_id, handoff.state, handoff.task_id.0
    )
}
