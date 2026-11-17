use chrono::{DateTime, Utc};
use office_domain::{
    Action, Actor, AgentId, AgentRole, BearerDigest, FileAction, GrantRight, Resource,
    SourceManifest, SourcePath, TaskAuthority, TaskId, TaskStatus, TeamId, TeamRole, UserId,
    Visibility, WorkPermissions, WorkspaceId,
};
use office_protocol::{
    AcknowledgeScope, AclGrant, CheckoutGrant, GetWorkScope, MAX_READ_DEPENDENCIES,
    NextScopeDeployment, Recipient, ScopeDeployment, SetTaskScope, TaskChange, WorkScope,
    WorkScopeDefinition, WorkScopeOperation, WorkScopeReceipt, WorkScopeState,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use super::{TaskRow, load_task};
use crate::{
    Store,
    checkouts::{
        areas::required_area_approvals,
        checkout_runner,
        coordinator::{checkout_root, require_epoch},
        grants::{bind_grant_validation, checkout_grant, verify_commands},
        load_checkout, lock_policy,
    },
    data_scope::{create_scope, subject},
    events::{Idempotency, append_event, append_workspace_event},
    identity::{
        IdentityError, WorkspaceContext, active_principal, bind_subject, require_permission,
        verify_active_recipient, workspace_context,
    },
    inbox::save_cursor,
    teams::member_role,
};

mod access;
mod revocations;

#[derive(sqlx::FromRow)]
struct ScopeRow {
    id: Uuid,
    workspace_id: Uuid,
    checkout_id: Uuid,
    task_id: Uuid,
    worker_user_id: Uuid,
    worker_agent_id: Uuid,
    revision: i64,
    grant_id: Uuid,
    grant_revision: i64,
    operation: String,
    access_request_id: Option<Uuid>,
    policy_sequence: i64,
    state: String,
    expires_at: DateTime<Utc>,
    permissions: Json<WorkPermissions>,
    required_read_paths: Json<Vec<SourcePath>>,
    acknowledged_at: Option<DateTime<Utc>>,
    deployment_sha256: Option<String>,
}

const SCOPE_QUERY: &str = "SELECT s.*, v.revision, v.grant_id, v.grant_revision, v.operation, v.access_request_id, v.policy_sequence,
        v.state, v.expires_at, v.permissions, v.required_read_paths, v.acknowledged_at, v.deployment_sha256
    FROM work_scopes s JOIN work_scope_revisions v ON v.workspace_id = s.workspace_id AND v.scope_id = s.id";

impl ScopeRow {
    fn worker(&self) -> Recipient {
        Recipient {
            user_id: UserId(self.worker_user_id),
            agent_id: Some(AgentId(self.worker_agent_id)),
        }
    }

    fn into_deployment(self) -> ScopeDeployment {
        let worker = self.worker();
        ScopeDeployment {
            policy_sequence: self.policy_sequence,
            scope: WorkScopeDefinition {
                workspace_id: WorkspaceId(self.workspace_id),
                task_id: TaskId(self.task_id),
                checkout_id: self.checkout_id,
                grant_id: self.grant_id,
                grant_revision: self.grant_revision,
                access_request_id: self.access_request_id,
                scope_id: self.id,
                revision: self.revision,
                operation: if self.operation == "revoke" {
                    WorkScopeOperation::Revoke
                } else {
                    WorkScopeOperation::Apply
                },
                worker,
                expires_at: self.expires_at,
                permissions: self.permissions.0,
                required_read_paths: self.required_read_paths.0,
            },
        }
    }

    fn into_scope(self) -> Result<WorkScope, IdentityError> {
        let state = match self.state.as_str() {
            "pending_activation" if self.operation == "revoke" => WorkScopeState::RevocationPending,
            "active" if self.operation == "revoke" => WorkScopeState::Revoked,
            "pending_activation" => WorkScopeState::PendingActivation,
            "active" => WorkScopeState::Active,
            "superseded" => WorkScopeState::Superseded,
            "rejected" => WorkScopeState::Rejected,
            _ => return Err(IdentityError::InvalidRole),
        };
        let acknowledged_at = self.acknowledged_at;
        let deployment = self.into_deployment();
        Ok(WorkScope {
            definition: deployment.scope,
            state,
            policy_sequence: deployment.policy_sequence,
            acknowledged_at,
        })
    }
}

impl Store {
    pub async fn set_task_scope(
        &self,
        digest: &BearerDigest,
        input: SetTaskScope,
    ) -> Result<WorkScopeReceipt, IdentityError> {
        if !input.permissions.is_valid()
            || !(1..i64::MAX).contains(&input.expected_task_revision)
            || input.worker.agent_id.is_none()
        {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "work_scope.set",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let (task, _) = load_task(&mut tx, context, input.task_id, true).await?;
        require_permission(
            context.actor,
            Action::ManageTask,
            Resource::Task(TaskAuthority {
                workspace: context.access,
                accountable_owner: UserId(task.accountable_user_id),
                accountable: subject(UserId(task.accountable_user_id), task.accountable_agent_id),
                assignee: None,
                executor: None,
                explicit_reviewer: None,
            }),
        )?;
        if task.revision != input.expected_task_revision {
            return Err(IdentityError::RevisionMismatch {
                current: task.revision,
            });
        }
        if task.awaiting_owner
            || task.execution_kind != "coding"
            || matches!(
                task.status()?,
                TaskStatus::Done | TaskStatus::Cancelled | TaskStatus::Failed
            )
        {
            return Err(IdentityError::InvalidTransition);
        }
        lock_policy(&mut tx, input.checkout_id).await?;
        let checkout = load_checkout(&mut tx, input.workspace_id, input.checkout_id, false).await?;
        if checkout.state != "registered" {
            return Err(IdentityError::InvalidTransition);
        }
        let grant = checkout_grant(&mut tx, input.workspace_id, input.grant_id).await?;
        verify_grant_owner(context, input.checkout_id, &grant)?;
        let now = db_now(&mut tx).await?;
        if grant.revoked_at.is_some()
            || grant.expires_at <= now
            || input.expires_at <= now
            || input.expires_at > grant.expires_at
        {
            return Err(IdentityError::AccessRequestRequired);
        }
        let effective = grant
            .permissions
            .narrow(&input.permissions)
            .filter(|scope| scope.within_manifest(&checkout.manifest.0))
            .ok_or(IdentityError::AccessRequestRequired)?;
        if !required_area_approvals(
            &mut tx,
            input.workspace_id,
            input.checkout_id,
            grant.recipient,
            &effective,
        )
        .await?
        .is_empty()
        {
            return Err(IdentityError::AccessRequestRequired);
        }
        bind_grant_validation(&mut tx, Some(grant.grant_id)).await?;
        verify_commands(&mut tx, context, &checkout, &effective).await?;
        bind_grant_validation(&mut tx, None).await?;
        let worker = verify_worker(&mut tx, context, &task, grant.recipient, input.worker).await?;
        let current = select_scope(&mut tx, input.workspace_id, input.task_id, None, false).await?;
        let mut required_paths = input.required_read_paths.clone().unwrap_or_else(|| {
            current
                .as_ref()
                .map_or_else(Vec::new, |row| row.required_read_paths.0.clone())
        });
        required_paths.sort_unstable();
        if required_paths.len() > MAX_READ_DEPENDENCIES
            || required_paths.windows(2).any(|pair| pair[0] == pair[1])
        {
            return Err(IdentityError::InvalidInput);
        }
        require_read_paths(&checkout.manifest.0, &effective, &required_paths)?;
        let (id, revision) = match current {
            Some(current) => {
                if input.expected_scope_revision != Some(current.revision) {
                    return Err(IdentityError::RevisionMismatch {
                        current: current.revision,
                    });
                }
                if current.state == "pending_activation"
                    || current.checkout_id != input.checkout_id
                    || current.worker() != input.worker
                {
                    return Err(IdentityError::InvalidTransition);
                }
                revocations::verify_fresh_grant(
                    &mut tx,
                    input.workspace_id,
                    current.id,
                    grant.grant_id,
                )
                .await?;
                (
                    current.id,
                    current
                        .revision
                        .checked_add(1)
                        .ok_or(IdentityError::InvalidTransition)?,
                )
            }
            None => {
                if input.expected_scope_revision.is_some() {
                    return Err(IdentityError::InvalidInput);
                }
                let id = Uuid::new_v4();
                add_scope(
                    &mut tx,
                    context,
                    &input,
                    &grant,
                    worker,
                    id,
                    UserId(checkout.owner_user_id),
                )
                .await?;
                (id, 1)
            }
        };
        sqlx::query("INSERT INTO work_scope_revisions (workspace_id, checkout_id, scope_id, revision,
                        grant_id, grant_revision, expires_at, permissions, required_read_paths) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)")
            .bind(input.workspace_id.0).bind(input.checkout_id).bind(id).bind(revision).bind(grant.grant_id)
            .bind(grant.revision).bind(input.expires_at).bind(Json(effective)).bind(Json(required_paths)).execute(&mut *tx).await?;
        let task_revision = sqlx::query_scalar::<_, i64>(
            "UPDATE tasks SET checkout_id = $3, work_scope_id = NULL, work_scope_revision = NULL,
                revision = revision + 1, updated_at = clock_timestamp()
             WHERE workspace_id = $1 AND id = $2 RETURNING revision",
        )
        .bind(input.workspace_id.0)
        .bind(input.task_id.0)
        .bind(input.checkout_id)
        .fetch_one(&mut *tx)
        .await?;
        let scope = select_scope(
            &mut tx,
            input.workspace_id,
            input.task_id,
            Some(revision),
            false,
        )
        .await?
        .ok_or(IdentityError::InvalidTransition)?
        .into_scope()?;
        let seq = append_event(&mut tx, context, id, "scope.pending_activation", json!({
            "scope_id": id, "task_id": input.task_id, "checkout_id": input.checkout_id, "revision": revision,
        }), None).await?;
        let result = WorkScopeReceipt {
            scope,
            task_revision,
            event_cursor: save_cursor(&mut tx, context, seq).await?,
        };
        idem.record(&mut tx, id, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn get_work_scope(
        &self,
        digest: &BearerDigest,
        input: GetWorkScope,
    ) -> Result<WorkScope, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let row = select_scope(
            &mut tx,
            input.workspace_id,
            input.task_id,
            input.revision,
            false,
        )
        .await?
        .ok_or(IdentityError::Forbidden)?;
        match load_task(&mut tx, context, input.task_id, false).await {
            Ok(_) => {}
            Err(IdentityError::Forbidden) => {
                let checkout =
                    load_checkout(&mut tx, input.workspace_id, row.checkout_id, false).await?;
                if context.actor != Actor::Human(UserId(checkout.owner_user_id)) {
                    return Err(IdentityError::Forbidden);
                }
            }
            Err(error) => return Err(error),
        }
        let answer = row.into_scope()?;
        tx.commit().await?;
        Ok(answer)
    }

    pub async fn next_scope_deployment(
        &self,
        digest: &BearerDigest,
        checkout: Uuid,
    ) -> Result<NextScopeDeployment, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let runner = checkout_runner(&mut tx, digest).await?;
        let (root, _) = checkout_root(&mut tx, &runner, checkout).await?;
        let pending = next_revision(&mut tx, runner.workspace_id, checkout)
            .await?
            .map(ScopeRow::into_deployment);
        tx.commit().await?;
        Ok(NextScopeDeployment {
            checkout_id: checkout,
            root,
            pending,
        })
    }

    pub async fn acknowledge_scope(
        &self,
        digest: &BearerDigest,
        input: AcknowledgeScope,
    ) -> Result<WorkScope, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let runner = checkout_runner(&mut tx, digest).await?;
        load_checkout(&mut tx, runner.workspace_id, input.checkout_id, false).await?;
        lock_policy(&mut tx, input.checkout_id).await?;
        require_epoch(
            &mut tx,
            runner.workspace_id,
            input.checkout_id,
            input.coordinator_epoch,
            true,
        )
        .await?;
        super::publications::publication_barrier(
            &mut tx,
            runner.workspace_id,
            input.checkout_id,
            input.coordinator_epoch,
        )
        .await?;
        super::snapshot_commands::require_snapshot_barrier(
            &mut tx,
            runner.workspace_id,
            input.checkout_id,
        )
        .await?;
        let row = sqlx::query_as::<_, ScopeRow>(&format!(
            "{SCOPE_QUERY} WHERE s.workspace_id = $1 AND s.checkout_id = $2
            AND s.id = $3 AND v.revision = $4 AND v.policy_sequence = $5"
        ))
        .bind(runner.workspace_id.0)
        .bind(input.checkout_id)
        .bind(input.scope_id)
        .bind(input.scope_revision)
        .bind(input.policy_sequence)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(IdentityError::Forbidden)?;
        if let Some(previous) = &row.deployment_sha256 {
            if previous != &input.deployment_sha256 {
                return Err(IdentityError::IdempotencyConflict);
            }
            let output = row.into_scope()?;
            tx.commit().await?;
            return Ok(output);
        }
        let pending = next_revision(&mut tx, runner.workspace_id, input.checkout_id).await?;
        if pending.map(|candidate| candidate.policy_sequence) != Some(input.policy_sequence) {
            return Err(IdentityError::InvalidTransition);
        }
        let deployment = row.into_deployment();
        let bytes = serde_json::to_vec(&deployment).map_err(|_| IdentityError::InvalidInput)?;
        if hex::encode(Sha256::digest(bytes)) != input.deployment_sha256 {
            return Err(IdentityError::InvalidInput);
        }
        let is_revocation = deployment.scope.operation == WorkScopeOperation::Revoke;
        let allowed = is_revocation
            || (!revocations::has_new_revocation(&mut tx, &deployment.scope).await?
                && live_grant(&mut tx, &deployment.scope, runner.owner_user_id, None).await?);
        let state = if allowed { "active" } else { "rejected" };
        if allowed {
            super::claims::fence_scope(
                &mut tx,
                runner.workspace_id,
                input.checkout_id,
                input.scope_id,
            )
            .await?;
            sqlx::query("UPDATE work_scope_revisions SET state='superseded' WHERE workspace_id=$1 AND scope_id=$2 AND state='active'")
                .bind(runner.workspace_id.0).bind(input.scope_id).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE work_scope_revisions SET state=$4, acknowledged_at=clock_timestamp(), deployment_sha256=$5
            WHERE workspace_id=$1 AND scope_id=$2 AND revision=$3")
            .bind(runner.workspace_id.0).bind(input.scope_id).bind(input.scope_revision).bind(state)
            .bind(&input.deployment_sha256).execute(&mut *tx).await?;
        let outcome = select_scope(
            &mut tx,
            runner.workspace_id,
            deployment.scope.task_id,
            Some(input.scope_revision),
            false,
        )
        .await?
        .ok_or(IdentityError::InvalidTransition)?
        .into_scope()?;
        append_workspace_event(&mut tx, runner.workspace_id, input.scope_id,
            if is_revocation { "scope.revoked" } else if allowed { "scope.changed" } else { "scope.activation_rejected" },
            json!({"checkout_id": input.checkout_id, "scope_id": input.scope_id, "task_id": deployment.scope.task_id,
                   "revision": input.scope_revision, "policy_sequence": input.policy_sequence, "runner_id": runner.runner_id}), None).await?;
        access::complete_activation(&mut tx, &deployment.scope, allowed).await?;
        tx.commit().await?;
        Ok(outcome)
    }
}

pub(super) async fn task_scope_reference(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRow,
) -> Result<Option<(Uuid, i64)>, IdentityError> {
    Ok(task_scope_definition(tx, task)
        .await?
        .map(|scope| (scope.scope_id, scope.revision)))
}

pub(super) async fn task_scope_definition(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRow,
) -> Result<Option<WorkScopeDefinition>, IdentityError> {
    if task.checkout_id.is_none() {
        return Ok(None);
    }
    Ok(select_scope(
        tx,
        WorkspaceId(task.workspace_id),
        TaskId(task.id),
        None,
        true,
    )
    .await?
    .map(|scope| scope.into_deployment().scope))
}

fn require_read_paths(
    manifest: &SourceManifest,
    permissions: &WorkPermissions,
    paths: &[SourcePath],
) -> Result<(), IdentityError> {
    if paths
        .iter()
        .any(|path| !permissions.permits(manifest, FileAction::Read(path)))
    {
        return Err(IdentityError::AccessRequestRequired);
    }
    Ok(())
}

pub(super) async fn has_work_scope(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRow,
) -> Result<bool, IdentityError> {
    if task.checkout_id.is_none() {
        return Ok(false);
    }
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM work_scopes WHERE workspace_id=$1 AND task_id=$2)",
    )
    .bind(task.workspace_id)
    .bind(task.id)
    .fetch_one(&mut **tx)
    .await?)
}

pub(super) async fn authorize_task_change(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: &TaskRow,
    command: &TaskChange,
) -> Result<(), IdentityError> {
    if task.execution_kind != "coding"
        || matches!(
            command,
            TaskChange::AddDependency { .. }
                | TaskChange::ReviseCriteria { .. }
                | TaskChange::ResolveReviewEscalation { .. }
                | TaskChange::Cancel { .. }
                | TaskChange::AcknowledgeCancel { .. }
                | TaskChange::Fail { .. }
                | TaskChange::Review { .. }
        )
    {
        return Ok(());
    }
    let scope = lock_active_scope(tx, context, task).await?;
    if matches!(
        command,
        TaskChange::Claim { .. }
            | TaskChange::Progress { .. }
            | TaskChange::RequestHelp { .. }
            | TaskChange::Submit { .. }
    ) && context.recipient() != scope.worker
    {
        return Err(IdentityError::Forbidden);
    }
    verify_scope_team_member(tx, context, task, scope.worker).await?;
    if let TaskChange::Assign { agent_id } = command
        && Some(*agent_id) != scope.worker.agent_id
    {
        return Err(IdentityError::Forbidden);
    }
    verify_scope_grant(tx, context, task, &scope).await
}

pub(super) async fn lock_worker_scope(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: &TaskRow,
) -> Result<(WorkScopeDefinition, office_domain::SourceManifest), IdentityError> {
    let scope = lock_active_scope(tx, context, task).await?;
    if context.recipient() != scope.worker {
        return Err(IdentityError::Forbidden);
    }
    verify_scope_team_member(tx, context, task, scope.worker).await?;
    verify_scope_grant(tx, context, task, &scope).await?;
    let checkout = access::access_checkout(tx, scope.workspace_id, scope.checkout_id).await?;
    if checkout.state != "registered" {
        return Err(IdentityError::AccessRequestRequired);
    }
    Ok((scope, checkout.manifest.0))
}

pub(super) async fn lock_execution_scope(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: &TaskRow,
    worker: Recipient,
) -> Result<Option<WorkScopeDefinition>, IdentityError> {
    if task.execution_kind != "coding" {
        return Ok(None);
    }
    let scope = lock_active_scope(tx, context, task).await?;
    if scope.worker != worker {
        return Err(IdentityError::Forbidden);
    }
    verify_scope_grant(tx, context, task, &scope).await?;
    let checkout = access::access_checkout(tx, scope.workspace_id, scope.checkout_id).await?;
    if checkout.state != "registered" {
        return Err(IdentityError::AccessRequestRequired);
    }
    Ok(Some(scope))
}

async fn lock_active_scope(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: &TaskRow,
) -> Result<WorkScopeDefinition, IdentityError> {
    let checkout = task
        .checkout_id
        .ok_or(IdentityError::AccessRequestRequired)?;
    lock_policy(tx, checkout).await?;
    select_scope(tx, context.access.workspace, TaskId(task.id), None, true)
        .await?
        .filter(|candidate| candidate.checkout_id == checkout)
        .map(|row| row.into_deployment().scope)
        .ok_or(IdentityError::AccessRequestRequired)
}

async fn verify_scope_team_member(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: &TaskRow,
    worker: Recipient,
) -> Result<(), IdentityError> {
    if context.recipient() == worker
        && let Some(team) = task.team_id.map(TeamId)
        && !matches!(
            member_role(tx, context.access.workspace, team, worker).await?,
            Some(TeamRole::Worker | TeamRole::Lead)
        )
    {
        return Err(IdentityError::Forbidden);
    }
    Ok(())
}

async fn verify_scope_grant(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: &TaskRow,
    scope: &WorkScopeDefinition,
) -> Result<(), IdentityError> {
    if !live_grant(tx, scope, context.access.member, task.accountable_agent_id).await? {
        return Err(IdentityError::AccessRequestRequired);
    }
    Ok(())
}

async fn select_scope(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    task: TaskId,
    revision: Option<i64>,
    active_only: bool,
) -> Result<Option<ScopeRow>, IdentityError> {
    Ok(sqlx::query_as::<_, ScopeRow>(&format!("{SCOPE_QUERY} WHERE s.workspace_id=$1 AND s.task_id=$2
        AND ($3::bigint IS NULL OR v.revision=$3) AND (NOT $4 OR (v.state='active' AND v.operation='apply'
          AND NOT EXISTS (SELECT 1 FROM work_scope_revisions revoked WHERE revoked.workspace_id=v.workspace_id
            AND revoked.scope_id=v.scope_id AND revoked.revision > v.revision AND revoked.operation='revoke'
            AND revoked.state='pending_activation'))) ORDER BY v.revision DESC LIMIT 1"))
        .bind(workspace.0).bind(task.0).bind(revision).bind(active_only).fetch_optional(&mut **tx).await?)
}

async fn next_revision(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
) -> Result<Option<ScopeRow>, IdentityError> {
    Ok(sqlx::query_as::<_, ScopeRow>(&format!(
        "{SCOPE_QUERY} WHERE s.workspace_id=$1 AND s.checkout_id=$2
        AND v.state='pending_activation' ORDER BY v.policy_sequence LIMIT 1"
    ))
    .bind(workspace.0)
    .bind(checkout)
    .fetch_optional(&mut **tx)
    .await?)
}

async fn live_grant(
    tx: &mut Transaction<'_, Postgres>,
    scope: &WorkScopeDefinition,
    bound_user: UserId,
    accountable_agent_id: Option<Uuid>,
) -> Result<bool, IdentityError> {
    bind_grant_validation(tx, Some(scope.grant_id)).await?;
    let grant = checkout_grant(tx, scope.workspace_id, scope.grant_id).await?;
    bind_grant_validation(tx, None).await?;
    let now = db_now(tx).await?;
    if grant.revoked_at.is_some()
        || accountable_agent_id.is_some_and(|lead| grant.recipient.agent_id != Some(AgentId(lead)))
        || grant.checkout_id != scope.checkout_id
        || grant.revision != scope.grant_revision
        || scope.expires_at > grant.expires_at
        || scope.expires_at <= now
    {
        return Ok(false);
    }
    match verify_active_recipient(tx, scope.workspace_id, grant.recipient, bound_user).await {
        Ok(Some(AgentRole::Lead)) => {
            access::authorize_approval(tx, scope, grant.recipient, bound_user).await
        }
        Ok(_) | Err(IdentityError::Forbidden) => Ok(false),
        Err(error) => Err(error),
    }
}

fn verify_grant_owner(
    context: WorkspaceContext,
    checkout: Uuid,
    grant: &CheckoutGrant,
) -> Result<(), IdentityError> {
    if grant.checkout_id != checkout
        || context.access.member != grant.recipient.user_id
        || context
            .agent_id()
            .is_some_and(|actor| Some(actor) != grant.recipient.agent_id)
    {
        return Err(IdentityError::Forbidden);
    }
    Ok(())
}

async fn verify_worker(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: &TaskRow,
    lead: Recipient,
    worker: Recipient,
) -> Result<AgentId, IdentityError> {
    let id = worker.agent_id.ok_or(IdentityError::InvalidInput)?;
    if worker.user_id.0 != task.accountable_user_id
        || worker.user_id != lead.user_id
        || task
            .accountable_agent_id
            .is_some_and(|agent| lead.agent_id != Some(AgentId(agent)))
        || task.assignee_agent_id.is_some_and(|agent| agent != id.0)
        || active_principal(tx, context, lead).await? != Some(AgentRole::Lead)
    {
        return Err(IdentityError::Forbidden);
    }
    if !matches!(
        active_principal(tx, context, worker).await?,
        Some(AgentRole::Lead | AgentRole::Worker)
    ) {
        return Err(IdentityError::Forbidden);
    }
    if let Some(grant) = task.delegation_grant_id {
        let grantee = sqlx::query_scalar::<_, Uuid>(
            "SELECT grantee_agent_id FROM delegation_grants WHERE workspace_id=$1 AND id=$2",
        )
        .bind(task.workspace_id)
        .bind(grant)
        .fetch_optional(&mut **tx)
        .await?;
        if grantee != Some(id.0) {
            return Err(IdentityError::Forbidden);
        }
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
    Ok(id)
}

async fn add_scope(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    input: &SetTaskScope,
    grant: &CheckoutGrant,
    worker: AgentId,
    id: Uuid,
    host_owner: UserId,
) -> Result<(), IdentityError> {
    let mut shares = vec![input.worker];
    if grant.recipient != input.worker {
        shares.push(grant.recipient);
    }
    if host_owner != context.access.member {
        shares.push(Recipient {
            user_id: host_owner,
            agent_id: None,
        });
    }
    let grants: Vec<_> = shares
        .into_iter()
        .map(|recipient| AclGrant {
            user_id: recipient.user_id,
            agent_id: recipient.agent_id,
            right: GrantRight::Read,
        })
        .collect();
    create_scope(tx, context, id, Visibility::ExplicitAcl, &grants).await?;
    sqlx::query("INSERT INTO work_scopes (id,workspace_id,checkout_id,task_id,worker_user_id,worker_agent_id)
      VALUES ($1,$2,$3,$4,$5,$6)").bind(id).bind(input.workspace_id.0).bind(input.checkout_id)
        .bind(input.task_id.0).bind(input.worker.user_id.0).bind(worker.0).execute(&mut **tx).await?;
    Ok(())
}

async fn db_now(tx: &mut Transaction<'_, Postgres>) -> Result<DateTime<Utc>, IdentityError> {
    Ok(sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?)
}
