use std::{path::PathBuf, process::ExitCode};

use clap::{Args, Subcommand};
use office_domain::{TaskId, WorkspaceId};
use office_protocol::{
    ClaimPaths, GetPathHandoff, ListPathClaims, PathClaimPage, PathClaimReceipt, PathHandoff,
    PathHandoffReceipt, ReleasePathClaim, RenewPathClaim, RequestPathHandoff, RespondPathHandoff,
};
use reqwest::Url;
use uuid::Uuid;

use crate::{
    identity::{authenticated_post, post_file},
    print_receipt,
};

#[derive(Subcommand)]
pub(crate) enum ClaimsCommand {
    /// Acquire or atomically replace your run's path claim.
    Acquire(ClaimFile),
    /// Renew a live claim at its current fence.
    Renew(ClaimFile),
    /// Release your entire claim set.
    Release(ClaimFile),
    /// Request a handoff from the holder of the requested paths.
    RequestHandoff(ClaimFile),
    /// Offer, reject or cancel a path handoff.
    RespondHandoff(ClaimFile),
    /// Inspect an addressed handoff.
    Handoff {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        id: Uuid,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// List claim history visible to your credential.
    List {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        checkout: Uuid,
        #[arg(long)]
        task: Option<Uuid>,
        #[arg(long)]
        before_fence: Option<i64>,
        #[arg(long)]
        limit: Option<u16>,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args)]
pub(crate) struct ClaimFile {
    #[arg(long)]
    file: PathBuf,
    #[arg(long, default_value = "http://127.0.0.1:8400")]
    url: Url,
    #[arg(long)]
    json: bool,
}

pub(crate) async fn run(command: ClaimsCommand) -> ExitCode {
    let (receipt, json) = match command {
        ClaimsCommand::Acquire(input) => (
            post_file::<ClaimPaths, PathClaimReceipt>(
                &input.file,
                input.url,
                "/v1/path-claims/acquire",
            )
            .await,
            input.json,
        ),
        ClaimsCommand::Renew(input) => (
            post_file::<RenewPathClaim, PathClaimReceipt>(
                &input.file,
                input.url,
                "/v1/path-claims/renew",
            )
            .await,
            input.json,
        ),
        ClaimsCommand::Release(input) => (
            post_file::<ReleasePathClaim, PathClaimReceipt>(
                &input.file,
                input.url,
                "/v1/path-claims/release",
            )
            .await,
            input.json,
        ),
        ClaimsCommand::RequestHandoff(input) => {
            let output = post_file::<RequestPathHandoff, PathHandoffReceipt>(
                &input.file,
                input.url,
                "/v1/path-handoffs/request",
            )
            .await;
            return print_receipt(output, input.json, |receipt| {
                handoff_label(&receipt.handoff)
            });
        }
        ClaimsCommand::RespondHandoff(input) => {
            let output = post_file::<RespondPathHandoff, PathHandoffReceipt>(
                &input.file,
                input.url,
                "/v1/path-handoffs/respond",
            )
            .await;
            return print_receipt(output, input.json, |receipt| {
                handoff_label(&receipt.handoff)
            });
        }
        ClaimsCommand::Handoff {
            workspace,
            id,
            url,
            json,
        } => {
            let input = GetPathHandoff {
                workspace_id: WorkspaceId(workspace),
                handoff_id: id,
            };
            let output =
                authenticated_post::<PathHandoff>(url, "/v1/path-handoffs/read", &input).await;
            return print_receipt(output, json, handoff_label);
        }
        ClaimsCommand::List {
            workspace,
            checkout,
            task,
            before_fence,
            limit,
            url,
            json,
        } => {
            let input = ListPathClaims {
                workspace_id: WorkspaceId(workspace),
                checkout_id: checkout,
                task_id: task.map(TaskId),
                before_fence,
                limit,
            };
            let output =
                authenticated_post::<PathClaimPage>(url, "/v1/path-claims/list", &input).await;
            return print_receipt(output, json, |page| {
                format!("{} path claims", page.claims.len())
            });
        }
    };
    print_receipt(receipt, json, |entry| {
        format!(
            "Path claim {} fence {}",
            entry.claim.claim_id, entry.claim.claim_fence
        )
    })
}

fn handoff_label(handoff: &PathHandoff) -> String {
    format!(
        "Path handoff {} revision {}",
        handoff.handoff_id, handoff.revision
    )
}
