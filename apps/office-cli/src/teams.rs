use std::{path::PathBuf, process::ExitCode};

use clap::{Args, Subcommand};
use office_domain::{TeamId, WorkspaceId};
use office_protocol::{
    CreateTeam, DecideTeamInvite, GetTeam, InviteTeamMember, ListTeamInvites, ListTeams,
    SetTeamMember, TeamInviteMutation, TeamInvitePage, TeamMutation, TeamPage, TeamView,
    UpdateTeam,
};
use reqwest::Url;
use uuid::Uuid;

use crate::{
    identity::{authenticated_post, post_file},
    print_receipt,
};

#[derive(Subcommand)]
pub(crate) enum TeamCommand {
    /// Create an expiring offer of membership for an outside principal.
    Invite(TeamFile),
    /// Accept or decline an invitation.
    Respond(TeamFile),
    /// Leave the team, remove a member, or appoint a lead from JSON.
    Update(TeamFile),
    /// List incoming and sent team invitations.
    Invitations {
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
    Create {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        name: String,
        #[arg(long)]
        idempotency_key: Option<Uuid>,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
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
    List {
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
    SetMember {
        #[arg(long)]
        file: PathBuf,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args)]
pub(crate) struct TeamFile {
    #[arg(long)]
    file: PathBuf,
    #[arg(long, default_value = "http://127.0.0.1:8400")]
    url: Url,
    #[arg(long)]
    json: bool,
}

pub(crate) async fn run(command: TeamCommand) -> ExitCode {
    match command {
        TeamCommand::Update(input) => print_receipt(
            post_file::<UpdateTeam, TeamMutation>(&input.file, input.url, "/v1/teams/update").await,
            input.json,
            display_mutation,
        ),
        TeamCommand::Invite(input) => print_receipt(
            post_file::<InviteTeamMember, TeamInviteMutation>(
                &input.file,
                input.url,
                "/v1/teams/invite",
            )
            .await,
            input.json,
            display_invite,
        ),
        TeamCommand::Respond(input) => print_receipt(
            post_file::<DecideTeamInvite, TeamInviteMutation>(
                &input.file,
                input.url,
                "/v1/team-invitations/respond",
            )
            .await,
            input.json,
            display_invite,
        ),
        TeamCommand::Invitations {
            workspace,
            after,
            limit,
            url,
            json,
        } => print_receipt(
            authenticated_post::<TeamInvitePage>(
                url,
                "/v1/team-invitations/list",
                &ListTeamInvites {
                    workspace_id: WorkspaceId(workspace),
                    after,
                    limit,
                },
            )
            .await,
            json,
            |page| {
                let mut lines: Vec<_> = page
                    .invitations
                    .iter()
                    .map(|invite| {
                        format!(
                            "{} · {:?} · {}",
                            invite.invitation_id, invite.state, invite.team_id.0
                        )
                    })
                    .collect();
                if let Some(after) = page.next_after {
                    lines.push(format!("Next page after {after}"));
                }
                lines.join("\n")
            },
        ),
        TeamCommand::Create {
            workspace,
            name,
            idempotency_key,
            url,
            json,
        } => print_receipt(
            authenticated_post::<TeamMutation>(
                url,
                "/v1/teams",
                &CreateTeam {
                    workspace_id: WorkspaceId(workspace),
                    name,
                    idempotency_key: idempotency_key.unwrap_or_else(Uuid::new_v4),
                },
            )
            .await,
            json,
            display_mutation,
        ),
        TeamCommand::Get {
            workspace,
            id,
            url,
            json,
        } => print_receipt(
            authenticated_post::<TeamView>(
                url,
                "/v1/teams/read",
                &GetTeam {
                    workspace_id: WorkspaceId(workspace),
                    team_id: TeamId(id),
                },
            )
            .await,
            json,
            |view| {
                let mut lines = vec![format!("{} · {}", view.team.team_id.0, view.team.name)];
                lines.extend(view.members.iter().map(|item| {
                    let principal = item
                        .member
                        .agent_id
                        .map_or(item.member.user_id.0, |agent| agent.0);
                    format!("{} · {}", principal, item.role.as_str())
                }));
                lines.join("\n")
            },
        ),
        TeamCommand::List {
            workspace,
            after,
            limit,
            url,
            json,
        } => print_receipt(
            authenticated_post::<TeamPage>(
                url,
                "/v1/teams/list",
                &ListTeams {
                    workspace_id: WorkspaceId(workspace),
                    after: after.map(TeamId),
                    limit,
                },
            )
            .await,
            json,
            |page| {
                let mut lines: Vec<_> = page
                    .teams
                    .iter()
                    .map(|team| format!("{} · {}", team.team_id.0, team.name))
                    .collect();
                if let Some(after) = page.next_after {
                    lines.push(format!("Next page after {}", after.0));
                }
                lines.join("\n")
            },
        ),
        TeamCommand::SetMember { file, url, json } => print_receipt(
            post_file::<SetTeamMember, TeamMutation>(&file, url, "/v1/teams/member").await,
            json,
            display_mutation,
        ),
    }
}

fn display_mutation(mutation: &TeamMutation) -> String {
    format!(
        "{} · {} · revision {}",
        mutation.team.team_id.0, mutation.team.name, mutation.team.revision
    )
}

fn display_invite(receipt: &TeamInviteMutation) -> String {
    format!(
        "{} · {:?} · {}",
        receipt.invitation.invitation_id, receipt.invitation.state, receipt.invitation.team_id.0
    )
}
