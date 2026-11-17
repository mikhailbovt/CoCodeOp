use chrono::{DateTime, Utc};
use office_domain::{BearerDigest, RunStatus, RunnerId, TaskStatus, WorkspaceId};
use office_protocol::{
    AuthorizeExecutionStart, ExecutionStartEffect, ExecutionStartGrant, ExecutionStartReceipt,
    ExecutionTaskBrief, ExecutionTerminalReason, ObserveExecutionStart, ReconcileExecutionStart,
    RenewExecutionLease, Run, RunnerIdentity, StartEffectState, StopObservation,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::{
    JobRow, TaskRow, bind_job, bind_task, cancel_job, delegation, get_job, leases, recovery, runs,
    settle_budget,
};
use crate::{
    Store,
    checkouts::checkout_runner,
    events::append_workspace_event,
    identity::{IdentityError, WorkspaceContext},
    scheduler::ready_profile,
};

mod completion;
pub(in crate::tasks) mod credentials;

#[derive(sqlx::FromRow)]
pub(super) struct StartEffectRow {
    id: Uuid,
    runner_id: Uuid,
    idempotency_key: Uuid,
    approval_sha256: String,
    runner_credential_id: Option<Uuid>,
    permit_until: Option<DateTime<Utc>>,
    state: String,
    external_id: Option<String>,
    authorized_at: DateTime<Utc>,
    pub(super) confirmed_at: Option<DateTime<Utc>>,
    reconciled_at: Option<DateTime<Utc>>,
    completion_key: Option<Uuid>,
    completion_sha256: Option<String>,
    process_exit_code: Option<i16>,
    result_task_revision: Option<i64>,
}

impl StartEffectRow {
    fn receipt(self, run: Run) -> Result<ExecutionStartReceipt, IdentityError> {
        let state = match self.state.as_str() {
            "authorized" => StartEffectState::Authorized,
            "started" => StartEffectState::Started,
            "unknown" => StartEffectState::Unknown,
            "never_started" => StartEffectState::NeverStarted,
            "stopped" => StartEffectState::Stopped,
            _ => return Err(IdentityError::InvalidRole),
        };
        Ok(ExecutionStartReceipt {
            run,
            effect: ExecutionStartEffect {
                effect_id: self.id,
                runner_id: RunnerId(self.runner_id),
                approval_sha256: self.approval_sha256,
                state,
                external_id: self.external_id,
                authorized_at: self.authorized_at,
                confirmed_at: self.confirmed_at,
                reconciled_at: self.reconciled_at,
            },
        })
    }
}

struct StartTarget {
    context: WorkspaceContext,
    task: TaskRow,
    job: JobRow,
    run: Run,
    runner: RunnerIdentity,
    worker_instructions_revision: Option<i64>,
}

impl StartTarget {
    async fn load(
        tx: &mut Transaction<'_, Postgres>,
        credential: &BearerDigest,
        lease: &RenewExecutionLease,
    ) -> Result<Self, IdentityError> {
        let runner = authenticate(tx, credential).await?;
        let run =
            runs::managed_for_runner(tx, runner.workspace_id, runner.runner_id, lease).await?;
        let context = leases::owner_context(tx, &runner).await?;
        bind_task(tx, Some(run.task_id)).await?;
        let task: TaskRow =
            sqlx::query_as("SELECT * FROM tasks WHERE workspace_id=$1 AND id=$2 FOR NO KEY UPDATE")
                .bind(runner.workspace_id.0)
                .bind(run.task_id.0)
                .fetch_optional(&mut **tx)
                .await?
                .ok_or(IdentityError::Forbidden)?;
        let job = get_job(tx, runner.workspace_id, lease.job_id).await?;
        bind_job(tx, Some(job.id)).await?;
        let run =
            runs::managed_for_runner(tx, runner.workspace_id, runner.runner_id, lease).await?;
        let worker_instructions_revision = sqlx::query_scalar(
            "SELECT profile_revision FROM managed_worker_run_profiles WHERE (workspace_id,run_id)=($1,$2)",
        ).bind(run.workspace_id.0).bind(run.run_id.0).fetch_optional(&mut **tx).await?;
        Ok(Self {
            context,
            task,
            job,
            run,
            runner,
            worker_instructions_revision,
        })
    }

    async fn verify_live(&self, tx: &mut Transaction<'_, Postgres>) -> Result<(), IdentityError> {
        crate::execution_control::require_live_run(tx, self.run.workspace_id, self.run.run_id)
            .await?;
        let live = sqlx::query_scalar::<_, bool>(
            "SELECT lease_until>clock_timestamp() FROM runs WHERE workspace_id=$1 AND id=$2",
        )
        .bind(self.job.workspace_id)
        .bind(self.run.run_id.0)
        .fetch_one(&mut **tx)
        .await?;
        if self.job.state != "leased"
            || self.job.current_run_id != Some(self.run.run_id.0)
            || self.task.status()? != TaskStatus::InProgress
            || !matches!(self.run.status, RunStatus::Starting | RunStatus::Running)
            || !live
        {
            return Err(IdentityError::RunLeaseStale);
        }
        let workspace = self.runner.workspace_id;
        let profile = ready_profile(tx, workspace, self.runner.runner_id).await?;
        if !profile.is_some_and(|p| {
            p.runtime_template_ids
                .contains(&self.job.runtime_template_id)
        }) || !leases::checkout_live(
            tx,
            workspace,
            self.job.checkout_id,
            self.job.gateway_required,
        )
        .await?
        {
            return Err(IdentityError::Forbidden);
        }
        leases::validate_target(
            tx,
            self.context,
            &self.task,
            &self.job,
            delegation::ManagedAttempt::Reserved,
            self.job.gateway_required && self.run.status == RunStatus::Running,
        )
        .await?;
        Ok(())
    }

    fn approval_sha256(&self, fence: i64, permit_until: Option<DateTime<Utc>>) -> String {
        let mut approval = json!({"version":1,"workspace_id":self.job.workspace_id,
            "job_id":self.job.id,"task_id":self.job.task_id,"run_id":self.run.run_id,
            "run_fence":fence,"runner_id":self.runner.runner_id,"task_revision":self.task.revision,
            "owner_user_id":self.job.owner_user_id,"worker_agent_id":self.job.worker_agent_id,
            "runtime_template_id":self.job.runtime_template_id,"checkout_id":self.job.checkout_id,
            "scope_id":self.job.scope_id,"scope_revision":self.job.scope_revision,
            "reserved_units":self.job.reserved_units});
        if let Some(revision) = self.worker_instructions_revision {
            approval["worker_instructions_revision"] = json!(revision);
        }
        if let Some(until) = permit_until {
            approval["version"] = json!(2);
            approval["runner_credential_id"] = json!(self.runner.credential_id);
            approval["permit_until"] = json!(until);
        }
        hex::encode(Sha256::digest(approval.to_string()))
    }

    fn verifies_approval(&self, effect: &StartEffectRow, fence: i64) -> bool {
        effect
            .runner_credential_id
            .is_none_or(|id| id == self.runner.credential_id.0)
            && self.approval_sha256(fence, effect.permit_until) == effect.approval_sha256
    }
}

impl Store {
    pub async fn authorize_execution_start(
        &self,
        credential: &BearerDigest,
        input: AuthorizeExecutionStart,
    ) -> Result<ExecutionStartGrant, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let target = StartTarget::load(&mut tx, credential, &input.lease).await?;
        target.verify_live(&mut tx).await?;
        let workspace = target.run.workspace_id;
        let effect = if let Some(previous) = find(&mut tx, workspace, input.lease.run_id.0).await? {
            if previous.idempotency_key != input.idempotency_key
                || !target.verifies_approval(&previous, input.lease.run_fence)
            {
                return Err(IdentityError::IdempotencyConflict);
            }
            if previous.state == "authorized"
                && !permit_live(&mut tx, previous.permit_until).await?
            {
                return Err(IdentityError::RunLeaseStale);
            }
            previous
        } else {
            let until = target
                .run
                .lease
                .as_ref()
                .ok_or(IdentityError::RunLeaseStale)?
                .lease_until;
            let binding = target.approval_sha256(input.lease.run_fence, Some(until));
            let row = sqlx::query_as::<_, StartEffectRow>(
                "INSERT INTO execution_start_effects (id,workspace_id,owner_user_id,job_id,run_id,runner_id,
                        run_fence,idempotency_key,approval_sha256,state,runner_credential_id,permit_until)
                 SELECT $2,workspace_id,executor_user_id,managed_job_id,id,managed_runner_id,run_fence,$3,$4,'authorized',$6,$7
                   FROM runs WHERE workspace_id=$1 AND id=$5 AND status='starting'
                     AND lease_until>clock_timestamp() AND $7>clock_timestamp()
                 ON CONFLICT DO NOTHING RETURNING *",
            )
            .bind(workspace.0)
            .bind(Uuid::new_v4())
            .bind(input.idempotency_key)
            .bind(binding)
            .bind(target.run.run_id.0)
            .bind(target.runner.credential_id.0)
            .bind(until)
            .fetch_optional(&mut *tx)
            .await?;
            let Some(row) = row else {
                let live: bool = sqlx::query_scalar(
                    "SELECT lease_until>clock_timestamp() FROM runs WHERE id=$1",
                )
                .bind(target.run.run_id.0)
                .fetch_one(&mut *tx)
                .await?;
                return Err(if live {
                    IdentityError::IdempotencyConflict
                } else {
                    IdentityError::RunLeaseStale
                });
            };
            publish(&mut tx, &target.run, &row, "execution.start_authorized").await?;
            row
        };
        let reply = ExecutionStartGrant {
            execution: effect.receipt(target.run)?,
            task: task_brief(target.task),
        };
        tx.commit().await?;
        Ok(reply)
    }

    pub async fn execution_start_effect(
        &self,
        credential: &BearerDigest,
        lease: RenewExecutionLease,
    ) -> Result<Option<ExecutionStartReceipt>, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let runner = authenticate(&mut tx, credential).await?;
        let run = runs::managed_for_runner(&mut tx, runner.workspace_id, runner.runner_id, &lease)
            .await?;
        bind_job(&mut tx, Some(lease.job_id)).await?;
        let reply = find(&mut tx, runner.workspace_id, run.run_id.0)
            .await?
            .map(|effect| effect.receipt(run))
            .transpose()?;
        tx.commit().await?;
        Ok(reply)
    }

    pub async fn observe_execution_start(
        &self,
        credential: &BearerDigest,
        input: ObserveExecutionStart,
    ) -> Result<ExecutionStartReceipt, IdentityError> {
        validate_external_id(&input.external_id)?;
        let mut tx = self.pool.begin().await?;
        let target = StartTarget::load(&mut tx, credential, &input.effect.lease).await?;
        target.verify_live(&mut tx).await?;
        let current = require_effect(&mut tx, &target.run, input.effect.effect_id).await?;
        if !target.verifies_approval(&current, input.effect.lease.run_fence) {
            return Err(IdentityError::RunLeaseStale);
        }
        let reply = if current.state == "started" {
            if current.external_id.as_deref() != Some(&input.external_id) {
                return Err(IdentityError::IdempotencyConflict);
            }
            current.receipt(target.run)?
        } else {
            if !permit_live(&mut tx, current.permit_until).await? {
                return Err(IdentityError::RunLeaseStale);
            }
            let run = runs::mark_managed_started(&mut tx, &target.run).await?;
            let effect = sqlx::query_as::<_, StartEffectRow>(
                "UPDATE execution_start_effects SET state='started',external_id=$2,confirmed_at=clock_timestamp()
                 WHERE id=$1 AND state='authorized' RETURNING *",
            )
            .bind(current.id)
            .bind(input.external_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(IdentityError::InvalidTransition)?;
            publish(&mut tx, &run, &effect, "execution.start_observed").await?;
            effect.receipt(run)?
        };
        tx.commit().await?;
        Ok(reply)
    }

    pub async fn reconcile_execution_start(
        &self,
        credential: &BearerDigest,
        input: ReconcileExecutionStart,
    ) -> Result<ExecutionStartReceipt, IdentityError> {
        let (state, external_id) = match &input.observation {
            StopObservation::NeverStarted => ("never_started", None),
            StopObservation::Stopped { external_id } => {
                validate_external_id(external_id)?;
                ("stopped", Some(external_id.as_str()))
            }
        };
        let mut tx = self.pool.begin().await?;
        let target = StartTarget::load(&mut tx, credential, &input.effect.lease).await?;
        let current = require_effect(&mut tx, &target.run, input.effect.effect_id).await?;
        if current.reconciled_at.is_some() {
            if state != current.state || external_id != current.external_id.as_deref() {
                return Err(IdentityError::IdempotencyConflict);
            }
            let reply = current.receipt(target.run)?;
            tx.commit().await?;
            return Ok(reply);
        }
        if current.external_id.is_some() && current.external_id.as_deref() != external_id {
            return Err(IdentityError::IdempotencyConflict);
        }
        if !matches!(target.job.state.as_str(), "leased" | "reconcile_required")
            || target.job.current_run_id != Some(target.run.run_id.0)
        {
            return Err(IdentityError::RunLeaseStale);
        }
        let task_cancelled = target.task.status()? == TaskStatus::Cancelled;
        let emergency_stopped = crate::execution_control::run_was_stopped(
            &mut tx,
            target.run.workspace_id,
            target.run.run_id,
        )
        .await?;
        let run_status = if task_cancelled || emergency_stopped {
            RunStatus::Cancelled
        } else {
            RunStatus::Failed
        };
        let run = runs::set_status(&mut tx, &target.run, run_status, Some(state), None).await?;
        let charge = if external_id.is_some() {
            target.job.reserved_units
        } else {
            0
        };
        if task_cancelled {
            let job = cancel_job(&mut tx, &target.job).await?;
            settle_budget(&mut tx, &job, charge).await?;
        } else {
            sqlx::query("UPDATE tasks SET status='failed',revision=revision+1,updated_at=clock_timestamp(),awaiting_owner=false
                WHERE workspace_id=$1 AND id=$2 AND status IN ('in_progress','blocked')")
                .bind(target.job.workspace_id).bind(target.job.task_id).execute(&mut *tx).await?;
            recovery::close_failed(
                &mut tx,
                &target.job,
                if emergency_stopped {
                    ExecutionTerminalReason::EmergencyStopped
                } else {
                    ExecutionTerminalReason::StartInterrupted
                },
                charge,
            )
            .await?;
        }
        let effect = sqlx::query_as::<_, StartEffectRow>(
            "UPDATE execution_start_effects SET state=$2,external_id=$3,reconciled_at=clock_timestamp()
             WHERE id=$1 AND reconciled_at IS NULL RETURNING *",
        )
        .bind(current.id)
        .bind(state)
        .bind(external_id)
        .fetch_one(&mut *tx)
        .await?;
        publish(&mut tx, &run, &effect, "execution.start_reconciled").await?;
        let reply = effect.receipt(run)?;
        tx.commit().await?;
        Ok(reply)
    }
}

async fn permit_live(
    tx: &mut Transaction<'_, Postgres>,
    until: Option<DateTime<Utc>>,
) -> Result<bool, IdentityError> {
    match until {
        Some(until) => Ok(sqlx::query_scalar("SELECT clock_timestamp()<$1")
            .bind(until)
            .fetch_one(&mut **tx)
            .await?),
        None => Ok(true),
    }
}

fn task_brief(task: TaskRow) -> ExecutionTaskBrief {
    ExecutionTaskBrief {
        task_id: office_domain::TaskId(task.id),
        task_revision: task.revision,
        objective: task.objective,
        criteria_revision: task.criteria_revision,
        acceptance_criteria: task.criteria.0,
        checkout_id: task.checkout_id,
    }
}

async fn authenticate(
    tx: &mut Transaction<'_, Postgres>,
    credential: &BearerDigest,
) -> Result<RunnerIdentity, IdentityError> {
    let runner = checkout_runner(tx, credential).await?;
    leases::bind_planner(tx, runner.runner_id).await?;
    Ok(runner)
}

pub(super) async fn find(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    run_id: Uuid,
) -> Result<Option<StartEffectRow>, IdentityError> {
    Ok(
        sqlx::query_as("SELECT * FROM execution_start_effects WHERE workspace_id=$1 AND run_id=$2")
            .bind(workspace.0)
            .bind(run_id)
            .fetch_optional(&mut **tx)
            .await?,
    )
}

async fn require_effect(
    tx: &mut Transaction<'_, Postgres>,
    run: &Run,
    effect: Uuid,
) -> Result<StartEffectRow, IdentityError> {
    find(tx, run.workspace_id, run.run_id.0)
        .await?
        .filter(|item| item.id == effect)
        .ok_or(IdentityError::Forbidden)
}

async fn publish(
    tx: &mut Transaction<'_, Postgres>,
    run: &Run,
    effect: &StartEffectRow,
    event: &str,
) -> Result<(), IdentityError> {
    append_workspace_event(tx, run.workspace_id, run.task_id.0, event,
        json!({"task_id":run.task_id,"run_id":run.run_id,"effect_id":effect.id,"state":effect.state}), None).await?;
    Ok(())
}

fn validate_external_id(value: &str) -> Result<(), IdentityError> {
    if !(1..=128).contains(&value.len())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
    {
        return Err(IdentityError::InvalidInput);
    }
    Ok(())
}
