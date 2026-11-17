use chrono::{DateTime, Utc};
use office_domain::{
    Action, AgentId, BearerDigest, Resource, TeamId, TeamRole, TeamState, UserId, WorkspaceId,
};
use office_protocol::{
    DecideTeamInvite, InviteTeamMember, ListTeamInvites, Recipient, TeamInvitation,
    TeamInviteDecision, TeamInviteMutation, TeamInvitePage, TeamInviteState,
};
use serde_json::json;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::{TeamRow, bind_team, check_room, load_team, write_member};
use crate::{
    Store,
    data_scope::{authorize_scope, create_directed_scope},
    events::{Idempotency, append_event},
    identity::{
        IdentityError, WorkspaceContext, active_principal, require_permission, workspace_context,
    },
    inbox::save_cursor,
};

#[derive(sqlx::FromRow)]
struct OfferRow {
    id: Uuid,
    workspace_id: Uuid,
    team_id: Uuid,
    issuer_user_id: Uuid,
    issuer_agent_id: Option<Uuid>,
    recipient_user_id: Uuid,
    recipient_agent_id: Option<Uuid>,
    requested_role: String,
    state: String,
    expires_at: DateTime<Utc>,
    expired: bool,
}

const COLUMNS: &str = "*, expires_at <= transaction_timestamp() AS expired";

impl OfferRow {
    fn into_offer(self) -> Result<TeamInvitation, IdentityError> {
        let state = match self.state.as_str() {
            "pending" if self.expired => TeamInviteState::Expired,
            "pending" => TeamInviteState::Pending,
            "accepted" => TeamInviteState::Accepted,
            "declined" => TeamInviteState::Declined,
            _ => return Err(IdentityError::InvalidRole),
        };
        Ok(TeamInvitation {
            invitation_id: self.id,
            workspace_id: WorkspaceId(self.workspace_id),
            team_id: TeamId(self.team_id),
            issued_by: Recipient {
                user_id: UserId(self.issuer_user_id),
                agent_id: self.issuer_agent_id.map(AgentId),
            },
            recipient: Recipient {
                user_id: UserId(self.recipient_user_id),
                agent_id: self.recipient_agent_id.map(AgentId),
            },
            role: self
                .requested_role
                .parse()
                .map_err(|_| IdentityError::InvalidRole)?,
            state,
            expires_at: self.expires_at,
        })
    }
}

impl Store {
    pub async fn invite_team_member(
        &self,
        digest: &BearerDigest,
        input: InviteTeamMember,
    ) -> Result<TeamInviteMutation, IdentityError> {
        if input.role == TeamRole::Lead || !(1..i64::MAX).contains(&input.expected_revision) {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "team.invite",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let team = load_team(&mut tx, context, input.team_id, Action::ManageTeam, true).await?;
        if team.revision != input.expected_revision {
            return Err(IdentityError::RevisionMismatch {
                current: team.revision,
            });
        }
        if team.state != TeamState::Active {
            return Err(IdentityError::InvalidTransition);
        }
        if input.recipient.user_id == team.owner_user_id {
            return Err(IdentityError::InvalidInput);
        }
        active_principal(&mut tx, context, input.recipient).await?;
        bind_team(&mut tx, input.team_id, true).await?;

        let id = Uuid::new_v4();
        create_directed_scope(&mut tx, context, id, input.recipient).await?;
        let row = sqlx::query_as::<_, OfferRow>(&format!(
            "INSERT INTO team_invitations (id, workspace_id, team_id, issuer_user_id, issuer_agent_id,
                                           recipient_user_id, recipient_agent_id, requested_role)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING {COLUMNS}"
        )).bind(id).bind(input.workspace_id.0).bind(input.team_id.0).bind(context.access.member.0)
         .bind(context.agent_id().map(|id| id.0)).bind(input.recipient.user_id.0)
         .bind(input.recipient.agent_id.map(|id| id.0)).bind(input.role.as_str())
         .fetch_one(&mut *tx).await?;
        let result = record_offer(
            &mut tx,
            context,
            &idem,
            row.into_offer()?,
            None,
            "team.invited",
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn decide_team_invite(
        &self,
        digest: &BearerDigest,
        input: DecideTeamInvite,
    ) -> Result<TeamInviteMutation, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "team.invite_decide",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let invitation = sqlx::query_as::<_, OfferRow>(&format!(
            "SELECT {COLUMNS} FROM team_invitations WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
        ))
        .bind(input.workspace_id.0)
        .bind(input.invitation_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(IdentityError::Forbidden)?
        .into_offer()?;
        require_permission(
            context.actor,
            Action::DecideOffer,
            Resource::DirectedOffer {
                workspace: context.access,
                recipient_owner: invitation.recipient.user_id,
                recipient_agent: invitation.recipient.agent_id,
            },
        )?;
        authorize_scope(&mut tx, context, invitation.invitation_id, Action::ReadData).await?;
        if invitation.state != TeamInviteState::Pending {
            return Err(IdentityError::InvalidTransition);
        }
        let (state, event, team) = match input.decision {
            TeamInviteDecision::Accept => (
                "accepted",
                "team.invite_accepted",
                Some(join_team(&mut tx, context, &invitation).await?),
            ),
            TeamInviteDecision::Decline => ("declined", "team.invite_declined", None),
        };
        let updated = sqlx::query_as::<_, OfferRow>(&format!(
            "UPDATE team_invitations SET state = $3, decided_at = clock_timestamp(),
               decided_by_user_id = $4, decided_by_agent_id = $5
             WHERE workspace_id = $1 AND id = $2 RETURNING {COLUMNS}"
        ))
        .bind(input.workspace_id.0)
        .bind(input.invitation_id)
        .bind(state)
        .bind(context.access.member.0)
        .bind(context.agent_id().map(|id| id.0))
        .fetch_one(&mut *tx)
        .await?;
        let result =
            record_offer(&mut tx, context, &idem, updated.into_offer()?, team, event).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn list_team_invites(
        &self,
        digest: &BearerDigest,
        input: ListTeamInvites,
    ) -> Result<TeamInvitePage, IdentityError> {
        let limit = input.limit.unwrap_or(25);
        if !(1..=50).contains(&limit) {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let mut rows = sqlx::query_as::<_, OfferRow>(&format!(
            "SELECT {COLUMNS} FROM team_invitations WHERE workspace_id = $1 AND ($2::uuid IS NULL OR id > $2)
             ORDER BY id LIMIT $3"
        )).bind(input.workspace_id.0).bind(input.after).bind(i64::from(limit) + 1)
         .fetch_all(&mut *tx).await?;
        let more = rows.len() > usize::from(limit);
        rows.truncate(usize::from(limit));
        let mut invitations = Vec::with_capacity(rows.len());
        for row in rows {
            authorize_scope(&mut tx, context, row.id, Action::ReadData).await?;
            invitations.push(row.into_offer()?);
        }
        let next_after = invitations
            .last()
            .filter(|_| more)
            .map(|offer| offer.invitation_id);
        tx.commit().await?;
        Ok(TeamInvitePage {
            invitations,
            next_after,
        })
    }
}

async fn join_team(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    invitation: &TeamInvitation,
) -> Result<i64, IdentityError> {
    sqlx::query("SELECT set_config('office.team_invitation_id', $1, true)")
        .bind(invitation.invitation_id.to_string())
        .execute(&mut **tx)
        .await?;
    let current = sqlx::query_as::<_, TeamRow>(
        "SELECT * FROM teams WHERE workspace_id = $1 AND id = $2 FOR NO KEY UPDATE",
    )
    .bind(invitation.workspace_id.0)
    .bind(invitation.team_id.0)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(IdentityError::Forbidden)?
    .into_team()?;
    if current.state != TeamState::Active
        || current.owner_user_id != invitation.issued_by.user_id
        || invitation
            .issued_by
            .agent_id
            .is_some_and(|lead| current.lead_agent_id != Some(lead))
    {
        return Err(IdentityError::Forbidden);
    }
    active_principal(tx, context, invitation.issued_by).await?;
    active_principal(tx, context, invitation.recipient).await?;
    bind_team(tx, invitation.team_id, true).await?;
    if let Some(lead) = invitation.issued_by.agent_id {
        let delegated = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM team_memberships WHERE workspace_id = $1 AND team_id = $2
               AND user_id = $3 AND agent_id = $4 AND role = 'lead' AND revoked_at IS NULL)",
        )
        .bind(invitation.workspace_id.0)
        .bind(invitation.team_id.0)
        .bind(invitation.issued_by.user_id.0)
        .bind(lead.0)
        .fetch_one(&mut **tx)
        .await?;
        if !delegated {
            return Err(IdentityError::Forbidden);
        }
    }
    check_room(
        tx,
        invitation.workspace_id,
        invitation.team_id,
        invitation.recipient,
    )
    .await?;
    write_member(
        tx,
        invitation.workspace_id,
        invitation.team_id,
        invitation.recipient,
        invitation.role,
    )
    .await?;
    Ok(sqlx::query_scalar::<_, i64>(
        "UPDATE teams SET revision = revision + 1 WHERE workspace_id = $1 AND id = $2 RETURNING revision",
    ).bind(invitation.workspace_id.0).bind(invitation.team_id.0).fetch_one(&mut **tx).await?)
}

async fn record_offer(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    idem: &Idempotency,
    invitation: TeamInvitation,
    joined_team_revision: Option<i64>,
    event: &str,
) -> Result<TeamInviteMutation, IdentityError> {
    let scope = invitation.invitation_id;
    let seq = append_event(
        tx,
        context,
        scope,
        event,
        json!({
            "invitation_id": scope, "team_id": invitation.team_id,
            "state": invitation.state, "recipient": invitation.recipient,
        }),
        None,
    )
    .await?;
    let result = TeamInviteMutation {
        invitation,
        joined_team_revision,
        event_cursor: save_cursor(tx, context, seq).await?,
    };
    idem.record(tx, scope, &result).await?;
    Ok(result)
}
