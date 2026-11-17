use chrono::{DateTime, Utc};
use office_domain::{BearerDigest, RunStatus, Subject, TaskStatus};
use office_protocol::{
    ExecutionJob, ExecutionState, ExplainTask, ReviewPolicy, Run, Task, TaskBlockReason,
    TaskCondition, TaskExecution, TaskExplanation,
};
use sqlx::{Postgres, Transaction};

use crate::{
    Store,
    data_scope::subject,
    execution_control,
    identity::{IdentityError, workspace_context},
};

use super::{execution, load_task, runs};

impl Store {
    pub async fn explain_task(
        &self,
        digest: &BearerDigest,
        input: ExplainTask,
    ) -> Result<TaskExplanation, IdentityError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let (row, scope) = load_task(&mut tx, context, input.task_id, false).await?;
        let task = row.into_task(&mut tx, scope).await?;
        let run = runs::latest(&mut tx, input.workspace_id, input.task_id).await?;
        let job = execution::latest_task_job(&mut tx, input.workspace_id, input.task_id).await?;
        let observed_at = sqlx::query_scalar::<_, DateTime<Utc>>("SELECT transaction_timestamp()")
            .fetch_one(&mut *tx)
            .await?;
        let own = context.access.member == task.owner_user_id;
        let (condition, next_actor) =
            condition(&mut tx, &task, job.as_ref(), run.as_ref(), observed_at, own).await?;
        tx.commit().await?;
        Ok(TaskExplanation {
            task_id: task.task_id,
            task_revision: task.revision,
            status: task.status,
            job_id: job.map(|job| job.job_id),
            run_id: run.map(|run| run.run_id),
            next_actor,
            observed_at,
            condition,
        })
    }
}

async fn condition(
    tx: &mut Transaction<'_, Postgres>,
    task: &Task,
    job: Option<&ExecutionJob>,
    run: Option<&Run>,
    now: DateTime<Utc>,
    own: bool,
) -> Result<(TaskCondition, Option<Subject>), IdentityError> {
    use TaskCondition as Wait;
    let owner = Some(Subject::Human {
        user_id: task.owner_user_id,
    });
    let accountable = Some(task.accountable);
    if task.block_reason == Some(TaskBlockReason::AwaitingOwner) {
        return Ok((Wait::AwaitingOwner, owner));
    }
    if task.status == TaskStatus::InReview
        || task.block_reason == Some(TaskBlockReason::ReviewEscalation)
    {
        let reviewer = match task.review_policy {
            ReviewPolicy::Accountable => accountable,
            ReviewPolicy::ExplicitReviewer { reviewer } => {
                Some(subject(reviewer.user_id, reviewer.agent_id.map(|id| id.0)))
            }
        };
        return Ok((Wait::AwaitingReview, reviewer));
    }
    if let Some(job) = job {
        match job.state {
            ExecutionState::ReconcileRequired => {
                return Ok((Wait::AwaitingStopConfirmation, owner));
            }
            ExecutionState::WaitingInput => return Ok((Wait::AwaitingInput, accountable)),
            ExecutionState::Queued => {
                return Ok((queued_condition(tx, job, now, own).await?, None));
            }
            _ => {}
        }
    }
    if matches!(
        task.status,
        TaskStatus::Done | TaskStatus::Cancelled | TaskStatus::Failed
    ) || job.is_some_and(|job| job.terminal_reason.is_some())
    {
        return Ok((
            Wait::Terminal {
                reason: job.and_then(|job| job.terminal_reason),
            },
            None,
        ));
    }
    if run.is_some_and(|run| run.status == RunStatus::WaitingInput) {
        return Ok((Wait::AwaitingInput, accountable));
    }
    if run.is_some_and(|run| run.status == RunStatus::CancelRequested) {
        return Ok((Wait::AwaitingStopConfirmation, owner));
    }
    let tasks = task
        .dependencies
        .iter()
        .filter(|item| item.status != TaskStatus::Done)
        .map(|item| item.task_id)
        .collect::<Vec<_>>();
    if !tasks.is_empty() {
        return Ok((Wait::Dependencies { tasks }, None));
    }
    if let TaskExecution::Coding {
        checkout_id,
        work_scope_revision: None,
        ..
    } = task.execution
    {
        return Ok((Wait::WorkScopeRequired { checkout_id }, accountable));
    }
    let condition = match task.status {
        TaskStatus::Draft => Wait::Prepare,
        TaskStatus::Ready | TaskStatus::Assigned => Wait::ReadyForAssignment,
        TaskStatus::InProgress => {
            let executor =
                run.map(|run| subject(run.executor.user_id, run.executor.agent_id.map(|id| id.0)));
            return Ok((Wait::InProgress, executor));
        }
        _ => Wait::NeedsAttention,
    };
    Ok((condition, accountable))
}

async fn queued_condition(
    tx: &mut Transaction<'_, Postgres>,
    job: &ExecutionJob,
    now: DateTime<Utc>,
    own: bool,
) -> Result<TaskCondition, IdentityError> {
    use TaskCondition as Wait;
    if let Some(earliest) = job.retry_not_before.filter(|time| *time > now) {
        return Ok(Wait::RetryDelay { earliest });
    }
    let template_id = job.runtime_template_id;
    if !own {
        return Ok(Wait::AwaitingDispatch { template_id });
    }
    if execution_control::launch_generation(tx, job.workspace_id, job.owner_user_id)
        .await?
        .is_none()
    {
        return Ok(Wait::ExecutionPaused);
    }
    if let Some(scope) = execution_control::capacity_limit(
        tx,
        job.workspace_id,
        job.owner_user_id,
        job.team_id.map(|id| id.0),
        job.worker_agent_id.0,
        true,
    )
    .await?
    {
        return Ok(Wait::CapacityLimit { scope });
    }
    let ready: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM runner_execution_profiles p
         JOIN runners r ON (r.workspace_id,r.id)=(p.workspace_id,p.runner_id)
         WHERE p.workspace_id=$1 AND p.owner_user_id=$2 AND p.capacity>0
           AND p.expires_at>$3 AND r.disabled_at IS NULL AND $4=ANY(p.runtime_template_ids))",
    )
    .bind(job.workspace_id.0)
    .bind(job.owner_user_id.0)
    .bind(now)
    .bind(template_id)
    .fetch_one(&mut **tx)
    .await?;
    Ok(if ready {
        Wait::AwaitingDispatch { template_id }
    } else {
        Wait::AwaitingCompatibleRunner { template_id }
    })
}
