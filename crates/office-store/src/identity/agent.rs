use office_domain::{
    Action, Actor, AgentId, BearerDigest, CredentialId, Resource, UserId, WorkspaceAccess,
    WorkspaceId,
};
use office_protocol::{AgentIdentity, CreateAgent, McpIdentity};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::{
    IdentityError, authorize_workspace, bind_credential_digest, bind_user, load_identity, lookup,
    require_permission, valid_name,
};
use crate::Store;

#[derive(sqlx::FromRow)]
pub(super) struct AgentBinding {
    pub(super) credential_id: Uuid,
    pub(super) agent_id: Uuid,
    pub(super) owner_user_id: Uuid,
    pub(super) workspace_id: Uuid,
}

pub(crate) fn agent_actor(identity: &AgentIdentity) -> Actor {
    Actor::Agent {
        owner: identity.owner_user_id,
        agent: identity.agent_id,
        credential: identity.credential_id,
        workspace: identity.workspace_id,
        role: identity.role,
    }
}

#[derive(sqlx::FromRow)]
struct AgentRow {
    display_name: String,
    workspace_name: String,
    membership_role: String,
    role: String,
}

impl Store {
    pub async fn authenticate_mcp(
        &self,
        credential: &BearerDigest,
    ) -> Result<McpIdentity, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let actor = match lookup(&mut tx, credential).await {
            Ok(user) => McpIdentity::Human(load_identity(&mut tx, user).await?),
            Err(IdentityError::Unauthenticated) => {
                let binding = agent_binding(&mut tx, credential).await?;
                McpIdentity::Agent(load_agent(&mut tx, binding).await?)
            }
            Err(error) => return Err(error),
        };
        tx.commit().await?;
        Ok(actor)
    }

    pub async fn create_agent(
        &self,
        requester: &BearerDigest,
        workspace: WorkspaceId,
        input: CreateAgent,
    ) -> Result<AgentIdentity, IdentityError> {
        let name = valid_name(&input.display_name)?;
        let credential =
            BearerDigest::decode(&input.credential_digest).ok_or(IdentityError::InvalidInput)?;
        let mut tx = self.pool.begin().await?;
        let principal = lookup(&mut tx, requester).await?;
        let workspace_access = authorize_workspace(
            &mut tx,
            Actor::Human(UserId(principal.user_id)),
            Action::RegisterAgent,
            workspace,
        )
        .await?;

        let agent_id = Uuid::new_v4();
        let inserted = sqlx::query(
            "INSERT INTO agents (id, workspace_id, owner_user_id, display_name, role, creation_key)
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (owner_user_id, creation_key) DO NOTHING",
        )
        .bind(agent_id)
        .bind(workspace.0)
        .bind(principal.user_id)
        .bind(name)
        .bind(input.role.as_str())
        .bind(input.idempotency_key)
        .execute(&mut *tx)
        .await?;
        let agent = if inserted.rows_affected() == 1 {
            let credential_id = Uuid::new_v4();
            let issued = sqlx::query(
                "INSERT INTO credentials (id, owner_user_id, digest, workspace_id, agent_id)
                 VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
            )
            .bind(credential_id)
            .bind(principal.user_id)
            .bind(credential.as_bytes())
            .bind(workspace.0)
            .bind(agent_id)
            .execute(&mut *tx)
            .await?;
            if issued.rows_affected() != 1 {
                return Err(IdentityError::IdempotencyConflict);
            }
            AgentIdentity {
                agent_id: AgentId(agent_id),
                owner_user_id: UserId(principal.user_id),
                credential_id: CredentialId(credential_id),
                display_name: name.to_owned(),
                workspace_id: workspace,
                workspace_name: workspace_access.name,
                role: input.role,
            }
        } else {
            let id: Uuid = sqlx::query_scalar(
                "SELECT id FROM agents WHERE owner_user_id = $1 AND creation_key = $2",
            )
            .bind(principal.user_id)
            .bind(input.idempotency_key)
            .fetch_one(&mut *tx)
            .await?;
            let binding =
                agent_binding(&mut tx, &credential)
                    .await
                    .map_err(|error| match error {
                        IdentityError::Unauthenticated => IdentityError::IdempotencyConflict,
                        other => other,
                    })?;
            if binding.agent_id != id
                || binding.workspace_id != workspace.0
                || binding.owner_user_id != principal.user_id
            {
                return Err(IdentityError::IdempotencyConflict);
            }
            let agent = load_agent(&mut tx, binding).await?;
            if agent.display_name != name || agent.role != input.role {
                return Err(IdentityError::IdempotencyConflict);
            }
            agent
        };
        tx.commit().await?;
        Ok(agent)
    }
}

pub(super) async fn agent_binding(
    tx: &mut Transaction<'_, Postgres>,
    digest: &BearerDigest,
) -> Result<AgentBinding, IdentityError> {
    bind_credential_digest(tx, digest).await?;
    sqlx::query_as(
        "SELECT c.id AS credential_id, c.agent_id, c.owner_user_id, c.workspace_id
         FROM credentials c JOIN users u ON u.id = c.owner_user_id
         WHERE c.digest = $1 AND c.agent_id IS NOT NULL AND c.revoked_at IS NULL AND u.disabled_at IS NULL
           AND (c.expires_at IS NULL OR c.expires_at > transaction_timestamp()) FOR SHARE OF c, u",
    )
    .bind(digest.as_bytes()).fetch_optional(&mut **tx).await?.ok_or(IdentityError::Unauthenticated)
}

pub(super) async fn load_agent(
    tx: &mut Transaction<'_, Postgres>,
    binding: AgentBinding,
) -> Result<AgentIdentity, IdentityError> {
    bind_user(tx, binding.owner_user_id).await?;
    let row = sqlx::query_as::<_, AgentRow>(
        "SELECT a.display_name, a.role, w.name AS workspace_name, m.role AS membership_role FROM agents a
         JOIN workspaces w ON w.id = a.workspace_id
         JOIN workspace_memberships m ON m.workspace_id = a.workspace_id AND m.user_id = a.owner_user_id
         WHERE a.id = $1 AND a.workspace_id = $2 AND a.owner_user_id = $3
           AND a.disabled_at IS NULL AND m.revoked_at IS NULL FOR SHARE OF a, m",
    )
    .bind(binding.agent_id).bind(binding.workspace_id).bind(binding.owner_user_id)
    .fetch_optional(&mut **tx).await?.ok_or(IdentityError::Unauthenticated)?;
    let workspace = WorkspaceId(binding.workspace_id);
    let owner = UserId(binding.owner_user_id);
    let agent = AgentId(binding.agent_id);
    let credential = CredentialId(binding.credential_id);
    let identity = AgentIdentity {
        agent_id: agent,
        owner_user_id: owner,
        credential_id: credential,
        workspace_id: workspace,
        display_name: row.display_name,
        workspace_name: row.workspace_name,
        role: row.role.parse().map_err(|_| IdentityError::InvalidRole)?,
    };
    require_permission(
        agent_actor(&identity),
        Action::ReadWorkspace,
        Resource::Workspace(WorkspaceAccess {
            workspace,
            member: owner,
            role: row
                .membership_role
                .parse()
                .map_err(|_| IdentityError::InvalidRole)?,
        }),
    )?;
    Ok(identity)
}
