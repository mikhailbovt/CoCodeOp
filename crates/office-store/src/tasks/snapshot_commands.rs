use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use office_domain::{BearerDigest, RunId, RunnerId, SourceManifest, TaskId, WorkspaceId};
use office_protocol::{
    CheckoutChange, CheckoutCommand, CheckoutCommandOrder, CheckoutCommandOutcome,
    CheckoutCommandState, CheckoutContent, CheckoutContentHead, CheckoutCoordinatorRef,
    CommandOutputFile, CommandSnapshot, CommandTemplateSpec, GetCheckoutCommand,
    MAX_COMMAND_LOG_BYTES, MAX_COMMAND_OUTPUT_BYTES, MAX_COMMAND_OUTPUT_FILES, MAX_SOURCE_BYTES,
    MAX_SOURCE_FILES, ReportCheckoutCommand, RunCheckoutCommand, RunnerIdentity,
    SnapshotCheckoutCommand, WorkScopeDefinition, is_sha256, valid_checkout_changes,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use super::{
    checkpoint_snapshots::sealed_head,
    gateway::authorize_checkout_run,
    publications::{load_head, scope_update_pending},
};
use crate::{
    Store,
    checkouts::{
        checkout_runner, coordinator::require_epoch, grants::bind_grant_validation, load_checkout,
        lock_policy,
    },
    events::append_workspace_event,
    identity::IdentityError,
};

#[derive(sqlx::FromRow)]
struct CommandRow {
    id: Uuid,
    checkout_id: Uuid,
    host_runner_id: Uuid,
    epoch: i64,
    task_id: Uuid,
    run_id: Uuid,
    scope_id: Uuid,
    scope_revision: i64,
    template_id: Uuid,
    source_snapshot_id: Option<Uuid>,
    request_sha256: Vec<u8>,
    spec: Json<CommandTemplateSpec>,
    argv: Json<Vec<String>>,
    expires_at: DateTime<Utc>,
    offered_until: Option<DateTime<Utc>>,
    permit_until: Option<DateTime<Utc>>,
    scope_definition: Option<Json<WorkScopeDefinition>>,
    head: Option<Json<CheckoutContentHead>>,
    snapshot: Option<Json<CommandSnapshot>>,
    outcome: Option<Json<CheckoutCommandOutcome>>,
    state: String,
}

impl CommandRow {
    fn view(&self) -> Result<CheckoutCommand, IdentityError> {
        let state = match self.state.as_str() {
            "queued" => CheckoutCommandState::Queued,
            "authorized" => CheckoutCommandState::Authorized,
            "running" => CheckoutCommandState::Running,
            "finished" => CheckoutCommandState::Finished,
            _ => return Err(IdentityError::InvalidRole),
        };
        Ok(CheckoutCommand {
            command_id: self.id,
            checkout_id: self.checkout_id,
            task_id: TaskId(self.task_id),
            run_id: RunId(self.run_id),
            template_id: self.template_id,
            host_runner_id: RunnerId(self.host_runner_id),
            coordinator_epoch: self.epoch,
            scope_revision: self.scope_revision,
            source_snapshot_id: self.source_snapshot_id,
            state,
            outcome: self.outcome.as_ref().map(|outcome| outcome.0.clone()),
        })
    }

    fn order(
        self,
        checked_at: DateTime<Utc>,
        revoked: bool,
    ) -> Result<CheckoutCommandOrder, IdentityError> {
        Ok(CheckoutCommandOrder {
            command_id: self.id,
            epoch: self.epoch,
            template_id: self.template_id,
            spec: self.spec.0,
            argv: self.argv.0,
            scope: self.scope_definition.ok_or(IdentityError::InvalidRole)?.0,
            head: self.head.ok_or(IdentityError::InvalidRole)?.0,
            source_snapshot_id: self.source_snapshot_id,
            permit_until: self.permit_until.ok_or(IdentityError::InvalidRole)?,
            checked_at,
            revoked,
            previous_snapshot: self.snapshot.map(|snapshot| snapshot.0),
        })
    }
}

impl Store {
    pub async fn run_checkout_command(
        &self,
        digest: &BearerDigest,
        input: RunCheckoutCommand,
    ) -> Result<CheckoutCommand, IdentityError> {
        if input.scope_revision < 1 || input.arguments.len() > 32 {
            return Err(IdentityError::InvalidInput);
        }
        let request_hash =
            Sha256::digest(serde_json::to_vec(&input).map_err(|_| IdentityError::InvalidInput)?);
        let mut tx = self.pool.begin().await?;
        let (access, scope, manifest) =
            authorize_checkout_run(&mut tx, digest, input.scope_revision).await?;
        let spec = load_template(&mut tx, &scope, &manifest, input.template_id).await?;
        let argv = input
            .arguments_for(&spec)
            .ok_or(IdentityError::InvalidInput)?;
        let existing: Option<CommandRow> = sqlx::query_as(
            "SELECT * FROM checkout_commands WHERE workspace_id=$1 AND run_id=$2 AND idempotency_key=$3",
        ).bind(scope.workspace_id.0).bind(access.run_id.0).bind(input.idempotency_key).fetch_optional(&mut *tx).await?;
        let row = if let Some(row) = existing {
            if row.request_sha256.as_slice() != request_hash.as_slice()
                || row.scope_revision != scope.revision
            {
                return Err(IdentityError::IdempotencyConflict);
            }
            row
        } else {
            let (epoch, host, _) = live_host(&mut tx, &scope).await?;
            if let Some(snapshot) = input.source_snapshot_id {
                sealed_head(&mut tx, scope.workspace_id, scope.checkout_id, snapshot).await?;
            }
            let pending: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM checkout_commands WHERE workspace_id=$1 AND run_id=$2
                 AND state='queued' AND expires_at>clock_timestamp()",
            )
            .bind(scope.workspace_id.0)
            .bind(access.run_id.0)
            .fetch_one(&mut *tx)
            .await?;
            if pending >= 4 {
                return Err(IdentityError::InvalidInput);
            }
            sqlx::query_as(
                "INSERT INTO checkout_commands (id,workspace_id,checkout_id,host_runner_id,epoch,
                 task_id,run_id,executor_user_id,worker_agent_id,scope_id,scope_revision,template_id,
                 idempotency_key,request_sha256,spec,argv,source_snapshot_id,expires_at)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,clock_timestamp()+interval '25 seconds')
                 RETURNING *",
            ).bind(Uuid::new_v4()).bind(scope.workspace_id.0).bind(scope.checkout_id).bind(host).bind(epoch)
            .bind(scope.task_id.0).bind(access.run_id.0).bind(scope.worker.user_id.0)
            .bind(scope.worker.agent_id.ok_or(IdentityError::InvalidRole)?.0).bind(scope.scope_id).bind(scope.revision)
            .bind(input.template_id).bind(input.idempotency_key).bind(request_hash.as_slice()).bind(Json(spec))
            .bind(Json(argv)).bind(input.source_snapshot_id).fetch_one(&mut *tx).await?
        };
        tx.commit().await?;
        if row.state != "queued" {
            return row.view();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let view = self
                .get_checkout_command(
                    digest,
                    GetCheckoutCommand {
                        command_id: row.id,
                        scope_revision: input.scope_revision,
                    },
                )
                .await?;
            if !matches!(view.state, CheckoutCommandState::Queued) || Instant::now() >= deadline {
                return Ok(view);
            }
            tokio::time::sleep(Duration::from_millis(180)).await;
        }
    }

    pub async fn get_checkout_command(
        &self,
        digest: &BearerDigest,
        input: GetCheckoutCommand,
    ) -> Result<CheckoutCommand, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let (access, scope, manifest) =
            authorize_checkout_run(&mut tx, digest, input.scope_revision).await?;
        let row = command(&mut tx, scope.checkout_id, input.command_id).await?;
        if row.run_id != access.run_id.0 || row.scope_id != scope.scope_id {
            return Err(IdentityError::Forbidden);
        }
        if row.scope_revision != scope.revision {
            return Err(IdentityError::ScopeStale);
        }
        if !scope
            .permissions
            .permits_command(row.template_id, &manifest, &row.spec.0.source_inputs)
        {
            return Err(IdentityError::AccessRequestRequired);
        }
        let mut result = row.view()?;
        if row.state == "queued" {
            let now = clock(&mut tx).await?;
            let (epoch, _, host_until) = live_host(&mut tx, &scope).await?;
            if row.expires_at <= now || row.epoch != epoch {
                return Err(IdentityError::CoordinatorStale);
            }
            if row.offered_until.is_some_and(|time| time > now)
                && !scope_update_pending(&mut tx, scope.workspace_id, scope.checkout_id).await?
                && let Some(head) = command_head(&mut tx, &scope, row.source_snapshot_id).await?
            {
                let permit_until = host_until
                    .min(access.lease_until)
                    .min(scope.expires_at)
                    .min(now + chrono::Duration::seconds(10));
                if permit_until <= now {
                    return Err(IdentityError::RunLeaseStale);
                }
                sqlx::query(
                    "UPDATE checkout_commands SET scope_definition=$2,head=$3,permit_until=$4,state='authorized'
                     WHERE id=$1 AND state='queued'",
                ).bind(row.id).bind(Json(&scope)).bind(Json(head)).bind(permit_until).execute(&mut *tx).await?;
                result.state = CheckoutCommandState::Authorized;
            }
        }
        tx.commit().await?;
        Ok(result)
    }

    pub async fn next_checkout_command(
        &self,
        digest: &BearerDigest,
        proof: CheckoutCoordinatorRef,
    ) -> Result<Option<CheckoutCommandOrder>, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let runner = checkout_runner(&mut tx, digest).await?;
        check_host(&mut tx, &runner, &proof).await?;
        let active = sqlx::query_as::<_, CommandRow>(
            "SELECT * FROM checkout_commands WHERE workspace_id=$1 AND checkout_id=$2
             AND state IN ('authorized','running')",
        )
        .bind(runner.workspace_id.0)
        .bind(proof.checkout_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = active {
            let now = clock(&mut tx).await?;
            let revoked = !live_command_run(&mut tx, runner.workspace_id, &row).await?;
            tx.commit().await?;
            return row.order(now, revoked).map(Some);
        }
        sqlx::query(
            "DELETE FROM checkout_commands WHERE id IN (
                SELECT id FROM checkout_commands WHERE workspace_id=$1 AND checkout_id=$2
                 AND state='queued' AND expires_at<clock_timestamp()-interval '1 minute' LIMIT 32
             )",
        )
        .bind(runner.workspace_id.0)
        .bind(proof.checkout_id)
        .execute(&mut *tx)
        .await?;
        if scope_update_pending(&mut tx, runner.workspace_id, proof.checkout_id).await? {
            return Ok(None);
        }
        sqlx::query(
            "UPDATE checkout_commands SET offered_until=clock_timestamp()+interval '3 seconds'
             WHERE id=(SELECT c.id FROM checkout_commands c JOIN work_scope_revisions s
                 ON (s.workspace_id,s.scope_id,s.revision)=(c.workspace_id,c.scope_id,c.scope_revision)
                 WHERE c.workspace_id=$1 AND c.checkout_id=$2 AND c.epoch=$3 AND c.state='queued'
                   AND c.expires_at>clock_timestamp() AND s.state='active' AND s.operation='apply'
                   AND s.expires_at>clock_timestamp()
                   AND NOT EXISTS (SELECT 1 FROM checkout_commands o WHERE o.workspace_id=c.workspace_id
                     AND o.checkout_id=c.checkout_id AND o.epoch=c.epoch AND o.state='queued'
                     AND o.offered_until>clock_timestamp() AND o.expires_at>clock_timestamp())
                 ORDER BY c.offered_until ASC NULLS FIRST,c.created_at,c.id LIMIT 1)",
        ).bind(runner.workspace_id.0).bind(proof.checkout_id).bind(proof.epoch).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(None)
    }

    pub async fn record_checkout_command_snapshot(
        &self,
        digest: &BearerDigest,
        input: SnapshotCheckoutCommand,
    ) -> Result<(), IdentityError> {
        let mut tx = self.pool.begin().await?;
        let runner = checkout_runner(&mut tx, digest).await?;
        let proof = &input.coordinator;
        check_host(&mut tx, &runner, proof).await?;
        let row = command(&mut tx, proof.checkout_id, input.command_id).await?;
        if row.epoch != proof.epoch || !matches!(row.state.as_str(), "authorized" | "running") {
            return Err(IdentityError::CoordinatorStale);
        }
        if let Some(snapshot) = row.snapshot {
            return if snapshot.0 == input.snapshot {
                Ok(())
            } else {
                Err(IdentityError::IdempotencyConflict)
            };
        }
        let head = &row.head.as_ref().ok_or(IdentityError::InvalidRole)?.0;
        let template_hash = row
            .spec
            .0
            .fingerprint()
            .map_err(|_| IdentityError::InvalidInput)?;
        let valid_environment = input
            .snapshot
            .environment
            .as_ref()
            .is_some_and(|environment| {
                is_sha256(&environment.image_id) && environment.template_sha256 == template_hash
            });
        if input.snapshot.checkout_revision != head.checkout_revision
            || input.snapshot.source_snapshot_id != row.source_snapshot_id
            || (row.source_snapshot_id.is_some() || input.snapshot.environment.is_some())
                && !valid_environment
            || !is_sha256(&input.snapshot.sha256)
            || input.snapshot.file_count > MAX_SOURCE_FILES
            || input.snapshot.total_bytes > MAX_SOURCE_BYTES
        {
            return Err(IdentityError::InvalidInput);
        }
        let now = clock(&mut tx).await?;
        if row.permit_until.is_none_or(|until| until <= now) {
            return Err(IdentityError::CoordinatorStale);
        }
        if !live_command_run(&mut tx, runner.workspace_id, &row).await? {
            return Err(IdentityError::RunLeaseStale);
        }
        sqlx::query("UPDATE checkout_commands SET snapshot=$2,state='running' WHERE id=$1")
            .bind(input.command_id)
            .bind(Json(input.snapshot))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn report_checkout_command(
        &self,
        digest: &BearerDigest,
        input: ReportCheckoutCommand,
    ) -> Result<(), IdentityError> {
        let mut tx = self.pool.begin().await?;
        let runner = checkout_runner(&mut tx, digest).await?;
        let proof = &input.coordinator;
        check_host(&mut tx, &runner, proof).await?;
        let row = command(&mut tx, proof.checkout_id, input.command_id).await?;
        if let Some(previous) = row.outcome {
            return if previous.0 == input.outcome {
                Ok(())
            } else {
                Err(IdentityError::IdempotencyConflict)
            };
        }
        if !matches!(row.state.as_str(), "authorized" | "running") {
            return Err(IdentityError::CoordinatorStale);
        }
        verify_outcome(
            row.snapshot.as_ref().map(|item| &item.0),
            &row.spec.0,
            &input.outcome,
        )?;
        sqlx::query("UPDATE checkout_commands SET outcome=$2,state='finished',scope_definition=NULL WHERE id=$1")
            .bind(input.command_id).bind(Json(&input.outcome)).execute(&mut *tx).await?;
        let checkout =
            load_checkout(&mut tx, runner.workspace_id, proof.checkout_id, false).await?;
        append_workspace_event(&mut tx, runner.workspace_id, checkout.project_id, "checkout.command_finished",
            json!({"checkout_id":proof.checkout_id, "command_id":input.command_id,"template_id":row.template_id}), None).await?;
        tx.commit().await?;
        Ok(())
    }
}

async fn live_command_run(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    command: &CommandRow,
) -> Result<bool, IdentityError> {
    sqlx::query("SELECT set_config('office.active_checkout_command_id',$1,true)")
        .bind(command.id.to_string())
        .execute(&mut **tx)
        .await?;
    let live = sqlx::query_scalar(
        "SELECT status='running' AND lease_until>clock_timestamp()
           AND NOT office_execution_stopped(workspace_id,executor_user_id,stop_generation)
         FROM runs WHERE (workspace_id,id)=($1,$2)",
    )
    .bind(workspace.0)
    .bind(command.run_id)
    .fetch_optional(&mut **tx)
    .await?;
    sqlx::query("SELECT set_config('office.active_checkout_command_id','',true)")
        .execute(&mut **tx)
        .await?;
    Ok(live.unwrap_or(false))
}

pub(super) async fn require_snapshot_barrier(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
) -> Result<(), IdentityError> {
    let writing: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM checkout_commands WHERE workspace_id=$1 AND checkout_id=$2
            AND state='authorized' AND source_snapshot_id IS NULL)",
    )
    .bind(workspace.0)
    .bind(checkout)
    .fetch_one(&mut **tx)
    .await?;
    if writing {
        return Err(IdentityError::PublicationPending);
    }
    Ok(())
}

async fn live_host(
    tx: &mut Transaction<'_, Postgres>,
    scope: &WorkScopeDefinition,
) -> Result<(i64, Uuid, DateTime<Utc>), IdentityError> {
    sqlx::query_as(
        "SELECT epoch,host_runner_id,lease_until FROM checkout_coordinator_sessions WHERE workspace_id=$1
         AND checkout_id=$2 AND ready_at IS NOT NULL AND stopped_at IS NULL AND lease_until>clock_timestamp()",
    ).bind(scope.workspace_id.0).bind(scope.checkout_id).fetch_optional(&mut **tx).await?
     .ok_or(IdentityError::CheckoutOffline)
}

async fn command_head(
    tx: &mut Transaction<'_, Postgres>,
    scope: &WorkScopeDefinition,
    snapshot: Option<Uuid>,
) -> Result<Option<CheckoutContentHead>, IdentityError> {
    if let Some(snapshot) = snapshot {
        return sealed_head(tx, scope.workspace_id, scope.checkout_id, snapshot)
            .await
            .map(Some);
    }
    if let Some(row) = load_head(tx, scope.workspace_id, scope.checkout_id).await? {
        if row.blocked {
            return Err(IdentityError::CheckoutReconciliationRequired);
        }
        return Ok(row.applying_change_id.is_none().then(|| row.head()));
    }
    let checkout = load_checkout(tx, scope.workspace_id, scope.checkout_id, false).await?;
    let source_sha256 = checkout
        .baseline
        .ok_or(IdentityError::AccessRequestRequired)?
        .0
        .manifest_sha256;
    Ok(Some(CheckoutContentHead {
        source_sha256,
        checkout_revision: 0,
    }))
}

async fn load_template(
    tx: &mut Transaction<'_, Postgres>,
    scope: &WorkScopeDefinition,
    manifest: &SourceManifest,
    id: Uuid,
) -> Result<CommandTemplateSpec, IdentityError> {
    if !scope.permissions.command_template_ids.contains(&id) {
        return Err(IdentityError::AccessRequestRequired);
    }
    bind_grant_validation(tx, Some(scope.grant_id)).await?;
    let template: Option<Json<CommandTemplateSpec>> = sqlx::query_scalar(
        "SELECT spec FROM command_templates WHERE workspace_id=$1 AND checkout_id=$2 AND id=$3",
    )
    .bind(scope.workspace_id.0)
    .bind(scope.checkout_id)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    bind_grant_validation(tx, None).await?;
    let spec = template.ok_or(IdentityError::AccessRequestRequired)?.0;
    if !scope
        .permissions
        .permits_command(id, manifest, &spec.source_inputs)
    {
        return Err(IdentityError::AccessRequestRequired);
    }
    Ok(spec)
}

async fn check_host(
    tx: &mut Transaction<'_, Postgres>,
    runner: &RunnerIdentity,
    proof: &CheckoutCoordinatorRef,
) -> Result<(), IdentityError> {
    lock_policy(tx, proof.checkout_id).await?;
    require_epoch(
        tx,
        runner.workspace_id,
        proof.checkout_id,
        Some(proof.epoch),
        true,
    )
    .await
}

async fn command(
    tx: &mut Transaction<'_, Postgres>,
    checkout: Uuid,
    id: Uuid,
) -> Result<CommandRow, IdentityError> {
    sqlx::query_as("SELECT * FROM checkout_commands WHERE checkout_id=$1 AND id=$2")
        .bind(checkout)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(IdentityError::Forbidden)
}

async fn clock(tx: &mut Transaction<'_, Postgres>) -> Result<DateTime<Utc>, IdentityError> {
    Ok(sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?)
}

fn verify_outcome(
    prior: Option<&CommandSnapshot>,
    spec: &CommandTemplateSpec,
    outcome: &CheckoutCommandOutcome,
) -> Result<(), IdentityError> {
    let valid = match outcome {
        CheckoutCommandOutcome::Exited {
            snapshot,
            exit_code,
            stdout,
            stderr,
            outputs,
            staged_changes,
            ..
        } => {
            prior == Some(snapshot)
                && (0..=255).contains(exit_code)
                && stdout.len() + stderr.len() <= MAX_COMMAND_LOG_BYTES
                && valid_command_files(spec, outputs, staged_changes)
        }
        CheckoutCommandOutcome::Timeout { snapshot } => prior == Some(snapshot),
        CheckoutCommandOutcome::NotConfigured | CheckoutCommandOutcome::SnapshotUnavailable => {
            prior.is_none()
        }
        CheckoutCommandOutcome::Interrupted | CheckoutCommandOutcome::Unknown => true,
    };
    if valid {
        Ok(())
    } else {
        Err(IdentityError::InvalidInput)
    }
}

fn valid_command_files(
    spec: &CommandTemplateSpec,
    outputs: &[CommandOutputFile],
    changes: &[CheckoutChange],
) -> bool {
    let selected = SourceManifest {
        include: spec.scratch_outputs.clone(),
        exclude: Vec::new(),
    };
    if spec.writable_staging {
        return outputs.is_empty()
            && changes.len() <= MAX_COMMAND_OUTPUT_FILES
            && changes.iter().all(|change| {
                !matches!(change, CheckoutChange::Rename { .. })
                    && change
                        .paths()
                        .into_iter()
                        .flatten()
                        .all(|path| selected.allows(path))
            })
            && valid_checkout_changes(changes, MAX_COMMAND_OUTPUT_BYTES as usize);
    }
    if !changes.is_empty()
        || outputs.len() > MAX_COMMAND_OUTPUT_FILES
        || outputs.windows(2).any(|pair| pair[0].path >= pair[1].path)
    {
        return false;
    }
    let mut total = 0;
    outputs.iter().all(|entry| {
        let file = &entry.file;
        let Some(bytes) = (CheckoutContent {
            encoding: file.encoding,
            content: file.content.clone(),
        })
        .bytes() else {
            return false;
        };
        total += bytes.len() as u64;
        total <= MAX_COMMAND_OUTPUT_BYTES
            && selected.allows(&entry.path)
            && bytes.len() as u64 == file.size_bytes
            && hex::encode(Sha256::digest(bytes)) == file.sha256
    })
}
