use std::{path::PathBuf, process::ExitCode};

use clap::{Args, Subcommand};
use office_domain::WorkspaceId;
use office_protocol::{
    CreateRuntimeTemplate, DelegateTask, DelegationGrant, DelegationReceipt, GetDelegation,
    IssueDelegation, RevokeDelegation, RuntimeTemplate,
};
use reqwest::Url;
use uuid::Uuid;

use crate::{
    identity::{authenticated_post, post_file},
    print_receipt,
};

#[derive(Subcommand)]
pub(crate) enum DelegationCommand {
    /// Register a runtime template owned by the current human principal.
    CreateRuntime(InputFile),
    /// Issue an envelope for your team's lead and an existing task.
    Issue(InputFile),
    /// Create a subtask with a narrower grant.
    Delegate(InputFile),
    /// Revoke further use of a grant and its descendants.
    Revoke(InputFile),
    /// Inspect a grant and its usage counters.
    Get {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        id: Uuid,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args)]
pub(crate) struct InputFile {
    #[arg(long)]
    file: PathBuf,
    #[arg(long, default_value = "http://127.0.0.1:8400")]
    url: Url,
    #[arg(long)]
    json: bool,
}

pub(crate) async fn run(command: DelegationCommand) -> ExitCode {
    match command {
        DelegationCommand::CreateRuntime(input) => print_receipt(
            post_file::<CreateRuntimeTemplate, RuntimeTemplate>(
                &input.file,
                input.url,
                "/v1/runtime-templates",
            )
            .await,
            input.json,
            |template| format!("{} · {}", template.template_id, template.name),
        ),
        DelegationCommand::Issue(input) => print_receipt(
            post_file::<IssueDelegation, DelegationReceipt>(
                &input.file,
                input.url,
                "/v1/delegations",
            )
            .await,
            input.json,
            |receipt| display(&receipt.grant),
        ),
        DelegationCommand::Delegate(input) => print_receipt(
            post_file::<DelegateTask, DelegationReceipt>(
                &input.file,
                input.url,
                "/v1/delegations/delegate",
            )
            .await,
            input.json,
            |receipt| display(&receipt.grant),
        ),
        DelegationCommand::Revoke(input) => print_receipt(
            post_file::<RevokeDelegation, DelegationReceipt>(
                &input.file,
                input.url,
                "/v1/delegations/revoke",
            )
            .await,
            input.json,
            |receipt| display(&receipt.grant),
        ),
        DelegationCommand::Get {
            workspace,
            id,
            url,
            json,
        } => print_receipt(
            authenticated_post::<DelegationGrant>(
                url,
                "/v1/delegations/read",
                &GetDelegation {
                    workspace_id: WorkspaceId(workspace),
                    grant_id: id,
                },
            )
            .await,
            json,
            display,
        ),
    }
}

fn display(grant: &DelegationGrant) -> String {
    let state = if grant.revoked_at.is_some() {
        "revoked"
    } else {
        "expires"
    };
    format!(
        "{} · {} · {state} {} · attempts {}/{} · subtasks {}/{}",
        grant.grant_id,
        grant.task_id.0,
        grant.revoked_at.unwrap_or(grant.limits.expires_at),
        grant.attempts_used,
        grant.limits.max_attempts,
        grant.subtasks_used,
        grant.limits.max_subtasks
    )
}
