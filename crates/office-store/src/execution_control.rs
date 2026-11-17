use chrono::{DateTime, Utc};
use office_domain::{BearerDigest, RunId, UserId, WorkspaceId, WorkspaceRole};
use office_protocol::{
    ExecutionControl, ExecutionControlScope, ExecutionLimits, ExecutionMode, GetExecutionControl,
    SetExecutionControl,
};
use serde_json::json;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::{
    Store,
    data_scope::create_scope,
    events::{Idempotency, append_event},
    identity::{IdentityError, WorkspaceContext, workspace_context},
    tasks::task_text,
};

#[derive(Clone, Copy)]
struct ScopeKey {
    kind: &'static str,
    id: Uuid,
    owner: Option<Uuid>,
    defaults: ExecutionLimits,
}

impl ScopeKey {
    fn new(context: WorkspaceContext, scope: ExecutionControlScope) -> Self {
        let workspace = context.access.workspace.0;
        let owner = context.access.member.0;
        let (kind, id, max_outstanding, max_concurrent) = match scope {
            ExecutionControlScope::Workspace => ("workspace", workspace, 2048, 128),
            ExecutionControlScope::Owner => ("owner", owner, 256, 32),
            ExecutionControlScope::Team { team_id } => ("team", team_id.0, 128, 16),
            ExecutionControlScope::Agent { agent_id } => ("agent", agent_id.0, 32, 4),
        };
        Self {
            kind,
            id,
            owner: (kind != "workspace").then_some(owner),
            defaults: ExecutionLimits {
                max_outstanding,
                max_concurrent,
            },
        }
    }

    async fn authorize(
        self,
        tx: &mut Transaction<'_, Postgres>,
        context: WorkspaceContext,
    ) -> Result<(), IdentityError> {
        if context.agent_id().is_some() {
            return Err(IdentityError::Forbidden);
        }
        let statement = match self.kind {
            "workspace" => {
                return if matches!(
                    context.access.role,
                    WorkspaceRole::Owner | WorkspaceRole::Admin
                ) {
                    Ok(())
                } else {
                    Err(IdentityError::Forbidden)
                };
            }
            "team" => "SELECT true FROM teams WHERE workspace_id=$1 AND id=$2 AND owner_user_id=$3",
            "agent" => {
                "SELECT true FROM agents WHERE workspace_id=$1 AND id=$2 AND owner_user_id=$3"
            }
            _ => return Ok(()),
        };
        sqlx::query_scalar::<_, bool>(statement)
            .bind(context.access.workspace.0)
            .bind(self.id)
            .bind(self.owner)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(IdentityError::Forbidden)?;
        Ok(())
    }
}

#[derive(sqlx::FromRow)]
struct ControlRow {
    revision: i64,
    mode: String,
    max_outstanding: i32,
    max_concurrent: i32,
    reason: String,
    updated_at: DateTime<Utc>,
}

impl Store {
    pub async fn get_execution_control(
        &self,
        digest: &BearerDigest,
        input: GetExecutionControl,
    ) -> Result<ExecutionControl, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let key = ScopeKey::new(context, input.scope);
        key.authorize(&mut tx, context).await?;
        let reply = read_control(&mut tx, input.workspace_id, input.scope, key).await?;
        tx.commit().await?;
        Ok(reply)
    }

    pub async fn set_execution_control(
        &self,
        digest: &BearerDigest,
        input: SetExecutionControl,
    ) -> Result<ExecutionControl, IdentityError> {
        let reason = task_text(&input.reason, 2048)?;
        if !input.limits.is_valid()
            || input.expected_revision < 0
            || (input.mode != ExecutionMode::Running
                && !matches!(
                    input.scope,
                    ExecutionControlScope::Workspace | ExecutionControlScope::Owner
                ))
        {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let key = ScopeKey::new(context, input.scope);
        key.authorize(&mut tx, context).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "execution.control",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        lock_admission(&mut tx, input.workspace_id).await?;
        let current = read_control(&mut tx, input.workspace_id, input.scope, key).await?;
        if current.revision != input.expected_revision {
            return Err(IdentityError::RevisionMismatch {
                current: current.revision,
            });
        }
        sqlx::query(
            "INSERT INTO execution_controls
                (workspace_id,kind,subject_id,owner_user_id,revision,mode,last_stop_generation,max_outstanding,max_concurrent,reason)
             VALUES ($1,$2,$3,$4,1,$5,CASE WHEN $5='stopped' THEN nextval('execution_stop_generation') ELSE 0 END,$6,$7,$8)
             ON CONFLICT(workspace_id,kind,subject_id) DO UPDATE SET revision=execution_controls.revision+1,
                mode=excluded.mode, last_stop_generation=greatest(excluded.last_stop_generation,execution_controls.last_stop_generation),
                max_outstanding=excluded.max_outstanding,max_concurrent=excluded.max_concurrent,reason=excluded.reason,
                updated_at=clock_timestamp()",
        )
        .bind(input.workspace_id.0)
        .bind(key.kind)
        .bind(key.id)
        .bind(key.owner)
        .bind(input.mode.as_str())
        .bind(i32::from(input.limits.max_outstanding))
        .bind(i32::from(input.limits.max_concurrent))
        .bind(reason)
        .execute(&mut *tx)
        .await?;
        let reply = read_control(&mut tx, input.workspace_id, input.scope, key).await?;
        let audit_scope = Uuid::new_v4();
        let visibility = if key.owner.is_none() {
            office_domain::Visibility::Workspace
        } else {
            office_domain::Visibility::OwnerPrivate
        };
        create_scope(&mut tx, context, audit_scope, visibility, &[]).await?;
        append_event(
            &mut tx,
            context,
            audit_scope,
            "execution.control_changed",
            json!({"scope":input.scope,"mode":input.mode,"revision":reply.revision,
                   "limits":input.limits,"reason":reason}),
            None,
        )
        .await?;
        idem.record(&mut tx, audit_scope, &reply).await?;
        tx.commit().await?;
        Ok(reply)
    }
}

async fn read_control(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    scope: ExecutionControlScope,
    key: ScopeKey,
) -> Result<ExecutionControl, IdentityError> {
    let row = sqlx::query_as::<_, ControlRow>(
        "SELECT revision,mode,max_outstanding,max_concurrent,reason,updated_at
         FROM execution_controls WHERE workspace_id=$1 AND kind=$2 AND subject_id=$3",
    )
    .bind(workspace.0)
    .bind(key.kind)
    .bind(key.id)
    .fetch_optional(&mut **tx)
    .await?;
    let mut result = ExecutionControl {
        workspace_id: workspace,
        scope,
        revision: 0,
        mode: ExecutionMode::Running,
        limits: key.defaults,
        reason: None,
        updated_at: None,
    };
    if let Some(row) = row {
        result.revision = row.revision;
        result.mode = match row.mode.as_str() {
            "running" => ExecutionMode::Running,
            "paused" => ExecutionMode::Paused,
            "stopped" => ExecutionMode::Stopped,
            _ => return Err(IdentityError::InvalidRole),
        };
        result.limits = ExecutionLimits {
            max_outstanding: row.max_outstanding as u16,
            max_concurrent: row.max_concurrent as u16,
        };
        result.reason = Some(row.reason);
        result.updated_at = Some(row.updated_at);
    }
    Ok(result)
}

pub(crate) async fn lock_admission(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
) -> Result<(), IdentityError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("execution-admission/{}", workspace.0))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// The generation must be read from the same policy snapshot as the launch decision.
pub(crate) async fn launch_generation(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    owner: UserId,
) -> Result<Option<i64>, IdentityError> {
    let (active, generation): (bool, i64) = sqlx::query_as(
        "SELECT coalesce(bool_and(mode='running'),true)
            AND (SELECT NOT restore_pending FROM office_installation WHERE singleton),
            coalesce(max(last_stop_generation),0)
         FROM execution_controls WHERE workspace_id=$1
           AND ((kind='workspace' AND subject_id=$1) OR (kind='owner' AND subject_id=$2))",
    )
    .bind(workspace.0)
    .bind(owner.0)
    .fetch_one(&mut **tx)
    .await?;
    Ok(active.then_some(generation))
}

pub(crate) async fn require_live_run(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    run: RunId,
) -> Result<(), IdentityError> {
    if run_was_stopped(tx, workspace, run).await? {
        return Err(IdentityError::RunLeaseStale);
    }
    Ok(())
}

pub(crate) async fn run_was_stopped(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    run: RunId,
) -> Result<bool, IdentityError> {
    let stopped = sqlx::query_scalar::<_, bool>(
        "SELECT office_execution_stopped(workspace_id,executor_user_id,stop_generation)
         FROM runs WHERE workspace_id=$1 AND id=$2",
    )
    .bind(workspace.0)
    .bind(run.0)
    .fetch_optional(&mut **tx)
    .await?;
    stopped.ok_or(IdentityError::RunLeaseStale)
}

/// Check the scopes in a scheduling transaction holding the workspace admission lock.
pub(crate) async fn capacity_available(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    owner: UserId,
    team: Option<Uuid>,
    worker: Uuid,
    start: bool,
) -> Result<bool, IdentityError> {
    Ok(capacity_limit(tx, workspace, owner, team, worker, start)
        .await?
        .is_none())
}

pub(crate) async fn capacity_limit(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    owner: UserId,
    team: Option<Uuid>,
    worker: Uuid,
    start: bool,
) -> Result<Option<String>, IdentityError> {
    sqlx::query("SELECT set_config('office.execution_capacity_workspace',$1,true)")
        .bind(workspace.0.to_string())
        .execute(&mut **tx)
        .await?;
    let limit = sqlx::query_scalar(
        "SELECT s.kind FROM (VALUES ('workspace',$1::uuid,2048,128,0),('owner',$2::uuid,256,32,1),
                   ('team',$3::uuid,128,16,2),('agent',$4::uuid,32,4,3)) AS s(kind,subject_id,max_pending,max_active,precedence)
            LEFT JOIN execution_controls c ON (c.workspace_id,c.kind,c.subject_id)=($1,s.kind,s.subject_id)
            CROSS JOIN LATERAL (
                SELECT count(*) AS outstanding, count(*) FILTER (WHERE j.state IN ('leased','reconcile_required')) AS active
                FROM execution_jobs j
                WHERE j.workspace_id=$1 AND j.state IN ('queued','leased','reconcile_required','waiting_input')
                  AND CASE s.kind WHEN 'workspace' THEN true WHEN 'owner' THEN j.owner_user_id=$2
                        WHEN 'team' THEN j.team_id=$3 ELSE j.worker_agent_id=$4 END
            ) counts
            WHERE s.subject_id IS NOT NULL AND (
                CASE WHEN $5 THEN counts.active>=coalesce(c.max_concurrent,s.max_active)
                     ELSE counts.outstanding>=coalesce(c.max_outstanding,s.max_pending) END
            )
            ORDER BY s.precedence LIMIT 1",
    )
    .bind(workspace.0)
    .bind(owner.0)
    .bind(team)
    .bind(worker)
    .bind(start)
    .fetch_optional(&mut **tx)
    .await?;
    sqlx::query("SELECT set_config('office.execution_capacity_workspace','',true)")
        .execute(&mut **tx)
        .await?;
    Ok(limit)
}
