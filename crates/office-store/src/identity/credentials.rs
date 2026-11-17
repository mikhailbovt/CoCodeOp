use chrono::{DateTime, Utc};
use office_domain::{Action, Actor, BearerDigest, CredentialId, Resource, UserId};
use office_protocol::{
    CredentialSubject, IssueCredential, IssuedCredential, RevokeCredential, RevokedCredential,
};
use uuid::Uuid;

use super::{
    IdentityError,
    agent::{AgentBinding, load_agent},
    lookup, require_permission,
    runner::{RunnerBinding, load_runner},
};
use crate::Store;

#[derive(sqlx::FromRow)]
struct Issuance {
    id: Uuid,
    workspace_id: Option<Uuid>,
    agent_id: Option<Uuid>,
    runner_id: Option<Uuid>,
    digest: Vec<u8>,
}

impl Store {
    pub async fn issue_credential(
        &self,
        actor: &BearerDigest,
        input: IssueCredential,
    ) -> Result<IssuedCredential, IdentityError> {
        let digest =
            BearerDigest::decode(&input.credential_digest).ok_or(IdentityError::InvalidInput)?;
        let mut tx = self.pool.begin().await?;
        let principal = lookup(&mut tx, actor).await?;
        let owner = UserId(principal.user_id);
        require_permission(
            Actor::Human(owner),
            Action::ManageCredentials,
            Resource::Account(owner),
        )?;
        let id = Uuid::new_v4();
        let owned_scope = |error| match error {
            IdentityError::Unauthenticated => IdentityError::Forbidden,
            other => other,
        };
        let (workspace, agent, runner) = match input.subject {
            CredentialSubject::User => (None, None, None),
            CredentialSubject::Agent {
                workspace_id,
                agent_id,
            } => {
                load_agent(
                    &mut tx,
                    AgentBinding {
                        credential_id: id,
                        agent_id: agent_id.0,
                        owner_user_id: principal.user_id,
                        workspace_id: workspace_id.0,
                    },
                )
                .await
                .map_err(owned_scope)?;
                (Some(workspace_id.0), Some(agent_id.0), None)
            }
            CredentialSubject::Runner {
                workspace_id,
                runner_id,
            } => {
                load_runner(
                    &mut tx,
                    RunnerBinding {
                        credential_id: id,
                        workspace_id: workspace_id.0,
                        runner_id: runner_id.0,
                        owner_user_id: principal.user_id,
                    },
                )
                .await
                .map_err(owned_scope)?;
                (Some(workspace_id.0), None, Some(runner_id.0))
            }
        };
        let inserted = sqlx::query(
            "INSERT INTO credentials (id, owner_user_id, digest, workspace_id, agent_id, runner_id, issuance_key)
             VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING",
        )
        .bind(id).bind(principal.user_id).bind(digest.as_bytes()).bind(workspace).bind(agent)
        .bind(runner).bind(input.idempotency_key).execute(&mut *tx).await?;

        let credential_id = if inserted.rows_affected() == 1 {
            id
        } else {
            let previous = sqlx::query_as::<_, Issuance>(
                "SELECT id, workspace_id, agent_id, runner_id, digest FROM credentials WHERE owner_user_id = $1
                 AND issuance_key = $2 AND revoked_at IS NULL
                 AND (expires_at IS NULL OR expires_at > transaction_timestamp()) FOR SHARE",
            )
            .bind(principal.user_id).bind(input.idempotency_key).fetch_optional(&mut *tx).await?
            .ok_or(IdentityError::IdempotencyConflict)?;
            if previous.workspace_id != workspace
                || previous.agent_id != agent
                || previous.runner_id != runner
                || previous.digest != digest.as_bytes()
            {
                return Err(IdentityError::IdempotencyConflict);
            }
            previous.id
        };
        tx.commit().await?;
        Ok(IssuedCredential {
            credential_id: CredentialId(credential_id),
            owner_user_id: UserId(principal.user_id),
            subject: input.subject,
        })
    }

    pub async fn revoke_credential(
        &self,
        actor: &BearerDigest,
        credential: CredentialId,
        input: RevokeCredential,
    ) -> Result<RevokedCredential, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let principal = lookup(&mut tx, actor).await?;
        let owner = UserId(principal.user_id);
        let target_owner: Uuid =
            sqlx::query_scalar("SELECT owner_user_id FROM credentials WHERE id = $1")
                .bind(credential.0)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(IdentityError::Forbidden)?;
        require_permission(
            Actor::Human(owner),
            Action::ManageCredentials,
            Resource::Account(UserId(target_owner)),
        )?;
        let result = sqlx::query_scalar::<_, DateTime<Utc>>(
            "UPDATE credentials SET revoked_at = coalesce(revoked_at, transaction_timestamp()),
             revocation_key = coalesce(revocation_key, $3)
             WHERE id = $1 AND owner_user_id = $2 AND (revocation_key IS NULL OR revocation_key = $3)
             RETURNING revoked_at",
        )
        .bind(credential.0).bind(principal.user_id).bind(input.idempotency_key)
        .fetch_optional(&mut *tx).await.map_err(|error| {
            if error.as_database_error().is_some_and(|cause| cause.is_unique_violation()) {
                IdentityError::IdempotencyConflict
            } else { error.into() }
        })?;
        let revoked_at = result.ok_or(IdentityError::Forbidden)?;
        tx.commit().await?;
        Ok(RevokedCredential {
            credential_id: credential,
            revoked_at,
        })
    }
}
