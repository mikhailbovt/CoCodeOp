use chrono::{DateTime, Utc};
use office_domain::{Action, Actor, BearerDigest, InvitationId, UserId, WorkspaceId};
use office_protocol::{CreateInvitation, Identity, Invitation};
use uuid::Uuid;

use super::{
    IdentityError, PrincipalRow, authorize_workspace, bind_credential_owner, bind_user,
    load_identity, lookup, valid_name,
};
use crate::Store;

#[derive(sqlx::FromRow)]
struct InvitationRow {
    id: Uuid,
    workspace_id: Uuid,
    inviter_user_id: Uuid,
    display_name: String,
    digest: Vec<u8>,
    expires_at: DateTime<Utc>,
    expired: bool,
    accepted_credential_id: Option<Uuid>,
    acceptance_key: Option<Uuid>,
}

const COLUMNS: &str = "id, workspace_id, inviter_user_id, display_name, digest, expires_at,
    expires_at <= clock_timestamp() AS expired, accepted_credential_id, acceptance_key";

impl Store {
    pub async fn invite_user(
        &self,
        actor: &BearerDigest,
        workspace: WorkspaceId,
        request: CreateInvitation,
    ) -> Result<Invitation, IdentityError> {
        let name = valid_name(&request.display_name)?;
        let digest =
            BearerDigest::decode(&request.invitation_digest).ok_or(IdentityError::InvalidInput)?;
        let mut tx = self.pool.begin().await?;
        let principal = lookup(&mut tx, actor).await?;
        authorize_workspace(
            &mut tx,
            Actor::Human(UserId(principal.user_id)),
            Action::InviteMember,
            workspace,
        )
        .await?;
        let id = Uuid::new_v4();
        let inserted: Option<DateTime<Utc>> = sqlx::query_scalar(
            "INSERT INTO workspace_invitations (id, workspace_id, inviter_user_id, idempotency_key, display_name, digest)
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING RETURNING expires_at",
        )
        .bind(id).bind(workspace.0).bind(principal.user_id).bind(request.idempotency_key).bind(name).bind(digest.as_bytes())
        .fetch_optional(&mut *tx).await?;

        let result = if let Some(expires_at) = inserted {
            Invitation {
                invitation_id: InvitationId(id),
                workspace_id: workspace,
                display_name: name.to_owned(),
                expires_at,
            }
        } else {
            let existing = sqlx::query_as::<_, InvitationRow>(&format!(
                "SELECT {COLUMNS} FROM workspace_invitations WHERE inviter_user_id = $1 AND idempotency_key = $2",
            ))
            .bind(principal.user_id).bind(request.idempotency_key).fetch_optional(&mut *tx).await?
            .ok_or(IdentityError::IdempotencyConflict)?;
            if existing.workspace_id != workspace.0
                || existing.display_name != name
                || existing.digest != digest.as_bytes()
            {
                return Err(IdentityError::IdempotencyConflict);
            }
            Invitation {
                invitation_id: InvitationId(existing.id),
                workspace_id: workspace,
                display_name: existing.display_name,
                expires_at: existing.expires_at,
            }
        };
        tx.commit().await?;
        Ok(result)
    }

    pub async fn accept_invitation(
        &self,
        invitation: &BearerDigest,
        credential: &BearerDigest,
        acceptance_key: Uuid,
    ) -> Result<Identity, IdentityError> {
        if invitation.as_bytes() == credential.as_bytes() {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT set_config('office.invitation_digest', $1, true)")
            .bind(hex::encode(invitation.as_bytes()))
            .execute(&mut *tx)
            .await?;
        let offer = sqlx::query_as::<_, InvitationRow>(&format!(
            "SELECT {COLUMNS} FROM workspace_invitations WHERE digest = $1 FOR UPDATE",
        ))
        .bind(invitation.as_bytes())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(IdentityError::Unauthenticated)?;

        if let Some(accepted) = offer.accepted_credential_id {
            let principal = lookup(&mut tx, credential).await?;
            if principal.credential_id != accepted || offer.acceptance_key != Some(acceptance_key) {
                return Err(IdentityError::Unauthenticated);
            }
            let identity = load_identity(&mut tx, principal).await?;
            if !identity
                .memberships
                .iter()
                .any(|member| member.workspace_id.0 == offer.workspace_id)
            {
                return Err(IdentityError::Forbidden);
            }
            tx.commit().await?;
            return Ok(identity);
        }
        if offer.expired {
            return Err(IdentityError::Unauthenticated);
        }
        authorize_workspace(
            &mut tx,
            Actor::Human(UserId(offer.inviter_user_id)),
            Action::InviteMember,
            WorkspaceId(offer.workspace_id),
        )
        .await?;

        let user_id = Uuid::new_v4();
        let credential_id = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, display_name) VALUES ($1, $2)")
            .bind(user_id)
            .bind(&offer.display_name)
            .execute(&mut *tx)
            .await?;
        bind_user(&mut tx, user_id).await?;
        sqlx::query("INSERT INTO workspace_memberships (workspace_id, user_id, role) VALUES ($1, $2, 'member')")
            .bind(offer.workspace_id).bind(user_id).execute(&mut *tx).await?;
        bind_credential_owner(&mut tx, user_id).await?;
        let inserted = sqlx::query("INSERT INTO credentials (id, owner_user_id, digest) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
            .bind(credential_id).bind(user_id).bind(credential.as_bytes()).execute(&mut *tx).await?;
        if inserted.rows_affected() != 1 {
            return Err(IdentityError::IdempotencyConflict);
        }
        sqlx::query(
            "UPDATE workspace_invitations SET accepted_user_id = $2, accepted_credential_id = $3,
             acceptance_key = $4, accepted_at = transaction_timestamp() WHERE id = $1",
        )
        .bind(offer.id)
        .bind(user_id)
        .bind(credential_id)
        .bind(acceptance_key)
        .execute(&mut *tx)
        .await?;
        let identity = load_identity(
            &mut tx,
            PrincipalRow {
                user_id,
                display_name: offer.display_name,
                credential_id,
            },
        )
        .await?;
        tx.commit().await?;
        Ok(identity)
    }
}
