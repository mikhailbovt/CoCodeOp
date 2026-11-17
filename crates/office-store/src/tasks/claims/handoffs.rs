use chrono::{DateTime, Duration, Utc};
use office_domain::{AgentId, BearerDigest, RunId, TaskId, UserId, WorkspaceId};
use office_protocol::{
    ClaimPaths, GetPathHandoff, PathHandoff, PathHandoffDecision, PathHandoffReceipt,
    PathHandoffState, Recipient, RequestPathHandoff, RespondPathHandoff, WorkScopeDefinition,
};
use serde_json::json;
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use super::{
    ClaimRow, ClaimSession, active_claim, admit_paths, bind_index, claim_context, clock,
    lock_claim_scope, publish, validate_paths,
};
use crate::{
    Store,
    checkouts::lock_policy,
    data_scope::create_directed_scope,
    events::{Idempotency, append_event},
    identity::{IdentityError, WorkspaceContext, workspace_context},
    inbox::save_cursor,
    tasks::{
        execution::start_effects::credentials::{RunAccess, authenticate_run},
        task_text,
    },
};
use office_domain::PathRule;

const REQUEST_TTL: Duration = Duration::seconds(60);
const OFFER_TTL: Duration = Duration::seconds(30);
const MAX_WAITERS: i64 = 16;

#[derive(sqlx::FromRow)]
struct HandoffRow {
    id: Uuid,
    checkout_id: Uuid,
    requested_task_id: Uuid,
    requested_run_id: Uuid,
    requested_scope_id: Uuid,
    scope_revision: i64,
    requesting_user_id: Uuid,
    requesting_agent_id: Uuid,
    holder_user_id: Uuid,
    holder_agent_id: Uuid,
    blocking_claim_id: Uuid,
    paths: Json<Vec<PathRule>>,
    reason: String,
    state: String,
    revision: i64,
    expires_at: DateTime<Utc>,
}

impl HandoffRow {
    fn requester(&self) -> Recipient {
        Recipient {
            user_id: UserId(self.requesting_user_id),
            agent_id: Some(AgentId(self.requesting_agent_id)),
        }
    }

    fn holder(&self) -> Recipient {
        Recipient {
            user_id: UserId(self.holder_user_id),
            agent_id: Some(AgentId(self.holder_agent_id)),
        }
    }

    fn into_handoff(self, now: DateTime<Utc>) -> Result<PathHandoff, IdentityError> {
        let state = match self.state.as_str() {
            "requested" | "offered" if self.expires_at <= now => PathHandoffState::Expired,
            "requested" => PathHandoffState::Requested,
            "offered" => PathHandoffState::Offered,
            "accepted" => PathHandoffState::Accepted,
            "rejected" => PathHandoffState::Rejected,
            "cancelled" => PathHandoffState::Cancelled,
            "expired" => PathHandoffState::Expired,
            _ => return Err(IdentityError::InvalidRole),
        };
        let requester = self.requester();
        let holder = self.holder();
        Ok(PathHandoff {
            handoff_id: self.id,
            checkout_id: self.checkout_id,
            requester,
            holder,
            paths: self.paths.0,
            reason: self.reason,
            expires_at: self.expires_at,
            state,
            revision: self.revision,
        })
    }
}

impl Store {
    pub async fn request_path_handoff(
        &self,
        digest: &BearerDigest,
        input: RequestPathHandoff,
    ) -> Result<PathHandoffReceipt, IdentityError> {
        self.request_path_handoff_as(digest, input, ClaimSession::Agent)
            .await
    }

    pub async fn request_run_path_handoff(
        &self,
        digest: &BearerDigest,
        input: RequestPathHandoff,
    ) -> Result<PathHandoffReceipt, IdentityError> {
        self.request_path_handoff_as(digest, input, ClaimSession::ManagedRun)
            .await
    }

    async fn request_path_handoff_as(
        &self,
        digest: &BearerDigest,
        input: RequestPathHandoff,
        session: ClaimSession,
    ) -> Result<PathHandoffReceipt, IdentityError> {
        validate_paths(&input.paths)?;
        let reason = task_text(&input.reason, 2048)?;
        let mut tx = self.pool.begin().await?;
        let (context, _) = claim_context(
            &mut tx,
            digest,
            input.workspace_id,
            input.task_id,
            input.run_id,
            session,
        )
        .await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "path.handoff_request",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let (scope, manifest) =
            lock_claim_scope(&mut tx, context, input.task_id, input.run_id, session).await?;
        admit_paths(&scope, &manifest, input.scope_revision, &input.paths)?;
        let now = clock(&mut tx).await?;
        if active_claim(
            &mut tx,
            input.workspace_id,
            input.task_id,
            input.run_id,
            now,
        )
        .await?
        .is_some()
        {
            return Err(IdentityError::InvalidTransition);
        }
        bind_index(&mut tx, Some(scope.checkout_id)).await?;
        expire_due(&mut tx, input.workspace_id, scope.checkout_id, now).await?;
        let holder = sqlx::query_as::<_, ClaimRow>(
            "SELECT * FROM path_claims
            WHERE workspace_id=$1 AND checkout_id=$2 AND state='active' AND expires_at>$3
            AND worker_agent_id<>$4 AND bounds @> office_claim_ranges($5) LIMIT 1",
        )
        .bind(input.workspace_id.0)
        .bind(scope.checkout_id)
        .bind(now)
        .bind(context.agent_id().map(|id| id.0))
        .bind(Json(&input.paths))
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(IdentityError::ClaimFenceStale)?;
        let (waiters, already_waiting) = sqlx::query_as::<_, (i64, bool)>(
            "SELECT count(*) FILTER (WHERE blocking_claim_id=$4), coalesce(bool_or(requested_run_id=$3), false)
            FROM path_claim_handoffs
            WHERE workspace_id=$1 AND checkout_id=$2 AND state IN ('requested','offered')
            AND (requested_run_id=$3 OR blocking_claim_id=$4)")
            .bind(input.workspace_id.0).bind(scope.checkout_id).bind(input.run_id.0).bind(holder.id)
            .fetch_one(&mut *tx).await?;
        if already_waiting || waiters >= MAX_WAITERS {
            return Err(IdentityError::PathHandoffPending);
        }
        let id = Uuid::new_v4();
        let recipient = holder.worker();
        create_directed_scope(&mut tx, context, id, recipient).await?;
        let row = sqlx::query_as::<_, HandoffRow>("INSERT INTO path_claim_handoffs
                (id,workspace_id,checkout_id,requested_task_id,requested_run_id,requested_scope_id,
                 scope_revision,requesting_user_id,requesting_agent_id,holder_user_id,holder_agent_id,
                 blocking_claim_id,paths,reason,expires_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15) RETURNING *")
            .bind(id).bind(input.workspace_id.0).bind(scope.checkout_id).bind(input.task_id.0)
            .bind(input.run_id.0).bind(scope.scope_id).bind(scope.revision)
            .bind(context.access.member.0).bind(context.agent_id().map(|id| id.0))
            .bind(holder.worker_user_id).bind(holder.worker_agent_id).bind(holder.id)
            .bind(Json(input.paths)).bind(reason).bind((now + REQUEST_TTL).min(scope.expires_at).min(holder.expires_at))
            .fetch_one(&mut *tx).await?;
        bind_index(&mut tx, None).await?;
        let handoff = row.into_handoff(now)?;
        let sequence = notify(
            &mut tx,
            context,
            &handoff,
            "path.handoff_requested",
            Some(recipient),
        )
        .await?;
        let event_cursor = save_cursor(&mut tx, context, sequence).await?;
        let result = PathHandoffReceipt {
            handoff,
            event_cursor,
        };
        idem.record(&mut tx, id, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn respond_path_handoff(
        &self,
        digest: &BearerDigest,
        input: RespondPathHandoff,
    ) -> Result<PathHandoffReceipt, IdentityError> {
        self.respond_path_handoff_as(digest, input, ClaimSession::Agent)
            .await
    }

    pub async fn respond_run_path_handoff(
        &self,
        digest: &BearerDigest,
        input: RespondPathHandoff,
    ) -> Result<PathHandoffReceipt, IdentityError> {
        self.respond_path_handoff_as(digest, input, ClaimSession::ManagedRun)
            .await
    }

    async fn respond_path_handoff_as(
        &self,
        digest: &BearerDigest,
        input: RespondPathHandoff,
        session: ClaimSession,
    ) -> Result<PathHandoffReceipt, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let (context, locked_row) = match session {
            ClaimSession::Agent => (
                workspace_context(&mut tx, digest, input.workspace_id).await?,
                None,
            ),
            ClaimSession::ManagedRun => {
                let run = checkout_run(&mut tx, digest, input.workspace_id).await?;
                lock_claim_scope(
                    &mut tx,
                    run.context,
                    TaskId(run.task.id),
                    run.run_id,
                    session,
                )
                .await?;
                let row =
                    get_for_run(&mut tx, input.workspace_id, input.handoff_id, run.run_id).await?;
                (run.context, Some(row))
            }
        };
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "path.handoff_respond",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let row = if let Some(row) = locked_row {
            row
        } else {
            let before = get(&mut tx, input.workspace_id, input.handoff_id).await?;
            lock_policy(&mut tx, before.checkout_id).await?;
            get(&mut tx, input.workspace_id, input.handoff_id).await?
        };
        let actor = context.recipient();
        match input.decision {
            PathHandoffDecision::Cancel if actor == row.requester() => (),
            PathHandoffDecision::Offer | PathHandoffDecision::Reject if actor == row.holder() => (),
            _ => return Err(IdentityError::Forbidden),
        }
        if row.revision != input.expected_revision {
            return Err(IdentityError::RevisionMismatch {
                current: row.revision,
            });
        }
        let now = clock(&mut tx).await?;
        if row.expires_at <= now
            || !(row.state == "requested"
                || (row.state == "offered" && input.decision == PathHandoffDecision::Cancel))
        {
            return Err(IdentityError::InvalidTransition);
        }
        bind_index(&mut tx, Some(row.checkout_id)).await?;
        let (state, expires, event) = match input.decision {
            PathHandoffDecision::Offer => {
                let released = sqlx::query_as::<_, ClaimRow>("UPDATE path_claims SET state='released'
                    WHERE workspace_id=$1 AND checkout_id=$2 AND id=$3
                    AND worker_user_id=$4 AND worker_agent_id=$5 AND state='active' AND expires_at>$6 RETURNING *")
                    .bind(input.workspace_id.0).bind(row.checkout_id).bind(row.blocking_claim_id)
                    .bind(row.holder_user_id).bind(row.holder_agent_id).bind(now)
                    .fetch_optional(&mut *tx).await?.ok_or(IdentityError::ClaimFenceStale)?;
                publish(&mut tx, &released, "path.released").await?;
                ("offered", now + OFFER_TTL, "path.handoff_offered")
            }
            PathHandoffDecision::Reject => ("rejected", row.expires_at, "path.handoff_resolved"),
            PathHandoffDecision::Cancel => ("cancelled", row.expires_at, "path.handoff_resolved"),
        };
        let updated = sqlx::query_as::<_, HandoffRow>("UPDATE path_claim_handoffs
           SET state=$3, expires_at=$4, revision=revision+1 WHERE workspace_id=$1 AND id=$2 RETURNING *")
            .bind(input.workspace_id.0).bind(row.id).bind(state).bind(expires)
            .fetch_one(&mut *tx).await?;
        if input.decision == PathHandoffDecision::Offer {
            expire_waiters(
                &mut tx,
                input.workspace_id,
                row.checkout_id,
                row.blocking_claim_id,
            )
            .await?;
        }
        bind_index(&mut tx, None).await?;
        let handoff = updated.into_handoff(now)?;
        let sequence = notify(&mut tx, context, &handoff, event, None).await?;
        let event_cursor = save_cursor(&mut tx, context, sequence).await?;
        let result = PathHandoffReceipt {
            handoff,
            event_cursor,
        };
        idem.record(&mut tx, row.id, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn get_path_handoff(
        &self,
        digest: &BearerDigest,
        input: GetPathHandoff,
    ) -> Result<PathHandoff, IdentityError> {
        let mut tx = self.pool.begin().await?;
        workspace_context(&mut tx, digest, input.workspace_id).await?;
        let row = get(&mut tx, input.workspace_id, input.handoff_id).await?;
        let now = clock(&mut tx).await?;
        tx.commit().await?;
        row.into_handoff(now)
    }

    pub async fn get_run_path_handoff(
        &self,
        digest: &BearerDigest,
        input: GetPathHandoff,
    ) -> Result<PathHandoff, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let run = checkout_run(&mut tx, digest, input.workspace_id).await?;
        let row = get_for_run(&mut tx, input.workspace_id, input.handoff_id, run.run_id).await?;
        let now = clock(&mut tx).await?;
        tx.commit().await?;
        row.into_handoff(now)
    }
}

async fn checkout_run(
    tx: &mut Transaction<'_, Postgres>,
    digest: &BearerDigest,
    workspace: WorkspaceId,
) -> Result<RunAccess, IdentityError> {
    let run = authenticate_run(tx, digest).await?;
    if !run.gateway_required || run.context.access.workspace != workspace {
        return Err(IdentityError::Forbidden);
    }
    Ok(run)
}

async fn get_for_run(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    id: Uuid,
    run: RunId,
) -> Result<HandoffRow, IdentityError> {
    sqlx::query_as(
        "SELECT handoff.* FROM path_claim_handoffs handoff
         WHERE handoff.workspace_id=$1 AND handoff.id=$2
           AND ((handoff.requested_run_id=$3 AND handoff.requesting_agent_id=office_agent_id())
             OR (handoff.holder_agent_id=office_agent_id() AND EXISTS (
                 SELECT 1 FROM path_claims holder
                 WHERE (holder.workspace_id,holder.id,holder.run_id)
                     =(handoff.workspace_id,handoff.blocking_claim_id,$3)
             )))",
    )
    .bind(workspace.0)
    .bind(id)
    .bind(run.0)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(IdentityError::Forbidden)
}

pub(super) async fn verify_reservation(
    tx: &mut Transaction<'_, Postgres>,
    scope: &WorkScopeDefinition,
    input: &ClaimPaths,
    now: DateTime<Utc>,
) -> Result<(), IdentityError> {
    if let Some(id) = input.handoff_id {
        let offered = get(tx, input.workspace_id, id).await?;
        if offered.requester() != scope.worker
            || offered.checkout_id != scope.checkout_id
            || offered.requested_task_id != input.task_id.0
            || offered.requested_run_id != input.run_id.0
        {
            return Err(IdentityError::Forbidden);
        }
        if offered.state != "offered"
            || offered.expires_at <= now
            || offered.requested_scope_id != scope.scope_id
            || offered.scope_revision != scope.revision
        {
            return Err(IdentityError::ClaimFenceStale);
        }
        if offered.paths.0 != input.paths {
            return Err(IdentityError::InvalidInput);
        }
    } else {
        let pending = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM path_claim_handoffs
            WHERE workspace_id=$1 AND checkout_id=$2 AND requested_run_id=$3 AND expires_at>$4
              AND state IN ('requested','offered'))",
        )
        .bind(input.workspace_id.0)
        .bind(scope.checkout_id)
        .bind(input.run_id.0)
        .bind(now)
        .fetch_one(&mut **tx)
        .await?;
        if pending {
            return Err(IdentityError::PathHandoffPending);
        }
    }
    let conflict = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM path_claim_handoffs
        WHERE workspace_id=$1 AND checkout_id=$2 AND state='offered' AND expires_at>$3
          AND id IS DISTINCT FROM $4 AND bounds && office_claim_ranges($5))",
    )
    .bind(input.workspace_id.0)
    .bind(scope.checkout_id)
    .bind(now)
    .bind(input.handoff_id)
    .bind(Json(&input.paths))
    .fetch_one(&mut **tx)
    .await?;
    if conflict {
        return Err(IdentityError::PathClaimed);
    }
    Ok(())
}

pub(super) async fn accept_offer(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    offer: Uuid,
    fence: i64,
    now: DateTime<Utc>,
) -> Result<(), IdentityError> {
    let updated = sqlx::query_as::<_, HandoffRow>(
        "UPDATE path_claim_handoffs
       SET state='accepted', accepted_fence=$3, revision=revision+1
       WHERE workspace_id=$1 AND id=$2 AND state='offered' RETURNING *",
    )
    .bind(context.access.workspace.0)
    .bind(offer)
    .bind(fence)
    .fetch_one(&mut **tx)
    .await?;
    let handoff = updated.into_handoff(now)?;
    notify(tx, context, &handoff, "path.handoff_resolved", None).await?;
    Ok(())
}

pub(super) async fn prevent_extension(
    tx: &mut Transaction<'_, Postgres>,
    claim: &ClaimRow,
    now: DateTime<Utc>,
) -> Result<(), IdentityError> {
    let pending = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM path_claim_handoffs
        WHERE workspace_id=$1 AND checkout_id=$2 AND blocking_claim_id=$3
          AND state='requested' AND expires_at>$4)",
    )
    .bind(claim.workspace_id)
    .bind(claim.checkout_id)
    .bind(claim.id)
    .bind(now)
    .fetch_one(&mut **tx)
    .await?;
    if pending {
        return Err(IdentityError::PathHandoffPending);
    }
    Ok(())
}

pub(super) async fn expire_waiters(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
    claim: Uuid,
) -> Result<(), IdentityError> {
    sqlx::query(
        "UPDATE path_claim_handoffs SET state='expired', revision=revision+1
        WHERE workspace_id=$1 AND checkout_id=$2 AND blocking_claim_id=$3 AND state='requested'",
    )
    .bind(workspace.0)
    .bind(checkout)
    .bind(claim)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(super) async fn expire_waiting_scope(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
    scope: Uuid,
) -> Result<(), IdentityError> {
    sqlx::query("UPDATE path_claim_handoffs SET state='expired', revision=revision+1
        WHERE workspace_id=$1 AND checkout_id=$2 AND requested_scope_id=$3 AND state IN ('requested','offered')")
        .bind(workspace.0).bind(checkout).bind(scope).execute(&mut **tx).await?;
    Ok(())
}

pub(super) async fn expire_waiting_run(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
    run: RunId,
) -> Result<(), IdentityError> {
    sqlx::query("UPDATE path_claim_handoffs SET state='expired', revision=revision+1
        WHERE workspace_id=$1 AND checkout_id=$2 AND requested_run_id=$3 AND state IN ('requested','offered')")
        .bind(workspace.0).bind(checkout).bind(run.0).execute(&mut **tx).await?;
    Ok(())
}

pub(super) async fn expire_waiting_grant(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
    grant: Uuid,
) -> Result<(), IdentityError> {
    sqlx::query("UPDATE path_claim_handoffs h SET state='expired', revision=h.revision+1
        FROM work_scope_revisions s
        WHERE h.workspace_id=$1 AND h.checkout_id=$2 AND h.state IN ('requested','offered') AND s.grant_id=$3
          AND (s.workspace_id,s.scope_id,s.revision)=(h.workspace_id,h.requested_scope_id,h.scope_revision)")
        .bind(workspace.0).bind(checkout).bind(grant).execute(&mut **tx).await?;
    Ok(())
}

async fn expire_due(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
    now: DateTime<Utc>,
) -> Result<(), IdentityError> {
    sqlx::query("UPDATE path_claim_handoffs SET state='expired', revision=revision+1
        WHERE workspace_id=$1 AND checkout_id=$2 AND expires_at<=$3 AND state IN ('requested','offered')")
        .bind(workspace.0).bind(checkout).bind(now).execute(&mut **tx).await?;
    Ok(())
}

async fn get(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<HandoffRow, IdentityError> {
    sqlx::query_as("SELECT * FROM path_claim_handoffs WHERE workspace_id=$1 AND id=$2")
        .bind(workspace.0)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(IdentityError::Forbidden)
}

async fn notify(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    handoff: &PathHandoff,
    event: &str,
    recipient: Option<Recipient>,
) -> Result<i64, IdentityError> {
    append_event(
        tx,
        context,
        handoff.handoff_id,
        event,
        json!({
            "handoff_id": handoff.handoff_id, "checkout_id": handoff.checkout_id,
            "state": handoff.state, "revision": handoff.revision, "expires_at": handoff.expires_at,
        }),
        recipient,
    )
    .await
}
