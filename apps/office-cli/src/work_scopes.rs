use std::{path::PathBuf, process::ExitCode};

use clap::{Args, Subcommand};
use office_domain::WorkspaceId;
use office_protocol::{
    AccessRequest, AccessRequestReceipt, Area, AreaReceipt, CheckoutGrant, CheckoutGrantReceipt,
    CommandTemplateReceipt, CreateCommandTemplate, DefineArea, GetAccessRequest, GetArea,
    GetCheckoutGrant, GetWorkScope, IssueCheckoutGrant, RequestAccess, RespondAccess,
    RevokeCheckoutGrant, RevokeWorkScope, ScopeRevocationReceipt, SetTaskScope, WorkScope,
    WorkScopeReceipt,
};
use reqwest::Url;
use uuid::Uuid;

use crate::{
    identity::{authenticated_post, post_file},
    print_receipt,
};

#[derive(Subcommand)]
pub(crate) enum ScopeCommand {
    /// Queue a revocation and close the task's current scope to new operations.
    Revoke(ScopeFile),
    /// Request a change to task permissions at the current scope revision.
    RequestAccess(ScopeFile),
    /// Decide or cancel an access request from JSON.
    RespondAccess(ScopeFile),
    /// Inspect a specific access request and its approvals.
    AccessRequest {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        id: Uuid,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Define or revise a checkout area's ownership or shared approval rule.
    DefineArea(ScopeFile),
    /// Read an area under its owner's credential.
    Area {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        id: Uuid,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Propose a new permission revision for a coding task.
    Set(ScopeFile),
    /// Read the most recent scope or an exact version for a task.
    Get {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        task: Uuid,
        #[arg(long)]
        revision: Option<i64>,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Issue an owner envelope for a named lead.
    Grant(ScopeFile),
    /// Inspect an authorized envelope.
    GetGrant {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        id: Uuid,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Revoke an owner envelope.
    RevokeGrant(ScopeFile),
    /// Register a checkout command template.
    CreateCommand(ScopeFile),
}

#[derive(Args)]
pub(crate) struct ScopeFile {
    #[arg(long)]
    file: PathBuf,
    #[arg(long, default_value = "http://127.0.0.1:8400")]
    url: Url,
    #[arg(long)]
    json: bool,
}

pub(crate) async fn run(command: ScopeCommand) -> ExitCode {
    match command {
        ScopeCommand::Revoke(input) => print_receipt(
            post_file::<RevokeWorkScope, ScopeRevocationReceipt>(
                &input.file,
                input.url,
                "/v1/work-scopes/revoke",
            )
            .await,
            input.json,
            |receipt| display_scope(&receipt.scope),
        ),
        ScopeCommand::RequestAccess(input) => print_receipt(
            post_file::<RequestAccess, AccessRequestReceipt>(
                &input.file,
                input.url,
                "/v1/access-requests",
            )
            .await,
            input.json,
            |receipt| display_request(&receipt.request),
        ),
        ScopeCommand::RespondAccess(input) => print_receipt(
            post_file::<RespondAccess, AccessRequestReceipt>(
                &input.file,
                input.url,
                "/v1/access-requests/respond",
            )
            .await,
            input.json,
            |receipt| display_request(&receipt.request),
        ),
        ScopeCommand::AccessRequest {
            workspace,
            id,
            url,
            json,
        } => print_receipt(
            authenticated_post::<AccessRequest>(
                url,
                "/v1/access-requests/read",
                &GetAccessRequest {
                    workspace_id: WorkspaceId(workspace),
                    access_request_id: id,
                },
            )
            .await,
            json,
            display_request,
        ),
        ScopeCommand::DefineArea(input) => print_receipt(
            post_file::<DefineArea, AreaReceipt>(&input.file, input.url, "/v1/checkout-areas")
                .await,
            input.json,
            |receipt| display_area(&receipt.area),
        ),
        ScopeCommand::Area {
            workspace,
            id,
            url,
            json,
        } => print_receipt(
            authenticated_post::<Area>(
                url,
                "/v1/checkout-areas/read",
                &GetArea {
                    workspace_id: WorkspaceId(workspace),
                    area_id: id,
                },
            )
            .await,
            json,
            display_area,
        ),
        ScopeCommand::Set(input) => print_receipt(
            post_file::<SetTaskScope, WorkScopeReceipt>(&input.file, input.url, "/v1/work-scopes")
                .await,
            input.json,
            |receipt| display_scope(&receipt.scope),
        ),
        ScopeCommand::Get {
            workspace,
            task,
            revision,
            url,
            json,
        } => print_receipt(
            authenticated_post::<WorkScope>(
                url,
                "/v1/work-scopes/read",
                &GetWorkScope {
                    workspace_id: WorkspaceId(workspace),
                    task_id: office_domain::TaskId(task),
                    revision,
                },
            )
            .await,
            json,
            display_scope,
        ),
        ScopeCommand::Grant(input) => print_receipt(
            post_file::<IssueCheckoutGrant, CheckoutGrantReceipt>(
                &input.file,
                input.url,
                "/v1/checkout-grants",
            )
            .await,
            input.json,
            |receipt| display_grant(&receipt.grant),
        ),
        ScopeCommand::GetGrant {
            workspace,
            id,
            url,
            json,
        } => print_receipt(
            authenticated_post::<CheckoutGrant>(
                url,
                "/v1/checkout-grants/read",
                &GetCheckoutGrant {
                    workspace_id: WorkspaceId(workspace),
                    grant_id: id,
                },
            )
            .await,
            json,
            display_grant,
        ),
        ScopeCommand::RevokeGrant(input) => print_receipt(
            post_file::<RevokeCheckoutGrant, CheckoutGrantReceipt>(
                &input.file,
                input.url,
                "/v1/checkout-grants/revoke",
            )
            .await,
            input.json,
            |receipt| display_grant(&receipt.grant),
        ),
        ScopeCommand::CreateCommand(input) => print_receipt(
            post_file::<CreateCommandTemplate, CommandTemplateReceipt>(
                &input.file,
                input.url,
                "/v1/command-templates",
            )
            .await,
            input.json,
            |receipt| format!("{} · {}", receipt.template_id, receipt.template.name),
        ),
    }
}

fn display_grant(grant: &CheckoutGrant) -> String {
    format!(
        "{} · checkout {} · revision {} · {}",
        grant.grant_id,
        grant.checkout_id,
        grant.revision,
        grant
            .revoked_at
            .map(|when| format!("revoked {when}"))
            .unwrap_or_else(|| format!("expires {}", grant.expires_at))
    )
}

fn display_scope(scope: &WorkScope) -> String {
    format!(
        "{} · task {} · revision {} · {:?}",
        scope.definition.scope_id,
        scope.definition.task_id.0,
        scope.definition.revision,
        scope.state
    )
}

fn display_area(area: &Area) -> String {
    format!(
        "{} · {} · revision {}",
        area.area_id, area.name, area.revision
    )
}

fn display_request(request: &AccessRequest) -> String {
    format!(
        "{} · {:?} · revision {} · approvals {}/{}",
        request.access_request_id,
        request.state,
        request.revision,
        request.approvals.len(),
        request.approvers.len()
    )
}
