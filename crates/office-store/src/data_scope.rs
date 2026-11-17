use office_domain::{
    Action, AgentId, DataAccess, DataGrant, GrantRight, Resource, Subject, TeamId, UserId,
    Visibility,
};
use office_protocol::{AclGrant, Recipient};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::identity::{IdentityError, WorkspaceContext, require_permission};

pub(crate) struct Scope {
    pub(crate) owner: UserId,
    pub(crate) author: Subject,
    pub(crate) visibility: Visibility,
}

#[derive(sqlx::FromRow)]
struct ScopeRow {
    owner_user_id: Uuid,
    author_agent_id: Option<Uuid>,
    visibility: String,
    team_id: Option<Uuid>,
    active_team: Option<Uuid>,
    access_right: Option<String>,
}

pub(crate) async fn create_scope(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    id: Uuid,
    visibility: Visibility,
    grants: &[AclGrant],
) -> Result<(), IdentityError> {
    if grants.len() > 32 || (visibility != Visibility::ExplicitAcl && !grants.is_empty()) {
        return Err(IdentityError::InvalidInput);
    }
    let workspace = context.access.workspace.0;
    let user = context.access.member.0;
    let agent = context.agent_id().map(|id| id.0);
    let (kind, team) = match visibility {
        Visibility::OwnerPrivate => ("owner_private", None),
        Visibility::Workspace => ("workspace", None),
        Visibility::ExplicitAcl => ("explicit_acl", None),
        Visibility::Team { team_id } => {
            let allowed = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM team_memberships WHERE workspace_id = $1 AND team_id = $2
                 AND user_id = $3 AND agent_id IS NOT DISTINCT FROM $4 AND revoked_at IS NULL)",
            )
            .bind(workspace).bind(team_id.0).bind(user).bind(agent)
            .fetch_one(&mut **tx).await?;
            if !allowed {
                return Err(IdentityError::Forbidden);
            }
            ("team", Some(team_id.0))
        }
    };
    require_permission(
        context.actor,
        Action::WriteData,
        Resource::Data(DataAccess {
            workspace: context.access,
            owner: context.access.member,
            author: context.subject,
            visibility,
            active_team: team.map(TeamId),
            grant: None,
        }),
    )?;
    sqlx::query(
        "INSERT INTO data_scopes (id, workspace_id, owner_user_id, author_agent_id, visibility, team_id)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id).bind(workspace).bind(user).bind(agent).bind(kind).bind(team)
    .execute(&mut **tx).await?;
    for grant in grants {
        sqlx::query(
            "INSERT INTO data_acl (workspace_id, scope_id, scope_owner_user_id, scope_author_id,
                 grantee_user_id, grantee_agent_id, access_right) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(workspace).bind(id).bind(user).bind(agent.unwrap_or(user))
        .bind(grant.user_id.0).bind(grant.agent_id.map(|id| id.0)).bind(grant.right.as_str())
        .execute(&mut **tx).await.map_err(|error| {
            if error.as_database_error().is_some_and(|cause| {
                cause.is_foreign_key_violation() || cause.is_unique_violation()
            }) {
                IdentityError::InvalidInput
            } else {
                error.into()
            }
        })?;
    }
    Ok(())
}

pub(crate) async fn create_directed_scope(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    id: Uuid,
    recipient: Recipient,
) -> Result<(), IdentityError> {
    let mut grants = vec![AclGrant {
        user_id: recipient.user_id,
        agent_id: None,
        right: GrantRight::Read,
    }];
    if let Some(agent_id) = recipient.agent_id {
        grants.push(AclGrant {
            user_id: recipient.user_id,
            agent_id: Some(agent_id),
            right: GrantRight::Read,
        });
    }
    create_scope(tx, context, id, Visibility::ExplicitAcl, &grants).await
}

pub(crate) async fn authorize_scope(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    id: Uuid,
    action: Action,
) -> Result<Scope, IdentityError> {
    let row = sqlx::query_as::<_, ScopeRow>(
        "SELECT scope.owner_user_id, scope.author_agent_id, scope.visibility, scope.team_id,
                team.team_id AS active_team, grant_row.access_right
         FROM data_scopes scope
         LEFT JOIN team_memberships team ON team.workspace_id = scope.workspace_id AND team.team_id = scope.team_id
              AND team.user_id = $3 AND team.agent_id IS NOT DISTINCT FROM $4 AND team.revoked_at IS NULL
         LEFT JOIN data_acl grant_row ON grant_row.workspace_id = scope.workspace_id AND grant_row.scope_id = scope.id
              AND grant_row.grantee_user_id = $3 AND grant_row.grantee_agent_id IS NOT DISTINCT FROM $4
         WHERE scope.workspace_id = $1 AND scope.id = $2",
    )
    .bind(context.access.workspace.0).bind(id).bind(context.access.member.0).bind(context.agent_id().map(|id| id.0))
    .fetch_optional(&mut **tx).await?.ok_or(IdentityError::Forbidden)?;
    let owner = UserId(row.owner_user_id);
    let author = subject(owner, row.author_agent_id);
    let visibility = match (row.visibility.as_str(), row.team_id) {
        ("owner_private", None) => Visibility::OwnerPrivate,
        ("workspace", None) => Visibility::Workspace,
        ("explicit_acl", None) => Visibility::ExplicitAcl,
        ("team", Some(team_id)) => Visibility::Team {
            team_id: TeamId(team_id),
        },
        _ => return Err(IdentityError::InvalidRole),
    };
    let grant = match row.access_right.as_deref() {
        Some("read") => Some(GrantRight::Read),
        Some("write") => Some(GrantRight::Write),
        None => None,
        _ => return Err(IdentityError::InvalidRole),
    };
    require_permission(
        context.actor,
        action,
        Resource::Data(DataAccess {
            workspace: context.access,
            owner,
            author,
            visibility,
            active_team: row.active_team.map(TeamId),
            grant: grant.map(|right| DataGrant {
                grantee: context.subject,
                right,
            }),
        }),
    )?;
    Ok(Scope {
        owner,
        author,
        visibility,
    })
}

pub(crate) fn subject(user_id: UserId, agent: Option<Uuid>) -> Subject {
    match agent {
        Some(id) => Subject::Agent {
            agent_id: AgentId(id),
        },
        None => Subject::Human { user_id },
    }
}
