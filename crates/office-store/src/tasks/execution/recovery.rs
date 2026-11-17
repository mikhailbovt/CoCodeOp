use office_domain::{AgentId, RunId, RunStatus, TaskId, TaskStatus, WorkspaceId};
use office_protocol::ExecutionTerminalReason;
use serde_json::json;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::{
    JobRow, TaskRow, bind_job, bind_task, delegation, get_job, leases, settle_budget, start_effects,
};
use crate::{
    events::append_workspace_event,
    execution_control,
    identity::{IdentityError, WorkspaceContext},
};

const MAX_ATTEMPTS: i32 = 3;

pub(super) async fn recover_expired(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
) -> Result<bool, IdentityError> {
    let workspace = context.access.workspace;
    let expired = sqlx::query_as::<_, (Uuid, Uuid, Uuid)>(
        "SELECT j.id,j.task_id,r.id FROM runs r JOIN execution_jobs j
           ON (r.workspace_id,r.managed_job_id,r.id)=(j.workspace_id,j.id,j.current_run_id)
         WHERE r.workspace_id=$1 AND r.executor_user_id=$2 AND j.state='leased'
           AND r.status IN ('starting','running','cancel_requested')
           AND (r.lease_until<=statement_timestamp()
             OR office_execution_stopped(r.workspace_id,r.executor_user_id,r.stop_generation))
         ORDER BY r.lease_until,r.id LIMIT 16",
    )
    .bind(workspace.0)
    .bind(context.access.member.0)
    .fetch_all(&mut **tx)
    .await?;
    for (job, task, run) in expired {
        bind_task(tx, Some(TaskId(task))).await?;
        let Some(task) = leases::lock_task(tx, workspace, task).await? else {
            continue;
        };
        let job = get_job(tx, workspace, job).await?;
        if job.state != "leased" || job.current_run_id != Some(run) {
            continue;
        }
        bind_job(tx, Some(job.id)).await?;
        sqlx::query("SELECT set_config('office.execution_recovery_run',$1,true)")
            .bind(run.to_string())
            .execute(&mut **tx)
            .await?;
        let effect = start_effects::find(tx, workspace, run).await?;
        let stopped = execution_control::run_was_stopped(tx, workspace, RunId(run)).await?;
        let status = match &effect {
            None if stopped => RunStatus::Cancelled,
            None => RunStatus::Lost,
            Some(effect) if effect.confirmed_at.is_some() => RunStatus::Unknown,
            Some(_) => RunStatus::StartUnknown,
        };
        let changed = sqlx::query(
            "UPDATE runs SET status=$4,
              ended_at=CASE WHEN $4 IN ('lost','cancelled') THEN clock_timestamp() END,
              summary=CASE WHEN $4='cancelled' THEN 'Emergency stop before start authorization'
                 WHEN $4='lost' THEN 'Lease expired before start authorization'
                 ELSE 'Check the runner for a pending start effect' END
             WHERE workspace_id=$1 AND id=$2 AND managed_job_id=$3
               AND status IN ('starting','running','cancel_requested') AND ($5 OR lease_until<=clock_timestamp())",
        )
        .bind(workspace.0)
        .bind(run)
        .bind(job.id)
        .bind(status.as_str())
        .bind(stopped)
        .execute(&mut **tx)
        .await?;
        if changed.rows_affected() != 1 {
            continue;
        }
        if effect.is_some() {
            require_reconciliation(tx, &job, run).await?;
            return Ok(true);
        }
        let reason = if stopped {
            Some(ExecutionTerminalReason::EmergencyStopped)
        } else {
            retry_blocker(tx, context, &task, &job).await?
        };
        if task.status()? == TaskStatus::InProgress {
            let status = match reason {
                Some(_) => TaskStatus::Failed,
                None => TaskStatus::Assigned,
            };
            let task_revision = sqlx::query_scalar::<_, i64>(
                "UPDATE tasks SET status=$3,revision=revision+1,updated_at=clock_timestamp(),
                     awaiting_owner=awaiting_owner AND $3<>'failed'
                 WHERE workspace_id=$1 AND id=$2 RETURNING revision",
            )
            .bind(workspace.0)
            .bind(job.task_id)
            .bind(status.as_str())
            .fetch_one(&mut **tx)
            .await?;
            if reason.is_none() {
                requeue(tx, &job, task_revision, run).await?;
            }
        }
        if let Some(reason) = reason {
            dead_letter(tx, &job, reason).await?;
        }
        return Ok(true);
    }
    Ok(false)
}

async fn retry_blocker(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: &TaskRow,
    job: &JobRow,
) -> Result<Option<ExecutionTerminalReason>, IdentityError> {
    if job.managed_attempts >= MAX_ATTEMPTS {
        return Ok(Some(ExecutionTerminalReason::AttemptLimit));
    }
    if task.status()? != TaskStatus::InProgress
        || task.awaiting_owner
        || task.team_id != job.team_id
        || job
            .issuer_agent_id
            .is_some_and(|agent| task.accountable_agent_id != Some(agent))
    {
        return Ok(Some(ExecutionTerminalReason::EligibilityChanged));
    }
    match delegation::verify_execution_grant(
        tx,
        context,
        task,
        AgentId(job.worker_agent_id),
        job.runtime_template_id,
        delegation::ManagedAttempt::Check,
    )
    .await
    {
        Ok(()) => Ok(None),
        Err(IdentityError::DelegationLimit) => Ok(Some(ExecutionTerminalReason::AttemptLimit)),
        Err(IdentityError::DelegationInactive | IdentityError::Forbidden) => {
            Ok(Some(ExecutionTerminalReason::EligibilityChanged))
        }
        Err(error) => Err(error),
    }
}

async fn require_reconciliation(
    tx: &mut Transaction<'_, Postgres>,
    job: &JobRow,
    run: Uuid,
) -> Result<(), IdentityError> {
    let effect = sqlx::query_scalar::<_, Uuid>(
        "UPDATE execution_start_effects SET state='unknown'
         WHERE workspace_id=$1 AND run_id=$2 AND state IN ('authorized','started') RETURNING id",
    )
    .bind(job.workspace_id)
    .bind(run)
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE execution_jobs SET state='reconcile_required',revision=revision+1 WHERE workspace_id=$1 AND id=$2",
    )
    .bind(job.workspace_id)
    .bind(job.id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE tasks SET status='blocked',revision=revision+1,updated_at=clock_timestamp()
         WHERE workspace_id=$1 AND id=$2 AND status='in_progress'",
    )
    .bind(job.workspace_id)
    .bind(job.task_id)
    .execute(&mut **tx)
    .await?;
    append_workspace_event(
        tx,
        WorkspaceId(job.workspace_id),
        job.task_id,
        "execution.reconciliation_required",
        json!({"task_id":job.task_id,"job_id":job.id,"run_id":run,"effect_id":effect}),
        None,
    )
    .await?;
    Ok(())
}

async fn requeue(
    tx: &mut Transaction<'_, Postgres>,
    job: &JobRow,
    task_revision: i64,
    run: Uuid,
) -> Result<(), IdentityError> {
    let updated = sqlx::query_as::<_, JobRow>(
        "UPDATE execution_jobs SET state='queued',current_run_id=NULL,task_revision=$3,
           revision=revision+1,retry_not_before=clock_timestamp()+
                 (1+random()) * CASE managed_attempts WHEN 1 THEN 2 ELSE 4 END * interval '1 second'
         WHERE workspace_id=$1 AND id=$2 AND state='leased' RETURNING *",
    )
    .bind(job.workspace_id)
    .bind(job.id)
    .bind(task_revision)
    .fetch_one(&mut **tx)
    .await?;
    append_workspace_event(
        tx,
        WorkspaceId(job.workspace_id),
        job.task_id,
        "execution.requeued",
        json!({"task_id":job.task_id,"job_id":job.id,"lost_run_id":run,
               "revision":updated.revision,"retry_not_before":updated.retry_not_before}),
        None,
    )
    .await?;
    Ok(())
}

pub(super) async fn dead_letter(
    tx: &mut Transaction<'_, Postgres>,
    job: &JobRow,
    reason: ExecutionTerminalReason,
) -> Result<(), IdentityError> {
    close_failed(tx, job, reason, 0).await
}

pub(super) async fn close_failed(
    tx: &mut Transaction<'_, Postgres>,
    job: &JobRow,
    reason: ExecutionTerminalReason,
    charge: i64,
) -> Result<(), IdentityError> {
    bind_job(tx, Some(job.id)).await?;
    let updated = sqlx::query_as::<_, JobRow>(
        "UPDATE execution_jobs SET state='dead_letter',ended_at=clock_timestamp(),
             revision=revision+1,terminal_reason=$3,retry_not_before=NULL
         WHERE workspace_id=$1 AND id=$2 AND state IN ('queued','leased','reconcile_required') RETURNING *",
    )
    .bind(job.workspace_id)
    .bind(job.id)
    .bind(reason.as_str())
    .fetch_one(&mut **tx)
    .await?;
    settle_budget(tx, &updated, charge).await?;
    append_workspace_event(
        tx,
        WorkspaceId(job.workspace_id),
        job.task_id,
        "execution.dead_lettered",
        json!({"task_id":job.task_id,"job_id":job.id,"revision":updated.revision,
               "current_run_id":updated.current_run_id,"reason":reason}),
        None,
    )
    .await?;
    Ok(())
}
