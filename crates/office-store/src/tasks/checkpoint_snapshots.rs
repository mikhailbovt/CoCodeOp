use chrono::{DateTime, Utc};
use office_domain::{Action, Actor, AgentRole, BearerDigest, RunnerId, WorkspaceId};
use office_protocol::{
    CheckoutContentHead, CheckoutCoordinatorRef, CheckoutSnapshot, CheckoutSnapshotOrder,
    CheckoutSnapshotOutcome, CheckoutSnapshotProof, CheckoutSnapshotState, CreateCheckoutSnapshot,
    GetCheckout, GetCheckoutSnapshot, MAX_SOURCE_BYTES, MAX_SOURCE_FILES, ReportCheckoutSnapshot,
    is_sha256,
};
use serde_json::json;
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use super::publications::load_head;
use crate::{
    Store,
    checkouts::{
        CheckoutRow, checkout_runner, coordinator::require_epoch, load_checkout, lock_policy,
    },
    data_scope::authorize_scope,
    events::{Idempotency, append_event, append_workspace_event},
    identity::{IdentityError, WorkspaceContext, workspace_context},
};

#[derive(sqlx::FromRow)]
struct SnapshotRow {
    id: Uuid,
    workspace_id: Uuid,
    checkout_id: Uuid,
    host_runner_id: Uuid,
    checkout_revision: i64,
    source_sha256: String,
    outcome: Option<Json<CheckoutSnapshotOutcome>>,
    created_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
    expired: bool,
}

impl SnapshotRow {
    fn head(&self) -> CheckoutContentHead {
        CheckoutContentHead {
            checkout_revision: self.checkout_revision,
            source_sha256: self.source_sha256.clone(),
        }
    }

    fn order(&self) -> CheckoutSnapshotOrder {
        CheckoutSnapshotOrder {
            snapshot_id: self.id,
            checkout_id: self.checkout_id,
            head: self.head(),
        }
    }

    fn view(self) -> CheckoutSnapshot {
        let head = self.head();
        let (state, proof) = match self.outcome.map(|value| value.0) {
            None if self.expired => (CheckoutSnapshotState::Expired, None),
            None => (CheckoutSnapshotState::Queued, None),
            Some(CheckoutSnapshotOutcome::Ready { proof }) => {
                (CheckoutSnapshotState::Ready, Some(proof))
            }
            Some(CheckoutSnapshotOutcome::SourceChanged) => {
                (CheckoutSnapshotState::SourceChanged, None)
            }
            Some(CheckoutSnapshotOutcome::StorageUnavailable) => {
                (CheckoutSnapshotState::StorageUnavailable, None)
            }
        };
        CheckoutSnapshot {
            workspace_id: WorkspaceId(self.workspace_id),
            checkout_id: self.checkout_id,
            snapshot_id: self.id,
            host_runner_id: RunnerId(self.host_runner_id),
            head,
            state,
            proof,
            created_at: self.created_at,
            completed_at: self.completed_at,
        }
    }
}

impl Store {
    pub async fn get_checkout_head(
        &self,
        digest: &BearerDigest,
        input: GetCheckout,
    ) -> Result<CheckoutContentHead, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let checkout = authorized_checkout(&mut tx, context, input.checkout_id).await?;
        let (head, _) = capture_head(&mut tx, input.workspace_id, &checkout).await?;
        tx.commit().await?;
        Ok(head)
    }

    pub async fn create_checkout_snapshot(
        &self,
        digest: &BearerDigest,
        input: CreateCheckoutSnapshot,
    ) -> Result<CheckoutSnapshot, IdentityError> {
        if input.expected_checkout_revision < 0 {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let checkout = authorized_checkout(&mut tx, context, input.checkout_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "checkout.snapshot",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        lock_policy(&mut tx, checkout.id).await?;
        let (source, publishing) = capture_head(&mut tx, input.workspace_id, &checkout).await?;
        if source.checkout_revision != input.expected_checkout_revision {
            return Err(IdentityError::RevisionMismatch {
                current: source.checkout_revision,
            });
        }
        if publishing {
            return Err(IdentityError::PublicationPending);
        }
        let existing = sqlx::query_as::<_, SnapshotRow>(
            "SELECT s.*,expires_at<=clock_timestamp() AS expired FROM checkout_snapshots s
             WHERE (workspace_id,checkout_id,checkout_revision)=($1,$2,$3)
               AND (state='ready' OR (state='queued' AND expires_at>clock_timestamp()))
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(input.workspace_id.0)
        .bind(checkout.id)
        .bind(input.expected_checkout_revision)
        .fetch_optional(&mut *tx)
        .await?;
        let result = if let Some(existing) = existing {
            if existing.head() != source {
                return Err(IdentityError::CheckoutReconciliationRequired);
            }
            existing.view()
        } else {
            let row = sqlx::query_as::<_, SnapshotRow>(
                "INSERT INTO checkout_snapshots (id,workspace_id,checkout_id,host_runner_id,checkout_revision,source_sha256)
                 VALUES ($1,$2,$3,$4,$5,$6) RETURNING *,false AS expired",
            ).bind(Uuid::new_v4()).bind(input.workspace_id.0).bind(checkout.id).bind(checkout.host_runner_id)
             .bind(source.checkout_revision).bind(source.source_sha256).fetch_one(&mut *tx).await?;
            append_event(&mut tx, context, checkout.project_id, "checkout.snapshot_requested",
                json!({"checkout_id":checkout.id,"snapshot_id":row.id,"checkout_revision":row.checkout_revision}), None).await?;
            row.view()
        };
        idem.record(&mut tx, checkout.project_id, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn get_checkout_snapshot(
        &self,
        digest: &BearerDigest,
        input: GetCheckoutSnapshot,
    ) -> Result<CheckoutSnapshot, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        authorized_checkout(&mut tx, context, input.checkout_id).await?;
        let row = find_snapshot(
            &mut tx,
            input.workspace_id,
            input.checkout_id,
            input.snapshot_id,
        )
        .await?;
        tx.commit().await?;
        Ok(row.view())
    }

    pub async fn next_checkout_snapshot(
        &self,
        digest: &BearerDigest,
        lease: CheckoutCoordinatorRef,
    ) -> Result<Option<CheckoutSnapshotOrder>, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let runner = checkout_runner(&mut tx, digest).await?;
        lock_policy(&mut tx, lease.checkout_id).await?;
        require_epoch(
            &mut tx,
            runner.workspace_id,
            lease.checkout_id,
            Some(lease.epoch),
            true,
        )
        .await?;
        let head = load_head(&mut tx, runner.workspace_id, lease.checkout_id).await?;
        if head.is_some_and(|head| head.blocked || head.applying_change_id.is_some()) {
            return Ok(None);
        }
        let row = sqlx::query_as::<_, SnapshotRow>(
            "SELECT s.*,false AS expired FROM checkout_snapshots s
             WHERE (workspace_id,checkout_id) = ($1,$2) AND state='queued' AND expires_at>clock_timestamp()
             ORDER BY created_at LIMIT 1",
        ).bind(runner.workspace_id.0).bind(lease.checkout_id).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        Ok(row.map(|value| value.order()))
    }

    pub async fn report_checkout_snapshot(
        &self,
        digest: &BearerDigest,
        input: ReportCheckoutSnapshot,
    ) -> Result<(), IdentityError> {
        let mut tx = self.pool.begin().await?;
        let runner = checkout_runner(&mut tx, digest).await?;
        let lease = input.coordinator;
        lock_policy(&mut tx, lease.checkout_id).await?;
        require_epoch(
            &mut tx,
            runner.workspace_id,
            lease.checkout_id,
            Some(lease.epoch),
            true,
        )
        .await?;
        let row = find_snapshot(
            &mut tx,
            runner.workspace_id,
            lease.checkout_id,
            input.snapshot_id,
        )
        .await?;
        if let Some(outcome) = row.outcome {
            return if outcome.0 == input.outcome {
                Ok(())
            } else {
                Err(IdentityError::IdempotencyConflict)
            };
        }
        if row.expired {
            return Err(IdentityError::CoordinatorStale);
        }
        let state = match &input.outcome {
            CheckoutSnapshotOutcome::Ready { proof } => {
                if proof.baseline.manifest_sha256 != row.source_sha256
                    || !is_sha256(&proof.git_tree_sha256)
                    || !is_sha256(&proof.git_commit_sha256)
                    || proof.baseline.file_count > MAX_SOURCE_FILES
                    || proof.baseline.total_bytes > MAX_SOURCE_BYTES
                    || proof.baseline.file_count == 0 && proof.baseline.total_bytes != 0
                {
                    return Err(IdentityError::InvalidInput);
                }
                "ready"
            }
            CheckoutSnapshotOutcome::SourceChanged => "source_changed",
            CheckoutSnapshotOutcome::StorageUnavailable => "storage_unavailable",
        };
        let result = sqlx::query(
            "UPDATE checkout_snapshots SET state=$3,outcome=$4,completed_at=clock_timestamp()
             WHERE (workspace_id,id)=($1,$2) AND state='queued' AND expires_at>clock_timestamp()",
        )
        .bind(runner.workspace_id.0)
        .bind(row.id)
        .bind(state)
        .bind(Json(input.outcome))
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            return Err(IdentityError::CoordinatorStale);
        }
        let checkout =
            load_checkout(&mut tx, runner.workspace_id, lease.checkout_id, false).await?;
        append_workspace_event(&mut tx, runner.workspace_id, checkout.project_id, "checkout.snapshot_finished",
            json!({"checkout_id":lease.checkout_id,"snapshot_id":row.id,"checkout_revision":row.checkout_revision,"state":state}), None).await?;
        tx.commit().await?;
        Ok(())
    }
}

pub(super) async fn snapshot_barrier(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
) -> Result<bool, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM checkout_snapshots WHERE (workspace_id,checkout_id)=($1,$2)
           AND state='queued' AND expires_at>clock_timestamp())",
    )
    .bind(workspace.0)
    .bind(checkout)
    .fetch_one(&mut **tx)
    .await?)
}

pub(super) async fn sealed_head(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
    snapshot: Uuid,
) -> Result<CheckoutContentHead, IdentityError> {
    ready_snapshot(tx, workspace, checkout, snapshot)
        .await
        .map(|(head, _)| head)
}

pub(super) async fn ready_snapshot(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
    snapshot: Uuid,
) -> Result<(CheckoutContentHead, CheckoutSnapshotProof), IdentityError> {
    let row = find_snapshot(tx, workspace, checkout, snapshot).await?;
    let head = row.head();
    match row.outcome {
        Some(Json(CheckoutSnapshotOutcome::Ready { proof })) => Ok((head, proof)),
        _ => Err(IdentityError::PublicationPending),
    }
}

pub(super) async fn authorized_checkout(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    checkout: Uuid,
) -> Result<CheckoutRow, IdentityError> {
    let row = load_checkout(tx, context.access.workspace, checkout, false).await?;
    authorize_scope(tx, context, row.project_id, Action::ReadData).await?;
    if context.access.member.0 != row.owner_user_id
        || !matches!(
            context.actor,
            Actor::Human(_)
                | Actor::Agent {
                    role: AgentRole::Lead,
                    ..
                }
        )
    {
        return Err(IdentityError::Forbidden);
    }
    Ok(row)
}

pub(super) async fn capture_head(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: &CheckoutRow,
) -> Result<(CheckoutContentHead, bool), IdentityError> {
    if let Some(head) = load_head(tx, workspace, checkout.id).await? {
        if head.blocked {
            return Err(IdentityError::CheckoutReconciliationRequired);
        }
        return Ok((head.head(), head.applying_change_id.is_some()));
    }
    let baseline = checkout
        .baseline
        .as_ref()
        .ok_or(IdentityError::CheckoutOffline)?;
    Ok((
        CheckoutContentHead {
            checkout_revision: 0,
            source_sha256: baseline.0.manifest_sha256.clone(),
        },
        false,
    ))
}

async fn find_snapshot(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
    id: Uuid,
) -> Result<SnapshotRow, IdentityError> {
    sqlx::query_as(
        "SELECT s.*,expires_at<=clock_timestamp() AS expired FROM checkout_snapshots s
         WHERE (workspace_id,checkout_id,id)=($1,$2,$3)",
    )
    .bind(workspace.0)
    .bind(checkout)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(IdentityError::Forbidden)
}
