use chrono::{DateTime, Utc};
use office_domain::{AgentId, BearerDigest, RunId, TaskId, TaskStatus, UserId, WorkspaceId};
use office_protocol::{
    AnswerManagedInput, GetManagedInput, ManagedInput, ManagedInputReceipt, ManagedInputState,
    QueueExecution, Recipient, Run, RunContinuation,
};
use serde_json::json;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::{
    JobRow, TaskRow, bind_job, bind_task, enqueue_job, get_job, leases, publish,
    require_task_control, settle_budget,
};
use crate::{
    Store,
    events::{Idempotency, append_event},
    identity::{IdentityError, WorkspaceContext, workspace_context},
    tasks::{claims, load_task, task_text},
};

mod native;
pub(super) use native::recorded_intent;

const MAX_INPUT_WAITS_PER_TASK: i64 = 4;

#[derive(sqlx::FromRow)]
struct WaitRow {
    task_id: Uuid,
    run_id: Uuid,
    job_id: Uuid,
    activation_job_id: Option<Uuid>,
    requester_user_id: Uuid,
    requester_agent_id: Uuid,
    checkpoint_id: String,
    question: String,
    expires_at: DateTime<Utc>,
    expired: bool,
    job_state: String,
    answer: Option<String>,
    respondent_user_id: Option<Uuid>,
    respondent_agent_id: Option<Uuid>,
    resumed_job_id: Option<Uuid>,
}

impl WaitRow {
    fn view(self) -> ManagedInput {
        let state = if self.answer.is_some() {
            ManagedInputState::Answered
        } else if self.job_state == "cancelled" {
            ManagedInputState::Cancelled
        } else if self.expired {
            ManagedInputState::Expired
        } else {
            ManagedInputState::Waiting
        };
        ManagedInput {
            task_id: TaskId(self.task_id),
            run_id: RunId(self.run_id),
            requester: Recipient {
                user_id: UserId(self.requester_user_id),
                agent_id: Some(AgentId(self.requester_agent_id)),
            },
            checkpoint_id: self.checkpoint_id,
            question: self.question,
            state,
            expires_at: self.expires_at,
            answer: self.answer,
            respondent: self.respondent_user_id.map(|id| Recipient {
                user_id: UserId(id),
                agent_id: self.respondent_agent_id.map(AgentId),
            }),
            resumed_job_id: self.resumed_job_id,
        }
    }
}

impl Store {
    pub async fn get_managed_input(
        &self,
        digest: &BearerDigest,
        input: GetManagedInput,
    ) -> Result<ManagedInput, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let (task, _) = load_task(&mut tx, context, input.task_id, false).await?;
        bind_task(&mut tx, Some(input.task_id)).await?;
        let wait = load_wait(&mut tx, task.workspace_id, task.id, input.run_id).await?;
        tx.commit().await?;
        Ok(wait.view())
    }

    pub async fn answer_managed_input(
        &self,
        digest: &BearerDigest,
        input: AnswerManagedInput,
    ) -> Result<ManagedInputReceipt, IdentityError> {
        let answer = task_text(&input.answer, 4096)?;
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "execution.answer_input",
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
        if task.revision != input.expected_task_revision {
            return Err(IdentityError::RevisionMismatch {
                current: task.revision,
            });
        }
        bind_task(&mut tx, Some(input.task_id)).await?;
        let wait = load_wait(&mut tx, task.workspace_id, task.id, input.run_id).await?;
        if task.status()? != TaskStatus::Blocked
            || task.awaiting_owner
            || task.review_escalated
            || wait.job_state != "waiting_input"
            || wait.expired
            || wait.answer.is_some()
        {
            return Err(IdentityError::InvalidTransition);
        }
        let job = get_job(&mut tx, input.workspace_id, wait.job_id).await?;
        crate::execution_control::require_live_run(&mut tx, input.workspace_id, input.run_id)
            .await?;
        let allow_reviewer = if context.agent_id().is_none()
            && let Some(origin) = wait.activation_job_id
        {
            sqlx::query_scalar::<_, bool>(
                "SELECT a.trigger='task.review_requested'
                 FROM activated_executions a JOIN activation_policies p ON (p.workspace_id,p.agent_id)=(a.workspace_id,a.agent_id)
                 WHERE (a.workspace_id,a.job_id, a.policy_revision)=($1,$2,p.revision) AND p.managed IS NOT NULL
                 FOR SHARE OF p",
            ).bind(task.workspace_id).bind(origin).fetch_optional(&mut *tx).await?.ok_or(IdentityError::InvalidTransition)?
        } else {
            // An accountable lead can answer for an assigned worker. Coordination
            // tasks created for automatic reviewers are controlled by the human owner.
            false
        };
        bind_job(&mut tx, Some(job.id)).await?;
        close_job(&mut tx, &job, "resumed", None).await?;
        let updated = sqlx::query_as::<_, TaskRow>(
            "UPDATE tasks SET status='assigned',revision=revision+1,updated_at=clock_timestamp()
             WHERE workspace_id=$1 AND id=$2 AND status='blocked' RETURNING *",
        )
        .bind(task.workspace_id)
        .bind(task.id)
        .fetch_one(&mut *tx)
        .await?;
        let resumed = enqueue_job(
            &mut tx,
            context,
            &updated,
            &QueueExecution {
                workspace_id: input.workspace_id,
                task_id: input.task_id,
                expected_task_revision: updated.revision,
                worker_agent_id: AgentId(job.worker_agent_id),
                runtime_template_id: job.runtime_template_id,
                idempotency_key: input.idempotency_key,
            },
            Uuid::new_v4(),
            allow_reviewer,
        )
        .await?;
        bind_job(&mut tx, Some(job.id)).await?;
        let changed = sqlx::query(
            "UPDATE managed_input_waits SET answer=$4,respondent_user_id=$5,respondent_agent_id=$6,resumed_job_id=$7
             WHERE (workspace_id,task_id,run_id)=($1,$2,$3) AND answer IS NULL AND expires_at>clock_timestamp()",
        ).bind(task.workspace_id).bind(task.id).bind(input.run_id.0).bind(answer)
         .bind(context.access.member.0).bind(context.agent_id().map(|agent| agent.0)).bind(resumed.id)
         .execute(&mut *tx).await?;
        if changed.rows_affected() != 1 {
            return Err(IdentityError::InvalidTransition);
        }
        bind_task(&mut tx, Some(input.task_id)).await?;
        let wait = load_wait(&mut tx, task.workspace_id, task.id, input.run_id)
            .await?
            .view();
        append_event(
            &mut tx,
            context,
            task.id,
            "task.input_received",
            json!({"task_id":task.id,"run_id":input.run_id,
            "job_id":resumed.id,"revision":updated.revision}),
            Some(wait.requester),
        )
        .await?;
        let job = resumed.into_job()?;
        publish(&mut tx, context, &job, "execution.queued", None).await?;
        let result = ManagedInputReceipt {
            input: wait,
            job,
            task_revision: updated.revision,
        };
        idem.record(&mut tx, task.id, &result).await?;
        tx.commit().await?;
        Ok(result)
    }
}

async fn load_wait(
    tx: &mut Transaction<'_, Postgres>,
    workspace: Uuid,
    task: Uuid,
    run: RunId,
) -> Result<WaitRow, IdentityError> {
    sqlx::query_as::<_, WaitRow>(
        "SELECT w.*,w.expires_at<=clock_timestamp() AS expired,j.state AS job_state FROM managed_input_waits w
         JOIN execution_jobs j ON (j.workspace_id,j.id)=(w.workspace_id,w.job_id)
         WHERE (w.workspace_id,w.task_id,w.run_id)=($1,$2,$3)",
    ).bind(workspace).bind(task).bind(run.0).fetch_optional(&mut **tx).await?.ok_or(IdentityError::Forbidden)
}

pub(super) async fn can_pause(
    tx: &mut Transaction<'_, Postgres>,
    workspace: Uuid,
    task: Uuid,
    question: &str,
) -> Result<bool, IdentityError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT count(*)<$3 AND NOT coalesce(bool_or(question=$4 AND answer IS NOT NULL),false)
         FROM managed_input_waits WHERE workspace_id=$1 AND task_id=$2",
    )
    .bind(workspace)
    .bind(task)
    .bind(MAX_INPUT_WAITS_PER_TASK)
    .bind(question)
    .fetch_one(&mut **tx)
    .await?)
}

pub(super) async fn begin_pause(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRow,
    job: &JobRow,
    run: &Run,
    question: &str,
) -> Result<(), IdentityError> {
    let checkpoint = run
        .backend
        .as_ref()
        .and_then(|evidence| evidence.checkpoint_id.as_deref())
        .ok_or(IdentityError::InvalidInput)?;
    let agent = run.executor.agent_id.ok_or(IdentityError::InvalidInput)?;
    let effect = sqlx::query(
        "UPDATE execution_jobs SET state='waiting_input', revision=revision+1
        WHERE (workspace_id,id,current_run_id)=($1,$2,$3) AND state='leased'",
    )
    .bind(job.workspace_id)
    .bind(job.id)
    .bind(run.run_id.0)
    .execute(&mut **tx)
    .await?;
    if effect.rows_affected() != 1 {
        return Err(IdentityError::RunLeaseStale);
    }
    sqlx::query("INSERT INTO managed_input_waits (workspace_id,task_id,run_id,job_id,requester_user_id,requester_agent_id,
                     checkpoint_id,question,activation_job_id)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,(SELECT job_id FROM activated_executions
                      WHERE (workspace_id,job_id)=($1,office_activation_job($1,$4))))")
        .bind(job.workspace_id).bind(job.task_id).bind(run.run_id.0).bind(job.id).bind(run.executor.user_id.0).bind(agent.0)
        .bind(checkpoint).bind(question).execute(&mut **tx).await?;
    claims::fence_run(tx, task, run.run_id).await?;
    settle_budget(tx, job, job.reserved_units).await
}

pub(super) async fn continuation(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    job: Uuid,
) -> Result<Option<RunContinuation>, IdentityError> {
    let row = sqlx::query_as::<_, (Uuid, String, String, String)>(
        "SELECT run_id,checkpoint_id,question,answer FROM managed_input_waits WHERE (workspace_id,resumed_job_id)=($1,$2)",
    ).bind(workspace.0).bind(job).fetch_optional(&mut **tx).await?;
    Ok(
        row.map(|(run, checkpoint_id, question, answer)| RunContinuation {
            run_id: RunId(run),
            checkpoint_id,
            question,
            answer,
        }),
    )
}

pub(super) async fn cancel_wait(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    job: JobRow,
    reason: &str,
) -> Result<(), IdentityError> {
    let closed = close_job(tx, &job, "cancelled", None).await?;
    publish(
        tx,
        context,
        &closed.into_job()?,
        "execution.cancelled",
        Some(reason),
    )
    .await?;
    Ok(())
}

pub(super) async fn expire_wait(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
) -> Result<bool, IdentityError> {
    let workspace = context.access.workspace;
    let claimed = sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT w.job_id,w.task_id FROM managed_input_waits w JOIN execution_jobs j ON (j.workspace_id,j.id)=(w.workspace_id,w.job_id)
         WHERE w.workspace_id=$1 AND w.requester_user_id=$2 AND w.answer IS NULL AND w.expires_at<=clock_timestamp()
           AND j.state='waiting_input' ORDER BY w.expires_at LIMIT 1",
    ).bind(workspace.0).bind(context.access.member.0).fetch_optional(&mut **tx).await?;
    let Some((job, task)) = claimed else {
        return Ok(false);
    };
    bind_task(tx, Some(TaskId(task))).await?;
    let Some(row) = leases::lock_task(tx, workspace, task).await? else {
        return Ok(false);
    };
    let current = get_job(tx, workspace, job).await?;
    if current.state != "waiting_input" || row.status()? != TaskStatus::Blocked {
        return Ok(false);
    }
    bind_job(tx, Some(job)).await?;
    let closed = close_job(tx, &current, "dead_letter", Some("input_expired")).await?;
    let revision: i64 = sqlx::query_scalar(
        "UPDATE tasks SET status='failed',revision=revision+1,updated_at=clock_timestamp()
         WHERE (workspace_id,id)=($1,$2) AND status='blocked' RETURNING revision",
    )
    .bind(workspace.0)
    .bind(task)
    .fetch_one(&mut **tx)
    .await?;
    append_event(
        tx,
        context,
        task,
        "task.input_expired",
        json!({"task_id":task,"revision":revision,"run_id":current.current_run_id}),
        None,
    )
    .await?;
    publish(
        tx,
        context,
        &closed.into_job()?,
        "execution.dead_letter",
        None,
    )
    .await?;
    Ok(true)
}

async fn close_job(
    tx: &mut Transaction<'_, Postgres>,
    job: &JobRow,
    state: &str,
    reason: Option<&str>,
) -> Result<JobRow, IdentityError> {
    sqlx::query_as::<_, JobRow>(
        "UPDATE execution_jobs SET state=$3,terminal_reason=$4,ended_at=clock_timestamp(),revision=revision+1
         WHERE (workspace_id,id)=($1,$2) AND state='waiting_input' RETURNING *",
    ).bind(job.workspace_id).bind(job.id).bind(state).bind(reason).fetch_optional(&mut **tx).await?.ok_or(IdentityError::InvalidTransition)
}
