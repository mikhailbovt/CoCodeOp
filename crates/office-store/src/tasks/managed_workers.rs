use chrono::{DateTime, Utc};
use office_domain::{
    Actor, AgentId, AgentRole, BearerDigest, RunId, TaskId, UserId, Visibility, WorkspaceId,
};
use office_protocol::{
    CreateManagedWorker, GetManagedWorkerStatus, ManagedWorker, ManagedWorkerSelector,
    ManagedWorkerStatus, ManagedWorkerTask, Recipient, Run, TaskView, UpdateManagedWorker,
    WorkerInstructions,
};
use serde_json::json;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::{execution::latest_worker_job, load_task, runs};
use crate::{
    Store,
    data_scope::create_scope,
    events::{Idempotency, append_event},
    identity::{IdentityError, WorkspaceContext, valid_name, workspace_context},
};

const WORKERS_PER_LEAD: i64 = 32;

#[derive(sqlx::FromRow)]
struct Profile {
    workspace_id: Uuid,
    agent_id: Uuid,
    owner_user_id: Uuid,
    lead_agent_id: Uuid,
    display_name: String,
    revision: i64,
    instructions: String,
    updated_at: DateTime<Utc>,
}

impl From<Profile> for ManagedWorker {
    fn from(value: Profile) -> Self {
        Self {
            workspace_id: WorkspaceId(value.workspace_id),
            worker: Recipient {
                user_id: UserId(value.owner_user_id),
                agent_id: Some(AgentId(value.agent_id)),
            },
            lead: Recipient {
                user_id: UserId(value.owner_user_id),
                agent_id: Some(AgentId(value.lead_agent_id)),
            },
            profile: WorkerInstructions {
                revision: value.revision,
                display_name: value.display_name,
                instructions: value.instructions,
            },
            updated_at: value.updated_at,
        }
    }
}

impl Store {
    pub async fn create_managed_worker(
        &self,
        digest: &BearerDigest,
        input: CreateManagedWorker,
    ) -> Result<ManagedWorker, IdentityError> {
        let name = valid_name(&input.display_name)?;
        let instructions = validated_instructions(&input.instructions)?;
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let Actor::Agent {
            agent: lead,
            role: AgentRole::Lead,
            ..
        } = context.actor
        else {
            return Err(IdentityError::Forbidden);
        };
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "worker.create",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let workspace = input.workspace_id.0;
        let owner = context.access.member.0;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(format!("managed-workers/{workspace}"))
            .execute(&mut *tx)
            .await?;
        sqlx::query("SELECT set_config('office.managed_worker_quota_workspace',$1,true)")
            .bind(workspace.to_string())
            .execute(&mut *tx)
            .await?;
        let (workspace_count, owner_count, count, duplicate) = sqlx::query_as::<_, (i64, i64, i64, bool)>(
            "SELECT count(*) FILTER (WHERE a.disabled_at IS NULL),
                 count(*) FILTER (WHERE a.disabled_at IS NULL AND w.owner_user_id=$2),
                 count(*) FILTER (WHERE a.disabled_at IS NULL AND w.owner_user_id=$2 AND w.lead_agent_id=$3),
                 coalesce(bool_or(w.name_key=lower($4)) FILTER (WHERE w.owner_user_id=$2 AND w.lead_agent_id=$3),false)
             FROM managed_workers w JOIN agents a ON (a.workspace_id,a.id,a.owner_user_id)=(w.workspace_id,w.agent_id,w.owner_user_id)
             WHERE w.workspace_id=$1",
        ).bind(workspace).bind(owner).bind(lead.0).bind(name).fetch_one(&mut *tx).await?;
        sqlx::query("SELECT set_config('office.managed_worker_quota_workspace','',true)")
            .execute(&mut *tx)
            .await?;
        if workspace_count >= 1024 || owner_count >= 128 || count >= WORKERS_PER_LEAD {
            return Err(IdentityError::DelegationLimit);
        }
        if duplicate {
            return Err(IdentityError::IdempotencyConflict);
        }
        let agent = Uuid::new_v4();
        create_scope(&mut tx, context, agent, Visibility::OwnerPrivate, &[]).await?;
        sqlx::query(
            "INSERT INTO agents (id,workspace_id,owner_user_id,display_name,role,creation_key)
                     VALUES ($1,$2,$3,$4,'worker',$5)",
        )
        .bind(agent)
        .bind(workspace)
        .bind(owner)
        .bind(name)
        .bind(Uuid::new_v4())
        .execute(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO managed_workers (workspace_id,agent_id,owner_user_id,lead_agent_id,display_name)
                     VALUES ($1,$2,$3,$4,$5)")
            .bind(workspace).bind(agent).bind(owner).bind(lead.0).bind(name).execute(&mut *tx).await?;
        save_instructions(&mut tx, context, agent, 1, instructions).await?;
        let worker = find_worker(
            &mut tx,
            context,
            ManagedWorkerSelector::Id {
                agent_id: AgentId(agent),
            },
        )
        .await?;
        append_event(
            &mut tx,
            context,
            agent,
            "worker.created",
            json!({"agent_id":agent,"lead_agent_id":lead,"profile_revision":1}),
            None,
        )
        .await?;
        idem.record(&mut tx, agent, &worker).await?;
        tx.commit().await?;
        Ok(worker)
    }

    pub async fn update_managed_worker(
        &self,
        digest: &BearerDigest,
        input: UpdateManagedWorker,
    ) -> Result<ManagedWorker, IdentityError> {
        let instructions = validated_instructions(&input.instructions)?;
        if input.expected_revision < 1 {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "worker.update",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let worker = find_worker(
            &mut tx,
            context,
            ManagedWorkerSelector::Id {
                agent_id: input.agent_id,
            },
        )
        .await?;
        if worker.profile.revision != input.expected_revision {
            return Err(IdentityError::RevisionMismatch {
                current: worker.profile.revision,
            });
        }
        let next = sqlx::query_scalar::<_, i64>(
            "UPDATE managed_workers SET revision=revision+1 WHERE (workspace_id,agent_id,revision)=($1,$2,$3) RETURNING revision",
        ).bind(input.workspace_id.0).bind(input.agent_id.0).bind(input.expected_revision)
         .fetch_optional(&mut *tx).await?.ok_or(IdentityError::RevisionConflict)?;
        save_instructions(&mut tx, context, input.agent_id.0, next, instructions).await?;
        let result = find_worker(
            &mut tx,
            context,
            ManagedWorkerSelector::Id {
                agent_id: input.agent_id,
            },
        )
        .await?;
        append_event(
            &mut tx,
            context,
            input.agent_id.0,
            "worker.instructions_updated",
            json!({"agent_id":input.agent_id,"profile_revision":next}),
            None,
        )
        .await?;
        idem.record(&mut tx, input.agent_id.0, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn managed_worker_status(
        &self,
        digest: &BearerDigest,
        input: GetManagedWorkerStatus,
    ) -> Result<ManagedWorkerStatus, IdentityError> {
        let limit = input.limit.unwrap_or(8);
        if !(1..=20).contains(&limit) {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let worker = find_worker(&mut tx, context, input.worker).await?;
        let agent = worker.worker.agent_id.ok_or(IdentityError::InvalidRole)?;
        sqlx::query("SELECT set_config('office.managed_worker_status_id',$1,true)")
            .bind(agent.0.to_string())
            .execute(&mut *tx)
            .await?;
        let mut ids = sqlx::query_scalar::<_, Uuid>(
            "SELECT t.id FROM tasks t WHERE t.workspace_id=$1 AND ($3::uuid IS NULL OR t.id>$3) AND (
                t.assignee_agent_id=$2 OR EXISTS (SELECT 1 FROM execution_jobs j WHERE (j.workspace_id,j.task_id)=(t.workspace_id,t.id)
                    AND j.worker_agent_id=$2) OR EXISTS (SELECT 1 FROM runs r WHERE (r.workspace_id,r.task_id)=(t.workspace_id,t.id)
                    AND r.executor_agent_id=$2)
            ) ORDER BY t.id LIMIT $4",
        ).bind(input.workspace_id.0).bind(agent.0).bind(input.tasks_after.map(|task| task.0)).bind(i64::from(limit)+1)
         .fetch_all(&mut *tx).await?;
        let has_more = ids.len() > usize::from(limit);
        ids.truncate(usize::from(limit));
        let next_tasks_after = ids.last().copied().filter(|_| has_more).map(TaskId);
        let mut tasks = Vec::with_capacity(ids.len());
        for id in ids {
            let (row, scope) = load_task(&mut tx, context, TaskId(id), false).await?;
            let task = row.into_task(&mut tx, scope).await?;
            let (runs, next_run_after) =
                runs::history(&mut tx, input.workspace_id, task.task_id, None, 4).await?;
            let job = latest_worker_job(&mut tx, input.workspace_id, task.task_id, agent).await?;
            tasks.push(ManagedWorkerTask {
                state: TaskView {
                    task,
                    runs,
                    next_run_after,
                },
                job,
            });
        }
        tx.commit().await?;
        Ok(ManagedWorkerStatus {
            worker,
            tasks,
            next_tasks_after,
        })
    }
}

pub(super) async fn pin_instructions(
    tx: &mut Transaction<'_, Postgres>,
    run: &Run,
) -> Result<(), IdentityError> {
    sqlx::query(
        "INSERT INTO managed_worker_run_profiles (workspace_id,task_id,run_id,owner_user_id,agent_id,profile_revision)
         SELECT workspace_id,$2,$3,owner_user_id,agent_id,revision FROM managed_workers
         WHERE (workspace_id,agent_id,owner_user_id)=($1,$4,$5)",
    ).bind(run.workspace_id.0).bind(run.task_id.0).bind(run.run_id.0).bind(run.executor.agent_id.map(|agent| agent.0))
     .bind(run.executor.user_id.0).execute(&mut **tx).await?;
    Ok(())
}

pub(super) async fn run_instructions(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    run: RunId,
) -> Result<Option<WorkerInstructions>, IdentityError> {
    let row = sqlx::query_as::<_, (i64, String, String)>(
        "SELECT p.profile_revision,w.display_name,r.instructions FROM managed_worker_run_profiles p
         JOIN managed_workers w ON (w.workspace_id,w.agent_id)=(p.workspace_id,p.agent_id)
         JOIN managed_worker_revisions r ON (r.workspace_id,r.agent_id,r.revision)=(p.workspace_id,p.agent_id,p.profile_revision)
         WHERE (p.workspace_id,p.run_id)=($1,$2)",
    ).bind(workspace.0).bind(run.0).fetch_optional(&mut **tx).await?;
    Ok(row.map(
        |(revision, display_name, instructions)| WorkerInstructions {
            revision,
            display_name,
            instructions,
        },
    ))
}

fn validated_instructions(value: &str) -> Result<&str, IdentityError> {
    let value = value.trim();
    if value.len() > 6000
        || value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(IdentityError::InvalidInput);
    }
    Ok(value)
}

async fn find_worker(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    selector: ManagedWorkerSelector,
) -> Result<ManagedWorker, IdentityError> {
    let (id, name) = match selector {
        ManagedWorkerSelector::Id { agent_id } => (Some(agent_id.0), None),
        ManagedWorkerSelector::Name { display_name } => {
            valid_name(&display_name)?;
            (None, Some(display_name))
        }
    };
    let rows = sqlx::query_as::<_, Profile>(
        "SELECT w.*,r.instructions,r.updated_at FROM managed_workers w
         JOIN agents a ON (a.workspace_id,a.id,a.owner_user_id)=(w.workspace_id,w.agent_id,w.owner_user_id)
         JOIN managed_worker_revisions r ON (r.workspace_id,r.agent_id,r.revision)=(w.workspace_id,w.agent_id,w.revision)
         WHERE (w.workspace_id,w.owner_user_id)=($1,$2) AND a.disabled_at IS NULL
           AND office_owns_data(w.owner_user_id,w.lead_agent_id)
           AND ($3::uuid IS NULL OR w.agent_id=$3) AND ($4::text IS NULL OR w.name_key=lower(trim($4)))
         ORDER BY w.agent_id LIMIT 2",
    ).bind(context.access.workspace.0).bind(context.access.member.0).bind(id).bind(name).fetch_all(&mut **tx).await?;
    match rows.as_slice() {
        [] => Err(IdentityError::Forbidden),
        [_] => rows
            .into_iter()
            .next()
            .map(Into::into)
            .ok_or(IdentityError::Forbidden),
        _ => Err(IdentityError::RevisionConflict),
    }
}

async fn save_instructions(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    agent: Uuid,
    revision: i64,
    instructions: &str,
) -> Result<(), IdentityError> {
    sqlx::query("INSERT INTO managed_worker_revisions (workspace_id,agent_id,owner_user_id,revision,updated_by_agent_id,instructions)
                 VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(context.access.workspace.0).bind(agent).bind(context.access.member.0).bind(revision)
        .bind(context.agent_id().map(|actor| actor.0)).bind(instructions).execute(&mut **tx).await?;
    Ok(())
}
