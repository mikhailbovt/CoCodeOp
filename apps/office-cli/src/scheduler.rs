use std::{path::PathBuf, process::ExitCode};

use clap::{Args, Subcommand};
use office_domain::{AgentId, RunId, RunnerId, TaskId, WorkspaceId};
use office_protocol::{
    ActivationPolicy, AnswerManagedInput, CancelExecution, ExecutionBudget, ExecutionBudgetReceipt,
    ExecutionControl, ExecutionJob, ExecutionReceipt, GetActivationPolicy, GetExecution,
    GetExecutionBudget, GetExecutionControl, GetManagedInput, GetRunnerProfile, ManagedInput,
    ManagedInputReceipt, QueueExecution, RunnerProfile, SetActivationPolicy, SetExecutionBudget,
    SetExecutionControl, SetWorkspaceBudget, WorkspaceBudget,
};
use reqwest::Url;
use uuid::Uuid;

use crate::{
    identity::{authenticated_post, post_file},
    print_receipt,
};

#[derive(Subcommand)]
pub(crate) enum SchedulerCommand {
    /// Configure a shared execution budget scoped to named workspace members.
    SetWorkspaceBudget(JobFile),
    /// Show the shared execution budget visible to the current owner.
    WorkspaceBudget {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Read the current execution limits and mode using an operator credential.
    Control(JobFile),
    /// Change limits, pause new work, stop active attempts, or resume admission.
    SetControl(JobFile),
    /// Queue managed execution for a named worker.
    Queue(JobFile),
    /// Configure an agent's opt-in for automatic turns from JSON.
    SetActivation(JobFile),
    /// Answer a managed execution's question and queue its next attempt from JSON.
    AnswerInput(JobFile),
    /// Inspect the question and checkpoint from a paused attempt.
    Input {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        task: Uuid,
        #[arg(long)]
        run: Uuid,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Inspect whether an agent is interactive or managed.
    Activation {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        agent: Uuid,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Cancel a queued execution and release its reservation.
    Cancel(JobFile),
    /// Read a managed execution by ID.
    Job {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        id: Uuid,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Configure the current owner's execution budget from JSON.
    SetBudget {
        #[arg(long)]
        file: PathBuf,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Read the current owner's budget and usage.
    Budget {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Read an execution profile for a runner you own.
    Profile {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        runner: Uuid,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args)]
pub(crate) struct JobFile {
    #[arg(long)]
    file: PathBuf,
    #[arg(long, default_value = "http://127.0.0.1:8400")]
    url: Url,
    #[arg(long)]
    json: bool,
}

pub(crate) async fn run(command: SchedulerCommand) -> ExitCode {
    match command {
        SchedulerCommand::SetWorkspaceBudget(args) => print_receipt(
            post_file::<SetWorkspaceBudget, WorkspaceBudget>(
                &args.file,
                args.url,
                "/v1/workspace-budget/set",
            )
            .await,
            args.json,
            workspace_budget_label,
        ),
        SchedulerCommand::WorkspaceBudget {
            workspace,
            url,
            json,
        } => print_receipt(
            authenticated_post::<Option<WorkspaceBudget>>(
                url,
                "/v1/workspace-budget/read",
                &GetExecutionBudget {
                    workspace_id: WorkspaceId(workspace),
                },
            )
            .await,
            json,
            |budget| {
                budget.as_ref().map_or_else(
                    || "No visible shared execution budget".into(),
                    workspace_budget_label,
                )
            },
        ),
        SchedulerCommand::Control(args) => print_receipt(
            post_file::<GetExecutionControl, ExecutionControl>(
                &args.file,
                args.url,
                "/v1/execution-control/read",
            )
            .await,
            args.json,
            control_label,
        ),
        SchedulerCommand::SetControl(args) => print_receipt(
            post_file::<SetExecutionControl, ExecutionControl>(
                &args.file,
                args.url,
                "/v1/execution-control/set",
            )
            .await,
            args.json,
            control_label,
        ),
        SchedulerCommand::AnswerInput(args) => print_receipt(
            post_file::<AnswerManagedInput, ManagedInputReceipt>(
                &args.file,
                args.url,
                "/v1/executions/input/answer",
            )
            .await,
            args.json,
            |answer| job_label(&answer.job),
        ),
        SchedulerCommand::Input {
            workspace,
            task,
            run,
            url,
            json,
        } => print_receipt(
            authenticated_post::<ManagedInput>(
                url,
                "/v1/executions/input/read",
                &GetManagedInput {
                    workspace_id: WorkspaceId(workspace),
                    task_id: TaskId(task),
                    run_id: RunId(run),
                },
            )
            .await,
            json,
            |input| {
                format!(
                    "{} · {:?} · checkpoint {}\n{}",
                    input.run_id.0, input.state, input.checkpoint_id, input.question
                )
            },
        ),
        SchedulerCommand::SetActivation(args) => print_receipt(
            post_file::<SetActivationPolicy, ActivationPolicy>(
                &args.file,
                args.url,
                "/v1/activation/set",
            )
            .await,
            args.json,
            activation_label,
        ),
        SchedulerCommand::Activation {
            workspace,
            agent,
            url,
            json,
        } => print_receipt(
            authenticated_post::<ActivationPolicy>(
                url,
                "/v1/activation/read",
                &GetActivationPolicy {
                    workspace_id: WorkspaceId(workspace),
                    agent_id: AgentId(agent),
                },
            )
            .await,
            json,
            activation_label,
        ),
        SchedulerCommand::Queue(args) => {
            let response = post_file::<QueueExecution, ExecutionReceipt>(
                &args.file,
                args.url,
                "/v1/executions/queue",
            )
            .await;
            print_receipt(response, args.json, |receipt| job_label(&receipt.job))
        }
        SchedulerCommand::Cancel(args) => {
            let response = post_file::<CancelExecution, ExecutionReceipt>(
                &args.file,
                args.url,
                "/v1/executions/cancel",
            )
            .await;
            print_receipt(response, args.json, |receipt| job_label(&receipt.job))
        }
        SchedulerCommand::Job {
            workspace,
            id,
            url,
            json,
        } => {
            let input = GetExecution {
                workspace_id: WorkspaceId(workspace),
                job_id: id,
            };
            let response =
                authenticated_post::<ExecutionJob>(url, "/v1/executions/read", &input).await;
            print_receipt(response, json, job_label)
        }
        SchedulerCommand::SetBudget { file, url, json } => {
            let response = post_file::<SetExecutionBudget, ExecutionBudgetReceipt>(
                &file,
                url,
                "/v1/execution-budget/set",
            )
            .await;
            print_receipt(response, json, |receipt| budget_label(&receipt.budget))
        }
        SchedulerCommand::Budget {
            workspace,
            url,
            json,
        } => {
            let query = GetExecutionBudget {
                workspace_id: WorkspaceId(workspace),
            };
            let response = authenticated_post::<Option<ExecutionBudget>>(
                url,
                "/v1/execution-budget/read",
                &query,
            )
            .await;
            print_receipt(response, json, |budget| {
                budget.as_ref().map_or_else(
                    || "Execution budget is awaiting owner configuration".into(),
                    budget_label,
                )
            })
        }
        SchedulerCommand::Profile {
            workspace,
            runner,
            url,
            json,
        } => {
            let query = GetRunnerProfile {
                workspace_id: WorkspaceId(workspace),
                runner_id: RunnerId(runner),
            };
            let response =
                authenticated_post::<RunnerProfile>(url, "/v1/runner-profiles/read", &query).await;
            print_receipt(response, json, |profile| {
                format!(
                    "Runner {} capacity {} expires {}",
                    profile.runner_id.0, profile.capacity, profile.expires_at
                )
            })
        }
    }
}

fn workspace_budget_label(budget: &WorkspaceBudget) -> String {
    format!(
        "{} reserved · {} charged · {} limit · {} eligible owners · revision {}",
        budget.reserved_units,
        budget.charged_units,
        budget.limit_units,
        budget.member_user_ids.len(),
        budget.revision
    )
}

fn control_label(control: &ExecutionControl) -> String {
    let limits = control.limits;
    let reason = control.reason.as_deref().unwrap_or("Default limits");
    format!(
        "{:?} · {} · revision {} · {} concurrent · {} outstanding\n{reason}",
        control.scope,
        control.mode.as_str(),
        control.revision,
        limits.max_concurrent,
        limits.max_outstanding,
    )
}

fn activation_label(view: &ActivationPolicy) -> String {
    match &view.managed {
        Some(plan) => format!(
            "{} · managed revision {} · runner {}",
            view.agent_id.0, view.revision, plan.runner_id.0
        ),
        None => format!(
            "{} · interactive revision {}",
            view.agent_id.0, view.revision
        ),
    }
}

fn job_label(job: &ExecutionJob) -> String {
    format!(
        "Execution {} for task {} is {:?} at revision {}",
        job.job_id, job.task_id.0, job.state, job.revision
    )
}

fn budget_label(budget: &ExecutionBudget) -> String {
    format!(
        "Budget {} units, {} charged, {} reserved",
        budget.limit_units, budget.charged_units, budget.reserved_units
    )
}
