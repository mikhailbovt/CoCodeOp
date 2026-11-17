use chrono::{DateTime, Utc};
use office_domain::{BearerDigest, RunnerId, WorkspaceId};
use office_protocol::{
    CheckoutContentHead, CheckoutCoordinatorLease, CheckoutCoordinatorRef, CheckoutHostHealth,
    InspectCheckoutHost, RootIdentity, RunnerIdentity, ScopeDeploymentProof,
    StartCheckoutCoordinator,
};
use serde_json::json;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::{checkout_runner, load_checkout, lock_policy};
use crate::tasks::publications::publication_barrier;
use crate::{Store, events::append_workspace_event, identity::IdentityError};

#[derive(sqlx::FromRow)]
struct CoordinatorRow {
    workspace_id: Uuid,
    checkout_id: Uuid,
    host_runner_id: Uuid,
    epoch: i64,
    started_at: DateTime<Utc>,
    lease_until: DateTime<Utc>,
    ready_at: Option<DateTime<Utc>>,
    stopped_at: Option<DateTime<Utc>>,
}

impl From<CoordinatorRow> for CheckoutCoordinatorLease {
    fn from(row: CoordinatorRow) -> Self {
        Self {
            workspace_id: WorkspaceId(row.workspace_id),
            checkout_id: row.checkout_id,
            runner_id: RunnerId(row.host_runner_id),
            epoch: row.epoch,
            started_at: row.started_at,
            lease_until: row.lease_until,
            ready_at: row.ready_at,
            stopped_at: row.stopped_at,
        }
    }
}

impl Store {
    pub async fn inspect_checkout_host(
        &self,
        digest: &BearerDigest,
        input: InspectCheckoutHost,
    ) -> Result<CheckoutHostHealth, IdentityError> {
        let checkout = input.checkout_id;
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await?;
        let runner = checkout_runner(&mut tx, digest).await?;
        let workspace = runner.workspace_id;
        let (root, _) = checkout_root(&mut tx, &runner, checkout).await?;
        let registered = load_checkout(&mut tx, workspace, checkout, false).await?;
        let manifest = registered.manifest.0;
        let lease = sqlx::query_as::<_, CoordinatorRow>(
            "SELECT * FROM checkout_coordinator_sessions WHERE workspace_id=$1 AND checkout_id=$2
             AND stopped_at IS NULL AND lease_until>transaction_timestamp()",
        )
        .bind(workspace.0)
        .bind(checkout)
        .fetch_optional(&mut *tx)
        .await?;
        let state_id = if let Some(lease) = &lease {
            sqlx::query_scalar::<_, Option<Uuid>>(
                "SELECT publication_state_id FROM checkout_coordinator_sessions
                 WHERE workspace_id=$1 AND checkout_id=$2 AND epoch=$3",
            )
            .bind(workspace.0)
            .bind(checkout)
            .bind(lease.epoch)
            .fetch_one(&mut *tx)
            .await?
        } else {
            None
        };
        let head = sqlx::query_as::<_, (i64, String, Option<Uuid>, bool)>(
            "SELECT checkout_revision,source_sha256,applying_change_id,blocked FROM checkout_content_heads
             WHERE workspace_id=$1 AND checkout_id=$2",
        ).bind(workspace.0).bind(checkout).fetch_optional(&mut *tx).await?;
        let (head, applies_pending, needs_reconcile) = match head {
            Some((checkout_revision, source_sha256, pending, blocked)) => (
                Some(CheckoutContentHead {
                    checkout_revision,
                    source_sha256,
                }),
                pending.is_some(),
                blocked,
            ),
            None => (
                registered.baseline.map(|baseline| CheckoutContentHead {
                    checkout_revision: 0,
                    source_sha256: baseline.0.manifest_sha256,
                }),
                false,
                false,
            ),
        };
        let scope = if let Some(task) = input.scope_task_id {
            sqlx::query_as::<_, (Uuid, i64, String)>(
                "SELECT r.scope_id,r.revision,r.deployment_sha256 FROM work_scope_revisions r
                 JOIN work_scopes s ON (r.workspace_id,r.scope_id)=(s.workspace_id,s.id)
                 WHERE r.workspace_id=$1 AND r.checkout_id=$2 AND s.task_id=$3
                   AND r.state='active' AND r.expires_at>transaction_timestamp()",
            )
            .bind(workspace.0)
            .bind(checkout)
            .bind(task.0)
            .fetch_optional(&mut *tx)
            .await?
            .map(
                |(scope_id, revision, deployment_sha256)| ScopeDeploymentProof {
                    scope_id,
                    revision,
                    deployment_sha256,
                },
            )
        } else {
            None
        };
        tx.commit().await?;
        Ok(CheckoutHostHealth {
            checkout_id: checkout,
            root,
            manifest,
            lease: lease.map(Into::into),
            state_id,
            head,
            applies_pending,
            needs_reconcile,
            scope,
        })
    }

    pub async fn start_checkout_coordinator(
        &self,
        digest: &BearerDigest,
        input: StartCheckoutCoordinator,
    ) -> Result<CheckoutCoordinatorLease, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let runner = checkout_runner(&mut tx, digest).await?;
        lock_policy(&mut tx, input.checkout_id).await?;
        let (root, project_id) = checkout_root(&mut tx, &runner, input.checkout_id).await?;
        if root != input.root {
            return Err(IdentityError::Forbidden);
        }
        if let Some(row) = sqlx::query_as::<_, CoordinatorRow>(
            "SELECT * FROM checkout_coordinator_sessions WHERE workspace_id=$1 AND checkout_id=$2 AND instance_key=$3",
        ).bind(runner.workspace_id.0).bind(input.checkout_id).bind(input.instance_key).fetch_optional(&mut *tx).await? {
            require_epoch(&mut tx, runner.workspace_id, input.checkout_id, Some(row.epoch), false).await?;
            tx.commit().await?;
            return Ok(row.into());
        }
        sqlx::query(
            "UPDATE checkout_coordinator_sessions SET stopped_at=clock_timestamp()
                      WHERE workspace_id=$1 AND checkout_id=$2 AND stopped_at IS NULL",
        )
        .bind(runner.workspace_id.0)
        .bind(input.checkout_id)
        .execute(&mut *tx)
        .await?;
        let row = sqlx::query_as::<_, CoordinatorRow>(
            "INSERT INTO checkout_coordinator_sessions (workspace_id,checkout_id,owner_user_id,host_runner_id,instance_key,lease_until)
             VALUES ($1,$2,$3,$4,$5,clock_timestamp()+interval '15 seconds') RETURNING *",
        ).bind(runner.workspace_id.0).bind(input.checkout_id).bind(runner.owner_user_id.0)
            .bind(runner.runner_id.0).bind(input.instance_key).fetch_one(&mut *tx).await?;
        append_workspace_event(
            &mut tx,
            runner.workspace_id,
            project_id,
            "checkout.coordinator_started",
            json!({"checkout_id":input.checkout_id,"runner_id":runner.runner_id,"epoch":row.epoch}),
            None,
        )
        .await?;
        tx.commit().await?;
        Ok(row.into())
    }

    pub async fn renew_checkout_coordinator(
        &self,
        digest: &BearerDigest,
        input: CheckoutCoordinatorRef,
    ) -> Result<CheckoutCoordinatorLease, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let runner = checkout_runner(&mut tx, digest).await?;
        lock_policy(&mut tx, input.checkout_id).await?;
        let established = sqlx::query_scalar::<_, bool>(
            "SELECT ready_at IS NOT NULL FROM checkout_coordinator_sessions
            WHERE workspace_id=$1 AND checkout_id=$2 AND host_runner_id=$3 AND epoch=$4
            AND stopped_at IS NULL AND lease_until>clock_timestamp()",
        )
        .bind(runner.workspace_id.0)
        .bind(input.checkout_id)
        .bind(runner.runner_id.0)
        .bind(input.epoch)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(IdentityError::CoordinatorStale)?;
        if !established {
            publication_barrier(
                &mut tx,
                runner.workspace_id,
                input.checkout_id,
                Some(input.epoch),
            )
            .await?;
        }
        let row = sqlx::query_as::<_, CoordinatorRow>(
            "UPDATE checkout_coordinator_sessions SET lease_until=clock_timestamp()+interval '15 seconds',
                   ready_at=coalesce(ready_at,clock_timestamp())
             WHERE workspace_id=$1 AND checkout_id=$2 AND host_runner_id=$3 AND epoch=$4
                   AND stopped_at IS NULL AND lease_until>clock_timestamp() RETURNING *",
        ).bind(runner.workspace_id.0).bind(input.checkout_id).bind(runner.runner_id.0).bind(input.epoch)
            .fetch_optional(&mut *tx).await?.ok_or(IdentityError::CoordinatorStale)?;
        tx.commit().await?;
        Ok(row.into())
    }

    pub async fn stop_checkout_coordinator(
        &self,
        digest: &BearerDigest,
        input: CheckoutCoordinatorRef,
    ) -> Result<CheckoutCoordinatorLease, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let runner = checkout_runner(&mut tx, digest).await?;
        lock_policy(&mut tx, input.checkout_id).await?;
        let row = sqlx::query_as::<_, CoordinatorRow>(
            "UPDATE checkout_coordinator_sessions SET stopped_at=coalesce(stopped_at,clock_timestamp())
             WHERE workspace_id=$1 AND checkout_id=$2 AND host_runner_id=$3 AND epoch=$4 RETURNING *",
        ).bind(runner.workspace_id.0).bind(input.checkout_id).bind(runner.runner_id.0).bind(input.epoch)
            .fetch_optional(&mut *tx).await?.ok_or(IdentityError::CoordinatorStale)?;
        tx.commit().await?;
        Ok(row.into())
    }
}

pub(crate) async fn checkout_root(
    tx: &mut Transaction<'_, Postgres>,
    runner: &RunnerIdentity,
    checkout: Uuid,
) -> Result<(RootIdentity, Uuid), IdentityError> {
    let row = load_checkout(tx, runner.workspace_id, checkout, false).await?;
    if row.host_runner_id != runner.runner_id.0 {
        return Err(IdentityError::Forbidden);
    }
    let root_id = row.root_ref.ok_or(IdentityError::InvalidTransition)?;
    let (host_fingerprint, directory_id, ancestors) =
        sqlx::query_as::<_, (String, String, Vec<String>)>(
            "SELECT host_fingerprint,directory_id,ancestor_ids FROM checkout_roots
         WHERE workspace_id=$1 AND id=$2 AND checkout_id=$3 AND host_runner_id=$4",
        )
        .bind(runner.workspace_id.0)
        .bind(root_id)
        .bind(checkout)
        .bind(runner.runner_id.0)
        .fetch_one(&mut **tx)
        .await?;
    Ok((
        RootIdentity {
            host_fingerprint,
            directory_id,
            ancestors,
        },
        row.project_id,
    ))
}

pub(crate) async fn require_epoch(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
    supplied: Option<i64>,
    ready: bool,
) -> Result<(), IdentityError> {
    let current = sqlx::query_as::<_, (i64, bool)>(
        "SELECT epoch,ready_at IS NOT NULL FROM checkout_coordinator_sessions
         WHERE workspace_id=$1 AND checkout_id=$2 AND stopped_at IS NULL AND lease_until>clock_timestamp()",
    ).bind(workspace.0).bind(checkout).fetch_optional(&mut **tx).await?;
    match (supplied, current) {
        (None, None) => Ok(()),
        (Some(supplied), Some((live, available))) if supplied == live && (!ready || available) => {
            Ok(())
        }
        _ => Err(IdentityError::CoordinatorStale),
    }
}
