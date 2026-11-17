use std::collections::HashSet;

use chrono::{DateTime, Utc};
use office_domain::{
    Action, Actor, AgentRole, BearerDigest, ErrorCode, RunId, TaskId, WorkspaceId,
};
use office_protocol::{
    AckInbox, ActorContext, ContextDiagnostic, GetContext, GetInboxDelivery, InboxDelivery,
    InboxDeliveryState, InboxEvent, InboxPage, InboxReceipt, ReadInbox, WorkflowGuide,
};
use serde_json::Value;
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use crate::{
    Store,
    data_scope::authorize_scope,
    identity::{IdentityError, WorkspaceContext, workspace_context},
    tasks::context_tasks,
};

#[derive(sqlx::FromRow)]
struct EventRow {
    id: Uuid,
    scope_id: Uuid,
    sequence: i64,
    event_type: String,
    payload: Json<Value>,
    created_at: DateTime<Utc>,
    acknowledged_at: Option<DateTime<Utc>>,
    received: bool,
}

#[derive(sqlx::FromRow)]
struct DeliveryRow {
    scope_id: Uuid,
    delivered_at: Option<DateTime<Utc>>,
    acknowledged_at: Option<DateTime<Utc>>,
}

impl Store {
    pub async fn read_inbox(
        &self,
        digest: &BearerDigest,
        input: ReadInbox,
        retain_count: u32,
    ) -> Result<InboxPage, IdentityError> {
        let limit = input.limit.unwrap_or(30);
        if !(1..=100).contains(&limit) {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let result = read_page(&mut tx, context, input.cursor, limit, retain_count, None).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn actor_context(
        &self,
        digest: &BearerDigest,
        input: GetContext,
        retain_count: u32,
    ) -> Result<ActorContext, IdentityError> {
        let limit = input.limit.unwrap_or(12);
        if !(1..=25).contains(&limit) {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let (inbox, diagnostics) = match read_page(
            &mut tx,
            context,
            input.inbox_cursor,
            limit,
            retain_count,
            None,
        )
        .await
        {
            Ok(page) => (Some(page), Vec::new()),
            Err(IdentityError::CursorExpired) => (
                None,
                vec![ContextDiagnostic {
                    code: ErrorCode::CursorExpired,
                    next_action:
                        "Retry with inbox_cursor omitted to start from the retained inbox window."
                            .into(),
                }],
            ),
            Err(err) => return Err(err),
        };
        let tasks = context_tasks(&mut tx, context, input.tasks_after, limit).await?;
        let role = match context.actor {
            Actor::Agent { role, .. } => Some(role),
            _ => None,
        };
        tx.commit().await?;
        Ok(ActorContext {
            workspace_id: input.workspace_id,
            actor: context.recipient(),
            role,
            inbox,
            tasks,
            workflow: workflow(role),
            diagnostics,
        })
    }

    pub async fn ack_inbox(
        &self,
        digest: &BearerDigest,
        input: AckInbox,
        retain_count: u32,
    ) -> Result<InboxReceipt, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let (floor, high_water) = event_window(&mut tx, input.workspace_id, retain_count).await?;
        let scope = sqlx::query_scalar::<_, Uuid>(
            "SELECT e.scope_id FROM outbox o JOIN events e ON e.workspace_id = o.workspace_id
                 AND e.sequence = o.sequence
             WHERE e.workspace_id = $1 AND e.id = $2 AND o.sequence > $3 AND o.sequence <= $4",
        )
        .bind(input.workspace_id.0)
        .bind(input.event_id)
        .bind(floor)
        .bind(high_water)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(IdentityError::Forbidden)?;
        authorize_scope(&mut tx, context, scope, Action::ReadData).await?;
        let acknowledged_at: DateTime<Utc> = sqlx::query_scalar(
            "INSERT INTO inbox_receipts (workspace_id, event_id, user_id, agent_id, delivered_at, acknowledged_at)
             VALUES ($1, $2, $3, $4, transaction_timestamp(), transaction_timestamp())
             ON CONFLICT (workspace_id, event_id, user_id, agent_id) DO UPDATE
             SET acknowledged_at = coalesce(inbox_receipts.acknowledged_at, greatest(inbox_receipts.delivered_at, clock_timestamp()))
             RETURNING acknowledged_at",
        ).bind(input.workspace_id.0).bind(input.event_id).bind(context.access.member.0)
         .bind(context.agent_id().map(|id| id.0)).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(InboxReceipt {
            event_id: input.event_id,
            acknowledged_at,
        })
    }

    pub async fn inbox_delivery(
        &self,
        digest: &BearerDigest,
        input: GetInboxDelivery,
        retain_count: u32,
    ) -> Result<InboxDelivery, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let (floor, high_water) = event_window(&mut tx, input.workspace_id, retain_count).await?;
        let row = sqlx::query_as::<_, DeliveryRow>(
            "SELECT e.scope_id, receipt.delivered_at, receipt.acknowledged_at
             FROM outbox o JOIN events e ON (e.workspace_id,e.sequence) = (o.workspace_id,o.sequence)
             LEFT JOIN inbox_receipts receipt ON (receipt.workspace_id,receipt.event_id,receipt.user_id) = (e.workspace_id,e.id,e.recipient_user_id)
                AND receipt.agent_id IS NOT DISTINCT FROM e.recipient_agent_id
             WHERE o.workspace_id=$1 AND e.id=$2 AND o.sequence>$3 AND o.sequence<=$4 AND e.recipient_user_id IS NOT NULL",
        ).bind(input.workspace_id.0).bind(input.event_id).bind(floor).bind(high_water)
         .fetch_optional(&mut *tx).await?.ok_or(IdentityError::Forbidden)?;
        authorize_scope(&mut tx, context, row.scope_id, Action::ReadData).await?;
        tx.commit().await?;
        let state = match (row.acknowledged_at, row.delivered_at) {
            (Some(_), _) => InboxDeliveryState::Acknowledged,
            (None, Some(_)) => InboxDeliveryState::Delivered,
            (None, None) => InboxDeliveryState::WaitingForAgent,
        };
        Ok(InboxDelivery {
            event_id: input.event_id,
            state,
            delivered_at: row.delivered_at,
            acknowledged_at: row.acknowledged_at,
        })
    }
}

fn workflow(role: Option<AgentRole>) -> WorkflowGuide {
    let mut steps = vec![
        "Start with office_bootstrap and office_get_context. Use next_cursor and next_after to retrieve further pages.",
        "Read the referenced thread, task, proposal revision or decision before acting. Acknowledge inbox events after processing them.",
    ];
    steps.extend(match role {
        Some(AgentRole::Lead) | None => &[
            "Agree on objectives, criteria and the owning team. Use a bounded WorkScope, then prepare and assign or queue the task.",
            "Resolve shared-file AccessRequests with the listed approvers. Review the submitted result against the current criteria and source evidence.",
        ],
        Some(AgentRole::Worker) => &[
            "Read the active task and scope, then fetch permitted files and their hashes. Claim overlapping paths before publishing a change with its read dependencies.",
            "On PATH_CLAIMED request a handoff. On PATH_FORBIDDEN request access. On CONTENT_CONFLICT reread and revise the change. Notify collaborators with the recorded change ID.",
        ],
        Some(AgentRole::Reviewer) => &[
            "Read the assigned task and revision-bound result. Verify the linked evidence against the acceptance criteria, then record a review.",
            "Return specific missing evidence or requested changes to the author through the task review.",
        ],
    });
    WorkflowGuide {
        version: 1,
        steps: steps.into_iter().map(str::to_owned).collect(),
    }
}

pub(crate) async fn read_page(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    cursor: Option<Uuid>,
    limit: u16,
    retain_count: u32,
    run: Option<(TaskId, RunId, Uuid)>,
) -> Result<InboxPage, IdentityError> {
    let workspace = context.access.workspace;
    let (floor, high_water) = event_window(tx, workspace, retain_count).await?;
    let agent = context.agent_id().map(|id| id.0);
    let after = match cursor {
        Some(cursor) => sqlx::query_scalar::<_, i64>(
            "SELECT position FROM inbox_cursors WHERE id = $1 AND workspace_id = $2
                 AND user_id = $3 AND agent_id IS NOT DISTINCT FROM $4
                 AND expires_at > transaction_timestamp() AND position BETWEEN $5 AND $6",
        )
        .bind(cursor)
        .bind(workspace.0)
        .bind(context.access.member.0)
        .bind(agent)
        .bind(floor)
        .bind(high_water)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(IdentityError::CursorExpired)?,
        None => floor,
    };
    let mut rows = sqlx::query_as::<_, EventRow>(
        "SELECT e.id, e.scope_id, e.sequence, e.event_type, e.payload, e.created_at, receipt.acknowledged_at,
                (e.recipient_user_id,e.recipient_agent_id) IS NOT DISTINCT FROM ($4::uuid,$5::uuid) AS received
         FROM outbox o JOIN events e ON e.workspace_id = o.workspace_id AND e.sequence = o.sequence
         LEFT JOIN inbox_receipts receipt ON receipt.workspace_id = e.workspace_id AND receipt.event_id = e.id
             AND receipt.user_id = $4 AND receipt.agent_id IS NOT DISTINCT FROM $5
         WHERE o.workspace_id = $1 AND o.sequence > $2 AND o.sequence <= $3
           AND (e.event_type<>'checkout.changed' OR NOT EXISTS (
               SELECT 1 FROM checkout_notification_areas older WHERE (older.workspace_id,older.sequence)=(o.workspace_id,o.sequence)
           ) OR EXISTS (
               SELECT 1 FROM checkout_notification_areas current
               WHERE (current.workspace_id,current.sequence)=(o.workspace_id,o.sequence) AND NOT EXISTS (
                   SELECT 1 FROM checkout_notification_areas newer
                   WHERE (newer.workspace_id,newer.checkout_id,newer.task_id,newer.area_key)=
                       (current.workspace_id,current.checkout_id,current.task_id,current.area_key)
                       AND newer.sequence>current.sequence AND newer.sequence<=$3
               )
           ))
           AND ($7::uuid IS NULL OR e.scope_id=$7 OR EXISTS (
               SELECT 1 FROM access_requests request WHERE (request.workspace_id,request.id)=(e.workspace_id,e.scope_id)
                 AND request.task_id=$7 AND request.run_id=$8
           ) OR (e.event_type IN ('path.handoff_requested','path.handoff_offered','path.handoff_resolved')
             AND EXISTS (
               SELECT 1 FROM path_claim_handoffs handoff
               WHERE (handoff.workspace_id,handoff.id)=(e.workspace_id,e.scope_id)
                 AND (handoff.requested_run_id=$8 OR EXISTS (
                     SELECT 1 FROM path_claims holder
                     WHERE (holder.workspace_id,holder.id,holder.run_id)
                         =(handoff.workspace_id,handoff.blocking_claim_id,$8)
                 ))
           )) OR (e.event_type='checkout.changed' AND EXISTS (
               SELECT 1 FROM checkout_notification_areas changed
               WHERE (changed.workspace_id,changed.sequence,changed.task_id)=(e.workspace_id,e.sequence,$7)
           )) OR EXISTS (
               SELECT 1 FROM activation_events source WHERE (source.workspace_id,source.event_id)=(e.workspace_id,e.id)
                 AND source.job_id=office_activation_job(e.workspace_id,$9)
           ))
         ORDER BY o.sequence LIMIT $6",
    ).bind(workspace.0).bind(after).bind(high_water).bind(context.access.member.0)
     .bind(agent).bind(i64::from(limit) + 1).bind(run.map(|(task, _, _)| task.0))
     .bind(run.map(|(_, run, _)| run.0)).bind(run.map(|(_, _, job)| job))
     .fetch_all(&mut **tx).await?;
    let has_more = rows.len() > usize::from(limit);
    rows.truncate(usize::from(limit));
    let mut checked = HashSet::new();
    for row in &rows {
        if checked.insert(row.scope_id) {
            authorize_scope(tx, context, row.scope_id, Action::ReadData).await?;
        }
    }
    let delivered: Vec<_> = rows
        .iter()
        .filter(|row| row.received)
        .map(|row| row.id)
        .collect();
    if !delivered.is_empty() {
        sqlx::query(
            "INSERT INTO inbox_receipts (workspace_id, event_id, user_id, agent_id)
             SELECT $1,id,$2,$3 FROM unnest($4::uuid[]) AS pending(id) ORDER BY id
             ON CONFLICT (workspace_id,event_id,user_id,agent_id) DO NOTHING",
        )
        .bind(workspace.0)
        .bind(context.access.member.0)
        .bind(agent)
        .bind(&delivered)
        .execute(&mut **tx)
        .await?;
    }
    // Advance through gaps without returning the private sequence values.
    // A writer committing after high_water will be observed on the next read.
    let position = if has_more {
        rows.last().map_or(after, |row| row.sequence)
    } else {
        high_water
    };
    let next_cursor = save_cursor(tx, context, position).await?;
    Ok(InboxPage {
        events: rows
            .into_iter()
            .map(|row| InboxEvent {
                event_id: row.id,
                event_type: row.event_type,
                payload: row.payload.0,
                created_at: row.created_at,
                acknowledged_at: row.acknowledged_at,
            })
            .collect(),
        next_cursor,
        has_more,
    })
}

async fn event_window(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    retain_count: u32,
) -> Result<(i64, i64), IdentityError> {
    let high_water = sqlx::query_scalar::<_, i64>(
        "SELECT last_sequence FROM workspace_event_counters WHERE workspace_id = $1",
    )
    .bind(workspace.0)
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or(0);
    Ok(((high_water - i64::from(retain_count)).max(0), high_water))
}

pub(crate) async fn save_cursor(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    position: i64,
) -> Result<Uuid, IdentityError> {
    let sequence = sqlx::query_scalar(
        "INSERT INTO inbox_cursors (id, workspace_id, user_id, agent_id, position) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (workspace_id, user_id, agent_id, position) DO UPDATE SET
             id = CASE WHEN inbox_cursors.expires_at > transaction_timestamp() THEN inbox_cursors.id ELSE excluded.id END,
             expires_at = greatest(inbox_cursors.expires_at, excluded.expires_at)
         RETURNING id",
    ).bind(Uuid::new_v4()).bind(context.access.workspace.0).bind(context.access.member.0)
     .bind(context.agent_id().map(|id| id.0)).bind(position).fetch_one(&mut **tx).await?;
    Ok(sequence)
}
