use office_domain::{
    Action, AgentId, AgentRole, BearerDigest, TeamId, TeamRole, TeamState, WorkspaceId,
};
use office_protocol::{Recipient, Team, TeamChange, TeamMember, TeamMutation, UpdateTeam};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::{
    TeamRow, bind_team, check_room, load_team, lock_team_task_creation, member_role, record_team,
    write_member,
};
use crate::{
    Store,
    events::Idempotency,
    identity::{IdentityError, WorkspaceContext, active_principal, workspace_context},
};

impl Store {
    pub async fn update_team(
        &self,
        digest: &BearerDigest,
        input: UpdateTeam,
    ) -> Result<TeamMutation, IdentityError> {
        if !(1..i64::MAX).contains(&input.expected_revision) {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "team.update",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let action = match input.change {
            TeamChange::Leave => {
                load_team(&mut tx, context, input.team_id, Action::LeaveTeam, false).await?;
                bind_custody(&mut tx, "office.team_leaving_id", input.team_id).await?;
                Action::LeaveTeam
            }
            TeamChange::RemoveMember { .. } => Action::ManageTeam,
            TeamChange::AppointLead { .. } => Action::AppointTeamLead,
        };
        lock_team_task_creation(&mut tx, input.team_id, true).await?;
        let team = load_team(&mut tx, context, input.team_id, action, true).await?;
        if team.revision != input.expected_revision {
            return Err(IdentityError::RevisionMismatch {
                current: team.revision,
            });
        }
        bind_team(&mut tx, input.team_id, true).await?;

        let result = match input.change {
            TeamChange::Leave => detach(&mut tx, context, &idem, team, context.recipient()).await?,
            TeamChange::RemoveMember { member } => {
                detach(&mut tx, context, &idem, team, member).await?
            }
            TeamChange::AppointLead { agent_id } => {
                appoint(&mut tx, context, &idem, team, agent_id).await?
            }
        };
        tx.commit().await?;
        Ok(result)
    }
}

async fn detach(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    idem: &Idempotency,
    team: Team,
    member: Recipient,
) -> Result<TeamMutation, IdentityError> {
    if member.user_id == team.owner_user_id && member.agent_id.is_none() {
        return Err(IdentityError::Forbidden);
    }
    let role = member_role(tx, team.workspace_id, team.team_id, member)
        .await?
        .ok_or(IdentityError::Forbidden)?;
    let lead_leaving = member.agent_id.is_some() && member.agent_id == team.lead_agent_id;
    if lead_leaving && member == context.recipient() {
        bind_custody(tx, "office.team_leaving_id", team.team_id).await?;
    }
    let row = sqlx::query_as::<_, TeamRow>(
        "UPDATE teams SET revision = revision + 1,
              departed_lead_agent_id = CASE WHEN $3 THEN lead_agent_id ELSE departed_lead_agent_id END,
              lead_agent_id = CASE WHEN $3 THEN NULL ELSE lead_agent_id END,
              lead_generation = lead_generation + CASE WHEN $3 THEN 1 ELSE 0 END,
              state = CASE WHEN $3 THEN 'awaiting_owner' ELSE state END
         WHERE workspace_id = $1 AND id = $2 RETURNING *",
    ).bind(team.workspace_id.0).bind(team.team_id.0).bind(lead_leaving).fetch_one(&mut **tx).await?;
    let event = if lead_leaving {
        bind_custody(tx, "office.team_custody_id", team.team_id).await?;
        sqlx::query(
            "UPDATE tasks SET awaiting_owner = true, revision = revision + 1, updated_at = clock_timestamp()
             WHERE workspace_id = $1 AND team_id = $2 AND status NOT IN ('done', 'cancelled', 'failed')",
        ).bind(team.workspace_id.0).bind(team.team_id.0).execute(&mut **tx).await?;
        "team.awaiting_owner"
    } else {
        "team.member_removed"
    };
    let receipt = record_team(
        tx,
        context,
        idem,
        row.into_team()?,
        Some(TeamMember { member, role }),
        event,
    )
    .await?;
    revoke_member(tx, team.workspace_id, team.team_id, member).await?;
    Ok(receipt)
}

async fn appoint(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    idem: &Idempotency,
    team: Team,
    agent: Option<AgentId>,
) -> Result<TeamMutation, IdentityError> {
    if team.state == TeamState::Active && team.lead_agent_id == agent {
        return Err(IdentityError::InvalidTransition);
    }
    let next = Recipient {
        user_id: team.owner_user_id,
        agent_id: agent,
    };
    if agent.is_some() && active_principal(tx, context, next).await? != Some(AgentRole::Lead) {
        return Err(IdentityError::Forbidden);
    }
    let previous = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT coalesce(lead_agent_id, departed_lead_agent_id) FROM teams WHERE workspace_id = $1 AND id = $2",
    ).bind(team.workspace_id.0).bind(team.team_id.0).fetch_one(&mut **tx).await?;
    if let Some(former) = team.lead_agent_id {
        revoke_member(
            tx,
            team.workspace_id,
            team.team_id,
            Recipient {
                user_id: team.owner_user_id,
                agent_id: Some(former),
            },
        )
        .await?;
    }
    check_room(tx, team.workspace_id, team.team_id, next).await?;
    write_member(tx, team.workspace_id, team.team_id, next, TeamRole::Lead).await?;
    let row = sqlx::query_as::<_, TeamRow>(
        "UPDATE teams SET lead_agent_id = $3, departed_lead_agent_id = NULL, lead_generation = lead_generation + 1,
               state = 'active', revision = revision + 1
         WHERE workspace_id = $1 AND id = $2 RETURNING *",
    )
    .bind(team.workspace_id.0)
    .bind(team.team_id.0)
    .bind(agent.map(|id| id.0))
    .fetch_one(&mut **tx)
    .await?;
    restore_tasks(tx, &team, previous, agent).await?;
    record_team(
        tx,
        context,
        idem,
        row.into_team()?,
        Some(TeamMember {
            member: next,
            role: TeamRole::Lead,
        }),
        "team.lead_appointed",
    )
    .await
}

async fn restore_tasks(
    tx: &mut Transaction<'_, Postgres>,
    team: &Team,
    previous: Option<Uuid>,
    next: Option<AgentId>,
) -> Result<(), IdentityError> {
    bind_custody(tx, "office.team_custody_id", team.team_id).await?;
    let next = next.map(|id| id.0);
    sqlx::query(
        "UPDATE tasks SET revision = revision + 1, updated_at = clock_timestamp(),
              accountable_agent_id = CASE WHEN accountable_agent_id = $3 THEN $4 ELSE accountable_agent_id END,
              assignee_user_id = CASE WHEN assignee_agent_id = $3 AND status = 'assigned' THEN NULL ELSE assignee_user_id END,
              assignee_agent_id = CASE WHEN assignee_agent_id = $3 AND status = 'assigned' THEN NULL ELSE assignee_agent_id END,
              status = CASE WHEN assignee_agent_id = $3 AND status = 'assigned' THEN 'ready' ELSE status END,
              awaiting_owner = EXISTS (SELECT 1 FROM runs WHERE workspace_id = tasks.workspace_id
                    AND task_id = tasks.id AND executor_agent_id = $3 AND $3 IS DISTINCT FROM $4
                    AND status IN ('running', 'waiting_input', 'cancel_requested'))
         WHERE workspace_id = $1 AND team_id = $2 AND status NOT IN ('done', 'cancelled', 'failed')
           AND (awaiting_owner OR accountable_agent_id = $3 OR assignee_agent_id = $3)",
    ).bind(team.workspace_id.0).bind(team.team_id.0).bind(previous).bind(next).execute(&mut **tx).await?;
    Ok(())
}

async fn revoke_member(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    team: TeamId,
    member: Recipient,
) -> Result<(), IdentityError> {
    sqlx::query(
        "UPDATE team_memberships SET revoked_at = clock_timestamp()
         WHERE workspace_id = $1 AND team_id = $2 AND user_id = $3 AND agent_id IS NOT DISTINCT FROM $4
           AND revoked_at IS NULL",
    ).bind(workspace.0).bind(team.0).bind(member.user_id.0).bind(member.agent_id.map(|id| id.0)).execute(&mut **tx).await?;
    Ok(())
}

async fn bind_custody(
    tx: &mut Transaction<'_, Postgres>,
    setting: &str,
    team: TeamId,
) -> Result<(), IdentityError> {
    sqlx::query("SELECT set_config($1, $2, true)")
        .bind(setting)
        .bind(team.0.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(())
}
