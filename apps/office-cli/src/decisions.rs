use std::{path::PathBuf, process::ExitCode};

use clap::{Args, Subcommand};
use office_domain::WorkspaceId;
use office_protocol::{
    AreaDecision, Decision, GetAreaDecision, GetDecision, GetProposal, Proposal, ProposalReceipt,
    ProposeDecision, RespondProposal, ReviseProposal,
};
use reqwest::Url;
use uuid::Uuid;

use crate::{
    identity::{authenticated_post, post_file},
    print_receipt,
};

#[derive(Subcommand)]
pub(crate) enum DecisionCommand {
    /// Open a proposal.
    Propose(ProposalFile),
    /// Publish a new version with fresh votes.
    Revise(ProposalFile),
    /// Approve, reject or request changes to a proposal.
    Respond(ProposalFile),
    /// Read a proposal and all responses to its specified revision.
    Proposal {
        #[command(flatten)]
        read: ReadDecision,
        #[arg(long)]
        revision: Option<i64>,
    },
    /// Read an accepted decision.
    Get(ReadDecision),
    /// Read the current decision generation for a shared area.
    Area {
        #[command(flatten)]
        read: ReadDecision,
        #[arg(long)]
        checkout: Uuid,
    },
}

#[derive(Args)]
pub(crate) struct ReadDecision {
    #[arg(long)]
    workspace: Uuid,
    #[arg(long)]
    id: Uuid,
    #[arg(long, default_value = "http://127.0.0.1:8400")]
    url: Url,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub(crate) struct ProposalFile {
    #[arg(long)]
    file: PathBuf,
    #[arg(long, default_value = "http://127.0.0.1:8400")]
    url: Url,
    #[arg(long)]
    json: bool,
}

pub(crate) async fn run(command: DecisionCommand) -> ExitCode {
    let (receipt, json) = match command {
        DecisionCommand::Propose(args) => (
            post_file::<ProposeDecision, ProposalReceipt>(&args.file, args.url, "/v1/proposals")
                .await,
            args.json,
        ),
        DecisionCommand::Revise(args) => (
            post_file::<ReviseProposal, ProposalReceipt>(
                &args.file,
                args.url,
                "/v1/proposals/revise",
            )
            .await,
            args.json,
        ),
        DecisionCommand::Respond(args) => (
            post_file::<RespondProposal, ProposalReceipt>(
                &args.file,
                args.url,
                "/v1/proposals/respond",
            )
            .await,
            args.json,
        ),
        DecisionCommand::Proposal { read, revision } => {
            let input = GetProposal {
                workspace_id: WorkspaceId(read.workspace),
                proposal_id: read.id,
                revision,
            };
            let output =
                authenticated_post::<Proposal>(read.url, "/v1/proposals/read", &input).await;
            return print_receipt(output, read.json, proposal_label);
        }
        DecisionCommand::Get(read) => {
            let input = GetDecision {
                workspace_id: WorkspaceId(read.workspace),
                decision_id: read.id,
            };
            let output =
                authenticated_post::<Decision>(read.url, "/v1/decisions/read", &input).await;
            return print_receipt(output, read.json, |decision| {
                format!(
                    "Decision {} from proposal revision {}",
                    decision.decision_id, decision.proposal_revision
                )
            });
        }
        DecisionCommand::Area { read, checkout } => {
            let input = GetAreaDecision {
                workspace_id: WorkspaceId(read.workspace),
                checkout_id: checkout,
                area_id: read.id,
            };
            let output =
                authenticated_post::<AreaDecision>(read.url, "/v1/decisions/area", &input).await;
            return print_receipt(output, read.json, |area| {
                format!(
                    "Area {} at decision generation {}",
                    area.area_id, area.generation
                )
            });
        }
    };
    print_receipt(receipt, json, |result| proposal_label(&result.proposal))
}

fn proposal_label(proposal: &Proposal) -> String {
    format!(
        "Proposal {} revision {}",
        proposal.proposal_id, proposal.revision
    )
}
