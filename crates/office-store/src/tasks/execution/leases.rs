use office_domain::{
    Action, Actor, AgentId, BearerDigest, RunnerId, Subject, TaskId, TaskStatus, UserId,
    WorkspaceAccess, WorkspaceId,
};
use office_protocol::{
    ClaimExecution, ExecutionLease, ExecutionTerminalReason, ManagedRuntime, Recipient,
    RenewExecutionLease, Run, RunnerIdentity, RunnerProfile,
};
use serde_json::json;
use sqlx::{Acquire, Postgres, Transaction};
use uuid::Uuid;

use super::{
    JobRow, TaskRow, activation, bind_job, bind_task, check_worker, delegation, dependencies,
    get_job, managed_input, recovery, runs, scopes, select_managed_runtime,
};
use crate::{
    Store,
    checkouts::checkout_runner,
    events::append_workspace_event,
    execution_control,
    identity::{IdentityError, WorkspaceContext, authorize_workspace},
    scheduler::{lock_runner, ready_profile},
    tasks::managed_workers::pin_instructions,
};

const CANDIDATE_LIMIT: i64 = 32;

impl Store {
    pub async fn claim_execution(
        &self,
        credential: &BearerDigest,
        input: ClaimExecution,
    ) -> Result<Option<ExecutionLease>, IdentityError> {
        if input
            .checkout_gateway_templates
            .as_ref()
            .is_some_and(|ids| ids.len() > 32)
        {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let runner = checkout_runner(&mut tx, credential).await?;
        lock_runner(&mut tx, runner.workspace_id, runner.runner_id).await?;
        bind_planner(&mut tx, runner.runner_id).await?;
        if let Some(run) = runs::assigned_by_key(
            &mut tx,
            runner.workspace_id,
            runner.runner_id,
            input.claim_key,
        )
        .await?
        {
            let lease = read_assignment(&mut tx, run).await?;
            tx.commit().await?;
            return Ok(Some(lease));
        }
        let Some(profile) = ready_profile(&mut tx, runner.workspace_id, runner.runner_id).await?
        else {
            tx.commit().await?;
            return Ok(None);
        };
        lock_dispatch(&mut tx, &runner).await?;
        let context = owner_context(&mut tx, &runner).await?;
        if recovery::recover_expired(&mut tx, context).await? {
            tx.commit().await?;
            return Ok(None);
        }
        if managed_input::expire_wait(&mut tx, context).await? {
            tx.commit().await?;
            return Ok(None);
        }
        if execution_control::launch_generation(&mut tx, runner.workspace_id, runner.owner_user_id)
            .await?
            .is_none()
        {
            tx.commit().await?;
            return Ok(None);
        }
        let occupied = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM runs
            WHERE workspace_id=$1 AND managed_runner_id=$2
              AND status IN ('starting','running','waiting_input','cancel_requested','start_unknown','unknown')",
        )
        .bind(runner.workspace_id.0)
        .bind(runner.runner_id.0)
        .fetch_one(&mut *tx)
        .await?;
        if occupied >= i64::from(profile.capacity) {
            tx.commit().await?;
            return Ok(None);
        }

        let mut rows = candidates(&mut tx, &runner, &profile, &input).await?;
        if rows.is_empty() && activation::admit(&mut tx, context, &runner, &profile, &input).await?
        {
            rows = candidates(&mut tx, &runner, &profile, &input).await?;
        }
        for before in rows {
            bind_task(&mut tx, Some(TaskId(before.task_id))).await?;
            let Some(task) = lock_task(&mut tx, runner.workspace_id, before.task_id).await? else {
                continue;
            };
            let job = get_job(&mut tx, runner.workspace_id, before.id).await?;
            if job.state != "queued" {
                continue;
            }
            let plan = match validate_plan(&mut tx, context, &task, &job).await {
                Ok(plan) => plan,
                Err(
                    IdentityError::Forbidden
                    | IdentityError::InvalidTransition
                    | IdentityError::AccessRequestRequired
                    | IdentityError::DelegationInactive
                    | IdentityError::DelegationLimit,
                ) => {
                    recovery::dead_letter(
                        &mut tx,
                        &job,
                        ExecutionTerminalReason::EligibilityChanged,
                    )
                    .await?;
                    tx.commit().await?;
                    return Ok(None);
                }
                Err(error) => return Err(error),
            };
            let gateway_required = job.checkout_id.is_some()
                && (job.gateway_required || input.checkout_gateway_templates.is_some());
            if ready_profile(&mut tx, runner.workspace_id, runner.runner_id)
                .await?
                .is_none()
                || !checkout_live(
                    &mut tx,
                    runner.workspace_id,
                    job.checkout_id,
                    gateway_required,
                )
                .await?
            {
                continue;
            }
            let mut admission = tx.begin().await?;
            execution_control::lock_admission(&mut admission, runner.workspace_id).await?;
            let Some(stop_generation) = execution_control::launch_generation(
                &mut admission,
                runner.workspace_id,
                runner.owner_user_id,
            )
            .await?
            else {
                admission.rollback().await?;
                break;
            };
            if !execution_control::capacity_available(
                &mut admission,
                runner.workspace_id,
                runner.owner_user_id,
                job.team_id,
                job.worker_agent_id,
                true,
            )
            .await?
            {
                admission.rollback().await?;
                continue;
            }
            delegation::verify_execution_grant(
                &mut admission,
                context,
                &task,
                AgentId(job.worker_agent_id),
                job.runtime_template_id,
                delegation::ManagedAttempt::Reserve,
            )
            .await?;
            let lease = assign(
                &mut admission,
                &runner,
                job,
                plan,
                input.claim_key,
                gateway_required,
                stop_generation,
            )
            .await?;
            admission.commit().await?;
            tx.commit().await?;
            return Ok(Some(lease));
        }
        tx.commit().await?;
        Ok(None)
    }

    pub async fn renew_execution_lease(
        &self,
        credential: &BearerDigest,
        input: RenewExecutionLease,
    ) -> Result<Run, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let runner = checkout_runner(&mut tx, credential).await?;
        bind_planner(&mut tx, runner.runner_id).await?;
        let job = get_job(&mut tx, runner.workspace_id, input.job_id).await?;
        if job.state != "leased" || job.current_run_id != Some(input.run_id.0) {
            return Err(IdentityError::RunLeaseStale);
        }
        bind_job(&mut tx, Some(job.id)).await?;
        activation::validate_activation(&mut tx, &job).await?;
        execution_control::require_live_run(&mut tx, runner.workspace_id, input.run_id).await?;
        let run =
            runs::renew_managed(&mut tx, runner.workspace_id, runner.runner_id, input).await?;
        tx.commit().await?;
        Ok(run)
    }
}

pub(super) async fn bind_planner(
    tx: &mut Transaction<'_, Postgres>,
    runner: RunnerId,
) -> Result<(), IdentityError> {
    sqlx::query("SELECT set_config('office.runner_planning_id',$1,true)")
        .bind(runner.0.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn lock_dispatch(
    tx: &mut Transaction<'_, Postgres>,
    runner: &RunnerIdentity,
) -> Result<(), IdentityError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!(
            "execution-dispatch/{}/{}",
            runner.workspace_id.0, runner.owner_user_id.0
        ))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub(super) async fn owner_context(
    tx: &mut Transaction<'_, Postgres>,
    runner: &RunnerIdentity,
) -> Result<WorkspaceContext, IdentityError> {
    let owner = runner.owner_user_id;
    let actor = Actor::Human(owner);
    let member = authorize_workspace(tx, actor, Action::ReadWorkspace, runner.workspace_id).await?;
    Ok(WorkspaceContext {
        actor,
        access: WorkspaceAccess {
            workspace: runner.workspace_id,
            member: owner,
            role: member.role,
        },
        subject: Subject::Human { user_id: owner },
    })
}

async fn candidates(
    tx: &mut Transaction<'_, Postgres>,
    runner: &RunnerIdentity,
    profile: &RunnerProfile,
    input: &ClaimExecution,
) -> Result<Vec<JobRow>, IdentityError> {
    Ok(sqlx::query_as("SELECT j.* FROM execution_jobs j
        LEFT JOIN execution_dispatch_order d ON (d.workspace_id,d.owner_user_id,d.lane_id) =
            (j.workspace_id,j.owner_user_id,coalesce(j.team_id,j.issuer_agent_id,j.owner_user_id))
        WHERE j.workspace_id=$1 AND j.owner_user_id=$2 AND j.state='queued' AND j.runtime_template_id=ANY($3)
        AND NOT EXISTS(SELECT 1 FROM activated_executions a
          WHERE (a.workspace_id,a.job_id)=(j.workspace_id,office_activation_job(j.workspace_id,j.id))
          AND (a.runner_id<>office_checkout_runner_id() OR EXISTS (
            SELECT 1 FROM execution_jobs active
            JOIN activated_executions other
              ON (other.workspace_id,other.job_id)=(active.workspace_id,office_activation_job(active.workspace_id,active.id))
            WHERE (active.workspace_id,active.worker_agent_id)=(j.workspace_id,j.worker_agent_id)
              AND active.id<>j.id AND active.state IN ('leased','reconcile_required')
          )))
        AND (j.retry_not_before IS NULL OR j.retry_not_before<=clock_timestamp())
        AND (j.checkout_id IS NULL OR (NOT $5 AND (
            ($6::uuid[] IS NULL AND NOT j.gateway_required AND EXISTS (SELECT 1 FROM checkout_execution_presence p
              WHERE p.workspace_id=j.workspace_id AND p.checkout_id=j.checkout_id AND p.expires_at>clock_timestamp())) OR
            (j.runtime_template_id=ANY($6) AND EXISTS (SELECT 1 FROM checkout_coordinator_sessions c
              WHERE c.workspace_id=j.workspace_id AND c.checkout_id=j.checkout_id AND c.ready_at IS NOT NULL
                AND c.stopped_at IS NULL AND c.lease_until>clock_timestamp())))))
        ORDER BY coalesce(d.last_fence,0),j.enqueued_at,j.id LIMIT $4")
        .bind(runner.workspace_id.0).bind(runner.owner_user_id.0).bind(&profile.runtime_template_ids)
        .bind(CANDIDATE_LIMIT).bind(input.analysis_only).bind(&input.checkout_gateway_templates).fetch_all(&mut **tx).await?)
}

pub(super) async fn lock_task(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    task: Uuid,
) -> Result<Option<TaskRow>, IdentityError> {
    Ok(sqlx::query_as(
        "SELECT * FROM tasks WHERE workspace_id=$1 AND id=$2 FOR NO KEY UPDATE SKIP LOCKED",
    )
    .bind(workspace.0)
    .bind(task)
    .fetch_optional(&mut **tx)
    .await?)
}

async fn validate_plan(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: &TaskRow,
    job: &JobRow,
) -> Result<ManagedRuntime, IdentityError> {
    if task.revision != job.task_revision
        || !matches!(task.status()?, TaskStatus::Ready | TaskStatus::Assigned)
    {
        return Err(IdentityError::InvalidTransition);
    }
    validate_target(
        tx,
        context,
        task,
        job,
        delegation::ManagedAttempt::Check,
        false,
    )
    .await
}

pub(super) async fn validate_target(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: &TaskRow,
    job: &JobRow,
    attempt: delegation::ManagedAttempt,
    allow_new_scope: bool,
) -> Result<ManagedRuntime, IdentityError> {
    if task.accountable_user_id != job.owner_user_id
        || task.awaiting_owner
        || task.team_id != job.team_id
        || job
            .issuer_agent_id
            .is_some_and(|agent| task.accountable_agent_id != Some(agent))
        || dependencies(tx, context.access.workspace, TaskId(task.id))
            .await?
            .iter()
            .any(|dep| dep.status != TaskStatus::Done)
    {
        return Err(IdentityError::InvalidTransition);
    }
    let worker = Recipient {
        user_id: UserId(job.owner_user_id),
        agent_id: Some(AgentId(job.worker_agent_id)),
    };
    let coordination_activation = activation::validate_activation(tx, job).await?;
    check_worker(tx, context, task, worker, coordination_activation).await?;
    let scope = scopes::lock_execution_scope(tx, context, task, worker).await?;
    let current_scope = scope.map_or((None, None, None), |scope| {
        (
            Some(scope.checkout_id),
            Some(scope.scope_id),
            Some(scope.revision),
        )
    });
    if current_scope.0 != job.checkout_id
        || (!allow_new_scope
            && current_scope != (job.checkout_id, job.scope_id, job.scope_revision))
    {
        return Err(IdentityError::AccessRequestRequired);
    }
    let runtime = select_managed_runtime(
        tx,
        context.access.workspace,
        worker.user_id,
        job.runtime_template_id,
    )
    .await?;
    delegation::verify_execution_grant(
        tx,
        context,
        task,
        AgentId(job.worker_agent_id),
        job.runtime_template_id,
        attempt,
    )
    .await?;
    bind_job(tx, Some(job.id)).await?;
    let funded = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM execution_budget_reservations
          WHERE workspace_id=$1 AND job_id=$2 AND units=$3 AND released_at IS NULL)",
    )
    .bind(job.workspace_id)
    .bind(job.id)
    .bind(runtime.reservation_units)
    .fetch_one(&mut **tx)
    .await?;
    if !funded {
        return Err(IdentityError::BudgetExhausted);
    }
    Ok(runtime)
}

pub(super) async fn checkout_live(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Option<Uuid>,
    gateway_required: bool,
) -> Result<bool, IdentityError> {
    let Some(checkout) = checkout else {
        return Ok(true);
    };
    let query = if gateway_required {
        "SELECT EXISTS (SELECT 1 FROM checkout_coordinator_sessions WHERE workspace_id=$1 AND checkout_id=$2
            AND stopped_at IS NULL AND ready_at IS NOT NULL AND lease_until>clock_timestamp())"
    } else {
        "SELECT EXISTS (SELECT 1 FROM checkout_execution_presence
          WHERE workspace_id=$1 AND checkout_id=$2 AND expires_at>clock_timestamp())"
    };
    Ok(sqlx::query_scalar(query)
        .bind(workspace.0)
        .bind(checkout)
        .fetch_one(&mut **tx)
        .await?)
}

async fn assign(
    tx: &mut Transaction<'_, Postgres>,
    runner: &RunnerIdentity,
    job: JobRow,
    managed_runtime: ManagedRuntime,
    claim_key: Uuid,
    gateway_required: bool,
    stop_generation: i64,
) -> Result<ExecutionLease, IdentityError> {
    let run = runs::start_managed(
        tx,
        runner.workspace_id,
        job.id,
        runner.runner_id,
        claim_key,
        stop_generation,
    )
    .await?;
    bind_job(tx, Some(job.id)).await?;
    pin_instructions(tx, &run).await?;
    let fence = run
        .lease
        .as_ref()
        .ok_or(IdentityError::InvalidRole)?
        .run_fence;
    let updated_job = sqlx::query(
        "UPDATE execution_jobs SET state='leased',current_run_id=$3,revision=revision+1,
             managed_attempts=managed_attempts+1,retry_not_before=NULL,gateway_required=$4
        WHERE workspace_id=$1 AND id=$2 AND state='queued'",
    )
    .bind(job.workspace_id)
    .bind(job.id)
    .bind(run.run_id.0)
    .bind(gateway_required)
    .execute(&mut **tx)
    .await?;
    let updated_task = sqlx::query(
        "UPDATE tasks SET status='in_progress',revision=revision+1,updated_at=clock_timestamp(),
             assignee_user_id=$3,assignee_agent_id=$4 WHERE workspace_id=$1 AND id=$2",
    )
    .bind(job.workspace_id)
    .bind(job.task_id)
    .bind(job.owner_user_id)
    .bind(job.worker_agent_id)
    .execute(&mut **tx)
    .await?;
    if updated_job.rows_affected() != 1 || updated_task.rows_affected() != 1 {
        return Err(IdentityError::InvalidTransition);
    }
    sqlx::query(
        "INSERT INTO execution_dispatch_order (workspace_id,owner_user_id,lane_id,last_fence)
        VALUES ($1,$2,$3,$4) ON CONFLICT (workspace_id,owner_user_id,lane_id)
        DO UPDATE SET last_fence=excluded.last_fence",
    )
    .bind(job.workspace_id)
    .bind(job.owner_user_id)
    .bind(
        job.team_id
            .or(job.issuer_agent_id)
            .unwrap_or(job.owner_user_id),
    )
    .bind(fence)
    .execute(&mut **tx)
    .await?;
    append_workspace_event(
        tx,
        runner.workspace_id,
        job.task_id,
        "execution.leased",
        json!({"job_id": job.id, "task_id": job.task_id, "run_id": run.run_id,
               "runner_id": runner.runner_id, "run_fence": fence}),
        None,
    )
    .await?;
    Ok(ExecutionLease {
        run,
        managed_runtime,
        checkout_id: job.checkout_id,
        scope_id: job.scope_id,
        scope_revision: job.scope_revision,
    })
}

async fn read_assignment(
    tx: &mut Transaction<'_, Postgres>,
    run: Run,
) -> Result<ExecutionLease, IdentityError> {
    let id = run.lease.as_ref().ok_or(IdentityError::InvalidRole)?.job_id;
    let job = get_job(tx, run.workspace_id, id).await?;
    let managed_runtime = select_managed_runtime(
        tx,
        run.workspace_id,
        run.executor.user_id,
        job.runtime_template_id,
    )
    .await?;
    Ok(ExecutionLease {
        run,
        managed_runtime,
        checkout_id: job.checkout_id,
        scope_id: job.scope_id,
        scope_revision: job.scope_revision,
    })
}
