use chrono::{DateTime, Utc};
use office_domain::{
    Action, AgentId, BearerDigest, Resource, TeamAuthority, TeamId, TeamRole, TeamState, UserId,
    Visibility, WorkspaceId,
};
use office_protocol::{
    CreateTeam, GetTeam, ListTeams, Recipient, SetTeamMember, Team, TeamMember, TeamMutation,
    TeamPage, TeamView,
};
use serde_json::json;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::{
    Store,
    data_scope::create_scope,
    events::{Idempotency, append_event},
    identity::{
        IdentityError, WorkspaceContext, require_permission, valid_name, workspace_context,
    },
    inbox::save_cursor,
};

mod custody;
mod invitations;

#[derive(sqlx::FromRow)]
struct TeamRow {
    id: Uuid,
    workspace_id: Uuid,
    owner_user_id: Uuid,
    lead_agent_id: Option<Uuid>,
    state: String,
    revision: i64,
    name: String,
    created_at: DateTime<Utc>,
}

impl TeamRow {
    fn into_team(self) -> Result<Team, IdentityError> {
        Ok(Team {
            team_id: TeamId(self.id),
            workspace_id: WorkspaceId(self.workspace_id),
            owner_user_id: UserId(self.owner_user_id),
            lead_agent_id: self.lead_agent_id.map(AgentId),
            state: self.state.parse().map_err(|_| IdentityError::InvalidRole)?,
            revision: self.revision,
            name: self.name,
            created_at: self.created_at,
        })
    }
}

impl Store {
    pub async fn create_team(
        &self,
        digest: &BearerDigest,
        input: CreateTeam,
    ) -> Result<TeamMutation, IdentityError> {
        let name = valid_name(&input.name)?;
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        require_permission(
            context.actor,
            Action::CreateTeam,
            Resource::Workspace(context.access),
        )?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "team.create",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let id = TeamId(Uuid::new_v4());
        let owner = context.access.member;
        let lead = context.agent_id();
        let row = sqlx::query_as::<_, TeamRow>(
            "INSERT INTO teams (id, workspace_id, owner_user_id, name, lead_agent_id)
             VALUES ($1, $2, $3, $4, $5) RETURNING *",
        )
        .bind(id.0)
        .bind(input.workspace_id.0)
        .bind(owner.0)
        .bind(name)
        .bind(lead.map(|id| id.0))
        .fetch_one(&mut *tx)
        .await?;
        bind_team(&mut tx, id, true).await?;
        write_member(
            &mut tx,
            input.workspace_id,
            id,
            Recipient {
                user_id: owner,
                agent_id: None,
            },
            TeamRole::Lead,
        )
        .await?;
        if let Some(lead) = lead {
            write_member(
                &mut tx,
                input.workspace_id,
                id,
                Recipient {
                    user_id: owner,
                    agent_id: Some(lead),
                },
                TeamRole::Lead,
            )
            .await?;
        }
        create_scope(
            &mut tx,
            context,
            id.0,
            Visibility::Team { team_id: id },
            &[],
        )
        .await?;
        let result = record_team(
            &mut tx,
            context,
            &idem,
            row.into_team()?,
            None,
            "team.created",
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn get_team(
        &self,
        digest: &BearerDigest,
        input: GetTeam,
    ) -> Result<TeamView, IdentityError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let team = load_team(&mut tx, context, input.team_id, Action::ReadTeam, false).await?;
        bind_team(&mut tx, input.team_id, false).await?;
        let members = sqlx::query_as::<_, (Uuid, Option<Uuid>, String)>(
            "SELECT user_id, agent_id, role FROM team_memberships
             WHERE workspace_id = $1 AND team_id = $2 AND revoked_at IS NULL ORDER BY user_id, agent_id NULLS FIRST",
        ).bind(input.workspace_id.0).bind(input.team_id.0).fetch_all(&mut *tx).await?
         .into_iter().map(|(user, agent, role)| Ok(TeamMember {
            member: Recipient { user_id: UserId(user), agent_id: agent.map(AgentId) },
            role: role.parse().map_err(|_| IdentityError::InvalidRole)?,
        })).collect::<Result<Vec<_>, IdentityError>>()?;
        tx.commit().await?;
        Ok(TeamView { team, members })
    }

    pub async fn list_teams(
        &self,
        digest: &BearerDigest,
        input: ListTeams,
    ) -> Result<TeamPage, IdentityError> {
        let limit = input.limit.unwrap_or(30);
        if !(1..=50).contains(&limit) {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let mut rows = sqlx::query_as::<_, TeamRow>(
            "SELECT * FROM teams WHERE workspace_id = $1 AND ($2::uuid IS NULL OR id > $2)
             ORDER BY id LIMIT $3",
        )
        .bind(input.workspace_id.0)
        .bind(input.after.map(|id| id.0))
        .bind(i64::from(limit) + 1)
        .fetch_all(&mut *tx)
        .await?;
        let more = rows.len() > usize::from(limit);
        rows.truncate(usize::from(limit));
        let mut teams = Vec::with_capacity(rows.len());
        for row in rows {
            let team = row.into_team()?;
            check_team(&mut tx, context, &team, Action::ReadTeam).await?;
            teams.push(team);
        }
        let next_after = teams.last().filter(|_| more).map(|team| team.team_id);
        tx.commit().await?;
        Ok(TeamPage { teams, next_after })
    }

    pub async fn set_team_member(
        &self,
        digest: &BearerDigest,
        input: SetTeamMember,
    ) -> Result<TeamMutation, IdentityError> {
        if !(1..i64::MAX).contains(&input.expected_revision) || input.role == TeamRole::Lead {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "team.set_member",
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
        if team.state != TeamState::Active || team.lead_agent_id == Some(input.agent_id) {
            return Err(IdentityError::InvalidTransition);
        }
        let exists = sqlx::query_scalar::<_, bool>(
            "SELECT true FROM agents WHERE workspace_id = $1 AND owner_user_id = $2
             AND id = $3 AND disabled_at IS NULL FOR SHARE",
        )
        .bind(input.workspace_id.0)
        .bind(team.owner_user_id.0)
        .bind(input.agent_id.0)
        .fetch_optional(&mut *tx)
        .await?;
        if exists.is_none() {
            return Err(IdentityError::Forbidden);
        }
        bind_team(&mut tx, input.team_id, true).await?;
        let member = Recipient {
            user_id: team.owner_user_id,
            agent_id: Some(input.agent_id),
        };
        check_room(&mut tx, input.workspace_id, input.team_id, member).await?;
        write_member(
            &mut tx,
            input.workspace_id,
            input.team_id,
            member,
            input.role,
        )
        .await?;
        let row = sqlx::query_as::<_, TeamRow>(
            "UPDATE teams SET revision = revision + 1 WHERE workspace_id = $1 AND id = $2 RETURNING *",
        ).bind(input.workspace_id.0).bind(input.team_id.0).fetch_one(&mut *tx).await?;
        let changed = TeamMember {
            member,
            role: input.role,
        };
        let result = record_team(
            &mut tx,
            context,
            &idem,
            row.into_team()?,
            Some(changed),
            "team.member_updated",
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }
}

pub(crate) async fn load_team(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    id: TeamId,
    action: Action,
    lock: bool,
) -> Result<Team, IdentityError> {
    let query = if lock {
        "SELECT * FROM teams WHERE workspace_id = $1 AND id = $2 FOR NO KEY UPDATE"
    } else {
        "SELECT * FROM teams WHERE workspace_id = $1 AND id = $2"
    };
    let team = sqlx::query_as::<_, TeamRow>(query)
        .bind(context.access.workspace.0)
        .bind(id.0)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(IdentityError::Forbidden)?
        .into_team()?;
    check_team(tx, context, &team, action).await?;
    Ok(team)
}

async fn check_team(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    team: &Team,
    action: Action,
) -> Result<(), IdentityError> {
    let role = member_role(
        tx,
        context.access.workspace,
        team.team_id,
        context.recipient(),
    )
    .await?;
    require_permission(
        context.actor,
        action,
        Resource::Team(TeamAuthority {
            workspace: context.access,
            owner: team.owner_user_id,
            lead_agent: team.lead_agent_id,
            member_role: role,
        }),
    )
}

/// Requires that transaction RLS sees the exact principal, or that its team roster is bound.
pub(crate) async fn member_role(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    team: TeamId,
    member: Recipient,
) -> Result<Option<TeamRole>, IdentityError> {
    sqlx::query_scalar::<_, String>(
        "SELECT role FROM team_memberships WHERE workspace_id = $1 AND team_id = $2 AND user_id = $3
         AND agent_id IS NOT DISTINCT FROM $4 AND revoked_at IS NULL",
    ).bind(workspace.0).bind(team.0).bind(member.user_id.0)
     .bind(member.agent_id.map(|id| id.0)).fetch_optional(&mut **tx).await?
     .map(|value| value.parse().map_err(|_| IdentityError::InvalidRole)).transpose()
}

pub(crate) async fn bind_team(
    tx: &mut Transaction<'_, Postgres>,
    team: TeamId,
    manage: bool,
) -> Result<(), IdentityError> {
    let id = team.0.to_string();
    sqlx::query("SELECT set_config('office.team_view_id', $1, true), set_config('office.team_manage_id', $2, true)")
        .bind(&id).bind(if manage { id.as_str() } else { "" }).execute(&mut **tx).await?;
    Ok(())
}

pub(crate) async fn lock_team_task_creation(
    tx: &mut Transaction<'_, Postgres>,
    team: TeamId,
    exclusive: bool,
) -> Result<(), IdentityError> {
    let statement = if exclusive {
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))"
    } else {
        "SELECT pg_advisory_xact_lock_shared(hashtextextended($1, 0))"
    };
    sqlx::query(statement)
        .bind(format!("team-task-creation/{}", team.0))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn check_room(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    team: TeamId,
    member: Recipient,
) -> Result<(), IdentityError> {
    let allowed = sqlx::query_scalar::<_, bool>(
        "SELECT count(*) < 128 OR coalesce(bool_or(user_id = $3 AND agent_id IS NOT DISTINCT FROM $4), false)
         FROM team_memberships WHERE workspace_id = $1 AND team_id = $2 AND revoked_at IS NULL",
    ).bind(workspace.0).bind(team.0).bind(member.user_id.0).bind(member.agent_id.map(|id| id.0))
     .fetch_one(&mut **tx).await?;
    allowed.then_some(()).ok_or(IdentityError::InvalidInput)
}

async fn write_member(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    team: TeamId,
    member: Recipient,
    role: TeamRole,
) -> Result<(), IdentityError> {
    sqlx::query(
        "INSERT INTO team_memberships (workspace_id, team_id, user_id, agent_id, role) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (workspace_id, team_id, user_id, agent_id) DO UPDATE SET role = excluded.role, revoked_at = NULL",
    ).bind(workspace.0).bind(team.0).bind(member.user_id.0)
     .bind(member.agent_id.map(|id| id.0)).bind(role.as_str()).execute(&mut **tx).await?;
    Ok(())
}

async fn record_team(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    idem: &Idempotency,
    team: Team,
    member: Option<TeamMember>,
    kind: &str,
) -> Result<TeamMutation, IdentityError> {
    let seq = append_event(
        tx,
        context,
        team.team_id.0,
        kind,
        json!({"team_id": team.team_id, "revision": team.revision, "state": team.state, "member": member}),
        None,
    )
    .await?;
    let result = TeamMutation {
        team,
        member,
        event_cursor: save_cursor(tx, context, seq).await?,
    };
    idem.record(tx, result.team.team_id.0, &result).await?;
    Ok(result)
}
