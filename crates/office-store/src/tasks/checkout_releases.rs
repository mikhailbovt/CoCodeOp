use chrono::{DateTime, Utc};
use office_domain::{BearerDigest, RunnerId, WorkspaceId};
use office_protocol::{
    CheckoutCommandOutcome, CheckoutContentHead, CheckoutCoordinatorRef, CheckoutRelease,
    CheckoutReleaseOrder, CheckoutReleaseOutcome, CheckoutReleasePolicy, CheckoutReleaseState,
    GetCheckout, GetCheckoutRelease, ReportCheckoutRelease, RequestCheckoutRelease,
    SetCheckoutReleasePolicy, is_sha256,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use super::checkpoint_snapshots::{authorized_checkout, capture_head, ready_snapshot};
use crate::{
    Store,
    checkouts::{checkout_runner, coordinator::require_epoch, load_checkout, lock_policy},
    events::{Idempotency, append_event, append_workspace_event},
    identity::{IdentityError, workspace_context},
};

const MAX_RELEASE_CONTRIBUTIONS: i64 = 4096;

#[derive(sqlx::FromRow)]
struct ReleaseRow {
    id: Uuid,
    workspace_id: Uuid,
    checkout_id: Uuid,
    snapshot_id: Uuid,
    host_runner_id: Uuid,
    checkout_revision: i64,
    source_sha256: String,
    tree_sha256: String,
    expected_commit_sha256: Option<String>,
    policy_revision: i64,
    authorization_sha256: String,
    change_count: i32,
    command_ids: Vec<Uuid>,
    state: String,
    outcome: Option<Json<CheckoutReleaseOutcome>>,
    finalized_at: Option<DateTime<Utc>>,
    expired: bool,
}

impl ReleaseRow {
    fn head(&self) -> CheckoutContentHead {
        CheckoutContentHead {
            checkout_revision: self.checkout_revision,
            source_sha256: self.source_sha256.clone(),
        }
    }

    fn view(self) -> Result<CheckoutRelease, IdentityError> {
        let state = match self.state.as_str() {
            "queued" if self.expired => CheckoutReleaseState::Expired,
            "queued" => CheckoutReleaseState::Queued,
            "applying" => CheckoutReleaseState::Applying,
            "accepted" => CheckoutReleaseState::Accepted,
            "source_changed" => CheckoutReleaseState::SourceChanged,
            "ref_conflict" => CheckoutReleaseState::RefConflict,
            "storage_unavailable" => CheckoutReleaseState::StorageUnavailable,
            _ => return Err(IdentityError::InvalidRole),
        };
        Ok(CheckoutRelease {
            head: self.head(),
            workspace_id: WorkspaceId(self.workspace_id),
            checkout_id: self.checkout_id,
            snapshot_id: self.snapshot_id,
            release_id: self.id,
            host_runner_id: RunnerId(self.host_runner_id),
            policy_revision: self.policy_revision,
            authorization_sha256: self.authorization_sha256,
            approved_changes: self.change_count as u32,
            command_ids: self.command_ids,
            state,
            outcome: self.outcome.map(|value| value.0),
            finalized_at: self.finalized_at,
        })
    }

    fn order(self) -> CheckoutReleaseOrder {
        CheckoutReleaseOrder {
            head: self.head(),
            checkout_id: self.checkout_id,
            release_id: self.id,
            snapshot_id: self.snapshot_id,
            tree_sha256: self.tree_sha256,
            expected_commit_sha256: self.expected_commit_sha256,
            authorization_sha256: self.authorization_sha256,
        }
    }
}

impl Store {
    pub async fn get_checkout_release_policy(
        &self,
        digest: &BearerDigest,
        input: GetCheckout,
    ) -> Result<CheckoutReleasePolicy, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        authorized_checkout(&mut tx, context, input.checkout_id).await?;
        let policy = policy(&mut tx, input.workspace_id, input.checkout_id).await?;
        tx.commit().await?;
        Ok(policy)
    }

    pub async fn set_checkout_release_policy(
        &self,
        digest: &BearerDigest,
        input: SetCheckoutReleasePolicy,
    ) -> Result<CheckoutReleasePolicy, IdentityError> {
        check_ids(&input.required_template_ids)?;
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let checkout = authorized_checkout(&mut tx, context, input.checkout_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "checkout.release_policy",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        lock_policy(&mut tx, checkout.id).await?;
        if release_barrier(&mut tx, input.workspace_id, checkout.id).await? {
            return Err(IdentityError::PublicationPending);
        }
        let previous = policy(&mut tx, input.workspace_id, checkout.id).await?;
        if previous.revision != input.expected_revision {
            return Err(IdentityError::RevisionMismatch {
                current: previous.revision,
            });
        }
        sqlx::query("SELECT set_config('office.releasing_checkout_id',$1,true)")
            .bind(checkout.id.to_string())
            .execute(&mut *tx)
            .await?;
        let suitable: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM command_templates WHERE (workspace_id,checkout_id)=($1,$2)
                AND id=ANY($3) AND spec->'source_inputs'=$4 AND NOT coalesce((spec->>'writable_staging')::boolean,false)",
        ).bind(input.workspace_id.0).bind(checkout.id).bind(&input.required_template_ids)
         .bind(Json(&checkout.manifest.0.include)).fetch_one(&mut *tx).await?;
        if suitable != input.required_template_ids.len() as i64 {
            return Err(IdentityError::InvalidInput);
        }
        sqlx::query(
            "INSERT INTO checkout_release_policies (workspace_id,checkout_id,revision,required_template_ids)
              VALUES ($1,$2,$3,$4) ON CONFLICT (workspace_id,checkout_id)
              DO UPDATE SET revision=excluded.revision,required_template_ids=excluded.required_template_ids",
        ).bind(input.workspace_id.0).bind(checkout.id).bind(previous.revision+1).bind(&input.required_template_ids)
         .execute(&mut *tx).await?;
        let result = policy(&mut tx, input.workspace_id, checkout.id).await?;
        append_event(
            &mut tx,
            context,
            checkout.project_id,
            "checkout.release_policy_updated",
            json!({"checkout_id":checkout.id,"revision":result.revision}),
            None,
        )
        .await?;
        idem.record(&mut tx, checkout.project_id, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn request_checkout_release(
        &self,
        digest: &BearerDigest,
        input: RequestCheckoutRelease,
    ) -> Result<CheckoutRelease, IdentityError> {
        check_ids(&input.command_ids)?;
        if input
            .expected_commit_sha256
            .as_deref()
            .is_some_and(|hash| !is_sha256(hash))
        {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let checkout = authorized_checkout(&mut tx, context, input.checkout_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "checkout.release",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        lock_policy(&mut tx, checkout.id).await?;
        if crate::execution_control::launch_generation(
            &mut tx,
            input.workspace_id,
            context.access.member,
        )
        .await?
        .is_none()
        {
            return Err(IdentityError::ExecutionPaused);
        }
        let (head, snapshot) =
            ready_snapshot(&mut tx, input.workspace_id, checkout.id, input.snapshot_id).await?;
        let (current, applying) = capture_head(&mut tx, input.workspace_id, &checkout).await?;
        if current != head {
            return Err(IdentityError::ContextStale);
        }
        if applying || release_barrier(&mut tx, input.workspace_id, checkout.id).await? {
            return Err(IdentityError::PublicationPending);
        }
        let policy = policy(&mut tx, input.workspace_id, checkout.id).await?;
        if policy.revision != input.expected_policy_revision
            || policy.accepted_commit_sha256 != input.expected_commit_sha256
        {
            return Err(IdentityError::RevisionConflict);
        }
        if policy.required_template_ids.is_empty()
            || (policy.accepted_snapshot_id.is_none() && !input.acknowledge_external_baseline)
        {
            return Err(IdentityError::InvalidInput);
        }
        let previous = latest_acceptance(&mut tx, input.workspace_id, checkout.id).await?;
        if previous
            .as_ref()
            .is_some_and(|value| value.checkout_revision >= head.checkout_revision)
        {
            return Err(IdentityError::InvalidTransition);
        }
        sqlx::query("SELECT set_config('office.releasing_checkout_id',$1,true)")
            .bind(checkout.id.to_string())
            .execute(&mut *tx)
            .await?;
        let checks = release_checks(&mut tx, &input, &head, &policy.required_template_ids).await?;
        let base = previous.map_or(0, |value| value.checkout_revision);
        let approval_ids = approvals(
            &mut tx,
            &input,
            base,
            head.checkout_revision,
            &policy.required_template_ids,
        )
        .await?;
        let authorization = json!({"snapshot_id":input.snapshot_id,"head":head,"tree_sha256":snapshot.git_tree_sha256,
            "expected_commit_sha256":input.expected_commit_sha256,"policy_revision":policy.revision,"checked_templates":checks,
            "contract_templates":policy.required_template_ids,"approved_contributions":approval_ids,
            "authorized_actor":context.recipient(),"baseline_acknowledged":input.acknowledge_external_baseline});
        let approval_hash = hex::encode(Sha256::digest(
            serde_json::to_vec(&authorization).map_err(|_| IdentityError::InvalidRole)?,
        ));
        let row = sqlx::query_as::<_, ReleaseRow>(
            "INSERT INTO checkout_releases (id,workspace_id,checkout_id,snapshot_id,host_runner_id,checkout_revision,source_sha256,
              tree_sha256,expected_commit_sha256,policy_revision,authorization_sha256,approval_binding,change_count,command_ids)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14) RETURNING *,false AS expired",
        ).bind(Uuid::new_v4()).bind(input.workspace_id.0).bind(checkout.id).bind(input.snapshot_id).bind(checkout.host_runner_id)
         .bind(head.checkout_revision).bind(head.source_sha256).bind(snapshot.git_tree_sha256).bind(input.expected_commit_sha256)
         .bind(policy.revision).bind(approval_hash).bind(Json(authorization)).bind(approval_ids.len() as i32).bind(input.command_ids)
         .fetch_one(&mut *tx).await?;
        append_event(
            &mut tx,
            context,
            checkout.project_id,
            "checkout.release_requested",
            json!({
                "checkout_id":checkout.id,"snapshot_id":row.snapshot_id,"release_id":row.id,
                "authorization_sha256":row.authorization_sha256,"change_count":row.change_count,
            }),
            None,
        )
        .await?;
        let result = row.view()?;
        idem.record(&mut tx, checkout.project_id, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn get_checkout_release(
        &self,
        digest: &BearerDigest,
        input: GetCheckoutRelease,
    ) -> Result<CheckoutRelease, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        authorized_checkout(&mut tx, context, input.checkout_id).await?;
        let result = load_release(
            &mut tx,
            input.workspace_id,
            input.checkout_id,
            input.release_id,
        )
        .await?
        .view()?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn next_checkout_release(
        &self,
        digest: &BearerDigest,
        lease: CheckoutCoordinatorRef,
    ) -> Result<Option<CheckoutReleaseOrder>, IdentityError> {
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
        let selected = sqlx::query_as::<_, ReleaseRow>(
            "SELECT r.*,false AS expired FROM checkout_releases r WHERE (workspace_id,checkout_id)=($1,$2)
             AND (state='applying' OR (state='queued' AND expires_at>clock_timestamp()))
             ORDER BY created_at LIMIT 1",
        ).bind(runner.workspace_id.0).bind(lease.checkout_id).fetch_optional(&mut *tx).await?;
        let Some(selected) = selected else {
            return Ok(None);
        };
        if selected.state == "queued" {
            if crate::execution_control::launch_generation(
                &mut tx,
                runner.workspace_id,
                runner.owner_user_id,
            )
            .await?
            .is_none()
            {
                return Ok(None);
            }
            let checkout =
                load_checkout(&mut tx, runner.workspace_id, lease.checkout_id, false).await?;
            let (head, applying) = capture_head(&mut tx, runner.workspace_id, &checkout).await?;
            if head != selected.head() || applying {
                return Err(IdentityError::CheckoutReconciliationRequired);
            }
            sqlx::query("UPDATE checkout_releases SET state='applying' WHERE (workspace_id,id)=($1,$2) AND state='queued'")
                .bind(runner.workspace_id.0).bind(selected.id).execute(&mut *tx).await?;
        }
        let result = selected.order();
        tx.commit().await?;
        Ok(Some(result))
    }

    pub async fn report_checkout_release(
        &self,
        digest: &BearerDigest,
        input: ReportCheckoutRelease,
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
        let row = load_release(
            &mut tx,
            runner.workspace_id,
            lease.checkout_id,
            input.release_id,
        )
        .await?;
        if let Some(result) = row.outcome {
            return if result.0 == input.outcome {
                Ok(())
            } else {
                Err(IdentityError::IdempotencyConflict)
            };
        }
        if row.state != "applying" {
            return Err(IdentityError::CoordinatorStale);
        }
        let state = match &input.outcome {
            CheckoutReleaseOutcome::Accepted {
                commit_sha256,
                tree_sha256,
            } if is_sha256(commit_sha256)
                && tree_sha256 == &row.tree_sha256
                && row.expected_commit_sha256.as_ref() != Some(commit_sha256) =>
            {
                "accepted"
            }
            CheckoutReleaseOutcome::Accepted { .. } => return Err(IdentityError::InvalidInput),
            CheckoutReleaseOutcome::RefConflict => "ref_conflict",
            CheckoutReleaseOutcome::SourceChanged => "source_changed",
            CheckoutReleaseOutcome::StorageUnavailable => "storage_unavailable",
        };
        sqlx::query(
            "UPDATE checkout_releases SET state=$3,outcome=$4,finalized_at=clock_timestamp()
                     WHERE (workspace_id,id)=($1,$2) AND state='applying'",
        )
        .bind(runner.workspace_id.0)
        .bind(row.id)
        .bind(state)
        .bind(Json(input.outcome))
        .execute(&mut *tx)
        .await?;
        let checkout =
            load_checkout(&mut tx, runner.workspace_id, lease.checkout_id, false).await?;
        append_workspace_event(&mut tx, runner.workspace_id, checkout.project_id, "checkout.release_finished",
           json!({"checkout_id":lease.checkout_id,"snapshot_id":row.snapshot_id,"release_id":row.id,"state":state}), None).await?;
        tx.commit().await?;
        Ok(())
    }
}

pub(super) async fn release_barrier(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
) -> Result<bool, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM checkout_releases WHERE (workspace_id,checkout_id)=($1,$2)
          AND (state='applying' OR (state='queued' AND expires_at>clock_timestamp())))",
    )
    .bind(workspace.0)
    .bind(checkout)
    .fetch_one(&mut **tx)
    .await?)
}

async fn latest_acceptance(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
) -> Result<Option<ReleaseRow>, IdentityError> {
    Ok(sqlx::query_as(
        "SELECT r.*,false AS expired FROM checkout_releases r WHERE (workspace_id,checkout_id)=($1,$2) AND state='accepted'
         ORDER BY checkout_revision DESC LIMIT 1",
    ).bind(workspace.0).bind(checkout).fetch_optional(&mut **tx).await?)
}

async fn policy(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
) -> Result<CheckoutReleasePolicy, IdentityError> {
    let (revision, required_template_ids) = sqlx::query_as::<_, (i64, Vec<Uuid>)>(
        "SELECT revision,required_template_ids FROM checkout_release_policies WHERE (workspace_id,checkout_id)=($1,$2)",
    ).bind(workspace.0).bind(checkout).fetch_optional(&mut **tx).await?.unwrap_or_default();
    let accepted = latest_acceptance(tx, workspace, checkout).await?;
    let hash = accepted.as_ref().and_then(|row| match &row.outcome {
        Some(Json(CheckoutReleaseOutcome::Accepted { commit_sha256, .. })) => {
            Some(commit_sha256.clone())
        }
        _ => None,
    });
    Ok(CheckoutReleasePolicy {
        workspace_id: workspace,
        checkout_id: checkout,
        revision,
        required_template_ids,
        accepted_snapshot_id: accepted.map(|row| row.snapshot_id),
        accepted_commit_sha256: hash,
    })
}

async fn load_release(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
    release: Uuid,
) -> Result<ReleaseRow, IdentityError> {
    sqlx::query_as(
        "SELECT r.*,expires_at<=clock_timestamp() AS expired FROM checkout_releases r WHERE (workspace_id,checkout_id,id)=($1,$2,$3)",
    ).bind(workspace.0).bind(checkout).bind(release).fetch_optional(&mut **tx).await?.ok_or(IdentityError::Forbidden)
}

fn check_ids(ids: &[Uuid]) -> Result<(), IdentityError> {
    if ids.is_empty()
        || ids.len() > 8
        || ids
            .iter()
            .enumerate()
            .any(|(index, id)| ids[..index].contains(id))
    {
        return Err(IdentityError::InvalidInput);
    }
    Ok(())
}

async fn release_checks(
    tx: &mut Transaction<'_, Postgres>,
    input: &RequestCheckoutRelease,
    head: &CheckoutContentHead,
    templates: &[Uuid],
) -> Result<Vec<(Uuid, Uuid, String)>, IdentityError> {
    let checked = sqlx::query_as::<_, (Uuid, Uuid, Json<CheckoutCommandOutcome>)>(
        "SELECT id,template_id,outcome FROM checkout_commands WHERE (workspace_id,checkout_id,source_snapshot_id)=($1,$2,$3)
          AND id=ANY($4) AND state='finished' AND NOT coalesce((spec->>'writable_staging')::boolean,false) ORDER BY id",
    ).bind(input.workspace_id.0).bind(input.checkout_id).bind(input.snapshot_id).bind(&input.command_ids)
     .fetch_all(&mut **tx).await?;
    if checked.len() != input.command_ids.len()
        || templates
            .iter()
            .any(|needed| !checked.iter().any(|(_, id, _)| id == needed))
    {
        return Err(IdentityError::InvalidInput);
    }
    let mut verified = Vec::new();
    for (id, template, outcome) in checked {
        if !matches!(&outcome.0, CheckoutCommandOutcome::Exited {snapshot,exit_code:0,..}
            if snapshot.source_snapshot_id==Some(input.snapshot_id) && snapshot.sha256==head.source_sha256
                && snapshot.environment.is_some() && snapshot.checkout_revision==head.checkout_revision)
        {
            return Err(IdentityError::ContextStale);
        }
        let bytes = serde_json::to_vec(&outcome.0).map_err(|_| IdentityError::InvalidRole)?;
        verified.push((id, template, hex::encode(Sha256::digest(bytes))));
    }
    Ok(verified)
}

async fn approvals(
    tx: &mut Transaction<'_, Postgres>,
    input: &RequestCheckoutRelease,
    base: i64,
    end: i64,
    templates: &[Uuid],
) -> Result<Vec<(Uuid, String)>, IdentityError> {
    if end - base > MAX_RELEASE_CONTRIBUTIONS {
        return Err(IdentityError::InvalidInput);
    }
    let rows = sqlx::query_as::<_, (Uuid, Option<String>)>(
        "SELECT p.id,a.approval_sha256 FROM checkout_publications p
         LEFT JOIN LATERAL (
             SELECT r.approval_sha256 FROM checkout_snapshot_reviews r
             WHERE (r.workspace_id,r.checkout_id,r.snapshot_id,r.task_id)=(p.workspace_id,p.checkout_id,$3,p.task_id)
               AND EXISTS (SELECT 1 FROM jsonb_array_elements(r.definition->'contributions') x WHERE x->>'change_id'=p.id::text)
               AND $6::uuid[] <@ ARRAY(SELECT (c->>'template_id')::uuid FROM jsonb_array_elements(r.definition->'checks') c)
             ORDER BY r.approved_at DESC LIMIT 1
         ) a ON true
         WHERE (p.workspace_id,p.checkout_id)=($1,$2) AND p.state='applied'
           AND (p.outcome->'head'->>'checkout_revision')::bigint BETWEEN $4 AND $5
         ORDER BY (p.outcome->'head'->>'checkout_revision')::bigint",
    ).bind(input.workspace_id.0).bind(input.checkout_id).bind(input.snapshot_id).bind(base+1).bind(end)
     .bind(templates).fetch_all(&mut **tx).await?;
    if rows.len() as i64 != end - base {
        return Err(IdentityError::CheckoutReconciliationRequired);
    }
    rows.into_iter()
        .map(|(id, hash)| {
            hash.map(|hash| (id, hash))
                .ok_or(IdentityError::DependencyNotReady)
        })
        .collect()
}
