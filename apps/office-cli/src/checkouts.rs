use std::{path::PathBuf, process::ExitCode};

use clap::{Args, Subcommand};
use office_domain::WorkspaceId;
use office_protocol::{
    ApproveCheckoutReview, CheckoutContentHead, CheckoutReceipt, CheckoutRelease,
    CheckoutReleasePolicy, CheckoutReview, CheckoutReviewReceipt, CheckoutSnapshot,
    CreateCheckoutSnapshot, CreateProject, GetCheckout, GetCheckoutRelease, GetCheckoutReview,
    GetCheckoutSnapshot, GetProject, Project, ProjectCheckout, ProjectReceipt, RequestCheckout,
    RequestCheckoutRelease, SetCheckoutReleasePolicy,
};
use reqwest::Url;
use uuid::Uuid;

use crate::{
    identity::{authenticated_post, post_file},
    print_receipt,
};

#[derive(Subcommand)]
pub(crate) enum CheckoutCommand {
    /// Create a project with the specified visibility.
    CreateProject(CheckoutFile),
    /// Read project metadata.
    Project(Lookup),
    /// Request enrollment by a runner owned by the project's owner.
    Request(CheckoutFile),
    /// Read a checkout's registry state.
    Get(Lookup),
    /// Read the authoritative content head.
    Head(Lookup),
    /// Capture the current approved source manifest into an immutable WIP checkpoint.
    CreateSnapshot(CheckoutFile),
    /// Prepare the exact snapshot and test evidence for the task's reviewer.
    SnapshotReview(CheckoutFile),
    /// Approve the exact task result and its sealed source evidence.
    ApproveSnapshotReview(CheckoutFile),
    /// Get the required contract templates and accepted ref.
    ReleasePolicy(Lookup),
    /// Configure the required full-source contract templates.
    SetReleasePolicy(CheckoutFile),
    /// Request a verified accepted checkpoint.
    RequestRelease(CheckoutFile),
    /// Get accepted checkpoint status and proof.
    Release {
        #[arg(long)]
        release: Uuid,
        #[command(flatten)]
        checkout: Lookup,
    },
    /// Read an immutable snapshot's status and content proof.
    Snapshot {
        #[arg(long)]
        snapshot: Uuid,
        #[command(flatten)]
        checkout: Lookup,
    },
}

#[derive(Args)]
pub(crate) struct CheckoutFile {
    #[arg(long)]
    file: PathBuf,
    #[arg(long, default_value = "http://127.0.0.1:8400")]
    url: Url,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub(crate) struct Lookup {
    #[arg(long)]
    workspace: Uuid,
    #[arg(long)]
    id: Uuid,
    #[arg(long, default_value = "http://127.0.0.1:8400")]
    url: Url,
    #[arg(long)]
    json: bool,
}

pub(crate) async fn run(command: CheckoutCommand) -> ExitCode {
    match command {
        CheckoutCommand::ReleasePolicy(input) => print_receipt(
            authenticated_post::<CheckoutReleasePolicy>(
                input.url,
                "/v1/checkouts/releases/read-policy",
                &GetCheckout {
                    workspace_id: WorkspaceId(input.workspace),
                    checkout_id: input.id,
                },
            )
            .await,
            input.json,
            display_policy,
        ),
        CheckoutCommand::SetReleasePolicy(input) => print_receipt(
            post_file::<SetCheckoutReleasePolicy, CheckoutReleasePolicy>(
                &input.file,
                input.url,
                "/v1/checkouts/releases/policy",
            )
            .await,
            input.json,
            display_policy,
        ),
        CheckoutCommand::RequestRelease(input) => print_receipt(
            post_file::<RequestCheckoutRelease, CheckoutRelease>(
                &input.file,
                input.url,
                "/v1/checkouts/releases",
            )
            .await,
            input.json,
            display_release,
        ),
        CheckoutCommand::Release { release, checkout } => print_receipt(
            authenticated_post::<CheckoutRelease>(
                checkout.url,
                "/v1/checkouts/releases/read",
                &GetCheckoutRelease {
                    workspace_id: WorkspaceId(checkout.workspace),
                    checkout_id: checkout.id,
                    release_id: release,
                },
            )
            .await,
            checkout.json,
            display_release,
        ),
        CheckoutCommand::SnapshotReview(input) => print_receipt(
            post_file::<GetCheckoutReview, CheckoutReview>(
                &input.file,
                input.url,
                "/v1/checkouts/snapshots/review",
            )
            .await,
            input.json,
            |review| {
                format!(
                    "Review {}  snapshot {}  {} verified checks",
                    review.revision_sha256,
                    review.content.snapshot_id,
                    review.content.checks.len()
                )
            },
        ),
        CheckoutCommand::ApproveSnapshotReview(input) => print_receipt(
            post_file::<ApproveCheckoutReview, CheckoutReviewReceipt>(
                &input.file,
                input.url,
                "/v1/checkouts/snapshots/approve-review",
            )
            .await,
            input.json,
            |receipt| {
                format!(
                    "Approved review {}  {}",
                    receipt.review.revision_sha256, receipt.approved_at
                )
            },
        ),
        CheckoutCommand::Head(input) => print_receipt(
            authenticated_post::<CheckoutContentHead>(
                input.url,
                "/v1/checkouts/head",
                &GetCheckout {
                    workspace_id: WorkspaceId(input.workspace),
                    checkout_id: input.id,
                },
            )
            .await,
            input.json,
            |head| {
                format!(
                    "Checkout revision {}  {}",
                    head.checkout_revision, head.source_sha256
                )
            },
        ),
        CheckoutCommand::CreateSnapshot(input) => print_receipt(
            post_file::<CreateCheckoutSnapshot, CheckoutSnapshot>(
                &input.file,
                input.url,
                "/v1/checkouts/snapshots",
            )
            .await,
            input.json,
            display_snapshot,
        ),
        CheckoutCommand::Snapshot { snapshot, checkout } => print_receipt(
            authenticated_post::<CheckoutSnapshot>(
                checkout.url,
                "/v1/checkouts/snapshots/read",
                &GetCheckoutSnapshot {
                    workspace_id: WorkspaceId(checkout.workspace),
                    checkout_id: checkout.id,
                    snapshot_id: snapshot,
                },
            )
            .await,
            checkout.json,
            display_snapshot,
        ),
        CheckoutCommand::CreateProject(input) => print_receipt(
            post_file::<CreateProject, ProjectReceipt>(&input.file, input.url, "/v1/projects")
                .await,
            input.json,
            |receipt| display_project(&receipt.project),
        ),
        CheckoutCommand::Project(input) => print_receipt(
            authenticated_post::<Project>(
                input.url,
                "/v1/projects/read",
                &GetProject {
                    workspace_id: WorkspaceId(input.workspace),
                    project_id: input.id,
                },
            )
            .await,
            input.json,
            display_project,
        ),
        CheckoutCommand::Request(input) => print_receipt(
            post_file::<RequestCheckout, CheckoutReceipt>(&input.file, input.url, "/v1/checkouts")
                .await,
            input.json,
            |receipt| display_checkout(&receipt.checkout),
        ),
        CheckoutCommand::Get(input) => print_receipt(
            authenticated_post::<ProjectCheckout>(
                input.url,
                "/v1/checkouts/read",
                &GetCheckout {
                    workspace_id: WorkspaceId(input.workspace),
                    checkout_id: input.id,
                },
            )
            .await,
            input.json,
            display_checkout,
        ),
    }
}

fn display_policy(policy: &CheckoutReleasePolicy) -> String {
    format!(
        "Release policy revision {}  {} required templates  accepted commit {}",
        policy.revision,
        policy.required_template_ids.len(),
        policy.accepted_commit_sha256.as_deref().unwrap_or("(none)")
    )
}

fn display_release(release: &CheckoutRelease) -> String {
    format!(
        "Release {}  {:?}  source revision {}  {} approved changes",
        release.release_id, release.state, release.head.checkout_revision, release.approved_changes
    )
}

fn display_snapshot(value: &CheckoutSnapshot) -> String {
    format!(
        "Snapshot {}  {:?}  revision {}  {}",
        value.snapshot_id, value.state, value.head.checkout_revision, value.head.source_sha256
    )
}

fn display_project(project: &Project) -> String {
    format!("{} · {}", project.project_id, project.name)
}

fn display_checkout(checkout: &ProjectCheckout) -> String {
    format!(
        "{} · {:?} · runner {} · revision {}",
        checkout.checkout_id, checkout.state, checkout.host_runner_id.0, checkout.revision
    )
}
