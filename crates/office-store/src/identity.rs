use office_domain::{
    Action, Actor, AgentId, AgentRole, BearerDigest, CredentialId, Resource, Subject, UserId,
    WorkspaceAccess, WorkspaceId, WorkspaceRole, authorize,
};
use office_protocol::{CreateWorkspace, Identity, Recipient, WorkspaceMembership};
use sqlx::{PgConnection, Postgres, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::Store;

mod agent;
mod credentials;
mod instances;
mod invitation;
pub(crate) mod runner;

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("invalid or inactive credential")]
    Unauthenticated,
    #[error("installation is already initialized")]
    AlreadyInitialized,
    #[error("display and workspace names must contain 1 to 128 bytes of text")]
    InvalidName,
    #[error("workspace does not allow this operation")]
    Forbidden,
    #[error("invalid input")]
    InvalidInput,
    #[error("idempotency key or credential digest conflicts with an existing request")]
    IdempotencyConflict,
    #[error("the resource revision has changed")]
    RevisionConflict,
    #[error(
        "inbox cursor expired or is unavailable; refresh authorized objects and read without a cursor"
    )]
    CursorExpired,
    #[error("current revision is {current}")]
    RevisionMismatch { current: i64 },
    #[error("operation is unavailable from the task's current state")]
    InvalidTransition,
    #[error("all prerequisites must be accepted before this operation")]
    DependencyNotReady,
    #[error("the requested dependency would create a cycle")]
    DependencyCycle,
    #[error("the worker already participates in the delegation lineage")]
    DelegationCycle,
    #[error("the request exceeds the delegation limits")]
    DelegationLimit,
    #[error("the delegation or an ancestor has expired, been revoked, or lost its team authority")]
    DelegationInactive,
    #[error("the directory overlaps an existing checkout on this host")]
    CheckoutRootOverlap,
    #[error("coding execution requires a verified checkout and active work scope")]
    AccessRequestRequired,
    #[error("one or more requested paths are already claimed")]
    PathClaimed,
    #[error("the path claim fence is absent, expired, or superseded")]
    ClaimFenceStale,
    #[error("the path claim has reached its renewal or lifetime limit")]
    ClaimRenewalLimit,
    #[error("a pending path handoff must be resolved, released, or allowed to expire")]
    PathHandoffPending,
    #[error("the owner has insufficient unreserved execution budget")]
    BudgetExhausted,
    #[error("new executions are paused by the workspace or owner")]
    ExecutionPaused,
    #[error("an execution capacity limit is reached")]
    ExecutionCapacityReached,
    #[error("the message rate limit has been reached")]
    MessageRateLimited,
    #[error("the owner's artifact quota is exhausted")]
    ArtifactQuotaExceeded,
    #[error("an upload is already open for this artifact revision")]
    ArtifactUploadOpen,
    #[error("the artifact is missing one or more chunks")]
    ArtifactIncomplete,
    #[error("the artifact bytes do not match the declared SHA-256")]
    ArtifactDigestMismatch,
    #[error("the artifact revision is unpublished or its retention period has ended")]
    ArtifactUnavailable,
    #[error("the runner, attempt or fence does not hold a current lease")]
    RunLeaseStale,
    #[error("the checkout coordinator lease has expired or been superseded")]
    CoordinatorStale,
    #[error("the registered checkout coordinator is unavailable")]
    CheckoutOffline,
    #[error("the requested path is outside the effective read scope")]
    PathForbidden,
    #[error("the effective checkout scope has changed")]
    ScopeStale,
    #[error("the file changed during the operation")]
    ContentConflict,
    #[error("a required read dependency is missing or has changed")]
    ContextStale,
    #[error("the checkout has stopped publications pending reconciliation")]
    CheckoutReconciliationRequired,
    #[error("the publication is pending; poll with the same idempotency key")]
    PublicationPending,
    #[error("the requested file is absent from the authorized source view")]
    FileNotFound,
    #[error("identity store is unavailable")]
    Database(#[from] sqlx::Error),
    #[error("identity operation timed out")]
    Timeout,
    #[error("identity store contains an unsupported workspace role")]
    InvalidRole,
}

#[derive(Clone, Copy)]
pub(crate) struct WorkspaceContext {
    pub(crate) actor: Actor,
    pub(crate) access: WorkspaceAccess,
    pub(crate) subject: Subject,
}

impl WorkspaceContext {
    pub(crate) fn agent_id(self) -> Option<AgentId> {
        match self.subject {
            Subject::Human { .. } => None,
            Subject::Agent { agent_id } => Some(agent_id),
        }
    }

    pub(crate) fn recipient(self) -> Recipient {
        Recipient {
            user_id: self.access.member,
            agent_id: self.agent_id(),
        }
    }
}

pub(crate) async fn active_principal(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    recipient: Recipient,
) -> Result<Option<AgentRole>, IdentityError> {
    verify_active_recipient(
        tx,
        context.access.workspace,
        recipient,
        context.access.member,
    )
    .await
}

pub(crate) async fn verify_active_recipient(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    recipient: Recipient,
    bound_user: UserId,
) -> Result<Option<AgentRole>, IdentityError> {
    bind_user(tx, recipient.user_id.0).await?;
    let member = sqlx::query_scalar::<_, bool>(
        "SELECT true FROM workspace_memberships m JOIN users u ON u.id = m.user_id
         WHERE m.workspace_id = $1 AND m.user_id = $2 AND m.revoked_at IS NULL AND u.disabled_at IS NULL
         FOR SHARE OF m, u",
    ).bind(workspace.0).bind(recipient.user_id.0).fetch_optional(&mut **tx).await?;
    let role = if let Some(agent) = recipient.agent_id
        && member.is_some()
    {
        sqlx::query_scalar::<_, String>(
            "SELECT role FROM agents WHERE workspace_id = $1 AND id = $2
             AND owner_user_id = $3 AND disabled_at IS NULL FOR SHARE",
        )
        .bind(workspace.0)
        .bind(agent.0)
        .bind(recipient.user_id.0)
        .fetch_optional(&mut **tx)
        .await?
    } else {
        None
    };
    bind_user(tx, bound_user.0).await?;
    if member.is_none() || (recipient.agent_id.is_some() && role.is_none()) {
        return Err(IdentityError::Forbidden);
    }
    role.map(|value| value.parse().map_err(|_| IdentityError::InvalidRole))
        .transpose()
}

pub(crate) async fn bind_subject(
    tx: &mut Transaction<'_, Postgres>,
    principal: Recipient,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "SELECT set_config('office.user_id', $1, true), set_config('office.agent_id', $2, true)",
    )
    .bind(principal.user_id.0.to_string())
    .bind(
        principal
            .agent_id
            .map_or_else(String::new, |id| id.0.to_string()),
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn workspace_context(
    tx: &mut Transaction<'_, Postgres>,
    digest: &BearerDigest,
    workspace: WorkspaceId,
) -> Result<WorkspaceContext, IdentityError> {
    let actor = match lookup(tx, digest).await {
        Ok(user) => Actor::Human(UserId(user.user_id)),
        Err(IdentityError::Unauthenticated) => {
            let binding = agent::agent_binding(tx, digest).await?;
            agent::agent_actor(&agent::load_agent(tx, binding).await?)
        }
        Err(error) => return Err(error),
    };
    let subject = actor.subject().ok_or(IdentityError::Forbidden)?;
    let membership = authorize_workspace(tx, actor, Action::ReadWorkspace, workspace).await?;
    let context = WorkspaceContext {
        actor,
        access: WorkspaceAccess {
            workspace,
            member: actor.owner(),
            role: membership.role,
        },
        subject,
    };
    sqlx::query("SELECT set_config('office.workspace_id', $1, true), set_config('office.agent_id', $2, true)")
        .bind(workspace.0.to_string())
        .bind(context.agent_id().map_or_else(String::new, |agent| agent.0.to_string()))
        .execute(&mut **tx)
        .await?;
    Ok(context)
}

#[derive(sqlx::FromRow)]
struct PrincipalRow {
    user_id: Uuid,
    display_name: String,
    credential_id: Uuid,
}

impl Store {
    /// Only the operator CLI can reach this application service via a local database connection.
    pub async fn bootstrap_installation(
        &self,
        user_name: &str,
        workspace_name: &str,
        digest: &BearerDigest,
    ) -> Result<Identity, IdentityError> {
        let user_name = valid_name(user_name)?;
        let workspace_name = valid_name(workspace_name)?;
        let mut tx = self.pool.begin().await?;
        let previous_owner: Option<Uuid> = sqlx::query_scalar(
            "SELECT bootstrap_user_id FROM office_installation WHERE singleton FOR UPDATE",
        )
        .fetch_one(&mut *tx)
        .await?;

        if let Some(previous_owner) = previous_owner {
            let principal = lookup(&mut tx, digest).await?;
            if principal.user_id != previous_owner || principal.display_name != user_name {
                return Err(IdentityError::AlreadyInitialized);
            }
            let identity = load_identity(&mut tx, principal).await?;
            if !identity.memberships.iter().any(|membership| {
                membership.name == workspace_name && membership.role == WorkspaceRole::Owner
            }) {
                return Err(IdentityError::AlreadyInitialized);
            }
            tx.commit().await?;
            return Ok(identity);
        }

        let user_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let credential_id = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, display_name) VALUES ($1, $2)")
            .bind(user_id)
            .bind(user_name)
            .execute(&mut *tx)
            .await?;
        bind_user(&mut tx, user_id).await?;
        sqlx::query("INSERT INTO workspaces (id, name, created_by_user_id) VALUES ($1, $2, $3)")
            .bind(workspace_id)
            .bind(workspace_name)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO workspace_memberships (workspace_id, user_id, role) VALUES ($1, $2, 'owner')")
            .bind(workspace_id).bind(user_id).execute(&mut *tx).await?;
        bind_credential_owner(&mut tx, user_id).await?;
        sqlx::query("INSERT INTO credentials (id, owner_user_id, digest) VALUES ($1, $2, $3)")
            .bind(credential_id)
            .bind(user_id)
            .bind(digest.as_bytes())
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE office_installation SET bootstrap_user_id = $1 WHERE singleton")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        let identity = load_identity(
            &mut tx,
            PrincipalRow {
                user_id,
                display_name: user_name.to_owned(),
                credential_id,
            },
        )
        .await?;
        tx.commit().await?;
        Ok(identity)
    }

    pub async fn authenticate_user(
        &self,
        digest: &BearerDigest,
    ) -> Result<Identity, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let principal = lookup(&mut tx, digest).await?;
        let identity = load_identity(&mut tx, principal).await?;
        tx.commit().await?;
        Ok(identity)
    }

    pub async fn create_workspace(
        &self,
        actor: &BearerDigest,
        input: CreateWorkspace,
    ) -> Result<WorkspaceMembership, IdentityError> {
        let name = valid_name(&input.name)?;
        let mut tx = self.pool.begin().await?;
        let principal = lookup(&mut tx, actor).await?;
        require_permission(
            Actor::Human(UserId(principal.user_id)),
            Action::CreateWorkspace,
            Resource::Account(UserId(principal.user_id)),
        )?;
        bind_user(&mut tx, principal.user_id).await?;
        // A new workspace becomes readable only after its owner membership is inserted.
        // Serialize retries before inserting, since ON CONFLICT also applies the SELECT policy.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!(
                "create-workspace/{}/{}",
                principal.user_id, input.idempotency_key
            ))
            .execute(&mut *tx)
            .await?;
        let existing = sqlx::query_as::<_, (Uuid, String)>(
            "SELECT w.id, w.name FROM workspaces w JOIN workspace_memberships m ON m.workspace_id = w.id
             WHERE w.created_by_user_id = $1 AND w.creation_key = $2 AND m.user_id = $1
               AND m.role = 'owner' AND m.revoked_at IS NULL FOR SHARE OF m",
        )
        .bind(principal.user_id).bind(input.idempotency_key)
        .fetch_optional(&mut *tx).await?;
        let workspace_id = if let Some((id, existing_name)) = existing {
            if existing_name != name {
                return Err(IdentityError::IdempotencyConflict);
            }
            id
        } else {
            let id = Uuid::new_v4();
            let inserted = sqlx::query("INSERT INTO workspaces (id, name, created_by_user_id, creation_key) VALUES ($1, $2, $3, $4)")
                .bind(id).bind(name).bind(principal.user_id).bind(input.idempotency_key)
                .execute(&mut *tx).await;
            if let Err(error) = inserted {
                if error
                    .as_database_error()
                    .is_some_and(|cause| cause.is_unique_violation())
                {
                    return Err(IdentityError::Forbidden);
                }
                return Err(error.into());
            }
            sqlx::query("INSERT INTO workspace_memberships (workspace_id, user_id, role) VALUES ($1, $2, 'owner')")
                .bind(id).bind(principal.user_id).execute(&mut *tx).await?;
            id
        };
        tx.commit().await?;
        Ok(WorkspaceMembership {
            workspace_id: WorkspaceId(workspace_id),
            name: name.to_owned(),
            role: WorkspaceRole::Owner,
        })
    }
}

async fn lookup(
    tx: &mut Transaction<'_, Postgres>,
    digest: &BearerDigest,
) -> Result<PrincipalRow, IdentityError> {
    bind_credential_digest(tx, digest).await?;
    let user: PrincipalRow = sqlx::query_as(
        "SELECT u.id AS user_id, u.display_name, c.id AS credential_id
         FROM credentials c JOIN users u ON u.id = c.owner_user_id
         WHERE c.digest = $1 AND c.agent_id IS NULL AND c.runner_id IS NULL
           AND c.revoked_at IS NULL AND u.disabled_at IS NULL
           AND (c.expires_at IS NULL OR c.expires_at > transaction_timestamp())
         FOR SHARE OF c, u",
    )
    .bind(digest.as_bytes())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(IdentityError::Unauthenticated)?;
    bind_credential_owner(tx, user.user_id).await?;
    Ok(user)
}

async fn load_identity(
    tx: &mut Transaction<'_, Postgres>,
    principal: PrincipalRow,
) -> Result<Identity, IdentityError> {
    bind_user(tx, principal.user_id).await?;
    let memberships = sqlx::query_as::<_, (Uuid, String, String)>(
        "SELECT w.id, w.name, m.role FROM workspace_memberships m
         JOIN workspaces w ON w.id = m.workspace_id
         WHERE m.user_id = $1 AND m.revoked_at IS NULL ORDER BY w.name, w.id",
    )
    .bind(principal.user_id)
    .fetch_all(&mut **tx)
    .await?;
    let memberships = memberships
        .into_iter()
        .map(|(id, name, role)| {
            let role = role.parse().map_err(|_| IdentityError::InvalidRole)?;
            let workspace = WorkspaceId(id);
            let user = UserId(principal.user_id);
            require_permission(
                Actor::Human(user),
                Action::ReadWorkspace,
                Resource::Workspace(WorkspaceAccess {
                    workspace,
                    member: user,
                    role,
                }),
            )?;
            Ok(WorkspaceMembership {
                workspace_id: workspace,
                name,
                role,
            })
        })
        .collect::<Result<Vec<_>, IdentityError>>()?;
    Ok(Identity {
        user_id: UserId(principal.user_id),
        display_name: principal.display_name,
        credential_id: CredentialId(principal.credential_id),
        memberships,
    })
}

pub(crate) fn require_permission(
    actor: Actor,
    action: Action,
    resource: Resource,
) -> Result<(), IdentityError> {
    authorize(actor, action, resource)
        .then_some(())
        .ok_or(IdentityError::Forbidden)
}

pub(crate) async fn authorize_workspace(
    tx: &mut Transaction<'_, Postgres>,
    actor: Actor,
    action: Action,
    workspace: WorkspaceId,
) -> Result<WorkspaceMembership, IdentityError> {
    let member = actor.owner();
    bind_user(tx, member.0).await?;
    let (name, role): (String, String) = sqlx::query_as(
        "SELECT w.name, m.role FROM workspace_memberships m
         JOIN workspaces w ON w.id = m.workspace_id JOIN users u ON u.id = m.user_id
         WHERE m.workspace_id = $1 AND m.user_id = $2 AND m.revoked_at IS NULL
           AND u.disabled_at IS NULL FOR SHARE OF m, u",
    )
    .bind(workspace.0)
    .bind(member.0)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(IdentityError::Forbidden)?;
    let role = role.parse().map_err(|_| IdentityError::InvalidRole)?;
    require_permission(
        actor,
        action,
        Resource::Workspace(WorkspaceAccess {
            workspace,
            member,
            role,
        }),
    )?;
    Ok(WorkspaceMembership {
        workspace_id: workspace,
        role,
        name,
    })
}

pub(crate) async fn bind_user(connection: &mut PgConnection, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT set_config('office.user_id', $1, true)")
        .bind(id.to_string())
        .execute(connection)
        .await?;
    Ok(())
}

async fn bind_credential_owner(connection: &mut PgConnection, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT set_config('office.credential_owner', $1, true)")
        .bind(id.to_string())
        .execute(connection)
        .await?;
    Ok(())
}

async fn bind_credential_digest(
    connection: &mut PgConnection,
    digest: &BearerDigest,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT set_config('office.credential_digest', $1, true)")
        .bind(hex::encode(digest.as_bytes()))
        .execute(connection)
        .await?;
    Ok(())
}

pub(crate) fn valid_name(name: &str) -> Result<&str, IdentityError> {
    let name = name.trim();
    if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        return Err(IdentityError::InvalidName);
    }
    Ok(name)
}
