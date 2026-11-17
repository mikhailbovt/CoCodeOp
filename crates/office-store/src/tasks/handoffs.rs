use chrono::{DateTime, Utc};
use office_domain::{
    Action, AgentId, AgentRole, BearerDigest, Resource, TaskAuthority, TaskId, TaskStatus, TeamId,
    UserId, WorkspaceId,
};
use office_protocol::{
    HandoffDecision, HandoffMutation, HandoffPage, HandoffState, ListHandoffs, Recipient,
    RequestHandoff, RespondHandoff, TaskHandoff,
};
use serde_json::json;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::{TaskRow, load_task, page_limit, scopes::has_work_scope, task_text};
use crate::{
    Store,
    data_scope::{authorize_scope, create_directed_scope, subject},
    events::{Idempotency, append_event},
    identity::{
        IdentityError, WorkspaceContext, active_principal, bind_subject, require_permission,
        workspace_context,
    },
    inbox::save_cursor,
    teams::member_role,
};

#[derive(sqlx::FromRow)]
struct HandoffRow {
    id: Uuid,
    workspace_id: Uuid,
    task_id: Uuid,
    expected_revision: i64,
    sender_user_id: Uuid,
    sender_agent_id: Option<Uuid>,
    recipient_user_id: Uuid,
    recipient_agent_id: Option<Uuid>,
    state: String,
    reason: String,
    expires_at: DateTime<Utc>,
    expired: bool,
}

const COLUMNS: &str = "*, expires_at <= clock_timestamp() AS expired";

impl HandoffRow {
    fn into_handoff(self) -> Result<TaskHandoff, IdentityError> {
        let state = match self.state.as_str() {
            "pending" if self.expired => HandoffState::Expired,
            "pending" => HandoffState::Pending,
            "accepted" => HandoffState::Accepted,
            "declined" => HandoffState::Declined,
            _ => return Err(IdentityError::InvalidRole),
        };
        Ok(TaskHandoff {
            handoff_id: self.id,
            workspace_id: WorkspaceId(self.workspace_id),
            task_id: TaskId(self.task_id),
            expected_revision: self.expected_revision,
            sender: Recipient {
                user_id: UserId(self.sender_user_id),
                agent_id: self.sender_agent_id.map(AgentId),
            },
            recipient: Recipient {
                user_id: UserId(self.recipient_user_id),
                agent_id: self.recipient_agent_id.map(AgentId),
            },
            state,
            reason: self.reason,
            expires_at: self.expires_at,
        })
    }
}

impl Store {
    pub async fn request_handoff(
        &self,
        digest: &BearerDigest,
        input: RequestHandoff,
    ) -> Result<HandoffMutation, IdentityError> {
        let reason = task_text(&input.reason, 2048)?;
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "handoff.request",
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
        handoff_status(&task, input.expected_revision)?;
        super::execution::ensure_idle(&mut tx, &task).await?;
        if has_work_scope(&mut tx, &task).await? {
            return Err(IdentityError::AccessRequestRequired);
        }
        if input.recipient
            == (Recipient {
                user_id: UserId(task.accountable_user_id),
                agent_id: task.accountable_agent_id.map(AgentId),
            })
        {
            return Err(IdentityError::InvalidInput);
        }
        check_recipient(&mut tx, context, context.recipient(), &task).await?;
        check_recipient(&mut tx, context, input.recipient, &task).await?;

        let id = Uuid::new_v4();
        create_directed_scope(&mut tx, context, id, input.recipient).await?;
        let row = sqlx::query_as::<_, HandoffRow>(&format!(
            "INSERT INTO task_handoffs (id, workspace_id, task_id, expected_revision, sender_user_id,
                                        sender_agent_id, recipient_user_id, recipient_agent_id, reason)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING {COLUMNS}"
        )).bind(id).bind(input.workspace_id.0).bind(input.task_id.0).bind(input.expected_revision)
         .bind(context.access.member.0).bind(context.agent_id().map(|id| id.0))
         .bind(input.recipient.user_id.0).bind(input.recipient.agent_id.map(|id| id.0)).bind(reason)
         .fetch_one(&mut *tx).await?;
        let result = record(
            &mut tx,
            context,
            &idem,
            row.into_handoff()?,
            None,
            "handoff.requested",
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn respond_handoff(
        &self,
        digest: &BearerDigest,
        input: RespondHandoff,
    ) -> Result<HandoffMutation, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "handoff.respond",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let handoff = sqlx::query_as::<_, HandoffRow>(&format!(
            "SELECT {COLUMNS} FROM task_handoffs WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
        ))
        .bind(input.workspace_id.0)
        .bind(input.handoff_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(IdentityError::Forbidden)?
        .into_handoff()?;
        require_permission(
            context.actor,
            Action::DecideOffer,
            Resource::DirectedOffer {
                workspace: context.access,
                recipient_owner: handoff.recipient.user_id,
                recipient_agent: handoff.recipient.agent_id,
            },
        )?;
        authorize_scope(&mut tx, context, handoff.handoff_id, Action::ReadData).await?;
        if handoff.state != HandoffState::Pending {
            return Err(IdentityError::InvalidTransition);
        }
        let (state, event, revision) = match input.decision {
            HandoffDecision::Accept => (
                "accepted",
                "handoff.accepted",
                Some(transfer(&mut tx, context, &handoff).await?),
            ),
            HandoffDecision::Decline => ("declined", "handoff.declined", None),
        };
        let row = sqlx::query_as::<_, HandoffRow>(&format!(
            "UPDATE task_handoffs SET state = $3, decided_at = clock_timestamp(),
                decided_by_user_id = $4, decided_by_agent_id = $5
             WHERE workspace_id = $1 AND id = $2 RETURNING {COLUMNS}",
        ))
        .bind(input.workspace_id.0)
        .bind(input.handoff_id)
        .bind(state)
        .bind(context.access.member.0)
        .bind(context.agent_id().map(|id| id.0))
        .fetch_one(&mut *tx)
        .await?;
        let result = record(
            &mut tx,
            context,
            &idem,
            row.into_handoff()?,
            revision,
            event,
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn list_handoffs(
        &self,
        digest: &BearerDigest,
        input: ListHandoffs,
    ) -> Result<HandoffPage, IdentityError> {
        let limit = page_limit(input.limit)?;
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let rows = sqlx::query_as::<_, HandoffRow>(&format!(
            "SELECT {COLUMNS} FROM task_handoffs WHERE workspace_id = $1 AND ($2::uuid IS NULL OR id > $2)
             ORDER BY id LIMIT $3",
        )).bind(input.workspace_id.0).bind(input.after).bind(i64::from(limit) + 1)
         .fetch_all(&mut *tx).await?;
        let more = rows.len() > usize::from(limit);
        let mut handoffs = Vec::with_capacity(rows.len().min(usize::from(limit)));
        for row in rows.into_iter().take(usize::from(limit)) {
            authorize_scope(&mut tx, context, row.id, Action::ReadData).await?;
            handoffs.push(row.into_handoff()?);
        }
        let next_after = handoffs
            .last()
            .filter(|_| more)
            .map(|request| request.handoff_id);
        tx.commit().await?;
        Ok(HandoffPage {
            handoffs,
            next_after,
        })
    }
}

async fn transfer(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    handoff: &TaskHandoff,
) -> Result<i64, IdentityError> {
    let (task, _) = load_task(tx, context, handoff.task_id, true).await?;
    let status = handoff_status(&task, handoff.expected_revision)?;
    super::execution::ensure_idle(tx, &task).await?;
    if has_work_scope(tx, &task).await? {
        return Err(IdentityError::AccessRequestRequired);
    }
    check_recipient(tx, context, handoff.sender, &task).await?;
    check_recipient(tx, context, handoff.recipient, &task).await?;
    let revision = sqlx::query_scalar::<_, i64>(
        "UPDATE tasks SET accountable_user_id = $3, accountable_agent_id = $4,
                assignee_user_id = NULL, assignee_agent_id = NULL, status = $5,
                revision = revision + 1, updated_at = clock_timestamp()
         WHERE workspace_id = $1 AND id = $2 RETURNING revision",
    )
    .bind(handoff.workspace_id.0)
    .bind(handoff.task_id.0)
    .bind(handoff.recipient.user_id.0)
    .bind(handoff.recipient.agent_id.map(|id| id.0))
    .bind(status.as_str())
    .fetch_one(&mut **tx)
    .await?;
    append_event(
        tx, context, handoff.task_id.0, "task.handed_off",
        json!({"task_id": handoff.task_id, "handoff_id": handoff.handoff_id, "revision": revision,
               "accountable": subject(handoff.recipient.user_id, handoff.recipient.agent_id.map(|id| id.0)),
               "status": status}),
        None,
    ).await?;
    Ok(revision)
}

fn handoff_status(task: &TaskRow, expected_revision: i64) -> Result<TaskStatus, IdentityError> {
    if !(1..i64::MAX).contains(&expected_revision) {
        return Err(IdentityError::InvalidInput);
    }
    if task.revision != expected_revision {
        return Err(IdentityError::RevisionMismatch {
            current: task.revision,
        });
    }
    if task.work_scope_id.is_some() {
        return Err(IdentityError::AccessRequestRequired);
    }
    if task.awaiting_owner || task.delegation_grant_id.is_some() {
        return Err(IdentityError::InvalidTransition);
    }
    match task.status()? {
        status @ (TaskStatus::Draft | TaskStatus::Ready | TaskStatus::ChangesRequested) => {
            Ok(status)
        }
        TaskStatus::Assigned => Ok(TaskStatus::Ready),
        _ => Err(IdentityError::InvalidTransition),
    }
}

async fn check_recipient(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    principal: Recipient,
    task: &TaskRow,
) -> Result<(), IdentityError> {
    if active_principal(tx, context, principal)
        .await?
        .is_some_and(|role| role != AgentRole::Lead)
    {
        return Err(IdentityError::Forbidden);
    }
    bind_subject(tx, principal).await?;
    let visible = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM data_scopes WHERE workspace_id = $1 AND id = $2)",
    )
    .bind(context.access.workspace.0)
    .bind(task.id)
    .fetch_one(&mut **tx)
    .await?;
    let team_member = if let Some(team) = task.team_id.map(TeamId) {
        member_role(tx, context.access.workspace, team, principal)
            .await?
            .is_some()
    } else {
        true
    };
    bind_subject(tx, context.recipient()).await?;
    if !visible || !team_member {
        return Err(IdentityError::Forbidden);
    }
    Ok(())
}

async fn record(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    idem: &Idempotency,
    handoff: TaskHandoff,
    transferred_task_revision: Option<i64>,
    event: &str,
) -> Result<HandoffMutation, IdentityError> {
    let scope = handoff.handoff_id;
    let seq = append_event(
        tx,
        context,
        scope,
        event,
        json!({"handoff_id": scope, "task_id": handoff.task_id, "state": handoff.state}),
        None,
    )
    .await?;
    let receipt = HandoffMutation {
        handoff,
        transferred_task_revision,
        event_cursor: save_cursor(tx, context, seq).await?,
    };
    idem.record(tx, scope, &receipt).await?;
    Ok(receipt)
}
