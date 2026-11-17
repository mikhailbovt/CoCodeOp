use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use office_domain::{AgentId, BearerDigest, PathRule, RunId, TaskId, UserId, WorkspaceId};
use office_protocol::{
    ApplyCheckoutChanges, CheckoutChange, CheckoutChangeReceipt, CheckoutContentHead,
    CheckoutCoordinatorRef, CheckoutDependency, CheckoutFileDelta, CheckoutPublicationError,
    CheckoutPublicationRecovery, CheckoutPublicationSession, CheckoutWriteOffer,
    CheckoutWriteOrder, CheckoutWriteOutcome, Recipient, ReportCheckoutWrite, RunnerIdentity,
    WorkScopeDefinition, is_sha256,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction, types::Json};
use tokio::{sync::Notify, time::timeout_at};
use uuid::Uuid;

use super::{
    checkout_releases::release_barrier, checkpoint_snapshots::snapshot_barrier, claims,
    execution::start_effects::credentials::RunAccess, gateway::authorize_checkout_run,
};
use crate::{
    Store,
    checkouts::{
        areas::notification_areas, checkout_runner, coordinator::require_epoch, load_checkout,
        lock_policy,
    },
    events::append_workspace_event,
    identity::IdentityError,
    inbox::save_cursor,
};

const HOST_POLL_WINDOW: Duration = Duration::from_millis(650);
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(160);

#[derive(Default)]
struct PublicationWake {
    host: Notify,
    worker: Notify,
}

#[derive(Default)]
pub(crate) struct PublicationWakes(Mutex<HashMap<Uuid, Weak<PublicationWake>>>);

impl PublicationWakes {
    fn checkout(&self, id: Uuid) -> Arc<PublicationWake> {
        let mut checkouts = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(active) = checkouts.get(&id).and_then(Weak::upgrade) {
            return active;
        }
        // Retain only checkouts with active publication requests. PostgreSQL owns all state.
        checkouts.retain(|_, value| value.strong_count() != 0);
        let wake = Arc::new(PublicationWake::default());
        checkouts.insert(id, Arc::downgrade(&wake));
        wake
    }
}

#[derive(sqlx::FromRow)]
pub(super) struct HeadRow {
    state_id: Uuid,
    checkout_revision: i64,
    source_sha256: String,
    pub(super) blocked: bool,
    pub(super) applying_change_id: Option<Uuid>,
}

impl HeadRow {
    pub(super) fn head(&self) -> CheckoutContentHead {
        CheckoutContentHead {
            checkout_revision: self.checkout_revision,
            source_sha256: self.source_sha256.clone(),
        }
    }
}

#[derive(sqlx::FromRow)]
struct PublicationRow {
    id: Uuid,
    compensates_change_id: Option<Uuid>,
    checkout_id: Uuid,
    host_runner_id: Uuid,
    epoch: i64,
    task_id: Uuid,
    run_id: Uuid,
    executor_user_id: Uuid,
    worker_agent_id: Uuid,
    run_fence: i64,
    scope_id: Uuid,
    scope_revision: i64,
    request_sha256: Vec<u8>,
    definition: Option<Json<ApplyCheckoutChanges>>,
    scope_definition: Option<Json<WorkScopeDefinition>>,
    state: String,
    admit_until: DateTime<Utc>,
    offered_until: Option<DateTime<Utc>>,
    previous_revision: Option<i64>,
    previous_sha256: Option<String>,
    claim_fence: Option<i64>,
    permit_until: Option<DateTime<Utc>>,
    outcome: Option<Json<CheckoutWriteOutcome>>,
    read_dependencies: Json<Vec<CheckoutDependency>>,
    event_sequence: Option<i64>,
}

impl PublicationRow {
    fn order(self) -> Result<CheckoutWriteOrder, IdentityError> {
        Ok(CheckoutWriteOrder {
            change_id: self.id,
            epoch: self.epoch,
            previous: CheckoutContentHead {
                checkout_revision: self.previous_revision.ok_or(IdentityError::InvalidRole)?,
                source_sha256: self.previous_sha256.ok_or(IdentityError::InvalidRole)?,
            },
            scope: self.scope_definition.ok_or(IdentityError::InvalidRole)?.0,
            permit_until: self.permit_until.ok_or(IdentityError::InvalidRole)?,
            input: self.definition.ok_or(IdentityError::InvalidRole)?.0,
        })
    }

    fn receipt(self, event_cursor: Option<Uuid>) -> Result<CheckoutChangeReceipt, IdentityError> {
        let outcome = self.outcome.ok_or(IdentityError::PublicationPending)?.0;
        let (head, files) = match outcome {
            CheckoutWriteOutcome::Applied { head, files } => (head, files),
            CheckoutWriteOutcome::ReconcileRequired => {
                return Err(IdentityError::CheckoutReconciliationRequired);
            }
            CheckoutWriteOutcome::Restored { .. } => return Err(IdentityError::InvalidRole),
            CheckoutWriteOutcome::Rejected { reason } => {
                return Err(match reason {
                    CheckoutPublicationError::Forbidden => IdentityError::PathForbidden,
                    CheckoutPublicationError::ContentConflict => IdentityError::ContentConflict,
                    CheckoutPublicationError::ContextStale => IdentityError::ContextStale,
                    CheckoutPublicationError::TooLarge => IdentityError::InvalidInput,
                    CheckoutPublicationError::InterruptedBeforeStart => {
                        IdentityError::CheckoutOffline
                    }
                });
            }
        };
        Ok(CheckoutChangeReceipt {
            change_id: self.id,
            compensates_change_id: self.compensates_change_id,
            checkout_id: self.checkout_id,
            task_id: TaskId(self.task_id),
            run_id: RunId(self.run_id),
            author: Recipient {
                user_id: UserId(self.executor_user_id),
                agent_id: Some(AgentId(self.worker_agent_id)),
            },
            host_runner_id: office_domain::RunnerId(self.host_runner_id),
            coordinator_epoch: self.epoch,
            scope_revision: self.scope_revision,
            run_fence: self.run_fence,
            claim_fence: self.claim_fence.ok_or(IdentityError::InvalidRole)?,
            checkout_revision: head.checkout_revision,
            files,
            read_dependencies: self.read_dependencies.0,
            event_cursor: event_cursor.ok_or(IdentityError::InvalidRole)?,
        })
    }
}

impl Store {
    pub async fn apply_checkout_changes(
        &self,
        digest: &BearerDigest,
        input: ApplyCheckoutChanges,
    ) -> Result<CheckoutChangeReceipt, IdentityError> {
        if !input.is_valid() {
            return Err(IdentityError::InvalidInput);
        }
        let bytes = serde_json::to_vec(&input).map_err(|_| IdentityError::InvalidInput)?;
        let digest_of_input = Sha256::digest(bytes);
        let deadline = Instant::now() + Duration::from_secs(7);
        loop {
            let mut tx = self.pool.begin().await?;
            let (access, scope) = authorize(&mut tx, digest, &input).await?;
            let wake = self.publication_wakes.checkout(scope.checkout_id);
            let changed = wake.worker.notified();
            let mut host_changed = false;
            let existing: Option<PublicationRow> = sqlx::query_as(
                "SELECT * FROM checkout_publications WHERE workspace_id=$1 AND run_id=$2 AND idempotency_key=$3",
            ).bind(scope.workspace_id.0).bind(access.run_id.0).bind(input.idempotency_key)
             .fetch_optional(&mut *tx).await?;
            if let Some(row) = existing {
                if row.request_sha256.as_slice() != digest_of_input.as_slice()
                    || row.scope_id != scope.scope_id
                    || row.scope_revision != scope.revision
                {
                    return Err(IdentityError::IdempotencyConflict);
                }
                if row.outcome.is_some() {
                    let cursor = if let Some(sequence) = row.event_sequence {
                        Some(save_cursor(&mut tx, access.context, sequence).await?)
                    } else {
                        None
                    };
                    tx.commit().await?;
                    return row.receipt(cursor);
                }
                if row.state == "queued" {
                    let now = clock(&mut tx).await?;
                    let (epoch, _, lease_until) = ready_host(&mut tx, &scope).await?;
                    if row.admit_until <= now || row.epoch != epoch {
                        return Err(IdentityError::CoordinatorStale);
                    }
                    if row.offered_until.is_some_and(|until| until > now) {
                        host_changed =
                            try_admit(&mut tx, &access, &scope, &input, &row, lease_until).await?;
                    }
                }
            } else {
                if input.compensates_change_id.is_some() {
                    let head = load_head(&mut tx, scope.workspace_id, scope.checkout_id)
                        .await?
                        .ok_or(IdentityError::Forbidden)?;
                    verify_compensation(&mut tx, &scope, &input, &head).await?;
                }
                let (epoch, host, _) = ready_host(&mut tx, &scope).await?;
                let pending: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM checkout_publications WHERE workspace_id=$1 AND run_id=$2
                     AND state='queued' AND admit_until>clock_timestamp()",
                )
                .bind(scope.workspace_id.0)
                .bind(access.run_id.0)
                .fetch_one(&mut *tx)
                .await?;
                if pending >= 4 {
                    return Err(IdentityError::PublicationPending);
                }
                sqlx::query(
                    "INSERT INTO checkout_publications (id,workspace_id,checkout_id,host_runner_id,epoch,task_id,
                     run_id,executor_user_id,worker_agent_id,run_fence,scope_id,scope_revision,idempotency_key,
                     request_sha256,definition,compensates_change_id,admit_until)
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,clock_timestamp()+interval '15 seconds')",
                ).bind(Uuid::new_v4()).bind(scope.workspace_id.0).bind(scope.checkout_id).bind(host).bind(epoch)
                 .bind(scope.task_id.0).bind(access.run_id.0).bind(scope.worker.user_id.0)
                 .bind(scope.worker.agent_id.ok_or(IdentityError::InvalidRole)?.0).bind(access.run_fence)
                 .bind(scope.scope_id).bind(scope.revision).bind(input.idempotency_key)
                 .bind(digest_of_input.as_slice()).bind(Json(&input)).bind(input.compensates_change_id).execute(&mut *tx).await?;
                host_changed = true;
            }
            tx.commit().await?;
            if host_changed {
                wake.host.notify_waiters();
            }
            if Instant::now() >= deadline {
                return Err(IdentityError::PublicationPending);
            }
            let next_poll = (Instant::now() + WORKER_POLL_INTERVAL).min(deadline);
            let _ = timeout_at(next_poll.into(), changed).await;
        }
    }

    pub async fn recover_checkout_publications(
        &self,
        digest: &BearerDigest,
        input: CheckoutPublicationSession,
    ) -> Result<CheckoutPublicationRecovery, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let runner = checkout_runner(&mut tx, digest).await?;
        let proof = &input.coordinator;
        let existing = checked_head(&mut tx, &runner, &input, false).await?;
        let blocked = existing.as_ref().is_some_and(|head| head.blocked);
        if blocked && !input.reconcile {
            return Err(IdentityError::CheckoutReconciliationRequired);
        }
        let outstanding = match &existing {
            Some(head) => match head.applying_change_id {
                Some(id) => Some(publication(&mut tx, proof.checkout_id, id).await?.order()?),
                None => None,
            },
            None => None,
        };
        sqlx::query(
            "UPDATE checkout_coordinator_sessions SET publication_state_id=$4
             WHERE workspace_id=$1 AND checkout_id=$2 AND epoch=$3",
        )
        .bind(runner.workspace_id.0)
        .bind(proof.checkout_id)
        .bind(proof.epoch)
        .bind(input.state_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(CheckoutPublicationRecovery {
            head: existing.as_ref().map(HeadRow::head),
            outstanding,
            blocked,
        })
    }

    pub async fn next_checkout_publication(
        &self,
        digest: &BearerDigest,
        input: CheckoutPublicationSession,
        wait: bool,
    ) -> Result<Option<CheckoutWriteOffer>, IdentityError> {
        let wake = self
            .publication_wakes
            .checkout(input.coordinator.checkout_id);
        let deadline = tokio::time::Instant::now() + HOST_POLL_WINDOW;
        loop {
            let changed = wake.host.notified();
            let order = self
                .poll_checkout_publication(digest, &input, &wake)
                .await?;
            if order.is_some() || !wait || timeout_at(deadline, changed).await.is_err() {
                return Ok(order);
            }
        }
    }

    async fn poll_checkout_publication(
        &self,
        digest: &BearerDigest,
        input: &CheckoutPublicationSession,
        wake: &PublicationWake,
    ) -> Result<Option<CheckoutWriteOffer>, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let runner = checkout_runner(&mut tx, digest).await?;
        let proof = &input.coordinator;
        let existing = host_head(&mut tx, &runner, input, true).await?;
        if let Some(id) = existing.as_ref().and_then(|head| head.applying_change_id) {
            let row = publication(&mut tx, proof.checkout_id, id).await?;
            if row.epoch != proof.epoch || row.state != "applying" {
                return Err(IdentityError::CheckoutReconciliationRequired);
            }
            let checked_at = clock(&mut tx).await?;
            tx.commit().await?;
            return Ok(Some(CheckoutWriteOffer {
                order: row.order()?,
                checked_at,
            }));
        }
        let Some(state_id) = input.state_id else {
            return Ok(None);
        };
        sqlx::query(
            "DELETE FROM checkout_publications WHERE id IN (
               SELECT id FROM checkout_publications WHERE workspace_id=$1 AND checkout_id=$2
                 AND state='queued' AND admit_until<clock_timestamp()-interval '1 minute' LIMIT 32
             )",
        )
        .bind(runner.workspace_id.0)
        .bind(proof.checkout_id)
        .execute(&mut *tx)
        .await?;
        if scope_update_pending(&mut tx, runner.workspace_id, proof.checkout_id).await? {
            return Ok(None);
        }
        let notified = offer_next_publication(&mut tx, runner.workspace_id, proof).await?;
        if notified && existing.is_none() {
            let checkout =
                load_checkout(&mut tx, runner.workspace_id, proof.checkout_id, false).await?;
            let source_sha = checkout
                .baseline
                .ok_or(IdentityError::InvalidRole)?
                .0
                .manifest_sha256;
            sqlx::query(
                "INSERT INTO checkout_content_heads (workspace_id,checkout_id,host_runner_id,state_id,source_sha256)
                 VALUES ($1,$2,$3,$4,$5)",
            ).bind(runner.workspace_id.0).bind(proof.checkout_id).bind(runner.runner_id.0)
             .bind(state_id).bind(source_sha).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        if notified {
            wake.worker.notify_waiters();
        }
        Ok(None)
    }

    pub async fn report_checkout_publication(
        &self,
        digest: &BearerDigest,
        input: ReportCheckoutWrite,
    ) -> Result<(), IdentityError> {
        let mut tx = self.pool.begin().await?;
        let runner = checkout_runner(&mut tx, digest).await?;
        let proof = &input.session.coordinator;
        let head = host_head_for_report(&mut tx, &runner, &input.session).await?;
        if let CheckoutWriteOutcome::Restored { head: observed } = &input.outcome {
            if !input.session.reconcile
                || input.change_id.is_some()
                || head.applying_change_id.is_some()
                || head.head() != *observed
            {
                return Err(IdentityError::CheckoutReconciliationRequired);
            }
            if head.blocked {
                update_head(
                    &mut tx,
                    runner.workspace_id,
                    proof.checkout_id,
                    &input.outcome,
                )
                .await?;
                let checkout =
                    load_checkout(&mut tx, runner.workspace_id, proof.checkout_id, false).await?;
                append_workspace_event(&mut tx, runner.workspace_id, checkout.project_id, "checkout.reconciled",
                    json!({"checkout_id":proof.checkout_id,"checkout_revision":observed.checkout_revision}), None).await?;
            }
            tx.commit().await?;
            return Ok(());
        }
        if let Some(id) = input.change_id {
            let row = publication(&mut tx, proof.checkout_id, id).await?;
            let restoring = input.session.reconcile
                && head.blocked
                && row.state == "reconcile_required"
                && proof.epoch > row.epoch;
            if let Some(previous) = &row.outcome {
                if previous.0 == input.outcome {
                    return Ok(());
                }
                if !restoring {
                    return Err(IdentityError::IdempotencyConflict);
                }
            }
            if (row.state != "applying" && !restoring) || head.applying_change_id != Some(id) {
                return Err(IdentityError::CoordinatorStale);
            }
            let definition = &row.definition.as_ref().ok_or(IdentityError::InvalidRole)?.0;
            let event = match &input.outcome {
                CheckoutWriteOutcome::Applied { head: next, files } => {
                    verify_receipt(&row, definition, &head, next, files)?;
                    let checkout =
                        load_checkout(&mut tx, runner.workspace_id, proof.checkout_id, false)
                            .await?;
                    let sequence = append_workspace_event(
                        &mut tx, runner.workspace_id, checkout.project_id, "checkout.changed",
                        json!({"checkout_id":proof.checkout_id,"change_id":id,"checkout_revision":next.checkout_revision}), None,
                    ).await?;
                    Some(sequence)
                }
                _ => None,
            };
            if definition.claim_fence.is_none() {
                claims::release_publication_claim(
                    &mut tx,
                    runner.workspace_id,
                    proof.checkout_id,
                    row.claim_fence.ok_or(IdentityError::InvalidRole)?,
                )
                .await?;
            }
            store_outcome(&mut tx, id, &input.outcome, event).await?;
            if let (Some(sequence), CheckoutWriteOutcome::Applied { files, .. }) =
                (event, &input.outcome)
            {
                record_change_paths(&mut tx, runner.workspace_id, &row, &input.outcome).await?;
                record_notification_areas(&mut tx, runner.workspace_id, &row, sequence, files)
                    .await?;
            }
        } else if input.outcome != CheckoutWriteOutcome::ReconcileRequired
            || head.applying_change_id.is_some()
        {
            return Err(IdentityError::InvalidInput);
        }
        update_head(
            &mut tx,
            runner.workspace_id,
            proof.checkout_id,
            &input.outcome,
        )
        .await?;
        // Give the next queued worker a turn as this host finishes its current write.
        // Its own transaction still checks the live run, scope and path claims.
        if matches!(
            input.outcome,
            CheckoutWriteOutcome::Applied { .. } | CheckoutWriteOutcome::Rejected { .. }
        ) && !scope_update_pending(&mut tx, runner.workspace_id, proof.checkout_id).await?
        {
            offer_next_publication(&mut tx, runner.workspace_id, proof).await?;
        }
        tx.commit().await?;
        self.publication_wakes
            .checkout(proof.checkout_id)
            .worker
            .notify_waiters();
        Ok(())
    }
}

async fn offer_next_publication(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    proof: &CheckoutCoordinatorRef,
) -> Result<bool, IdentityError> {
    let result = sqlx::query(
        "UPDATE checkout_publications SET offered_until=clock_timestamp()+interval '3 seconds'
         WHERE id=(SELECT p.id FROM checkout_publications p
             JOIN work_scope_revisions s ON (s.workspace_id,s.scope_id,s.revision)=(p.workspace_id,p.scope_id,p.scope_revision)
             WHERE p.workspace_id=$1 AND p.checkout_id=$2 AND p.epoch=$3 AND p.state='queued'
               AND p.admit_until>clock_timestamp() AND s.state='active' AND s.operation='apply'
               AND s.expires_at>clock_timestamp()
             ORDER BY p.offered_until ASC NULLS FIRST,p.created_at,p.id LIMIT 1)",
    ).bind(workspace.0).bind(proof.checkout_id).bind(proof.epoch).execute(&mut **tx).await?;
    Ok(result.rows_affected() != 0)
}

async fn record_notification_areas(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    publication: &PublicationRow,
    sequence: i64,
    files: &[CheckoutFileDelta],
) -> Result<(), IdentityError> {
    let areas = notification_areas(tx, workspace, publication.checkout_id).await?;
    let mut keys = BTreeSet::new();
    for file in files {
        let area = areas
            .iter()
            .filter(|(_, _, rule)| rule.matches(&file.path))
            .max_by_key(|(_, _, rule)| match rule {
                PathRule::File { path } => (path.as_str().len(), true),
                PathRule::Directory { path } => (path.as_str().len(), false),
                PathRule::Tree => (0, false),
            });
        let key = match area {
            Some((id, revision, _)) => format!("{id}/{revision}"),
            None => {
                let parent = file
                    .path
                    .as_str()
                    .rsplit_once('/')
                    .map_or("", |(parent, _)| parent);
                format!("directory/{}", hex::encode(Sha256::digest(parent)))
            }
        };
        keys.insert(key);
    }
    let keys: Vec<_> = keys.into_iter().collect();
    sqlx::query(
        "INSERT INTO checkout_notification_areas (workspace_id,sequence,checkout_id,task_id,area_key)
         SELECT $1,$2,$3,$4,area FROM unnest($5::text[]) AS area",
    ).bind(workspace.0).bind(sequence).bind(publication.checkout_id).bind(publication.task_id).bind(&keys)
     .execute(&mut **tx).await?;
    Ok(())
}

async fn authorize(
    tx: &mut Transaction<'_, Postgres>,
    digest: &BearerDigest,
    input: &ApplyCheckoutChanges,
) -> Result<(RunAccess, WorkScopeDefinition), IdentityError> {
    let (access, scope, manifest) =
        authorize_checkout_run(tx, digest, input.scope_revision).await?;
    if !input.is_permitted(&manifest, &scope) {
        return Err(IdentityError::PathForbidden);
    }
    if !input.includes_scope_dependencies(&scope) {
        return Err(IdentityError::ContextStale);
    }
    Ok((access, scope))
}

async fn ready_host(
    tx: &mut Transaction<'_, Postgres>,
    scope: &WorkScopeDefinition,
) -> Result<(i64, Uuid, DateTime<Utc>), IdentityError> {
    if load_head(tx, scope.workspace_id, scope.checkout_id)
        .await?
        .is_some_and(|head| head.blocked)
    {
        return Err(IdentityError::CheckoutReconciliationRequired);
    }
    sqlx::query_as(
        "SELECT epoch,host_runner_id,lease_until FROM checkout_coordinator_sessions
         WHERE workspace_id=$1 AND checkout_id=$2 AND stopped_at IS NULL
           AND lease_until>clock_timestamp() AND ready_at IS NOT NULL AND publication_state_id IS NOT NULL",
    ).bind(scope.workspace_id.0).bind(scope.checkout_id).fetch_optional(&mut **tx).await?
        .ok_or(IdentityError::CheckoutOffline)
}

async fn try_admit(
    tx: &mut Transaction<'_, Postgres>,
    access: &RunAccess,
    scope: &WorkScopeDefinition,
    input: &ApplyCheckoutChanges,
    row: &PublicationRow,
    coordinator_until: DateTime<Utc>,
) -> Result<bool, IdentityError> {
    let head = load_head(tx, scope.workspace_id, scope.checkout_id)
        .await?
        .ok_or(IdentityError::PublicationPending)?;
    if head.blocked {
        return Err(IdentityError::CheckoutReconciliationRequired);
    }
    if head.applying_change_id.is_some()
        || snapshot_barrier(tx, scope.workspace_id, scope.checkout_id).await?
        || release_barrier(tx, scope.workspace_id, scope.checkout_id).await?
        || scope_update_pending(tx, scope.workspace_id, scope.checkout_id).await?
    {
        return Ok(false);
    }
    if input.base_checkout_revision > head.checkout_revision {
        return Err(IdentityError::RevisionMismatch {
            current: head.checkout_revision,
        });
    }
    verify_compensation(tx, scope, input, &head).await?;
    let (fence, claim_until) = claims::publication_claim(tx, scope, access.run_id, input).await?;
    let permit_until = scope
        .expires_at
        .min(claim_until)
        .min(access.lease_until)
        .min(coordinator_until);
    if permit_until <= clock(tx).await? {
        return Err(IdentityError::RunLeaseStale);
    }
    sqlx::query(
        "UPDATE checkout_publications SET state='applying',scope_definition=$2,previous_revision=$3,
         previous_sha256=$4,claim_fence=$5,permit_until=$6,read_dependencies=$7 WHERE id=$1 AND state='queued'",
    ).bind(row.id).bind(Json(scope)).bind(head.checkout_revision).bind(&head.source_sha256)
     .bind(fence).bind(permit_until).bind(Json(&input.read_dependencies)).execute(&mut **tx).await?;
    let marked = sqlx::query(
        "UPDATE checkout_content_heads SET applying_change_id=$3
         WHERE workspace_id=$1 AND checkout_id=$2 AND applying_change_id IS NULL AND NOT blocked",
    )
    .bind(scope.workspace_id.0)
    .bind(scope.checkout_id)
    .bind(row.id)
    .execute(&mut **tx)
    .await?;
    if marked.rows_affected() != 1 {
        return Err(IdentityError::CoordinatorStale);
    }
    Ok(true)
}

async fn verify_compensation(
    tx: &mut Transaction<'_, Postgres>,
    scope: &WorkScopeDefinition,
    input: &ApplyCheckoutChanges,
    current: &HeadRow,
) -> Result<(), IdentityError> {
    let Some(original) = input.compensates_change_id else {
        return Ok(());
    };
    sqlx::query("SELECT set_config('office.compensated_change_id',$1,true)")
        .bind(original.to_string())
        .execute(&mut **tx)
        .await?;
    let outcome = sqlx::query_scalar::<_, Json<CheckoutWriteOutcome>>(
        "SELECT outcome FROM checkout_publications WHERE (workspace_id,checkout_id,id)=($1,$2,$3)
         AND state='applied' AND (executor_user_id,worker_agent_id)=(office_user_id(),office_agent_id())",
    ).bind(scope.workspace_id.0).bind(scope.checkout_id).bind(original).fetch_optional(&mut **tx).await?
     .ok_or(IdentityError::Forbidden)?;
    let CheckoutWriteOutcome::Applied { head, mut files } = outcome.0 else {
        return Err(IdentityError::InvalidRole);
    };
    if head.checkout_revision > current.checkout_revision {
        return Err(IdentityError::CheckoutReconciliationRequired);
    }
    let mut requested = expected_files(input)?;
    for file in &mut files {
        std::mem::swap(&mut file.before_sha256, &mut file.after_sha256);
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    requested.sort_by(|left, right| left.path.cmp(&right.path));
    if requested != files {
        return Err(IdentityError::InvalidInput);
    }
    let paths: Vec<_> = files.iter().map(|file| file.path.as_str()).collect();
    let later: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM checkout_change_paths
         WHERE (workspace_id,checkout_id)=($1,$2) AND checkout_revision>$3 AND path=ANY($4))",
    )
    .bind(scope.workspace_id.0)
    .bind(scope.checkout_id)
    .bind(head.checkout_revision)
    .bind(paths)
    .fetch_one(&mut **tx)
    .await?;
    if later {
        return Err(IdentityError::ContentConflict);
    }
    sqlx::query("SELECT set_config('office.compensated_change_id','',true)")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn record_change_paths(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    row: &PublicationRow,
    outcome: &CheckoutWriteOutcome,
) -> Result<(), IdentityError> {
    let CheckoutWriteOutcome::Applied { head, files } = outcome else {
        return Ok(());
    };
    let paths: Vec<_> = files.iter().map(|delta| delta.path.as_str()).collect();
    sqlx::query(
        "INSERT INTO checkout_change_paths (workspace_id,checkout_id,change_id,checkout_revision,path)
         SELECT $1,$2,$3,$4,path FROM unnest($5::text[]) AS paths(path)",
    ).bind(workspace.0).bind(row.checkout_id).bind(row.id).bind(head.checkout_revision).bind(paths).execute(&mut **tx).await?;
    Ok(())
}

pub(crate) async fn publication_barrier(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
    epoch: Option<i64>,
) -> Result<(), IdentityError> {
    if let Some(head) = load_head(tx, workspace, checkout).await? {
        if head.blocked {
            return Err(IdentityError::CheckoutReconciliationRequired);
        }
        if head.applying_change_id.is_some() {
            return Err(IdentityError::PublicationPending);
        }
        let state: Option<Uuid> = sqlx::query_scalar(
            "SELECT publication_state_id FROM checkout_coordinator_sessions
             WHERE workspace_id=$1 AND checkout_id=$2 AND epoch=$3",
        )
        .bind(workspace.0)
        .bind(checkout)
        .bind(epoch)
        .fetch_optional(&mut **tx)
        .await?
        .flatten();
        if state != Some(head.state_id) {
            return Err(IdentityError::CheckoutReconciliationRequired);
        }
    }
    Ok(())
}

async fn host_head(
    tx: &mut Transaction<'_, Postgres>,
    runner: &RunnerIdentity,
    session: &CheckoutPublicationSession,
    ready: bool,
) -> Result<Option<HeadRow>, IdentityError> {
    let head = checked_head(tx, runner, session, ready).await?;
    if head.as_ref().is_some_and(|row| row.blocked) {
        return Err(IdentityError::CheckoutReconciliationRequired);
    }
    Ok(head)
}

async fn host_head_for_report(
    tx: &mut Transaction<'_, Postgres>,
    runner: &RunnerIdentity,
    session: &CheckoutPublicationSession,
) -> Result<HeadRow, IdentityError> {
    checked_head(tx, runner, session, false)
        .await?
        .ok_or(IdentityError::CoordinatorStale)
}

async fn checked_head(
    tx: &mut Transaction<'_, Postgres>,
    runner: &RunnerIdentity,
    session: &CheckoutPublicationSession,
    ready: bool,
) -> Result<Option<HeadRow>, IdentityError> {
    let proof = &session.coordinator;
    lock_policy(tx, proof.checkout_id).await?;
    require_epoch(
        tx,
        runner.workspace_id,
        proof.checkout_id,
        Some(proof.epoch),
        ready,
    )
    .await?;
    let head = load_head(tx, runner.workspace_id, proof.checkout_id).await?;
    if head
        .as_ref()
        .is_some_and(|row| Some(row.state_id) != session.state_id)
    {
        return Err(IdentityError::CheckoutReconciliationRequired);
    }
    if ready {
        let registered: Option<Uuid> = sqlx::query_scalar(
            "SELECT publication_state_id FROM checkout_coordinator_sessions WHERE workspace_id=$1 AND checkout_id=$2 AND epoch=$3",
        ).bind(runner.workspace_id.0).bind(proof.checkout_id).bind(proof.epoch).fetch_one(&mut **tx).await?;
        if registered != session.state_id {
            return Err(IdentityError::CoordinatorStale);
        }
    }
    Ok(head)
}

pub(super) async fn scope_update_pending(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
) -> Result<bool, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM work_scope_revisions
         WHERE workspace_id=$1 AND checkout_id=$2 AND state='pending_activation')",
    )
    .bind(workspace.0)
    .bind(checkout)
    .fetch_one(&mut **tx)
    .await?)
}

pub(super) async fn load_head(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
) -> Result<Option<HeadRow>, IdentityError> {
    Ok(sqlx::query_as(
        "SELECT * FROM checkout_content_heads WHERE workspace_id=$1 AND checkout_id=$2",
    )
    .bind(workspace.0)
    .bind(checkout)
    .fetch_optional(&mut **tx)
    .await?)
}

async fn publication(
    tx: &mut Transaction<'_, Postgres>,
    checkout: Uuid,
    id: Uuid,
) -> Result<PublicationRow, IdentityError> {
    sqlx::query_as("SELECT * FROM checkout_publications WHERE checkout_id=$1 AND id=$2")
        .bind(checkout)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(IdentityError::CoordinatorStale)
}

async fn clock(tx: &mut Transaction<'_, Postgres>) -> Result<DateTime<Utc>, IdentityError> {
    Ok(sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?)
}

fn verify_receipt(
    row: &PublicationRow,
    input: &ApplyCheckoutChanges,
    previous: &HeadRow,
    next: &CheckoutContentHead,
    files: &[CheckoutFileDelta],
) -> Result<(), IdentityError> {
    if Some(previous.checkout_revision) != row.previous_revision
        || Some(&previous.source_sha256) != row.previous_sha256.as_ref()
        || previous.checkout_revision.checked_add(1) != Some(next.checkout_revision)
        || !is_sha256(&next.source_sha256)
        || files != expected_files(input)?
    {
        return Err(IdentityError::InvalidInput);
    }
    Ok(())
}

fn expected_files(input: &ApplyCheckoutChanges) -> Result<Vec<CheckoutFileDelta>, IdentityError> {
    let mut expected = Vec::new();
    for change in &input.operations {
        let path = change.paths()[0].ok_or(IdentityError::InvalidInput)?;
        let before = change.before_sha256().map(str::to_owned);
        let after = match change.content() {
            Some(file) => Some(hex::encode(Sha256::digest(
                file.bytes().ok_or(IdentityError::InvalidInput)?,
            ))),
            None => None,
        };
        expected.push(CheckoutFileDelta {
            path: path.clone(),
            before_sha256: before.clone(),
            after_sha256: after,
        });
        if let CheckoutChange::Rename { target, .. } = change {
            expected.push(CheckoutFileDelta {
                path: target.clone(),
                before_sha256: None,
                after_sha256: before,
            });
        }
    }
    Ok(expected)
}

async fn store_outcome(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    outcome: &CheckoutWriteOutcome,
    event: Option<i64>,
) -> Result<(), IdentityError> {
    let (state, keep) = match outcome {
        CheckoutWriteOutcome::Applied { .. } => ("applied", false),
        CheckoutWriteOutcome::Rejected { .. } => ("rejected", false),
        CheckoutWriteOutcome::ReconcileRequired => ("reconcile_required", true),
        CheckoutWriteOutcome::Restored { .. } => return Err(IdentityError::InvalidInput),
    };
    sqlx::query(
        "UPDATE checkout_publications SET state=$2,outcome=$3,event_sequence=$4,finalized_at=clock_timestamp(),
         definition=CASE WHEN $5 THEN definition ELSE NULL END,
         scope_definition=CASE WHEN $5 THEN scope_definition ELSE NULL END WHERE id=$1",
    )
    .bind(id)
    .bind(state)
    .bind(Json(outcome))
    .bind(event)
    .bind(keep)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn update_head(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
    outcome: &CheckoutWriteOutcome,
) -> Result<(), IdentityError> {
    match outcome {
        CheckoutWriteOutcome::Applied { head, .. } => {
            sqlx::query(
                "UPDATE checkout_content_heads SET checkout_revision=$3,source_sha256=$4,applying_change_id=NULL,blocked=false
                 WHERE workspace_id=$1 AND checkout_id=$2",
            ).bind(workspace.0).bind(checkout).bind(head.checkout_revision).bind(&head.source_sha256).execute(&mut **tx).await?;
        }
        CheckoutWriteOutcome::Rejected { .. } | CheckoutWriteOutcome::Restored { .. } => {
            sqlx::query("UPDATE checkout_content_heads SET applying_change_id=NULL,blocked=false WHERE workspace_id=$1 AND checkout_id=$2")
                .bind(workspace.0).bind(checkout).execute(&mut **tx).await?;
        }
        CheckoutWriteOutcome::ReconcileRequired => {
            sqlx::query("UPDATE checkout_content_heads SET blocked=true WHERE workspace_id=$1 AND checkout_id=$2")
                .bind(workspace.0).bind(checkout).execute(&mut **tx).await?;
            sqlx::query("UPDATE checkout_coordinator_sessions SET stopped_at=clock_timestamp() WHERE workspace_id=$1 AND checkout_id=$2 AND stopped_at IS NULL")
                .bind(workspace.0).bind(checkout).execute(&mut **tx).await?;
        }
    }
    Ok(())
}
