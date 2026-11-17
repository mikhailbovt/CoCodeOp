use chrono::{DateTime, Duration, Utc};
use office_domain::{
    Action, AgentId, BearerDigest, Resource, TaskId, TaskStatus, TeamId, TeamRole, TeamState,
    UserId, Visibility, WorkspaceId,
};
use office_protocol::{
    CreateRuntimeTemplate, DelegateTask, DelegationGrant, DelegationLimits, DelegationReceipt,
    GetDelegation, IssueDelegation, Recipient, RevokeDelegation, RuntimeKind, RuntimeTemplate,
    TaskChange,
};
use serde_json::json;
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use super::{TaskRow, load_task, scopes::has_work_scope, task_criteria, task_text};
use crate::{
    Store,
    data_scope::create_scope,
    events::{Idempotency, append_event},
    identity::{
        IdentityError, WorkspaceContext, require_permission, valid_name, workspace_context,
    },
    inbox::save_cursor,
    teams::{bind_team, load_team, lock_team_task_creation, member_role},
};

#[derive(sqlx::FromRow)]
struct GrantRow {
    id: Uuid,
    workspace_id: Uuid,
    team_id: Uuid,
    owner_user_id: Uuid,
    task_id: Uuid,
    parent_grant_id: Option<Uuid>,
    issuer_agent_id: Option<Uuid>,
    grantee_agent_id: Uuid,
    lead_generation: i64,
    remaining_depth: i16,
    expires_at: DateTime<Utc>,
    max_attempts: i16,
    max_subtasks: i16,
    attempts_used: i16,
    subtasks_used: i16,
    revision: i64,
    revoked_at: Option<DateTime<Utc>>,
    runtime_template_ids: Vec<Uuid>,
}

impl GrantRow {
    fn into_grant(self) -> DelegationGrant {
        DelegationGrant {
            grant_id: self.id,
            workspace_id: WorkspaceId(self.workspace_id),
            team_id: TeamId(self.team_id),
            owner_user_id: UserId(self.owner_user_id),
            task_id: TaskId(self.task_id),
            parent_grant_id: self.parent_grant_id,
            issuer_agent_id: self.issuer_agent_id.map(AgentId),
            grantee_agent_id: AgentId(self.grantee_agent_id),
            limits: DelegationLimits {
                remaining_depth: self.remaining_depth as u8,
                expires_at: self.expires_at,
                max_attempts: self.max_attempts as u16,
                max_subtasks: self.max_subtasks as u16,
                runtime_template_ids: self.runtime_template_ids,
            },
            attempts_used: self.attempts_used as u16,
            subtasks_used: self.subtasks_used as u16,
            revision: self.revision,
            revoked_at: self.revoked_at,
        }
    }

    fn authority(&self, context: WorkspaceContext) -> Resource {
        Resource::Delegation {
            workspace: context.access,
            owner: UserId(self.owner_user_id),
            issuer: self.issuer_agent_id.map(AgentId),
            grantee: AgentId(self.grantee_agent_id),
        }
    }
}

const TEMPLATE_IDS: &str = "ARRAY (SELECT template_id FROM delegation_templates dt
    WHERE dt.workspace_id = g.workspace_id AND dt.grant_id = g.id ORDER BY template_id) AS runtime_template_ids";

impl Store {
    pub async fn create_runtime_template(
        &self,
        digest: &BearerDigest,
        input: CreateRuntimeTemplate,
    ) -> Result<RuntimeTemplate, IdentityError> {
        let name = valid_name(&input.name)?;
        let kind = match (input.kind, &input.managed) {
            (RuntimeKind::Interactive, None) => "interactive",
            (RuntimeKind::Managed, Some(config)) if config.is_valid() => "managed",
            _ => return Err(IdentityError::InvalidInput),
        };
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        require_permission(
            context.actor,
            Action::CreateRuntimeTemplate,
            Resource::Account(context.access.member),
        )?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "runtime_template.create",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let id = Uuid::new_v4();
        create_scope(&mut tx, context, id, Visibility::OwnerPrivate, &[]).await?;
        let created_at = sqlx::query_scalar::<_, DateTime<Utc>>(
            "INSERT INTO runtime_templates (id, workspace_id, owner_user_id, name, kind, managed)
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING created_at",
        )
        .bind(id)
        .bind(input.workspace_id.0)
        .bind(context.access.member.0)
        .bind(name)
        .bind(kind)
        .bind(input.managed.as_ref().map(Json))
        .fetch_one(&mut *tx)
        .await?;
        let sequence = append_event(
            &mut tx,
            context,
            id,
            "runtime_template.created",
            json!({"template_id": id, "kind": input.kind}),
            None,
        )
        .await?;
        let result = RuntimeTemplate {
            template_id: id,
            workspace_id: input.workspace_id,
            owner_user_id: context.access.member,
            name: name.into(),
            kind: input.kind,
            managed: input.managed,
            created_at,
            event_cursor: save_cursor(&mut tx, context, sequence).await?,
        };
        idem.record(&mut tx, id, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn issue_delegation(
        &self,
        digest: &BearerDigest,
        input: IssueDelegation,
    ) -> Result<DelegationReceipt, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        require_permission(
            context.actor,
            Action::IssueDelegation,
            Resource::Account(context.access.member),
        )?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "delegation.issue",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let (task, scope) = load_task(&mut tx, context, input.task_id, true).await?;
        check_revision(task.revision, input.expected_revision)?;
        let team_id = TeamId(task.team_id.ok_or(IdentityError::InvalidInput)?);
        let team = load_team(&mut tx, context, team_id, Action::ManageTeam, false).await?;
        if task.accountable_user_id != context.access.member.0
            || team.owner_user_id != context.access.member
            || team.lead_agent_id != Some(input.lead_agent_id)
        {
            return Err(IdentityError::Forbidden);
        }
        if task.delegation_grant_id.is_some()
            || task.awaiting_owner
            || team.state != TeamState::Active
            || !matches!(task.status()?, TaskStatus::Draft | TaskStatus::Ready)
            || !matches!(
                scope.visibility,
                Visibility::Workspace | Visibility::Team { .. }
            )
        {
            return Err(IdentityError::InvalidTransition);
        }
        lock_lineage(&mut tx, input.workspace_id).await?;
        validate_limits(&mut tx, &input.limits).await?;
        verify_templates(&mut tx, context, &input.limits.runtime_template_ids).await?;
        bind_team(&mut tx, team_id, false).await?;
        require_executor(&mut tx, context, team_id, input.lead_agent_id).await?;
        let generation = lead_generation(&mut tx, input.workspace_id, team_id).await?;
        let id = insert_grant(
            &mut tx,
            context,
            NewGrant {
                team_id,
                task_id: input.task_id,
                grantee: input.lead_agent_id,
                lead_generation: generation,
                parent: None,
                limits: &input.limits,
            },
        )
        .await?;
        let revision = sqlx::query_scalar::<_, i64>(
            "UPDATE tasks SET delegation_grant_id = $3, revision = revision + 1, updated_at = clock_timestamp()
             WHERE workspace_id = $1 AND id = $2 RETURNING revision",
        ).bind(input.workspace_id.0).bind(input.task_id.0).bind(id).fetch_one(&mut *tx).await?;
        let result = record(
            &mut tx,
            context,
            &idem,
            id,
            Some(revision),
            "delegation.issued",
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn delegate_task(
        &self,
        digest: &BearerDigest,
        input: DelegateTask,
    ) -> Result<DelegationReceipt, IdentityError> {
        let objective = task_text(&input.objective, 4096)?;
        let criteria = task_criteria(&input.acceptance_criteria)?;
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "delegation.delegate",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let grant = get_grant(&mut tx, input.workspace_id, input.grant_id).await?;
        require_permission(
            context.actor,
            Action::UseDelegation,
            grant.authority(context),
        )?;
        lock_team_task_creation(&mut tx, TeamId(grant.team_id), false).await?;
        let (parent, _) = load_task(&mut tx, context, TaskId(grant.task_id), true).await?;
        if parent.awaiting_owner
            || matches!(
                parent.status()?,
                TaskStatus::Done | TaskStatus::Cancelled | TaskStatus::Failed
            )
            || parent.work_scope_id.is_some()
            || has_work_scope(&mut tx, &parent).await?
        {
            return Err(IdentityError::InvalidTransition);
        }
        lock_lineage(&mut tx, input.workspace_id).await?;
        let lineage = active_lineage(&mut tx, context, input.grant_id).await?;
        let grant = &lineage[0];
        check_revision(grant.revision, input.expected_revision)?;
        validate_limits(&mut tx, &input.limits).await?;
        check_subset(grant, &input.limits)?;
        if lineage
            .iter()
            .any(|ancestor| ancestor.grantee_agent_id == input.worker_agent_id.0)
        {
            return Err(IdentityError::DelegationCycle);
        }
        let team_id = TeamId(grant.team_id);
        require_executor(&mut tx, context, team_id, input.worker_agent_id).await?;
        reserve(&mut tx, input.workspace_id, &lineage, Consumption::Subtask).await?;
        let child_task = TaskId(Uuid::new_v4());
        let child_grant = insert_grant(
            &mut tx,
            context,
            NewGrant {
                team_id,
                task_id: child_task,
                grantee: input.worker_agent_id,
                lead_generation: grant.lead_generation,
                parent: Some(grant.id),
                limits: &input.limits,
            },
        )
        .await?;
        create_scope(
            &mut tx,
            context,
            child_task.0,
            Visibility::Team { team_id },
            &[],
        )
        .await?;
        let status = if parent.execution_kind == "coding" {
            "draft"
        } else {
            "assigned"
        };
        let root = lineage.last().ok_or(IdentityError::DelegationInactive)?;
        let revision = sqlx::query_scalar::<_, i64>(
            "INSERT INTO tasks (id, workspace_id, team_id, parent_task_id, delegation_grant_id, accountable_user_id,
                      accountable_agent_id, assignee_user_id, assignee_agent_id, objective, criteria, status, execution_kind, checkout_id)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$6,$8,$9,$10,$11,$12,$13) RETURNING revision",
        ).bind(child_task.0).bind(input.workspace_id.0).bind(team_id.0).bind(parent.id).bind(child_grant)
         .bind(grant.owner_user_id).bind(root.grantee_agent_id)
         .bind(input.worker_agent_id.0).bind(objective).bind(Json(criteria)).bind(status)
         .bind(parent.execution_kind).bind(parent.checkout_id).fetch_one(&mut *tx).await?;
        let result = record(
            &mut tx,
            context,
            &idem,
            child_grant,
            Some(revision),
            "task.delegated",
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn get_delegation(
        &self,
        digest: &BearerDigest,
        input: GetDelegation,
    ) -> Result<DelegationGrant, IdentityError> {
        let mut tx = self.pool.begin().await?;
        workspace_context(&mut tx, digest, input.workspace_id).await?;
        let grant = get_grant(&mut tx, input.workspace_id, input.grant_id)
            .await?
            .into_grant();
        tx.commit().await?;
        Ok(grant)
    }

    pub async fn revoke_delegation(
        &self,
        digest: &BearerDigest,
        input: RevokeDelegation,
    ) -> Result<DelegationReceipt, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "delegation.revoke",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        lock_lineage(&mut tx, input.workspace_id).await?;
        let grant = get_grant(&mut tx, input.workspace_id, input.grant_id).await?;
        require_permission(
            context.actor,
            Action::RevokeDelegation,
            grant.authority(context),
        )?;
        check_revision(grant.revision, input.expected_revision)?;
        if grant.revoked_at.is_some() {
            return Err(IdentityError::InvalidTransition);
        }
        sqlx::query(
            "UPDATE delegation_grants SET revoked_at = clock_timestamp(), revision = revision + 1
                     WHERE workspace_id = $1 AND id = $2",
        )
        .bind(input.workspace_id.0)
        .bind(grant.id)
        .execute(&mut *tx)
        .await?;
        let result = record(
            &mut tx,
            context,
            &idem,
            grant.id,
            None,
            "delegation.revoked",
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }
}

pub(super) async fn check_task_command(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: &TaskRow,
    command: &TaskChange,
) -> Result<(), IdentityError> {
    let Some(grant) = task.delegation_grant_id else {
        if matches!(
            command,
            TaskChange::Claim {
                runtime_template_id: Some(_),
                ..
            }
        ) {
            return Err(IdentityError::InvalidInput);
        }
        return Ok(());
    };
    if matches!(
        command,
        TaskChange::Cancel { .. }
            | TaskChange::ResolveReviewEscalation { .. }
            | TaskChange::AcknowledgeCancel { .. }
            | TaskChange::Fail { .. }
            | TaskChange::Review { .. }
    ) {
        return Ok(());
    }
    let workspace = context.access.workspace;
    let lineage = lock_active_lineage(tx, context, grant).await?;
    let exact = &lineage[0];
    match command {
        TaskChange::Assign { agent_id } if agent_id.0 != exact.grantee_agent_id => {
            Err(IdentityError::Forbidden)
        }
        TaskChange::Claim {
            runtime_template_id,
            ..
        } => {
            require_permission(
                context.actor,
                Action::UseDelegation,
                exact.authority(context),
            )?;
            let id = runtime_template_id.ok_or(IdentityError::InvalidInput)?;
            if !lineage
                .iter()
                .all(|node| node.runtime_template_ids.contains(&id))
            {
                return Err(IdentityError::Forbidden);
            }
            let interactive = sqlx::query_scalar::<_, bool>("SELECT EXISTS
                (SELECT 1 FROM runtime_templates WHERE workspace_id=$1 AND owner_user_id=$2 AND id=$3 AND kind='interactive')")
                .bind(workspace.0).bind(context.access.member.0).bind(id).fetch_one(&mut **tx).await?;
            if !interactive {
                return Err(IdentityError::InvalidTransition);
            }
            reserve(tx, workspace, &lineage, Consumption::Attempt).await
        }
        _ => Ok(()),
    }
}

pub(super) async fn require_active_task_grant(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: &TaskRow,
) -> Result<(), IdentityError> {
    if let Some(grant) = task.delegation_grant_id {
        lock_active_lineage(tx, context, grant).await?;
    }
    Ok(())
}

pub(super) enum ManagedAttempt {
    Check,
    Reserve,
    Reserved,
}

pub(super) async fn verify_execution_grant(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: &TaskRow,
    worker: AgentId,
    template: Uuid,
    admission: ManagedAttempt,
) -> Result<(), IdentityError> {
    let Some(grant) = task.delegation_grant_id else {
        return Ok(());
    };
    let lineage = lock_active_lineage(tx, context, grant).await?;
    let endpoint = &lineage[0];
    if endpoint.grantee_agent_id != worker.0
        || !lineage
            .iter()
            .all(|node| node.runtime_template_ids.contains(&template))
    {
        return Err(IdentityError::Forbidden);
    }
    if !matches!(admission, ManagedAttempt::Reserved)
        && lineage
            .iter()
            .any(|node| node.attempts_used >= node.max_attempts)
    {
        return Err(IdentityError::DelegationLimit);
    }
    if matches!(admission, ManagedAttempt::Reserve) {
        reserve(tx, context.access.workspace, &lineage, Consumption::Attempt).await?;
    }
    Ok(())
}

async fn lock_active_lineage(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    grant: Uuid,
) -> Result<Vec<GrantRow>, IdentityError> {
    lock_lineage(tx, context.access.workspace).await?;
    active_lineage(tx, context, grant).await
}

async fn check_active_team(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    root: &GrantRow,
) -> Result<(), IdentityError> {
    let id = TeamId(root.team_id);
    let team = load_team(tx, context, id, Action::ReadTeam, false).await?;
    if team.state != TeamState::Active
        || team.owner_user_id.0 != root.owner_user_id
        || context.access.member != team.owner_user_id
        || team.lead_agent_id != Some(AgentId(root.grantee_agent_id))
        || lead_generation(tx, context.access.workspace, id).await? != root.lead_generation
    {
        return Err(IdentityError::DelegationInactive);
    }
    bind_team(tx, id, false).await?;
    Ok(())
}

async fn active_lineage(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    id: Uuid,
) -> Result<Vec<GrantRow>, IdentityError> {
    let lineage = sqlx::query_as::<_, GrantRow>(&format!(
        "WITH RECURSIVE lineage AS (
            SELECT delegation_grants.*, 1 AS hop FROM delegation_grants WHERE workspace_id = $1 AND id = $2
            UNION ALL
            SELECT p.*, g.hop + 1 FROM lineage g JOIN delegation_grants p ON p.workspace_id = g.workspace_id
                AND p.id = g.parent_grant_id WHERE g.hop < 4
         ) SELECT g.*, {TEMPLATE_IDS} FROM lineage g ORDER BY hop",
    )).bind(context.access.workspace.0).bind(id).fetch_all(&mut **tx).await?;
    let root = lineage.last().ok_or(IdentityError::Forbidden)?;
    check_active_team(tx, context, root).await?;
    let now = db_now(tx).await?;
    if root.parent_grant_id.is_some()
        || lineage
            .iter()
            .any(|node| node.revoked_at.is_some() || node.expires_at <= now)
    {
        return Err(IdentityError::DelegationInactive);
    }
    for node in &lineage {
        require_executor(
            tx,
            context,
            TeamId(node.team_id),
            AgentId(node.grantee_agent_id),
        )
        .await
        .map_err(|error| match error {
            IdentityError::Forbidden => IdentityError::DelegationInactive,
            other => other,
        })?;
    }
    Ok(lineage)
}

async fn require_executor(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    team_id: TeamId,
    agent: AgentId,
) -> Result<(), IdentityError> {
    let role = member_role(
        tx,
        context.access.workspace,
        team_id,
        Recipient {
            user_id: context.access.member,
            agent_id: Some(agent),
        },
    )
    .await?;
    let active = sqlx::query_scalar::<_, bool>(
        "SELECT true FROM agents WHERE workspace_id = $1 AND owner_user_id = $2 AND id = $3
            AND role IN ('lead', 'worker') AND disabled_at IS NULL FOR SHARE",
    )
    .bind(context.access.workspace.0)
    .bind(context.access.member.0)
    .bind(agent.0)
    .fetch_optional(&mut **tx)
    .await?;
    if active.is_none() || !matches!(role, Some(TeamRole::Lead | TeamRole::Worker)) {
        return Err(IdentityError::Forbidden);
    }
    Ok(())
}

fn check_subset(parent: &GrantRow, limits: &DelegationLimits) -> Result<(), IdentityError> {
    if i16::from(limits.remaining_depth) >= parent.remaining_depth
        || limits.expires_at > parent.expires_at
        || limits.max_attempts > parent.max_attempts as u16
        || limits.max_subtasks > parent.max_subtasks as u16
        || !limits
            .runtime_template_ids
            .iter()
            .all(|id| parent.runtime_template_ids.contains(id))
    {
        return Err(IdentityError::DelegationLimit);
    }
    Ok(())
}

async fn validate_limits(
    tx: &mut Transaction<'_, Postgres>,
    limits: &DelegationLimits,
) -> Result<(), IdentityError> {
    let now = db_now(tx).await?;
    let templates = &limits.runtime_template_ids;
    if limits.remaining_depth > 3
        || !(1..=128).contains(&limits.max_attempts)
        || limits.max_subtasks > 128
        || limits.expires_at <= now
        || limits.expires_at > now + Duration::days(7)
        || !(1..=16).contains(&templates.len())
        || templates
            .iter()
            .enumerate()
            .any(|(index, id)| templates[..index].contains(id))
    {
        return Err(IdentityError::InvalidInput);
    }
    Ok(())
}

async fn verify_templates(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    ids: &[Uuid],
) -> Result<(), IdentityError> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM runtime_templates WHERE workspace_id = $1 AND owner_user_id = $2 AND id = ANY($3)",
    ).bind(context.access.workspace.0).bind(context.access.member.0).bind(ids).fetch_one(&mut **tx).await?;
    if count != ids.len() as i64 {
        return Err(IdentityError::Forbidden);
    }
    Ok(())
}

struct NewGrant<'a> {
    team_id: TeamId,
    task_id: TaskId,
    grantee: AgentId,
    lead_generation: i64,
    parent: Option<Uuid>,
    limits: &'a DelegationLimits,
}

async fn insert_grant(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    input: NewGrant<'_>,
) -> Result<Uuid, IdentityError> {
    let id = Uuid::new_v4();
    create_scope(
        tx,
        context,
        id,
        Visibility::Team {
            team_id: input.team_id,
        },
        &[],
    )
    .await?;
    let workspace = context.access.workspace.0;
    let owner = context.access.member.0;
    let limits = input.limits;
    sqlx::query(
        "INSERT INTO delegation_grants (id, workspace_id, team_id, owner_user_id, task_id, parent_grant_id,
                issuer_agent_id, grantee_agent_id, lead_generation, expires_at, remaining_depth, max_attempts, max_subtasks)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)",
    ).bind(id).bind(workspace).bind(input.team_id.0).bind(owner).bind(input.task_id.0).bind(input.parent)
     .bind(context.agent_id().map(|id| id.0)).bind(input.grantee.0).bind(input.lead_generation).bind(limits.expires_at)
     .bind(i16::from(limits.remaining_depth)).bind(limits.max_attempts as i16).bind(limits.max_subtasks as i16)
     .execute(&mut **tx).await?;
    sqlx::query(
        "INSERT INTO delegation_templates (workspace_id, owner_user_id, grant_id, template_id)
                 SELECT $1, $2, $3, id FROM unnest($4::uuid[]) id",
    )
    .bind(workspace)
    .bind(owner)
    .bind(id)
    .bind(&limits.runtime_template_ids)
    .execute(&mut **tx)
    .await?;
    Ok(id)
}

enum Consumption {
    Attempt,
    Subtask,
}

async fn reserve(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    lineage: &[GrantRow],
    consumption: Consumption,
) -> Result<(), IdentityError> {
    let ids: Vec<_> = lineage.iter().map(|node| node.id).collect();
    let query = match consumption {
        Consumption::Attempt => {
            "UPDATE delegation_grants SET attempts_used = attempts_used + 1
             WHERE workspace_id = $1 AND id = ANY($2) AND attempts_used < max_attempts"
        }
        Consumption::Subtask => {
            "UPDATE delegation_grants SET subtasks_used = subtasks_used + 1
             WHERE workspace_id = $1 AND id = ANY($2) AND subtasks_used < max_subtasks"
        }
    };
    let result = sqlx::query(query)
        .bind(workspace.0)
        .bind(&ids)
        .execute(&mut **tx)
        .await?;
    if result.rows_affected() != ids.len() as u64 {
        return Err(IdentityError::DelegationLimit);
    }
    Ok(())
}

async fn get_grant(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<GrantRow, IdentityError> {
    sqlx::query_as::<_, GrantRow>(&format!(
        "SELECT g.*, {TEMPLATE_IDS} FROM delegation_grants g WHERE g.workspace_id = $1 AND g.id = $2",
    )).bind(workspace.0).bind(id).fetch_optional(&mut **tx).await?.ok_or(IdentityError::Forbidden)
}

async fn record(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    idem: &Idempotency,
    id: Uuid,
    revision: Option<i64>,
    event: &str,
) -> Result<DelegationReceipt, IdentityError> {
    let grant = get_grant(tx, context.access.workspace, id)
        .await?
        .into_grant();
    let sequence = append_event(tx, context, id, event, json!({"grant_id": id, "task_id": grant.task_id,
        "grantee_agent_id": grant.grantee_agent_id, "revision": grant.revision, "task_revision": revision}), None).await?;
    let receipt = DelegationReceipt {
        grant,
        task_revision: revision,
        event_cursor: save_cursor(tx, context, sequence).await?,
    };
    idem.record(tx, id, &receipt).await?;
    Ok(receipt)
}

async fn lock_lineage(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
) -> Result<(), IdentityError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("delegation/{}", workspace.0))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn db_now(tx: &mut Transaction<'_, Postgres>) -> Result<DateTime<Utc>, IdentityError> {
    Ok(sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?)
}

async fn lead_generation(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    team: TeamId,
) -> Result<i64, IdentityError> {
    Ok(
        sqlx::query_scalar("SELECT lead_generation FROM teams WHERE workspace_id = $1 AND id = $2")
            .bind(workspace.0)
            .bind(team.0)
            .fetch_one(&mut **tx)
            .await?,
    )
}

fn check_revision(current: i64, expected: i64) -> Result<(), IdentityError> {
    if !(1..i64::MAX).contains(&expected) {
        return Err(IdentityError::InvalidInput);
    }
    if current != expected {
        return Err(IdentityError::RevisionMismatch { current });
    }
    Ok(())
}
