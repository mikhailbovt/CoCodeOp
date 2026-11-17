use office_domain::{
    Action, Actor, BearerDigest, CredentialId, Resource, RunnerId, UserId, WorkspaceId,
};
use office_protocol::{CreateRunner, RunnerIdentity};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::{
    IdentityError, authorize_workspace, bind_credential_digest, bind_user, lookup,
    require_permission, valid_name,
};
use crate::Store;

#[derive(sqlx::FromRow)]
pub(super) struct RunnerBinding {
    pub(super) credential_id: Uuid,
    pub(super) runner_id: Uuid,
    pub(super) owner_user_id: Uuid,
    pub(super) workspace_id: Uuid,
}

impl Store {
    pub async fn authenticate_runner(
        &self,
        credential: &BearerDigest,
    ) -> Result<RunnerIdentity, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let identity = authenticate_runner_in(&mut tx, credential).await?;
        tx.commit().await?;
        Ok(identity)
    }

    pub async fn register_runner(
        &self,
        actor: &BearerDigest,
        workspace: WorkspaceId,
        input: CreateRunner,
    ) -> Result<RunnerIdentity, IdentityError> {
        let name = valid_name(&input.name)?;
        let credential =
            BearerDigest::decode(&input.credential_digest).ok_or(IdentityError::InvalidInput)?;
        let mut tx = self.pool.begin().await?;
        let principal = lookup(&mut tx, actor).await?;
        authorize_workspace(
            &mut tx,
            Actor::Human(UserId(principal.user_id)),
            Action::RegisterRunner,
            workspace,
        )
        .await?;

        let runner_id = Uuid::new_v4();
        let inserted = sqlx::query(
            "INSERT INTO runners (id, workspace_id, owner_user_id, name, registration_key)
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (owner_user_id, registration_key) DO NOTHING",
        )
        .bind(runner_id)
        .bind(workspace.0)
        .bind(principal.user_id)
        .bind(name)
        .bind(input.idempotency_key)
        .execute(&mut *tx)
        .await?;
        let identity = if inserted.rows_affected() == 1 {
            let credential_id = Uuid::new_v4();
            let issued = sqlx::query(
                "INSERT INTO credentials (id, owner_user_id, digest, workspace_id, runner_id)
                 VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
            )
            .bind(credential_id)
            .bind(principal.user_id)
            .bind(credential.as_bytes())
            .bind(workspace.0)
            .bind(runner_id)
            .execute(&mut *tx)
            .await?;
            if issued.rows_affected() != 1 {
                return Err(IdentityError::IdempotencyConflict);
            }
            RunnerIdentity {
                runner_id: RunnerId(runner_id),
                owner_user_id: UserId(principal.user_id),
                credential_id: CredentialId(credential_id),
                workspace_id: workspace,
                name: name.to_owned(),
            }
        } else {
            let id: Uuid = sqlx::query_scalar(
                "SELECT id FROM runners WHERE owner_user_id = $1 AND registration_key = $2",
            )
            .bind(principal.user_id)
            .bind(input.idempotency_key)
            .fetch_one(&mut *tx)
            .await?;
            let binding =
                runner_binding(&mut tx, &credential)
                    .await
                    .map_err(|error| match error {
                        IdentityError::Unauthenticated => IdentityError::IdempotencyConflict,
                        other => other,
                    })?;
            if binding.runner_id != id
                || binding.workspace_id != workspace.0
                || binding.owner_user_id != principal.user_id
            {
                return Err(IdentityError::IdempotencyConflict);
            }
            let identity = load_runner(&mut tx, binding).await?;
            if identity.name != name {
                return Err(IdentityError::IdempotencyConflict);
            }
            identity
        };
        tx.commit().await?;
        Ok(identity)
    }
}

pub(crate) async fn authenticate_runner_in(
    tx: &mut Transaction<'_, Postgres>,
    credential: &BearerDigest,
) -> Result<RunnerIdentity, IdentityError> {
    let binding = runner_binding(tx, credential).await?;
    let actor = Actor::Runner {
        workspace: WorkspaceId(binding.workspace_id),
        runner: RunnerId(binding.runner_id),
        owner: UserId(binding.owner_user_id),
    };
    let identity = load_runner(tx, binding).await?;
    require_permission(
        actor,
        Action::UseRunnerIdentity,
        Resource::Runner {
            workspace: identity.workspace_id,
            runner: identity.runner_id,
            owner: identity.owner_user_id,
        },
    )?;
    Ok(identity)
}

async fn runner_binding(
    tx: &mut Transaction<'_, Postgres>,
    credential: &BearerDigest,
) -> Result<RunnerBinding, IdentityError> {
    bind_credential_digest(tx, credential).await?;
    sqlx::query_as(
        "SELECT c.id AS credential_id, c.runner_id, c.owner_user_id, c.workspace_id
         FROM credentials c JOIN users u ON u.id = c.owner_user_id
         WHERE c.digest = $1 AND c.runner_id IS NOT NULL AND c.revoked_at IS NULL AND u.disabled_at IS NULL
           AND (c.expires_at IS NULL OR c.expires_at > transaction_timestamp()) FOR SHARE OF c, u",
    ).bind(credential.as_bytes()).fetch_optional(&mut **tx).await?.ok_or(IdentityError::Unauthenticated)
}

pub(super) async fn load_runner(
    tx: &mut Transaction<'_, Postgres>,
    binding: RunnerBinding,
) -> Result<RunnerIdentity, IdentityError> {
    bind_user(tx, binding.owner_user_id).await?;
    let name = sqlx::query_scalar::<_, String>(
        "SELECT r.name FROM runners r
         JOIN workspace_memberships m ON m.workspace_id = r.workspace_id AND m.user_id = r.owner_user_id
         WHERE r.id = $1 AND r.workspace_id = $2 AND r.owner_user_id = $3
           AND r.disabled_at IS NULL AND m.revoked_at IS NULL FOR SHARE OF r, m",
    ).bind(binding.runner_id).bind(binding.workspace_id).bind(binding.owner_user_id)
    .fetch_optional(&mut **tx).await?.ok_or(IdentityError::Unauthenticated)?;
    Ok(RunnerIdentity {
        runner_id: RunnerId(binding.runner_id),
        workspace_id: WorkspaceId(binding.workspace_id),
        credential_id: CredentialId(binding.credential_id),
        owner_user_id: UserId(binding.owner_user_id),
        name,
    })
}
