use chrono::{DateTime, Utc};
use office_domain::{
    Actor, AgentId, BearerDigest, InstanceId, RunId, RunStatus, RunnerId, TaskId, UserId,
    WorkspaceId,
};
use office_protocol::{
    BackendEvidence, ReadRunLogs, Recipient, RenewExecutionLease, Run, RunLease, RunLog,
    RunLogEvent,
};
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use crate::{
    Store, execution_control,
    identity::{IdentityError, WorkspaceContext, workspace_context},
};

impl Store {
    pub async fn read_run_logs(
        &self,
        digest: &BearerDigest,
        input: ReadRunLogs,
    ) -> Result<RunLog, IdentityError> {
        let limit = super::page_limit(input.limit)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        super::load_task(&mut tx, context, input.task_id, false).await?;
        let run = sqlx::query_as::<_, PersistedRun>(
            "SELECT * FROM runs WHERE (workspace_id,task_id,id)=($1,$2,$3)",
        )
        .bind(input.workspace_id.0)
        .bind(input.task_id.0)
        .bind(input.run_id.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(IdentityError::Forbidden)?
        .into_run()?;
        let after = if let Some(id) = input.after {
            sqlx::query_scalar::<_, i64>(
                "SELECT sequence FROM events WHERE (workspace_id,scope_id,id)=($1,$2,$3)
                 AND payload->>'run_id'=$4",
            )
            .bind(input.workspace_id.0)
            .bind(input.task_id.0)
            .bind(id)
            .bind(input.run_id.0.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(IdentityError::CursorExpired)?
        } else {
            0
        };
        let rows = sqlx::query_as::<_, (Uuid, String, DateTime<Utc>, Option<String>)>(
            "SELECT id,event_type,created_at,payload->>'detail' FROM events
             WHERE (workspace_id,scope_id)=($1,$2) AND sequence>$3 AND payload->>'run_id'=$4
             ORDER BY sequence LIMIT $5",
        )
        .bind(input.workspace_id.0)
        .bind(input.task_id.0)
        .bind(after)
        .bind(input.run_id.0.to_string())
        .bind(i64::from(limit) + 1)
        .fetch_all(&mut *tx)
        .await?;
        let more = rows.len() > usize::from(limit);
        let events = rows
            .into_iter()
            .take(usize::from(limit))
            .map(|(event_id, event_type, created_at, detail)| RunLogEvent {
                event_id,
                event_type,
                created_at,
                detail,
            })
            .collect::<Vec<_>>();
        let next_after = events.last().filter(|_| more).map(|entry| entry.event_id);
        tx.commit().await?;
        Ok(RunLog {
            run,
            events,
            next_after,
        })
    }
}

#[derive(sqlx::FromRow)]
struct PersistedRun {
    id: Uuid,
    workspace_id: Uuid,
    task_id: Uuid,
    attempt_number: i64,
    executor_user_id: Uuid,
    executor_agent_id: Option<Uuid>,
    instance_id: Option<Uuid>,
    runtime_template_id: Option<Uuid>,
    managed_job_id: Option<Uuid>,
    managed_runner_id: Option<Uuid>,
    run_fence: Option<i64>,
    lease_until: Option<DateTime<Utc>>,
    backend_evidence: Option<Json<BackendEvidence>>,
    status: String,
    summary: Option<String>,
    waiting_until: Option<DateTime<Utc>>,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
}

impl PersistedRun {
    fn into_run(self) -> Result<Run, IdentityError> {
        let lease = match (
            self.managed_job_id,
            self.managed_runner_id,
            self.run_fence,
            self.lease_until,
        ) {
            (Some(job_id), Some(runner), Some(run_fence), Some(lease_until)) => Some(RunLease {
                job_id,
                runner_id: RunnerId(runner),
                run_fence,
                lease_until,
            }),
            (None, None, None, None) => None,
            _ => return Err(IdentityError::InvalidRole),
        };
        Ok(Run {
            run_id: RunId(self.id),
            task_id: TaskId(self.task_id),
            workspace_id: WorkspaceId(self.workspace_id),
            attempt_number: self.attempt_number,
            executor: Recipient {
                user_id: UserId(self.executor_user_id),
                agent_id: self.executor_agent_id.map(AgentId),
            },
            instance_id: self.instance_id.map(InstanceId),
            runtime_template_id: self.runtime_template_id,
            lease,
            backend: self.backend_evidence.map(|value| value.0),
            status: self
                .status
                .parse()
                .map_err(|_| IdentityError::InvalidRole)?,
            summary: self.summary,
            waiting_until: self.waiting_until,
            started_at: self.started_at,
            ended_at: self.ended_at,
        })
    }
}

pub(super) async fn assigned_by_key(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    runner: RunnerId,
    key: Uuid,
) -> Result<Option<Run>, IdentityError> {
    sqlx::query_as::<_, PersistedRun>(
        "SELECT * FROM runs WHERE workspace_id=$1 AND managed_runner_id=$2 AND runner_claim_key=$3",
    )
    .bind(workspace.0)
    .bind(runner.0)
    .bind(key)
    .fetch_optional(&mut **tx)
    .await?
    .map(PersistedRun::into_run)
    .transpose()
}

pub(super) async fn start_managed(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    job: Uuid,
    runner: RunnerId,
    claim_key: Uuid,
    stop_generation: i64,
) -> Result<Run, IdentityError> {
    sqlx::query_as::<_, PersistedRun>("INSERT INTO runs
        (id,workspace_id,task_id,attempt_number,executor_user_id,executor_agent_id,runtime_template_id,status,
         managed_job_id,managed_runner_id,run_fence,lease_until,runner_claim_key,stop_generation)
        SELECT gen_random_uuid(), j.workspace_id, j.task_id,
               coalesce((SELECT max(r.attempt_number) FROM runs r WHERE r.workspace_id=j.workspace_id AND r.task_id=j.task_id),0)+1,
               j.owner_user_id, j.worker_agent_id, j.runtime_template_id, 'starting',
               j.id, $3, nextval('execution_run_fence'), clock_timestamp()+interval '30 seconds', $4, $5
        FROM execution_jobs j WHERE j.workspace_id=$1 AND j.id=$2 AND j.state='queued' RETURNING *")
        .bind(workspace.0).bind(job).bind(runner.0).bind(claim_key).bind(stop_generation)
        .fetch_one(&mut **tx).await?.into_run()
}

pub(super) async fn renew_managed(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    runner: RunnerId,
    lease: RenewExecutionLease,
) -> Result<Run, IdentityError> {
    sqlx::query_as::<_, PersistedRun>("UPDATE runs SET lease_until=clock_timestamp()+interval '30 seconds'
        WHERE workspace_id=$1 AND managed_runner_id=$2 AND id=$3 AND managed_job_id=$4 AND run_fence=$5
          AND status IN ('starting','running') AND lease_until>clock_timestamp() RETURNING *")
        .bind(workspace.0).bind(runner.0).bind(lease.run_id.0).bind(lease.job_id).bind(lease.run_fence)
        .fetch_optional(&mut **tx).await?.ok_or(IdentityError::RunLeaseStale)?.into_run()
}

pub(super) async fn managed_for_runner(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    runner: RunnerId,
    lease: &RenewExecutionLease,
) -> Result<Run, IdentityError> {
    sqlx::query_as::<_, PersistedRun>(
        "SELECT * FROM runs WHERE workspace_id=$1 AND managed_runner_id=$2 AND id=$3
           AND managed_job_id=$4 AND run_fence=$5",
    )
    .bind(workspace.0)
    .bind(runner.0)
    .bind(lease.run_id.0)
    .bind(lease.job_id)
    .bind(lease.run_fence)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(IdentityError::RunLeaseStale)?
    .into_run()
}

pub(super) async fn mark_managed_started(
    tx: &mut Transaction<'_, Postgres>,
    run: &Run,
) -> Result<Run, IdentityError> {
    let lease = run.lease.as_ref().ok_or(IdentityError::RunLeaseStale)?;
    sqlx::query_as::<_, PersistedRun>(
        "UPDATE runs SET status='running' WHERE workspace_id=$1 AND id=$2 AND managed_job_id=$3
            AND managed_runner_id=$4 AND run_fence=$5 AND status='starting'
            AND lease_until>clock_timestamp() RETURNING *",
    )
    .bind(run.workspace_id.0)
    .bind(run.run_id.0)
    .bind(lease.job_id)
    .bind(lease.runner_id.0)
    .bind(lease.run_fence)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(IdentityError::RunLeaseStale)?
    .into_run()
}

pub(super) async fn finish_managed(
    tx: &mut Transaction<'_, Postgres>,
    run: &Run,
    status: RunStatus,
    summary: &str,
    backend: Option<&BackendEvidence>,
) -> Result<Run, IdentityError> {
    let lease = run.lease.as_ref().ok_or(IdentityError::RunLeaseStale)?;
    sqlx::query_as::<_, PersistedRun>(
        "UPDATE runs SET status=$6,ended_at=clock_timestamp(),summary=$7,backend_evidence=$8
         WHERE workspace_id=$1 AND id=$2 AND managed_job_id=$3 AND managed_runner_id=$4
           AND run_fence=$5 AND status='running' AND lease_until>clock_timestamp() RETURNING *",
    )
    .bind(run.workspace_id.0)
    .bind(run.run_id.0)
    .bind(lease.job_id)
    .bind(lease.runner_id.0)
    .bind(lease.run_fence)
    .bind(status.as_str())
    .bind(summary)
    .bind(backend.map(Json))
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(IdentityError::RunLeaseStale)?
    .into_run()
}

pub(super) async fn request_managed_cancel(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    job: Uuid,
    run: RunId,
) -> Result<Run, IdentityError> {
    sqlx::query_as::<_, PersistedRun>(
        "UPDATE runs SET status='cancel_requested',waiting_until=NULL
        WHERE workspace_id=$1 AND managed_job_id=$2 AND id=$3
          AND status IN ('starting','running','waiting_input','start_unknown','unknown','cancel_requested') RETURNING *",
    )
    .bind(workspace.0)
    .bind(job)
    .bind(run.0)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(IdentityError::InvalidTransition)?
    .into_run()
}

pub(super) async fn cancel_unstarted(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    job: Uuid,
    run: RunId,
) -> Result<Run, IdentityError> {
    sqlx::query_as::<_, PersistedRun>(
        "UPDATE runs SET status='cancelled',ended_at=clock_timestamp()
        WHERE workspace_id=$1 AND managed_job_id=$2 AND id=$3 AND status='starting' RETURNING *",
    )
    .bind(workspace.0)
    .bind(job)
    .bind(run.0)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(IdentityError::InvalidTransition)?
    .into_run()
}

pub(super) async fn latest(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    task: TaskId,
) -> Result<Option<Run>, IdentityError> {
    sqlx::query_as::<_, PersistedRun>(
        "SELECT * FROM runs WHERE workspace_id = $1 AND task_id = $2 ORDER BY attempt_number DESC LIMIT 1",
    ).bind(workspace.0).bind(task.0).fetch_optional(&mut **tx).await?
     .map(PersistedRun::into_run).transpose()
}

pub(super) async fn history(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    task: TaskId,
    after: Option<RunId>,
    limit: u16,
) -> Result<(Vec<Run>, Option<RunId>), IdentityError> {
    let after_number = if let Some(run) = after {
        Some(sqlx::query_scalar::<_, i64>(
            "SELECT attempt_number FROM runs WHERE workspace_id = $1 AND task_id = $2 AND id = $3",
        ).bind(workspace.0).bind(task.0).bind(run.0).fetch_optional(&mut **tx).await?
         .ok_or(IdentityError::CursorExpired)?)
    } else {
        None
    };
    let mut rows = sqlx::query_as::<_, PersistedRun>(
        "SELECT * FROM runs WHERE workspace_id = $1 AND task_id = $2
         AND ($3::bigint IS NULL OR attempt_number < $3) ORDER BY attempt_number DESC LIMIT $4",
    )
    .bind(workspace.0)
    .bind(task.0)
    .bind(after_number)
    .bind(i64::from(limit) + 1)
    .fetch_all(&mut **tx)
    .await?;
    let more = rows.len() > usize::from(limit);
    rows.truncate(usize::from(limit));
    let cursor = rows.last().filter(|_| more).map(|row| RunId(row.id));
    let list = rows
        .into_iter()
        .map(PersistedRun::into_run)
        .collect::<Result<Vec<_>, _>>()?;
    Ok((list, cursor))
}

pub(super) async fn start(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: TaskId,
    instance: Option<InstanceId>,
    runtime_template: Option<Uuid>,
    previous_attempt: i64,
) -> Result<Run, IdentityError> {
    verify_instance(tx, context, instance).await?;
    let stop_generation =
        execution_control::launch_generation(tx, context.access.workspace, context.access.member)
            .await?
            .ok_or(IdentityError::ExecutionPaused)?;
    let attempt = previous_attempt
        .checked_add(1)
        .ok_or(IdentityError::InvalidTransition)?;
    sqlx::query_as::<_, PersistedRun>(
        "INSERT INTO runs (id, workspace_id, task_id, attempt_number, executor_user_id,
                           executor_agent_id, instance_id, runtime_template_id, status, stop_generation)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'running', $9) RETURNING *",
    )
    .bind(Uuid::new_v4())
    .bind(context.access.workspace.0)
    .bind(task.0)
    .bind(attempt)
    .bind(context.access.member.0)
    .bind(context.agent_id().map(|id| id.0))
    .bind(instance.map(|id| id.0))
    .bind(runtime_template)
    .bind(stop_generation)
    .fetch_one(&mut **tx)
    .await?
    .into_run()
}

async fn verify_instance(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    instance: Option<InstanceId>,
) -> Result<(), IdentityError> {
    match (context.actor, instance) {
        (Actor::Human(_), None) => Ok(()),
        (
            Actor::Agent {
                agent,
                credential,
                workspace,
                owner,
                ..
            },
            Some(instance),
        ) => {
            sqlx::query("SELECT set_config('office.credential_id', $1, true)")
                .bind(credential.0.to_string())
                .execute(&mut **tx)
                .await?;
            let found = sqlx::query_scalar::<_, bool>(
                "SELECT true FROM agent_instances WHERE id = $1 AND workspace_id = $2
                 AND agent_id = $3 AND owner_user_id = $4 AND credential_id = $5
                 AND expires_at > transaction_timestamp() FOR SHARE",
            )
            .bind(instance.0)
            .bind(workspace.0)
            .bind(agent.0)
            .bind(owner.0)
            .bind(credential.0)
            .fetch_optional(&mut **tx)
            .await?;
            found.map(|_| ()).ok_or(IdentityError::Forbidden)
        }
        _ => Err(IdentityError::InvalidInput),
    }
}

pub(super) async fn set_status(
    tx: &mut Transaction<'_, Postgres>,
    run: &Run,
    status: RunStatus,
    detail: Option<&str>,
    wait_seconds: Option<u32>,
) -> Result<Run, IdentityError> {
    sqlx::query_as::<_, PersistedRun>(
        "UPDATE runs SET status = $4, summary = coalesce($5, summary),
            waiting_until = CASE WHEN $6::integer IS NULL THEN NULL
                                 ELSE clock_timestamp() + make_interval(secs => $6::integer) END,
            ended_at = CASE WHEN $4 IN ('succeeded', 'failed', 'cancelled') THEN clock_timestamp() ELSE NULL END
         WHERE workspace_id = $1 AND task_id = $2 AND id = $3 AND status = $7 RETURNING *",
    ).bind(run.workspace_id.0).bind(run.task_id.0).bind(run.run_id.0).bind(status.as_str())
     .bind(detail).bind(wait_seconds.map(|seconds| seconds as i32)).bind(run.status.as_str())
     .fetch_optional(&mut **tx).await?.ok_or(IdentityError::InvalidTransition)?.into_run()
}
