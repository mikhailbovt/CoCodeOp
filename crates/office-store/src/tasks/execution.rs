use chrono::{DateTime, Utc};
use office_domain::{
    Action, AgentId, AgentRole, BearerDigest, Resource, RunId, TaskAuthority, TaskId, TaskStatus,
    TeamId, TeamRole, UserId, WorkspaceId,
};
use office_protocol::{
    CancelExecution, ExecutionJob, ExecutionReceipt, ExecutionState, ExecutionTerminalReason,
    GetExecution, ManagedRuntime, QueueExecution, Recipient, Run,
};
use serde_json::json;
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use super::{TaskRow, delegation, dependencies, load_task, runs, scopes, task_text};
use crate::{
    Store,
    data_scope::subject,
    events::{Idempotency, append_event},
    execution_control,
    identity::{
        IdentityError, WorkspaceContext, active_principal, bind_subject, require_permission,
        workspace_context,
    },
    inbox::save_cursor,
    scheduler::lock_budget,
    teams::member_role,
};

mod activation;
mod leases;
mod managed_input;
mod recovery;
pub(super) mod start_effects;

#[derive(sqlx::FromRow)]
struct JobRow {
    id: Uuid,
    workspace_id: Uuid,
    task_id: Uuid,
    task_revision: i64,
    owner_user_id: Uuid,
    worker_agent_id: Uuid,
    issuer_agent_id: Option<Uuid>,
    team_id: Option<Uuid>,
    runtime_template_id: Uuid,
    checkout_id: Option<Uuid>,
    scope_id: Option<Uuid>,
    scope_revision: Option<i64>,
    revision: i64,
    state: String,
    current_run_id: Option<Uuid>,
    managed_attempts: i32,
    retry_not_before: Option<DateTime<Utc>>,
    terminal_reason: Option<String>,
    gateway_required: bool,
    reserved_units: i64,
    enqueued_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
}

impl JobRow {
    fn into_job(self) -> Result<ExecutionJob, IdentityError> {
        Ok(ExecutionJob {
            job_id: self.id,
            workspace_id: WorkspaceId(self.workspace_id),
            task_id: TaskId(self.task_id),
            task_revision: self.task_revision,
            owner_user_id: UserId(self.owner_user_id),
            worker_agent_id: AgentId(self.worker_agent_id),
            team_id: self.team_id.map(TeamId),
            runtime_template_id: self.runtime_template_id,
            checkout_id: self.checkout_id,
            scope_id: self.scope_id,
            scope_revision: self.scope_revision,
            revision: self.revision,
            state: match self.state.as_str() {
                "queued" => ExecutionState::Queued,
                "leased" => ExecutionState::Leased,
                "reconcile_required" => ExecutionState::ReconcileRequired,
                "cancelled" => ExecutionState::Cancelled,
                "dead_letter" => ExecutionState::DeadLetter,
                "completed" => ExecutionState::Completed,
                "waiting_input" => ExecutionState::WaitingInput,
                "resumed" => ExecutionState::Resumed,
                _ => return Err(IdentityError::InvalidRole),
            },
            current_run_id: self.current_run_id.map(RunId),
            managed_attempts: self.managed_attempts,
            retry_not_before: self.retry_not_before,
            terminal_reason: match self.terminal_reason.as_deref() {
                None => None,
                Some("attempt_limit") => Some(ExecutionTerminalReason::AttemptLimit),
                Some("eligibility_changed") => Some(ExecutionTerminalReason::EligibilityChanged),
                Some("start_interrupted") => Some(ExecutionTerminalReason::StartInterrupted),
                Some("execution_failed") => Some(ExecutionTerminalReason::ExecutionFailed),
                Some("input_expired") => Some(ExecutionTerminalReason::InputExpired),
                Some("emergency_stopped") => Some(ExecutionTerminalReason::EmergencyStopped),
                Some("restore_review_required") => {
                    Some(ExecutionTerminalReason::RestoreReviewRequired)
                }
                Some(_) => return Err(IdentityError::InvalidRole),
            },
            reserved_units: self.reserved_units,
            enqueued_at: self.enqueued_at,
            ended_at: self.ended_at,
        })
    }
}

impl Store {
    pub async fn queue_execution(
        &self,
        digest: &BearerDigest,
        input: QueueExecution,
    ) -> Result<ExecutionReceipt, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "execution.queue",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let (task, _) = load_task(&mut tx, context, input.task_id, true).await?;
        require_task_control(context, &task)?;
        let row = enqueue_job(&mut tx, context, &task, &input, Uuid::new_v4(), false).await?;
        let result = record(&mut tx, context, &idem, row, "execution.queued", None).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn cancel_execution(
        &self,
        digest: &BearerDigest,
        input: CancelExecution,
    ) -> Result<ExecutionReceipt, IdentityError> {
        let reason = task_text(&input.reason, 2048)?;
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "execution.cancel",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let snapshot = get_job(&mut tx, input.workspace_id, input.job_id).await?;
        let (task, _) = load_task(&mut tx, context, TaskId(snapshot.task_id), true).await?;
        require_task_control(context, &task)?;
        let current = get_job(&mut tx, input.workspace_id, snapshot.id).await?;
        if current.revision != input.expected_revision {
            return Err(IdentityError::RevisionMismatch {
                current: current.revision,
            });
        }
        if current.state != "queued" {
            return Err(IdentityError::InvalidTransition);
        }
        let (row, _) = cancel_before_start(&mut tx, current).await?;
        let receipt = record(
            &mut tx,
            context,
            &idem,
            row,
            "execution.cancelled",
            Some(reason),
        )
        .await?;
        tx.commit().await?;
        Ok(receipt)
    }

    pub async fn get_execution(
        &self,
        digest: &BearerDigest,
        input: GetExecution,
    ) -> Result<ExecutionJob, IdentityError> {
        let mut tx = self.pool.begin().await?;
        workspace_context(&mut tx, digest, input.workspace_id).await?;
        let row = get_job(&mut tx, input.workspace_id, input.job_id).await?;
        tx.commit().await?;
        row.into_job()
    }
}

async fn enqueue_job(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: &TaskRow,
    input: &QueueExecution,
    job_id: Uuid,
    allow_reviewer: bool,
) -> Result<JobRow, IdentityError> {
    if task.revision != input.expected_task_revision {
        return Err(IdentityError::RevisionMismatch {
            current: task.revision,
        });
    }
    if task.awaiting_owner || !matches!(task.status()?, TaskStatus::Ready | TaskStatus::Assigned) {
        return Err(IdentityError::InvalidTransition);
    }
    ensure_idle(tx, task).await?;
    if dependencies(tx, input.workspace_id, input.task_id)
        .await?
        .iter()
        .any(|item| item.status != TaskStatus::Done)
    {
        return Err(IdentityError::DependencyNotReady);
    }
    let worker = Recipient {
        user_id: UserId(task.accountable_user_id),
        agent_id: Some(input.worker_agent_id),
    };
    check_worker(tx, context, task, worker, allow_reviewer).await?;
    let scope = scopes::lock_execution_scope(tx, context, task, worker).await?;
    let template = select_managed_runtime(
        tx,
        input.workspace_id,
        worker.user_id,
        input.runtime_template_id,
    )
    .await?;
    delegation::verify_execution_grant(
        tx,
        context,
        task,
        input.worker_agent_id,
        input.runtime_template_id,
        delegation::ManagedAttempt::Check,
    )
    .await?;
    // Acquire the owner's budget lock before workspace capacity. A continuation
    // already holds it while handing the reservation over to its next job.
    lock_budget(tx, input.workspace_id, worker.user_id).await?;
    execution_control::lock_admission(tx, input.workspace_id).await?;
    if execution_control::launch_generation(tx, input.workspace_id, worker.user_id)
        .await?
        .is_none()
    {
        return Err(IdentityError::ExecutionPaused);
    }
    if !execution_control::capacity_available(
        tx,
        input.workspace_id,
        worker.user_id,
        task.team_id,
        input.worker_agent_id.0,
        false,
    )
    .await?
    {
        return Err(IdentityError::ExecutionCapacityReached);
    }
    let row = sqlx::query_as::<_, JobRow>(
        "INSERT INTO execution_jobs
         (id,workspace_id,task_id,task_revision,owner_user_id,issuer_agent_id,worker_agent_id,
            team_id,runtime_template_id,checkout_id,scope_id,scope_revision,reserved_units)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) RETURNING *",
    )
    .bind(job_id)
    .bind(input.workspace_id.0)
    .bind(task.id)
    .bind(task.revision)
    .bind(worker.user_id.0)
    .bind(context.agent_id().map(|id| id.0))
    .bind(input.worker_agent_id.0)
    .bind(task.team_id)
    .bind(input.runtime_template_id)
    .bind(scope.as_ref().map(|item| item.checkout_id))
    .bind(scope.as_ref().map(|item| item.scope_id))
    .bind(scope.as_ref().map(|item| item.revision))
    .bind(template.reservation_units)
    .fetch_one(&mut **tx)
    .await?;
    reserve_budget(tx, &row).await?;
    Ok(row)
}

pub(super) async fn ensure_idle(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRow,
) -> Result<(), IdentityError> {
    bind_task(tx, Some(TaskId(task.id))).await?;
    let queued = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM execution_jobs
            WHERE workspace_id=$1 AND task_id=$2 AND state IN ('queued','leased','reconcile_required','waiting_input'))",
    )
    .bind(task.workspace_id)
    .bind(task.id)
    .fetch_one(&mut **tx)
    .await?;
    bind_task(tx, None).await?;
    if queued {
        return Err(IdentityError::InvalidTransition);
    }
    Ok(())
}

pub(super) async fn cancel_task_queue(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: &TaskRow,
    reason: &str,
) -> Result<Option<Run>, IdentityError> {
    bind_task(tx, Some(TaskId(task.id))).await?;
    let queued = sqlx::query_as::<_, JobRow>(
        "SELECT * FROM execution_jobs
             WHERE workspace_id=$1 AND task_id=$2 AND state IN ('queued','leased','reconcile_required','waiting_input')",
    )
    .bind(task.workspace_id)
    .bind(task.id)
    .fetch_optional(&mut **tx)
    .await?;
    let mut run = None;
    if let Some(row) = queued {
        bind_job(tx, Some(row.id)).await?;
        if row.state == "waiting_input" {
            managed_input::cancel_wait(tx, context, row, reason).await?;
        } else if let Some(run_id) = row.current_run_id
            && start_effects::find(tx, context.access.workspace, run_id)
                .await?
                .is_some()
        {
            run = Some(
                runs::request_managed_cancel(tx, context.access.workspace, row.id, RunId(run_id))
                    .await?,
            );
            publish(
                tx,
                context,
                &row.into_job()?,
                "execution.cancel_requested",
                Some(reason),
            )
            .await?;
        } else {
            let (cancelled, observed) = cancel_before_start(tx, row).await?;
            run = observed;
            publish(
                tx,
                context,
                &cancelled.into_job()?,
                "execution.cancelled",
                Some(reason),
            )
            .await?;
        }
    }
    bind_job(tx, None).await?;
    bind_task(tx, None).await?;
    Ok(run)
}

fn require_task_control(context: WorkspaceContext, task: &TaskRow) -> Result<(), IdentityError> {
    let owner = UserId(task.accountable_user_id);
    require_permission(
        context.actor,
        Action::ManageTask,
        Resource::Task(TaskAuthority {
            workspace: context.access,
            accountable_owner: owner,
            accountable: subject(owner, task.accountable_agent_id),
            assignee: None,
            executor: None,
            explicit_reviewer: None,
        }),
    )
}

async fn check_worker(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: &TaskRow,
    worker: Recipient,
    allow_reviewer: bool,
) -> Result<(), IdentityError> {
    if task.assignee_user_id.is_some()
        && (task.assignee_user_id != Some(worker.user_id.0)
            || task.assignee_agent_id != worker.agent_id.map(|id| id.0))
    {
        return Err(IdentityError::Forbidden);
    }
    let worker_role = active_principal(tx, context, worker).await?;
    if !(matches!(worker_role, Some(AgentRole::Lead | AgentRole::Worker))
        || allow_reviewer && worker_role == Some(AgentRole::Reviewer))
    {
        return Err(IdentityError::Forbidden);
    }
    bind_subject(tx, worker).await?;
    let visible = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM tasks WHERE workspace_id=$1 AND id=$2)",
    )
    .bind(task.workspace_id)
    .bind(task.id)
    .fetch_one(&mut **tx)
    .await?;
    let role = if let Some(team) = task.team_id.map(TeamId) {
        member_role(tx, context.access.workspace, team, worker).await?
    } else {
        Some(TeamRole::Worker)
    };
    bind_subject(tx, context.recipient()).await?;
    if !visible || !matches!(role, Some(TeamRole::Lead | TeamRole::Worker)) {
        return Err(IdentityError::Forbidden);
    }
    Ok(())
}

async fn select_managed_runtime(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    owner: UserId,
    id: Uuid,
) -> Result<ManagedRuntime, IdentityError> {
    sqlx::query_scalar::<_, Json<ManagedRuntime>>(
        "SELECT managed FROM runtime_templates
            WHERE workspace_id=$1 AND owner_user_id=$2 AND id=$3 AND kind='managed'",
    )
    .bind(workspace.0)
    .bind(owner.0)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .map(|json| json.0)
    .ok_or(IdentityError::Forbidden)
}

async fn reserve_budget(
    tx: &mut Transaction<'_, Postgres>,
    job: &JobRow,
) -> Result<(), IdentityError> {
    let shared = crate::workspace_budget::admission(
        tx,
        WorkspaceId(job.workspace_id),
        UserId(job.owner_user_id),
        job.reserved_units,
    )
    .await?;
    lock_budget(tx, WorkspaceId(job.workspace_id), UserId(job.owner_user_id)).await?;
    bind_job(tx, Some(job.id)).await?;
    let budget_id = sqlx::query_scalar::<_, Uuid>("UPDATE execution_budgets SET reserved_units=reserved_units+$3
        WHERE workspace_id=$1 AND owner_user_id=$2 AND reserved_units+charged_units+$3<=limit_units RETURNING id")
        .bind(job.workspace_id).bind(job.owner_user_id).bind(job.reserved_units).fetch_optional(&mut **tx).await?
        .ok_or(IdentityError::BudgetExhausted)?;
    sqlx::query("INSERT INTO execution_budget_reservations (job_id,workspace_id,owner_user_id,budget_id,units,uses_workspace_budget)
            VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(job.id).bind(job.workspace_id).bind(job.owner_user_id).bind(budget_id).bind(job.reserved_units).bind(shared)
        .execute(&mut **tx).await?;
    bind_job(tx, None).await
}

async fn release_budget(
    tx: &mut Transaction<'_, Postgres>,
    job: &JobRow,
) -> Result<(), IdentityError> {
    settle_budget(tx, job, 0).await
}

async fn settle_budget(
    tx: &mut Transaction<'_, Postgres>,
    job: &JobRow,
    charge: i64,
) -> Result<(), IdentityError> {
    lock_budget(tx, WorkspaceId(job.workspace_id), UserId(job.owner_user_id)).await?;
    let (budget, units) = sqlx::query_as::<_, (Uuid, i64)>(
        "UPDATE execution_budget_reservations SET released_at=clock_timestamp(),charged_units=$3
             WHERE workspace_id=$1 AND job_id=$2 AND released_at IS NULL AND $3 BETWEEN 0 AND units
             RETURNING budget_id,units",
    )
    .bind(job.workspace_id)
    .bind(job.id)
    .bind(charge)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(IdentityError::InvalidTransition)?;
    let changed = sqlx::query(
        "UPDATE execution_budgets SET reserved_units=reserved_units-$3,charged_units=charged_units+$4
            WHERE workspace_id=$1 AND id=$2 AND reserved_units >= $3",
    )
    .bind(job.workspace_id)
    .bind(budget)
    .bind(units)
    .bind(charge)
    .execute(&mut **tx)
    .await?;
    if changed.rows_affected() != 1 {
        return Err(IdentityError::InvalidTransition);
    }
    Ok(())
}

async fn cancel_before_start(
    tx: &mut Transaction<'_, Postgres>,
    job: JobRow,
) -> Result<(JobRow, Option<Run>), IdentityError> {
    if !matches!(job.state.as_str(), "queued" | "leased") {
        return Err(IdentityError::InvalidTransition);
    }
    bind_job(tx, Some(job.id)).await?;
    let run = if let Some(run) = job.current_run_id {
        Some(runs::cancel_unstarted(tx, WorkspaceId(job.workspace_id), job.id, RunId(run)).await?)
    } else {
        None
    };
    let row = cancel_job(tx, &job).await?;
    release_budget(tx, &row).await?;
    bind_job(tx, None).await?;
    Ok((row, run))
}

async fn cancel_job(
    tx: &mut Transaction<'_, Postgres>,
    job: &JobRow,
) -> Result<JobRow, IdentityError> {
    sqlx::query_as::<_, JobRow>(
        "UPDATE execution_jobs SET state='cancelled',ended_at=clock_timestamp(),revision=revision+1,retry_not_before=NULL
            WHERE workspace_id=$1 AND id=$2 AND state IN ('queued','leased','reconcile_required') RETURNING *",
    )
    .bind(job.workspace_id)
    .bind(job.id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(IdentityError::InvalidTransition)
}

async fn get_job(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<JobRow, IdentityError> {
    sqlx::query_as("SELECT * FROM execution_jobs WHERE workspace_id=$1 AND id=$2")
        .bind(workspace.0)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(IdentityError::Forbidden)
}

pub(super) async fn latest_worker_job(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    task: TaskId,
    worker: AgentId,
) -> Result<Option<ExecutionJob>, IdentityError> {
    sqlx::query_as::<_, JobRow>(
        "SELECT * FROM execution_jobs WHERE (workspace_id,task_id,worker_agent_id)=($1,$2,$3)
         ORDER BY enqueued_at DESC,id DESC LIMIT 1",
    )
    .bind(workspace.0)
    .bind(task.0)
    .bind(worker.0)
    .fetch_optional(&mut **tx)
    .await?
    .map(JobRow::into_job)
    .transpose()
}

pub(super) async fn latest_task_job(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    task: TaskId,
) -> Result<Option<ExecutionJob>, IdentityError> {
    bind_task(tx, Some(task)).await?;
    sqlx::query_as::<_, JobRow>(
        "SELECT * FROM execution_jobs WHERE (workspace_id,task_id)=($1,$2)
         ORDER BY enqueued_at DESC,id DESC LIMIT 1",
    )
    .bind(workspace.0)
    .bind(task.0)
    .fetch_optional(&mut **tx)
    .await?
    .map(JobRow::into_job)
    .transpose()
}

async fn bind_job(
    tx: &mut Transaction<'_, Postgres>,
    job: Option<Uuid>,
) -> Result<(), IdentityError> {
    sqlx::query("SELECT set_config('office.execution_job_context',$1,true)")
        .bind(job.map_or_else(String::new, |id| id.to_string()))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn bind_task(
    tx: &mut Transaction<'_, Postgres>,
    task: Option<TaskId>,
) -> Result<(), IdentityError> {
    sqlx::query("SELECT set_config('office.execution_task_context',$1,true)")
        .bind(task.map_or_else(String::new, |id| id.0.to_string()))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn record(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    idem: &Idempotency,
    row: JobRow,
    event: &str,
    reason: Option<&str>,
) -> Result<ExecutionReceipt, IdentityError> {
    let job = row.into_job()?;
    let sequence = publish(tx, context, &job, event, reason).await?;
    let result = ExecutionReceipt {
        job,
        event_cursor: save_cursor(tx, context, sequence).await?,
    };
    idem.record(tx, result.job.task_id.0, &result).await?;
    Ok(result)
}

async fn publish(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    job: &ExecutionJob,
    event: &str,
    reason: Option<&str>,
) -> Result<i64, IdentityError> {
    append_event(tx, context, job.task_id.0, event, json!({
        "task_id": job.task_id, "job_id": job.job_id, "revision": job.revision, "state": job.state, "reason": reason,
    }), None).await
}
