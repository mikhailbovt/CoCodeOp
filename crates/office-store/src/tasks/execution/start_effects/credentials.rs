use chrono::{DateTime, Utc};
use office_domain::{
    Actor, AgentId, BearerDigest, CredentialId, RunId, Subject, TaskId, UserId, WorkspaceAccess,
    WorkspaceId,
};
use office_protocol::{
    GetRunWorkContext, IssueRunCredential, Recipient, RunContextView, RunCredentialReceipt,
    RunWorkContext,
};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::super::{activation::run_activation, managed_input::continuation};
use super::{StartTarget, permit_live, require_effect, task_brief};
use crate::{
    Store,
    identity::{IdentityError, WorkspaceContext, bind_subject},
    inbox::read_page,
    tasks::{TaskRow, managed_workers::run_instructions, scopes},
};

#[derive(sqlx::FromRow)]
struct SessionRow {
    id: Uuid,
    workspace_id: Uuid,
    owner_user_id: Uuid,
    job_id: Uuid,
    run_id: Uuid,
    worker_agent_id: Uuid,
    runner_id: Uuid,
    runner_credential_id: Uuid,
    run_fence: i64,
    digest: Vec<u8>,
    issued_at: DateTime<Utc>,
}

impl Store {
    pub async fn issue_run_credential(
        &self,
        credential: &BearerDigest,
        input: IssueRunCredential,
    ) -> Result<RunCredentialReceipt, IdentityError> {
        let digest =
            BearerDigest::decode(&input.credential_digest).ok_or(IdentityError::InvalidInput)?;
        let mut tx = self.pool.begin().await?;
        let target = StartTarget::load(&mut tx, credential, &input.effect.lease).await?;
        target.verify_live(&mut tx).await?;
        let effect = require_effect(&mut tx, &target.run, input.effect.effect_id).await?;
        if !matches!(effect.state.as_str(), "authorized" | "started")
            || !target.verifies_approval(&effect, input.effect.lease.run_fence)
            || (effect.state == "authorized" && !permit_live(&mut tx, effect.permit_until).await?)
        {
            return Err(IdentityError::RunLeaseStale);
        }
        sqlx::query(
            "INSERT INTO run_credentials (id,workspace_id,owner_user_id,job_id,run_id,worker_agent_id,
                         runner_id,runner_credential_id,run_fence,digest)
             SELECT $2,workspace_id,executor_user_id,managed_job_id,id,executor_agent_id,managed_runner_id,$3,run_fence,$4
             FROM runs WHERE workspace_id=$1 AND id=$5 AND lease_until>clock_timestamp()
             ON CONFLICT DO NOTHING",
        )
        .bind(target.job.workspace_id)
        .bind(Uuid::new_v4())
        .bind(target.runner.credential_id.0)
        .bind(digest.as_bytes())
        .bind(target.run.run_id.0)
        .execute(&mut *tx)
        .await?;
        let issued = sqlx::query_as::<_, SessionRow>(
            "SELECT * FROM run_credentials WHERE workspace_id=$1 AND run_id=$2",
        )
        .bind(target.job.workspace_id)
        .bind(target.run.run_id.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(IdentityError::IdempotencyConflict)?;
        if issued.digest != digest.as_bytes()
            || issued.runner_credential_id != target.runner.credential_id.0
        {
            return Err(IdentityError::IdempotencyConflict);
        }
        tx.commit().await?;
        Ok(RunCredentialReceipt {
            credential_id: issued.id,
            run_id: RunId(issued.run_id),
            issued_at: issued.issued_at,
        })
    }

    pub async fn run_context(
        &self,
        digest: &BearerDigest,
    ) -> Result<RunContextView, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let access = authenticate_run(&mut tx, digest).await?;
        let run = access.view(&mut tx).await?;
        tx.commit().await?;
        Ok(run)
    }

    pub async fn run_work_context(
        &self,
        digest: &BearerDigest,
        input: GetRunWorkContext,
        retain_count: u32,
    ) -> Result<RunWorkContext, IdentityError> {
        let limit = input.limit.unwrap_or(12);
        if !(1..=25).contains(&limit) {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let access = authenticate_run(&mut tx, digest).await?;
        let inbox = read_page(
            &mut tx,
            access.context,
            input.inbox_cursor,
            limit,
            retain_count,
            Some((TaskId(access.task.id), access.run_id, access.job_id)),
        )
        .await?;
        let run = access.view(&mut tx).await?;
        tx.commit().await?;
        Ok(RunWorkContext { run, inbox })
    }
}

pub(in crate::tasks) struct RunAccess {
    pub context: WorkspaceContext,
    pub task: TaskRow,
    pub run_id: RunId,
    pub job_id: Uuid,
    pub run_fence: i64,
    pub lease_until: DateTime<Utc>,
    pub gateway_required: bool,
}

impl RunAccess {
    async fn view(
        self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<RunContextView, IdentityError> {
        let scope = scopes::task_scope_definition(tx, &self.task)
            .await?
            .filter(|scope| scope.worker == self.context.recipient());
        let activation = run_activation(tx, self.context, self.job_id).await?;
        let continuation = continuation(tx, self.context.access.workspace, self.job_id).await?;
        let worker_instructions =
            run_instructions(tx, self.context.access.workspace, self.run_id).await?;
        Ok(RunContextView {
            workspace_id: self.context.access.workspace,
            run_id: self.run_id,
            executor: self.context.recipient(),
            task: task_brief(self.task),
            scope_revision: scope.as_ref().map(|scope| scope.revision),
            required_read_paths: scope.map_or_else(Vec::new, |scope| scope.required_read_paths),
            activation,
            continuation,
            worker_instructions,
        })
    }
}

#[derive(sqlx::FromRow)]
struct RunningTask {
    #[sqlx(flatten)]
    task: TaskRow,
    worker_role: String,
    member_role: String,
    lease_until: DateTime<Utc>,
    gateway_required: bool,
}

pub(in crate::tasks) async fn authenticate_run(
    tx: &mut Transaction<'_, Postgres>,
    digest: &BearerDigest,
) -> Result<RunAccess, IdentityError> {
    sqlx::query("SELECT set_config('office.run_credential_digest',$1,true)")
        .bind(hex::encode(digest.as_bytes()))
        .execute(&mut **tx)
        .await?;
    let context = sqlx::query_as::<_, SessionRow>("SELECT * FROM run_credentials WHERE digest=$1")
        .bind(digest.as_bytes())
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(IdentityError::Unauthenticated)?;
    let executor = Recipient {
        user_id: UserId(context.owner_user_id),
        agent_id: Some(AgentId(context.worker_agent_id)),
    };
    bind_subject(tx, executor).await?;
    sqlx::query("SELECT set_config('office.workspace_id',$1,true)")
        .bind(context.workspace_id.to_string())
        .execute(&mut **tx)
        .await?;
    let task = sqlx::query_as::<_, RunningTask>(
            "SELECT t.*,a.role AS worker_role,m.role AS member_role,r.lease_until,j.gateway_required FROM runs r
             JOIN execution_jobs j ON (j.workspace_id,j.id,j.current_run_id)=(r.workspace_id,r.managed_job_id,r.id)
             JOIN tasks t ON (t.workspace_id,t.id)=(r.workspace_id,r.task_id)
             JOIN workspace_memberships m ON (m.workspace_id,m.user_id)=(r.workspace_id,r.executor_user_id)
             JOIN agents a ON (a.workspace_id,a.id,a.owner_user_id)=(r.workspace_id,r.executor_agent_id,r.executor_user_id)
             JOIN users u ON u.id=r.executor_user_id
             JOIN runners runner ON (runner.workspace_id,runner.id,runner.owner_user_id)=(r.workspace_id,r.managed_runner_id,r.executor_user_id)
             JOIN credentials c ON (c.workspace_id,c.runner_id,c.owner_user_id)=(r.workspace_id,r.managed_runner_id,r.executor_user_id)
             WHERE r.workspace_id=$1 AND j.id=$2 AND r.id=$3 AND r.executor_agent_id=$4 AND r.managed_runner_id=$5
               AND r.run_fence=$6 AND c.id=$7 AND r.status='running' AND r.lease_until>clock_timestamp()
               AND j.state='leased' AND t.status='in_progress' AND NOT t.awaiting_owner
               AND t.assignee_agent_id=$4 AND t.assignee_user_id=r.executor_user_id
               AND u.disabled_at IS NULL AND runner.disabled_at IS NULL AND a.disabled_at IS NULL AND m.revoked_at IS NULL
               AND NOT EXISTS(SELECT 1 FROM activated_executions turn JOIN activation_policies p
                    ON (p.workspace_id,p.agent_id)=(turn.workspace_id,turn.agent_id)
                    WHERE (turn.workspace_id,turn.job_id)=(j.workspace_id,office_activation_job(j.workspace_id,j.id))
                    AND (turn.policy_revision<>p.revision OR p.managed IS NULL OR turn.runner_id<>r.managed_runner_id))
               AND c.revoked_at IS NULL AND (c.expires_at IS NULL OR c.expires_at>clock_timestamp())",
        )
        .bind(context.workspace_id)
        .bind(context.job_id)
        .bind(context.run_id)
        .bind(context.worker_agent_id)
        .bind(context.runner_id)
        .bind(context.run_fence)
        .bind(context.runner_credential_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(IdentityError::Unauthenticated)?;
    let workspace = WorkspaceId(context.workspace_id);
    crate::execution_control::require_live_run(tx, workspace, RunId(context.run_id)).await?;
    let owner = executor.user_id;
    let agent = AgentId(context.worker_agent_id);
    let role = task
        .worker_role
        .parse()
        .map_err(|_| IdentityError::InvalidRole)?;
    let member_role = task
        .member_role
        .parse()
        .map_err(|_| IdentityError::InvalidRole)?;
    Ok(RunAccess {
        context: WorkspaceContext {
            actor: Actor::Agent {
                workspace,
                owner,
                agent,
                credential: CredentialId(context.id),
                role,
            },
            access: WorkspaceAccess {
                workspace,
                member: owner,
                role: member_role,
            },
            subject: Subject::Agent { agent_id: agent },
        },
        task: task.task,
        run_id: RunId(context.run_id),
        job_id: context.job_id,
        run_fence: context.run_fence,
        lease_until: task.lease_until,
        gateway_required: task.gateway_required,
    })
}
