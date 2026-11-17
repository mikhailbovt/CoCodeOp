use office_domain::{
    Action, Actor, BearerDigest, Resource, TaskAuthority, UserId, WorkPermissions, WorkspaceId,
};
use office_protocol::{RevokeWorkScope, ScopeRevocationReceipt, WorkScopeDefinition};
use serde_json::json;
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use super::{access::access_checkout, select_scope};
use crate::{
    Store,
    checkouts::{grants::bind_grant_validation, lock_policy},
    data_scope::{authorize_scope, subject},
    events::{Idempotency, append_event},
    identity::{IdentityError, require_permission, workspace_context},
    inbox::save_cursor,
    tasks::{load_task, task_text},
};

impl Store {
    pub async fn revoke_work_scope(
        &self,
        digest: &BearerDigest,
        input: RevokeWorkScope,
    ) -> Result<ScopeRevocationReceipt, IdentityError> {
        let reason = task_text(&input.reason, 2048)?;
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "work_scope.revoke",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let scope = select_scope(&mut tx, input.workspace_id, input.task_id, None, false)
            .await?
            .ok_or(IdentityError::Forbidden)?;
        authorize_scope(&mut tx, context, scope.id, Action::ReadData).await?;
        let checkout = access_checkout(&mut tx, input.workspace_id, scope.checkout_id).await?;
        let host_owner = context.actor == Actor::Human(UserId(checkout.owner_user_id));
        if !host_owner {
            let (task, _) = load_task(&mut tx, context, input.task_id, true).await?;
            require_permission(
                context.actor,
                Action::ManageTask,
                Resource::Task(TaskAuthority {
                    workspace: context.access,
                    accountable_owner: UserId(task.accountable_user_id),
                    accountable: subject(
                        UserId(task.accountable_user_id),
                        task.accountable_agent_id,
                    ),
                    assignee: None,
                    executor: None,
                    explicit_reviewer: None,
                }),
            )?;
        }
        lock_policy(&mut tx, scope.checkout_id).await?;
        let latest = select_scope(&mut tx, input.workspace_id, input.task_id, None, false)
            .await?
            .ok_or(IdentityError::Forbidden)?;
        if latest.revision != input.expected_scope_revision {
            return Err(IdentityError::RevisionMismatch {
                current: latest.revision,
            });
        }
        if latest.operation == "revoke" {
            return Err(IdentityError::InvalidTransition);
        }
        let revision = latest
            .revision
            .checked_add(1)
            .ok_or(IdentityError::InvalidTransition)?;
        sqlx::query("SELECT set_config('office.scope_revocation_context',$1,true)")
            .bind(latest.id.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO work_scope_revisions (workspace_id,checkout_id,scope_id,revision,grant_id,grant_revision,
                 expires_at,permissions,operation,requires_new_grant,required_read_paths) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'revoke',$9,$10)")
            .bind(input.workspace_id.0).bind(latest.checkout_id).bind(latest.id).bind(revision).bind(latest.grant_id)
            .bind(latest.grant_revision).bind(latest.expires_at).bind(Json(WorkPermissions::default()))
            .bind(host_owner).bind(latest.required_read_paths).execute(&mut *tx).await?;
        crate::tasks::claims::fence_scope(
            &mut tx,
            input.workspace_id,
            latest.checkout_id,
            latest.id,
        )
        .await?;
        let scope = select_scope(
            &mut tx,
            input.workspace_id,
            input.task_id,
            Some(revision),
            false,
        )
        .await?
        .ok_or(IdentityError::InvalidTransition)?
        .into_scope()?;
        let sequence = append_event(
            &mut tx,
            context,
            latest.id,
            "scope.revocation_pending",
            json!({
                "scope_id": latest.id, "task_id": input.task_id, "checkout_id": latest.checkout_id,
                "revision": revision, "reason": reason,
            }),
            None,
        )
        .await?;
        let result = ScopeRevocationReceipt {
            scope,
            event_cursor: save_cursor(&mut tx, context, sequence).await?,
        };
        idem.record(&mut tx, latest.id, &result).await?;
        tx.commit().await?;
        Ok(result)
    }
}

pub(super) async fn has_new_revocation(
    tx: &mut Transaction<'_, Postgres>,
    scope: &WorkScopeDefinition,
) -> Result<bool, IdentityError> {
    Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM work_scope_revisions WHERE workspace_id=$1 AND scope_id=$2
                         AND revision > $3 AND operation='revoke' AND state IN ('pending_activation','active'))")
        .bind(scope.workspace_id.0).bind(scope.scope_id).bind(scope.revision).fetch_one(&mut **tx).await?)
}

pub(super) async fn verify_fresh_grant(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    scope: Uuid,
    grant: Uuid,
) -> Result<(), IdentityError> {
    bind_grant_validation(tx, Some(grant)).await?;
    let fresh = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM checkout_grants g WHERE g.workspace_id=$1 AND g.id=$3
           AND NOT EXISTS (SELECT 1 FROM work_scope_revisions r WHERE r.workspace_id=g.workspace_id
              AND r.scope_id=$2 AND r.requires_new_grant AND r.authorization_order >= g.authorization_order))",
    )
    .bind(workspace.0)
    .bind(scope)
    .bind(grant)
    .fetch_one(&mut **tx)
    .await?;
    bind_grant_validation(tx, None).await?;
    if !fresh {
        return Err(IdentityError::AccessRequestRequired);
    }
    Ok(())
}
