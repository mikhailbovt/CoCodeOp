use chrono::{DateTime, Duration, Utc};
use office_domain::{
    Action, Actor, AgentId, AgentRole, BearerDigest, GrantRight, Resource, RunId, RunStatus,
    TaskAuthority, TaskId, TaskStatus, UserId, Visibility, WorkPermissions, WorkspaceId,
};
use office_protocol::{
    AccessApproval, AccessDecision, AccessRequest, AccessRequestReceipt, AccessRequestState,
    AclGrant, Area, AreaRevision, CheckoutGrant, GetAccessRequest, Recipient, RequestAccess,
    RespondAccess, WorkScope, WorkScopeDefinition,
};
use serde_json::json;
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use super::{ScopeRow, db_now, require_read_paths, revocations::verify_fresh_grant, select_scope};
use crate::{
    Store,
    checkouts::{
        CheckoutRow,
        areas::required_area_approvals,
        grants::{bind_grant_validation, checkout_grant, verify_commands},
        load_checkout, lock_policy,
    },
    data_scope::{authorize_scope, create_scope, subject},
    decisions::current_decision_covers_access,
    events::{Idempotency, append_event, append_workspace_event},
    identity::{
        IdentityError, WorkspaceContext, active_principal, require_permission,
        verify_active_recipient, workspace_context,
    },
    inbox::save_cursor,
    tasks::{
        TaskRow, execution::start_effects::credentials::authenticate_run, load_task, runs,
        task_text,
    },
};

#[derive(sqlx::FromRow)]
struct RequestRow {
    id: Uuid,
    workspace_id: Uuid,
    checkout_id: Uuid,
    task_id: Uuid,
    scope_id: Uuid,
    base_scope_revision: i64,
    run_id: Option<Uuid>,
    requester_user_id: Uuid,
    requester_agent_id: Option<Uuid>,
    worker_user_id: Uuid,
    worker_agent_id: Uuid,
    lead_user_id: Uuid,
    lead_agent_id: Uuid,
    host_owner_user_id: Uuid,
    effective_grant_id: Uuid,
    needs_owner_grant: bool,
    reason: String,
    decision_id: Option<Uuid>,
    permissions: Json<WorkPermissions>,
    approvers: Json<Vec<Recipient>>,
    approvals: Json<Vec<AccessApproval>>,
    area_revisions: Json<Vec<AreaRevision>>,
    state: String,
    revision: i64,
    expires_at: DateTime<Utc>,
    scope_expires_at: DateTime<Utc>,
    issued_scope_revision: Option<i64>,
    expired: bool,
}

const REQUEST_FIELDS: &str = "*, expires_at <= clock_timestamp() AS expired";

impl RequestRow {
    fn requester(&self) -> Recipient {
        recipient(self.requester_user_id, self.requester_agent_id)
    }
    fn worker(&self) -> Recipient {
        recipient(self.worker_user_id, Some(self.worker_agent_id))
    }
    fn lead(&self) -> Recipient {
        recipient(self.lead_user_id, Some(self.lead_agent_id))
    }

    fn state(&self) -> Result<AccessRequestState, IdentityError> {
        match self.state.as_str() {
            "requested" | "approved_pending_apply" if self.expired => {
                Ok(AccessRequestState::Expired)
            }
            "requested" => Ok(AccessRequestState::Requested),
            "approved_pending_apply" => Ok(AccessRequestState::ApprovedPendingApply),
            "approved" => Ok(AccessRequestState::Approved),
            "rejected" => Ok(AccessRequestState::Rejected),
            "cancelled" => Ok(AccessRequestState::Cancelled),
            "expired" => Ok(AccessRequestState::Expired),
            _ => Err(IdentityError::InvalidRole),
        }
    }

    fn into_request(self) -> Result<AccessRequest, IdentityError> {
        Ok(AccessRequest {
            access_request_id: self.id,
            workspace_id: WorkspaceId(self.workspace_id),
            checkout_id: self.checkout_id,
            task_id: TaskId(self.task_id),
            scope_id: self.scope_id,
            run_id: self.run_id.map(RunId),
            base_scope_revision: self.base_scope_revision,
            requester: self.requester(),
            worker: self.worker(),
            lead: self.lead(),
            state: self.state()?,
            reason: self.reason,
            decision_id: self.decision_id,
            permissions: self.permissions.0,
            revision: self.revision,
            expires_at: self.expires_at,
            scope_expires_at: self.scope_expires_at,
            approvers: self.approvers.0,
            approvals: self.approvals.0,
            area_revisions: self.area_revisions.0,
            needs_owner_grant: self.needs_owner_grant,
            effective_grant_id: self.effective_grant_id,
            issued_scope_revision: self.issued_scope_revision,
        })
    }
}

impl Store {
    pub async fn request_access(
        &self,
        digest: &BearerDigest,
        input: RequestAccess,
    ) -> Result<AccessRequestReceipt, IdentityError> {
        self.request_access_as(digest, input, false).await
    }

    pub async fn request_run_access(
        &self,
        digest: &BearerDigest,
        input: RequestAccess,
    ) -> Result<AccessRequestReceipt, IdentityError> {
        self.request_access_as(digest, input, true).await
    }

    async fn request_access_as(
        &self,
        digest: &BearerDigest,
        input: RequestAccess,
        managed: bool,
    ) -> Result<AccessRequestReceipt, IdentityError> {
        let reason = task_text(&input.reason, 2048)?;
        if !input.permissions.is_valid() || !(1..i64::MAX).contains(&input.expected_scope_revision)
        {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let (context, bound_run) = if managed {
            let access = authenticate_run(&mut tx, digest).await?;
            if !access.gateway_required
                || access.context.access.workspace != input.workspace_id
                || access.task.id != input.task_id.0
            {
                return Err(IdentityError::Forbidden);
            }
            (access.context, Some(access.run_id))
        } else {
            (
                workspace_context(&mut tx, digest, input.workspace_id).await?,
                None,
            )
        };
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "access.request",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let (task, _) = load_task(&mut tx, context, input.task_id, true).await?;
        if task.revision != input.expected_task_revision {
            return Err(IdentityError::RevisionMismatch {
                current: task.revision,
            });
        }
        continue_task(&task)?;
        let checkout_id = task
            .checkout_id
            .ok_or(IdentityError::AccessRequestRequired)?;
        lock_policy(&mut tx, checkout_id).await?;
        let base = select_scope(&mut tx, input.workspace_id, input.task_id, None, true)
            .await?
            .ok_or(IdentityError::AccessRequestRequired)?;
        if base.revision != input.expected_scope_revision {
            return Err(IdentityError::RevisionMismatch {
                current: base.revision,
            });
        }
        if context.recipient() != base.worker() {
            require_permission(
                context.actor,
                Action::ManageTask,
                Resource::Task(TaskAuthority {
                    workspace: context.access,
                    accountable_owner: UserId(task.accountable_user_id),
                    accountable: subject(
                        UserId(task.accountable_user_id),
                        task.accountable_agent_id,
                    ),
                    assignee: None,
                    executor: None,
                    explicit_reviewer: None,
                }),
            )?;
        }
        let checkout = access_checkout(&mut tx, input.workspace_id, checkout_id).await?;
        if !input.permissions.within_manifest(&checkout.manifest.0) {
            return Err(IdentityError::AccessRequestRequired);
        }
        let now = db_now(&mut tx).await?;
        if input.expires_at <= now
            || input.expires_at > now + Duration::hours(24)
            || input.scope_expires_at <= now
            || input.scope_expires_at > now + Duration::days(30)
        {
            return Err(IdentityError::InvalidInput);
        }
        let grant = read_grant(&mut tx, input.workspace_id, base.grant_id).await?;
        let lead = grant.recipient;
        ensure_current_lead(&task, lead)?;
        let effective = grant.permissions.narrow(&input.permissions);
        let needs_owner_grant = effective.is_none()
            || grant.revoked_at.is_some()
            || input.scope_expires_at > grant.expires_at;
        let required = required_area_approvals(
            &mut tx,
            input.workspace_id,
            checkout_id,
            lead,
            if needs_owner_grant {
                &input.permissions
            } else {
                effective
                    .as_ref()
                    .ok_or(IdentityError::AccessRequestRequired)?
            },
        )
        .await?;
        if !current_decision_covers_access(
            &mut tx,
            input.workspace_id,
            &required,
            input.decision_id,
        )
        .await?
        {
            return Err(IdentityError::AccessRequestRequired);
        }
        let host_owner = recipient(checkout.owner_user_id, None);
        let approvers = required_people(lead, host_owner, needs_owner_grant, &required)?;
        let worker = base.worker();
        let active = active_run(&mut tx, input.workspace_id, input.task_id).await?;
        if let Some(run) = bound_run
            && (active != Some(run)
                || context.recipient() != worker
                || authenticate_run(&mut tx, digest).await?.run_id != run)
        {
            return Err(IdentityError::RunLeaseStale);
        }
        let id = Uuid::new_v4();
        let grants = request_acl(&approvers, worker, host_owner);
        create_scope(&mut tx, context, id, Visibility::ExplicitAcl, &grants).await?;
        let row = sqlx::query_as::<_, RequestRow>(&format!(
            "INSERT INTO access_requests (id, workspace_id, checkout_id, task_id, scope_id, base_scope_revision,
                run_id, requester_user_id, requester_agent_id, worker_user_id, worker_agent_id, lead_user_id, lead_agent_id,
                host_owner_user_id, effective_grant_id, needs_owner_grant, reason, permissions, approvers, area_revisions, expires_at, scope_expires_at, decision_id)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23) RETURNING {REQUEST_FIELDS}",
        )).bind(id).bind(input.workspace_id.0).bind(checkout_id).bind(input.task_id.0).bind(base.id).bind(base.revision)
          .bind(active.map(|run| run.0)).bind(context.access.member.0).bind(context.agent_id().map(|id|id.0))
          .bind(worker.user_id.0).bind(worker.agent_id.map(|id|id.0)).bind(lead.user_id.0).bind(lead.agent_id.map(|id|id.0))
          .bind(checkout.owner_user_id).bind(grant.grant_id).bind(needs_owner_grant).bind(reason).bind(Json(input.permissions))
          .bind(Json(approvers)).bind(Json(versions(&required))).bind(input.expires_at).bind(input.scope_expires_at)
          .bind(input.decision_id)
          .fetch_one(&mut *tx).await?;
        let answer = record(
            &mut tx,
            context,
            &idem,
            row.into_request()?,
            None,
            "access.requested",
        )
        .await?;
        tx.commit().await?;
        Ok(answer)
    }

    pub async fn get_access_request(
        &self,
        digest: &BearerDigest,
        input: GetAccessRequest,
    ) -> Result<AccessRequest, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        authorize_scope(&mut tx, context, input.access_request_id, Action::ReadData).await?;
        let request = load_request(&mut tx, input.workspace_id, input.access_request_id)
            .await?
            .into_request()?;
        tx.commit().await?;
        Ok(request)
    }

    pub async fn get_run_access_request(
        &self,
        digest: &BearerDigest,
        input: GetAccessRequest,
    ) -> Result<AccessRequest, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let access = authenticate_run(&mut tx, digest).await?;
        if !access.gateway_required || access.context.access.workspace != input.workspace_id {
            return Err(IdentityError::Forbidden);
        }
        authorize_scope(
            &mut tx,
            access.context,
            input.access_request_id,
            Action::ReadData,
        )
        .await?;
        let request = load_request(&mut tx, input.workspace_id, input.access_request_id)
            .await?
            .into_request()?;
        if request.task_id.0 != access.task.id || request.worker != access.context.recipient() {
            return Err(IdentityError::Forbidden);
        }
        tx.commit().await?;
        Ok(request)
    }

    pub async fn respond_access(
        &self,
        digest: &BearerDigest,
        input: RespondAccess,
    ) -> Result<AccessRequestReceipt, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "access.respond",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        authorize_scope(&mut tx, context, input.access_request_id, Action::ReadData).await?;
        let before = load_request(&mut tx, input.workspace_id, input.access_request_id).await?;
        lock_policy(&mut tx, before.checkout_id).await?;
        let mut row = load_request(&mut tx, input.workspace_id, input.access_request_id).await?;
        if row.revision != input.expected_revision {
            return Err(IdentityError::RevisionMismatch {
                current: row.revision,
            });
        }
        let state = row.state()?;
        if state != AccessRequestState::Requested
            && !(state == AccessRequestState::ApprovedPendingApply
                && input.decision == AccessDecision::Cancel)
        {
            return Err(IdentityError::InvalidTransition);
        }
        let approver = match input.decision {
            AccessDecision::Cancel => {
                if context.recipient() != row.requester()
                    && context.actor != Actor::Human(UserId(row.requester_user_id))
                {
                    return Err(IdentityError::Forbidden);
                }
                if input.as_approver.is_some() || input.replacement_grant_id.is_some() {
                    return Err(IdentityError::InvalidInput);
                }
                None
            }
            AccessDecision::Approve | AccessDecision::Reject => {
                let target = input.as_approver.unwrap_or(context.recipient());
                if !row.approvers.0.contains(&target) {
                    return Err(IdentityError::Forbidden);
                }
                require_permission(
                    context.actor,
                    Action::DecideOffer,
                    Resource::DirectedOffer {
                        workspace: context.access,
                        recipient_owner: target.user_id,
                        recipient_agent: target.agent_id,
                    },
                )?;
                active_principal(&mut tx, context, target).await?;
                if row
                    .approvals
                    .0
                    .iter()
                    .any(|decision| decision.approver == target)
                {
                    return Err(IdentityError::InvalidTransition);
                }
                Some(target)
            }
        };
        bind_request(&mut tx, Some(row.id)).await?;
        let mut staged = None;
        let mut state = match input.decision {
            AccessDecision::Cancel => "cancelled",
            AccessDecision::Reject => "rejected",
            AccessDecision::Approve => "requested",
        };
        if let Some(replacement) = input.replacement_grant_id {
            if input.decision != AccessDecision::Approve
                || !row.needs_owner_grant
                || context.actor != Actor::Human(UserId(row.host_owner_user_id))
                || approver != Some(recipient(row.host_owner_user_id, None))
            {
                return Err(IdentityError::Forbidden);
            }
            let grant = read_grant(&mut tx, input.workspace_id, replacement).await?;
            selected_permissions(&row, &grant, db_now(&mut tx).await?)?;
            row.effective_grant_id = replacement;
        } else if row.needs_owner_grant
            && input.decision == AccessDecision::Approve
            && approver == Some(recipient(row.host_owner_user_id, None))
        {
            return Err(IdentityError::AccessRequestRequired);
        }
        if input.decision == AccessDecision::Approve {
            let approver = approver.ok_or(IdentityError::Forbidden)?;
            row.approvals.0.push(AccessApproval {
                approver,
                decided_by: context.recipient(),
                decided_at: db_now(&mut tx).await?,
            });
            if row.approvals.0.len() == row.approvers.0.len() {
                let new_scope = stage_approved_scope(&mut tx, context, &row).await?;
                row.issued_scope_revision = Some(new_scope.definition.revision);
                staged = Some(new_scope);
                state = "approved_pending_apply";
            }
        }
        let updated = sqlx::query_as::<_, RequestRow>(&format!(
            "UPDATE access_requests SET state=$3, approvals=$4, effective_grant_id=$5, issued_scope_revision=$6,
                decision_by_user_id=$7, decision_by_agent_id=$8, decided_at=clock_timestamp(), revision=revision+1
             WHERE workspace_id=$1 AND id=$2 RETURNING {REQUEST_FIELDS}",
        )).bind(input.workspace_id.0).bind(row.id).bind(state).bind(row.approvals).bind(row.effective_grant_id)
          .bind(row.issued_scope_revision).bind(context.access.member.0).bind(context.agent_id().map(|id|id.0))
          .fetch_one(&mut *tx).await?;
        bind_request(&mut tx, None).await?;
        let event = if state == "requested" {
            "access.approval_recorded"
        } else {
            "access.resolved"
        };
        let result = record(
            &mut tx,
            context,
            &idem,
            updated.into_request()?,
            staged,
            event,
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }
}

pub(super) async fn authorize_approval(
    tx: &mut Transaction<'_, Postgres>,
    scope: &WorkScopeDefinition,
    lead: Recipient,
    bound_user: UserId,
) -> Result<bool, IdentityError> {
    let required = required_area_approvals(
        tx,
        scope.workspace_id,
        scope.checkout_id,
        lead,
        &scope.permissions,
    )
    .await?;
    let Some(id) = scope.access_request_id else {
        return Ok(required.is_empty());
    };
    let row = load_request(tx, scope.workspace_id, id).await?;
    if !matches!(
        row.state()?,
        AccessRequestState::Approved | AccessRequestState::ApprovedPendingApply
    ) || row.lead() != lead
        || row.scope_id != scope.scope_id
        || row.issued_scope_revision != Some(scope.revision)
        || row.effective_grant_id != scope.grant_id
        || row.area_revisions.0 != versions(&required)
        || !current_decision_covers_access(tx, scope.workspace_id, &required, row.decision_id)
            .await?
    {
        return Ok(false);
    }
    for approver in &row.approvers.0 {
        if !row
            .approvals
            .0
            .iter()
            .any(|vote| vote.approver == *approver)
        {
            return Ok(false);
        }
        match verify_active_recipient(tx, scope.workspace_id, *approver, bound_user).await {
            Ok(Some(AgentRole::Lead) | None) => {}
            Ok(Some(_)) | Err(IdentityError::Forbidden) => return Ok(false),
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}

pub(super) async fn complete_activation(
    tx: &mut Transaction<'_, Postgres>,
    scope: &WorkScopeDefinition,
    allowed: bool,
) -> Result<(), IdentityError> {
    let Some(id) = scope.access_request_id else {
        return Ok(());
    };
    let changed = sqlx::query_scalar::<_, String>(
        "UPDATE access_requests SET state=CASE WHEN $4 THEN 'approved' WHEN expires_at <= clock_timestamp() THEN 'expired' ELSE 'rejected' END,
            revision=revision+1 WHERE workspace_id=$1 AND id=$2 AND issued_scope_revision=$3 AND state='approved_pending_apply'
         RETURNING state",
    ).bind(scope.workspace_id.0).bind(id).bind(scope.revision).bind(allowed).fetch_optional(&mut **tx).await?;
    if let Some(state) = changed {
        append_workspace_event(tx, scope.workspace_id, id, "access.resolved", json!({"access_request_id": id,
            "task_id": scope.task_id, "scope_id": scope.scope_id, "scope_revision": scope.revision, "state": state}), None).await?;
    }
    Ok(())
}

async fn stage_approved_scope(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    request: &RequestRow,
) -> Result<WorkScope, IdentityError> {
    let workspace = context.access.workspace;
    let id = TaskId(request.task_id);
    let row = sqlx::query_as::<_, TaskRow>("SELECT * FROM tasks WHERE workspace_id=$1 AND id=$2")
        .bind(workspace.0)
        .bind(id.0)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(IdentityError::Forbidden)?;
    continue_task(&row)?;
    ensure_current_lead(&row, request.lead())?;
    if request.run_id.is_some()
        && active_run(tx, workspace, id).await?.map(|run| run.0) != request.run_id
    {
        return Err(IdentityError::InvalidTransition);
    }
    let current = select_scope(tx, workspace, id, None, true)
        .await?
        .ok_or(IdentityError::AccessRequestRequired)?;
    ensure_scope_unchanged(request, &current)?;
    let latest = select_scope(tx, workspace, id, None, false)
        .await?
        .ok_or(IdentityError::AccessRequestRequired)?;
    if latest.state == "pending_activation" {
        return Err(IdentityError::RevisionMismatch {
            current: latest.revision,
        });
    }
    let grant = read_grant(tx, workspace, request.effective_grant_id).await?;
    verify_fresh_grant(tx, workspace, request.scope_id, grant.grant_id).await?;
    let permissions = selected_permissions(request, &grant, db_now(tx).await?)?;
    let checkout = access_checkout(tx, workspace, request.checkout_id).await?;
    if !permissions.within_manifest(&checkout.manifest.0) {
        return Err(IdentityError::AccessRequestRequired);
    }
    require_read_paths(
        &checkout.manifest.0,
        &permissions,
        &current.required_read_paths.0,
    )?;
    let required = required_area_approvals(
        tx,
        workspace,
        request.checkout_id,
        request.lead(),
        &permissions,
    )
    .await?;
    if request.area_revisions.0 != versions(&required)
        || !current_decision_covers_access(tx, workspace, &required, request.decision_id).await?
    {
        return Err(IdentityError::RevisionConflict);
    }
    for person in &request.approvers.0 {
        active_principal(tx, context, *person).await?;
    }
    bind_grant_validation(tx, Some(grant.grant_id)).await?;
    verify_commands(tx, context, &checkout, &permissions).await?;
    bind_grant_validation(tx, None).await?;
    let revision = latest
        .revision
        .checked_add(1)
        .ok_or(IdentityError::InvalidTransition)?;
    sqlx::query("INSERT INTO work_scope_revisions (workspace_id,checkout_id,scope_id,revision,grant_id,grant_revision,access_request_id,expires_at,permissions,required_read_paths)
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
        .bind(workspace.0).bind(request.checkout_id).bind(request.scope_id).bind(revision).bind(grant.grant_id).bind(grant.revision)
        .bind(request.id).bind(request.scope_expires_at).bind(Json(permissions)).bind(current.required_read_paths).execute(&mut **tx).await?;
    select_scope(tx, workspace, id, Some(revision), false)
        .await?
        .ok_or(IdentityError::InvalidTransition)?
        .into_scope()
}

fn selected_permissions(
    request: &RequestRow,
    grant: &CheckoutGrant,
    now: DateTime<Utc>,
) -> Result<WorkPermissions, IdentityError> {
    if grant.checkout_id != request.checkout_id
        || grant.owner_user_id.0 != request.host_owner_user_id
        || grant.recipient != request.lead()
    {
        return Err(IdentityError::Forbidden);
    }
    if grant.revoked_at.is_some()
        || request.scope_expires_at > grant.expires_at
        || request.scope_expires_at <= now
    {
        return Err(IdentityError::AccessRequestRequired);
    }
    grant
        .permissions
        .narrow(&request.permissions.0)
        .ok_or(IdentityError::AccessRequestRequired)
}

fn ensure_scope_unchanged(request: &RequestRow, current: &ScopeRow) -> Result<(), IdentityError> {
    if current.id != request.scope_id
        || current.checkout_id != request.checkout_id
        || current.worker() != request.worker()
        || current.revision != request.base_scope_revision
        || current.state != "active"
    {
        return Err(IdentityError::RevisionMismatch {
            current: current.revision,
        });
    }
    Ok(())
}

fn ensure_current_lead(task: &TaskRow, lead: Recipient) -> Result<(), IdentityError> {
    if task.accountable_user_id != lead.user_id.0
        || task
            .accountable_agent_id
            .is_some_and(|id| lead.agent_id != Some(AgentId(id)))
    {
        return Err(IdentityError::Forbidden);
    }
    Ok(())
}

fn continue_task(task: &TaskRow) -> Result<(), IdentityError> {
    if task.awaiting_owner
        || matches!(
            task.status()?,
            TaskStatus::Done | TaskStatus::Cancelled | TaskStatus::Failed
        )
    {
        return Err(IdentityError::InvalidTransition);
    }
    Ok(())
}

async fn active_run(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    task: TaskId,
) -> Result<Option<RunId>, IdentityError> {
    Ok(runs::latest(tx, workspace, task)
        .await?
        .filter(|run| matches!(run.status, RunStatus::Running | RunStatus::WaitingInput))
        .map(|run| run.run_id))
}

fn required_people(
    lead: Recipient,
    host_owner: Recipient,
    needs_owner_grant: bool,
    areas: &[Area],
) -> Result<Vec<Recipient>, IdentityError> {
    let mut approvers = vec![lead];
    for area in areas {
        for approver in area.policy.approvers() {
            if !approvers.contains(approver) {
                approvers.push(*approver);
            }
        }
    }
    if needs_owner_grant && !approvers.contains(&host_owner) {
        approvers.push(host_owner);
    }
    if approvers.len() > 14 {
        return Err(IdentityError::InvalidInput);
    }
    Ok(approvers)
}

fn request_acl(approvers: &[Recipient], worker: Recipient, host_owner: Recipient) -> Vec<AclGrant> {
    let mut people = vec![worker, host_owner];
    for person in approvers {
        for candidate in [*person, recipient(person.user_id.0, None)] {
            if !people.contains(&candidate) {
                people.push(candidate);
            }
        }
    }
    people
        .into_iter()
        .map(|person| AclGrant {
            user_id: person.user_id,
            agent_id: person.agent_id,
            right: GrantRight::Read,
        })
        .collect()
}

fn versions(areas: &[Area]) -> Vec<AreaRevision> {
    areas
        .iter()
        .map(|area| AreaRevision {
            area_id: area.area_id,
            revision: area.revision,
        })
        .collect()
}

fn recipient(user: Uuid, agent: Option<Uuid>) -> Recipient {
    Recipient {
        user_id: UserId(user),
        agent_id: agent.map(AgentId),
    }
}

pub(super) async fn access_checkout(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
) -> Result<CheckoutRow, IdentityError> {
    sqlx::query("SELECT set_config('office.access_checkout_context',$1,true)")
        .bind(checkout.to_string())
        .execute(&mut **tx)
        .await?;
    let row = load_checkout(tx, workspace, checkout, false).await?;
    sqlx::query("SELECT set_config('office.access_checkout_context','',true)")
        .execute(&mut **tx)
        .await?;
    Ok(row)
}

async fn read_grant(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    grant: Uuid,
) -> Result<CheckoutGrant, IdentityError> {
    bind_grant_validation(tx, Some(grant)).await?;
    let answer = checkout_grant(tx, workspace, grant).await?;
    bind_grant_validation(tx, None).await?;
    Ok(answer)
}

async fn bind_request(
    tx: &mut Transaction<'_, Postgres>,
    id: Option<Uuid>,
) -> Result<(), IdentityError> {
    sqlx::query("SELECT set_config('office.access_request_context',$1,true)")
        .bind(id.map_or_else(String::new, |id| id.to_string()))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn load_request(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    access: Uuid,
) -> Result<RequestRow, IdentityError> {
    sqlx::query_as::<_, RequestRow>(&format!(
        "SELECT {REQUEST_FIELDS} FROM access_requests WHERE workspace_id=$1 AND id=$2"
    ))
    .bind(workspace.0)
    .bind(access)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(IdentityError::Forbidden)
}

async fn record(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    idem: &Idempotency,
    request: AccessRequest,
    work_scope: Option<WorkScope>,
    event: &str,
) -> Result<AccessRequestReceipt, IdentityError> {
    let id = request.access_request_id;
    let seq = append_event(tx, context, id, event, json!({"access_request_id": id, "task_id": request.task_id,
        "scope_id": request.scope_id, "base_scope_revision": request.base_scope_revision, "state": request.state,
        "revision": request.revision, "issued_scope_revision": request.issued_scope_revision}), None).await?;
    let answer = AccessRequestReceipt {
        request,
        work_scope,
        event_cursor: save_cursor(tx, context, seq).await?,
    };
    idem.record(tx, id, &answer).await?;
    Ok(answer)
}
