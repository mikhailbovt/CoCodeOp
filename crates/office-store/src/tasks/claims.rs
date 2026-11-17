use chrono::{DateTime, Duration, Utc};
use office_domain::{
    AgentId, BearerDigest, PathRule, RunId, RunStatus, SourceManifest, TaskId, TaskStatus, UserId,
    WorkspaceId,
};
use office_protocol::{
    ApplyCheckoutChanges, ClaimPaths, ListPathClaims, PathClaim, PathClaimPage, PathClaimReceipt,
    PathClaimState, Recipient, ReleasePathClaim, RenewPathClaim, WorkScopeDefinition,
};
use serde_json::json;
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use super::{
    TaskRow, delegation, execution::start_effects::credentials::authenticate_run, load_task, runs,
    scopes,
};
use crate::{
    Store,
    checkouts::lock_policy,
    events::{Idempotency, append_workspace_event},
    identity::{IdentityError, WorkspaceContext, workspace_context},
    inbox::save_cursor,
};

const MAX_RENEWALS: i16 = 2;
const LIFETIME: Duration = Duration::minutes(5);

mod handoffs;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClaimSession {
    Agent,
    ManagedRun,
}

#[derive(sqlx::FromRow)]
struct ClaimRow {
    id: Uuid,
    workspace_id: Uuid,
    checkout_id: Uuid,
    task_id: Uuid,
    run_id: Uuid,
    scope_id: Uuid,
    scope_revision: i64,
    worker_user_id: Uuid,
    worker_agent_id: Uuid,
    fence: i64,
    paths: Json<Vec<PathRule>>,
    started_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    renewals: i16,
    state: String,
}

impl ClaimRow {
    fn worker(&self) -> Recipient {
        Recipient {
            user_id: UserId(self.worker_user_id),
            agent_id: Some(AgentId(self.worker_agent_id)),
        }
    }

    fn into_claim(self, now: DateTime<Utc>) -> Result<PathClaim, IdentityError> {
        let state = match self.state.as_str() {
            "active" if self.expires_at <= now => PathClaimState::Expired,
            "active" => PathClaimState::Active,
            "released" => PathClaimState::Released,
            "expired" => PathClaimState::Expired,
            "fenced" => PathClaimState::Fenced,
            _ => return Err(IdentityError::InvalidRole),
        };
        let worker = self.worker();
        Ok(PathClaim {
            claim_id: self.id,
            checkout_id: self.checkout_id,
            task_id: TaskId(self.task_id),
            run_id: RunId(self.run_id),
            scope_id: self.scope_id,
            scope_revision: self.scope_revision,
            worker,
            claim_fence: self.fence,
            paths: self.paths.0,
            started_at: self.started_at,
            expires_at: self.expires_at,
            renewals: self.renewals,
            state,
        })
    }
}

impl Store {
    pub async fn claim_paths(
        &self,
        digest: &BearerDigest,
        input: ClaimPaths,
    ) -> Result<PathClaimReceipt, IdentityError> {
        self.claim_paths_as(digest, input, ClaimSession::Agent)
            .await
    }

    pub async fn claim_run_paths(
        &self,
        digest: &BearerDigest,
        input: ClaimPaths,
    ) -> Result<PathClaimReceipt, IdentityError> {
        self.claim_paths_as(digest, input, ClaimSession::ManagedRun)
            .await
    }

    async fn claim_paths_as(
        &self,
        digest: &BearerDigest,
        input: ClaimPaths,
        session: ClaimSession,
    ) -> Result<PathClaimReceipt, IdentityError> {
        let ttl = ttl(input.ttl_seconds)?;
        validate_paths(&input.paths)?;
        let mut tx = self.pool.begin().await?;
        let (context, run_until) = claim_context(
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
            "path.claim",
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
        bind_index(&mut tx, Some(scope.checkout_id)).await?;
        sqlx::query(
            "UPDATE path_claims SET state='expired'
           WHERE workspace_id=$1 AND checkout_id=$2 AND state='active' AND expires_at<=$3",
        )
        .bind(input.workspace_id.0)
        .bind(scope.checkout_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let current = active_claim(
            &mut tx,
            input.workspace_id,
            input.task_id,
            input.run_id,
            now,
        )
        .await?;
        if input.expected_fence != current.as_ref().map(|row| row.fence) {
            return Err(IdentityError::ClaimFenceStale);
        }
        if let Some(claim) = &current {
            handoffs::prevent_extension(&mut tx, claim, now).await?;
        }
        handoffs::verify_reservation(&mut tx, &scope, &input, now).await?;
        let conflicted = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM path_claims
             WHERE workspace_id=$1 AND checkout_id=$2 AND state='active' AND expires_at>$3
             AND run_id<>$4 AND bounds && office_claim_ranges($5))",
        )
        .bind(input.workspace_id.0)
        .bind(scope.checkout_id)
        .bind(now)
        .bind(input.run_id.0)
        .bind(Json(&input.paths))
        .fetch_one(&mut *tx)
        .await?;
        if conflicted {
            return Err(IdentityError::PathClaimed);
        }
        let started_at = current.as_ref().map_or(now, |row| row.started_at);
        let renewals = current.as_ref().map_or(0, |row| row.renewals);
        let expires_at = (now + ttl)
            .min(started_at + LIFETIME)
            .min(scope.expires_at)
            .min(run_until.unwrap_or(scope.expires_at));
        if expires_at <= now {
            return Err(IdentityError::ClaimRenewalLimit);
        }
        if let Some(mut former) = current {
            if former.worker() != context.recipient() {
                return Err(IdentityError::Forbidden);
            }
            sqlx::query("UPDATE path_claims SET state='fenced' WHERE id=$1")
                .bind(former.id)
                .execute(&mut *tx)
                .await?;
            former.state = "fenced".into();
            publish(&mut tx, &former, "path.released").await?;
        }
        let row = sqlx::query_as::<_, ClaimRow>(
            "INSERT INTO path_claims
             (id,workspace_id,checkout_id,task_id,run_id,scope_id,scope_revision,
              worker_user_id,worker_agent_id,paths,started_at,expires_at,renewals)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) RETURNING *",
        )
        .bind(Uuid::new_v4())
        .bind(scope.workspace_id.0)
        .bind(scope.checkout_id)
        .bind(scope.task_id.0)
        .bind(input.run_id.0)
        .bind(scope.scope_id)
        .bind(scope.revision)
        .bind(context.access.member.0)
        .bind(context.agent_id().map(|id| id.0))
        .bind(Json(input.paths))
        .bind(started_at)
        .bind(expires_at)
        .bind(renewals)
        .fetch_one(&mut *tx)
        .await?;
        if let Some(offer) = input.handoff_id {
            handoffs::accept_offer(&mut tx, context, offer, row.fence, now).await?;
        }
        bind_index(&mut tx, None).await?;
        let receipt = record(&mut tx, context, &idem, row, "path.claimed", now).await?;
        tx.commit().await?;
        Ok(receipt)
    }

    pub async fn renew_path_claim(
        &self,
        digest: &BearerDigest,
        input: RenewPathClaim,
    ) -> Result<PathClaimReceipt, IdentityError> {
        self.renew_path_claim_as(digest, input, ClaimSession::Agent)
            .await
    }

    pub async fn renew_run_path_claim(
        &self,
        digest: &BearerDigest,
        input: RenewPathClaim,
    ) -> Result<PathClaimReceipt, IdentityError> {
        self.renew_path_claim_as(digest, input, ClaimSession::ManagedRun)
            .await
    }

    async fn renew_path_claim_as(
        &self,
        digest: &BearerDigest,
        input: RenewPathClaim,
        session: ClaimSession,
    ) -> Result<PathClaimReceipt, IdentityError> {
        let ttl = ttl(input.ttl_seconds)?;
        let mut tx = self.pool.begin().await?;
        let (context, run_until) = claim_context(
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
            "path.renew",
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
        let now = clock(&mut tx).await?;
        let claim = active_claim(
            &mut tx,
            input.workspace_id,
            input.task_id,
            input.run_id,
            now,
        )
        .await?
        .filter(|row| {
            row.worker() == context.recipient()
                && row.fence == input.claim_fence
                && row.scope_revision == scope.revision
        })
        .ok_or(IdentityError::ClaimFenceStale)?;
        if !claim
            .paths
            .0
            .iter()
            .all(|path| scope.permissions.permits_claim(&manifest, path))
        {
            return Err(IdentityError::AccessRequestRequired);
        }
        let expires = (now + ttl)
            .min(claim.started_at + LIFETIME)
            .min(scope.expires_at)
            .min(run_until.unwrap_or(scope.expires_at));
        if claim.renewals >= MAX_RENEWALS || expires <= claim.expires_at {
            return Err(IdentityError::ClaimRenewalLimit);
        }
        bind_index(&mut tx, Some(scope.checkout_id)).await?;
        handoffs::prevent_extension(&mut tx, &claim, now).await?;
        let row = sqlx::query_as::<_, ClaimRow>(
            "UPDATE path_claims SET expires_at=$2, renewals=renewals+1 WHERE id=$1 RETURNING *",
        )
        .bind(claim.id)
        .bind(expires)
        .fetch_one(&mut *tx)
        .await?;
        bind_index(&mut tx, None).await?;
        let receipt = record(&mut tx, context, &idem, row, "path.renewed", now).await?;
        tx.commit().await?;
        Ok(receipt)
    }

    pub async fn release_path_claim(
        &self,
        digest: &BearerDigest,
        input: ReleasePathClaim,
    ) -> Result<PathClaimReceipt, IdentityError> {
        self.release_path_claim_as(digest, input, ClaimSession::Agent)
            .await
    }

    pub async fn release_run_path_claim(
        &self,
        digest: &BearerDigest,
        input: ReleasePathClaim,
    ) -> Result<PathClaimReceipt, IdentityError> {
        self.release_path_claim_as(digest, input, ClaimSession::ManagedRun)
            .await
    }

    async fn release_path_claim_as(
        &self,
        digest: &BearerDigest,
        input: ReleasePathClaim,
        session: ClaimSession,
    ) -> Result<PathClaimReceipt, IdentityError> {
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
        if session == ClaimSession::Agent
            && runs::latest(&mut tx, input.workspace_id, input.task_id)
                .await?
                .is_some_and(|run| run.run_id == input.run_id && run.lease.is_some())
        {
            return Err(IdentityError::RunLeaseStale);
        }
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "path.release",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let claim = sqlx::query_as::<_, ClaimRow>(
            "SELECT * FROM path_claims
            WHERE workspace_id=$1 AND task_id=$2 AND run_id=$3 AND fence=$4",
        )
        .bind(input.workspace_id.0)
        .bind(input.task_id.0)
        .bind(input.run_id.0)
        .bind(input.claim_fence)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(IdentityError::ClaimFenceStale)?;
        if claim.worker() != context.recipient() {
            return Err(IdentityError::Forbidden);
        }
        lock_policy(&mut tx, claim.checkout_id).await?;
        bind_index(&mut tx, Some(claim.checkout_id)).await?;
        let now = clock(&mut tx).await?;
        let row = sqlx::query_as::<_, ClaimRow>(
            "UPDATE path_claims SET state='released'
            WHERE id=$1 AND state='active' AND expires_at>$2 RETURNING *",
        )
        .bind(claim.id)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(IdentityError::ClaimFenceStale)?;
        handoffs::expire_waiters(&mut tx, input.workspace_id, claim.checkout_id, claim.id).await?;
        bind_index(&mut tx, None).await?;
        let receipt = record(&mut tx, context, &idem, row, "path.released", now).await?;
        tx.commit().await?;
        Ok(receipt)
    }

    pub async fn list_path_claims(
        &self,
        digest: &BearerDigest,
        input: ListPathClaims,
    ) -> Result<PathClaimPage, IdentityError> {
        let limit = input.limit.unwrap_or(25);
        if !(1..=50).contains(&limit) || input.before_fence.is_some_and(|fence| fence < 1) {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        workspace_context(&mut tx, digest, input.workspace_id).await?;
        let now = clock(&mut tx).await?;
        let mut rows = sqlx::query_as::<_, ClaimRow>(
            "SELECT * FROM path_claims
            WHERE workspace_id=$1 AND checkout_id=$2 AND ($3::uuid IS NULL OR task_id=$3)
              AND ($4::bigint IS NULL OR fence<$4) ORDER BY fence DESC LIMIT $5",
        )
        .bind(input.workspace_id.0)
        .bind(input.checkout_id)
        .bind(input.task_id.map(|id| id.0))
        .bind(input.before_fence)
        .bind(i64::from(limit) + 1)
        .fetch_all(&mut *tx)
        .await?;
        let more = rows.len() > usize::from(limit);
        rows.truncate(usize::from(limit));
        let next_before_fence = rows.last().filter(|_| more).map(|row| row.fence);
        let claims = rows
            .into_iter()
            .map(|row| row.into_claim(now))
            .collect::<Result<_, _>>()?;
        tx.commit().await?;
        Ok(PathClaimPage {
            claims,
            next_before_fence,
        })
    }
}

async fn claim_context(
    tx: &mut Transaction<'_, Postgres>,
    digest: &BearerDigest,
    workspace: WorkspaceId,
    task: TaskId,
    run: RunId,
    session: ClaimSession,
) -> Result<(WorkspaceContext, Option<DateTime<Utc>>), IdentityError> {
    match session {
        ClaimSession::Agent => Ok((workspace_context(tx, digest, workspace).await?, None)),
        ClaimSession::ManagedRun => {
            let access = authenticate_run(tx, digest).await?;
            if !access.gateway_required
                || access.run_id != run
                || access.context.access.workspace != workspace
                || access.task.id != task.0
            {
                return Err(IdentityError::Forbidden);
            }
            Ok((access.context, Some(access.lease_until)))
        }
    }
}

async fn lock_claim_scope(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task_id: TaskId,
    run_id: RunId,
    session: ClaimSession,
) -> Result<(WorkScopeDefinition, SourceManifest), IdentityError> {
    let (task, _) = load_task(tx, context, task_id, true).await?;
    let run = runs::latest(tx, context.access.workspace, task_id)
        .await?
        .filter(|run| run.run_id == run_id && run.executor == context.recipient())
        .ok_or(IdentityError::Forbidden)?;
    if run.lease.is_some() != (session == ClaimSession::ManagedRun) {
        return Err(IdentityError::RunLeaseStale);
    }
    if run.status != RunStatus::Running
        || task.status()? != TaskStatus::InProgress
        || task.awaiting_owner
    {
        return Err(IdentityError::InvalidTransition);
    }
    let granted = scopes::lock_worker_scope(tx, context, &task).await?;
    delegation::require_active_task_grant(tx, context, &task).await?;
    Ok(granted)
}

fn validate_paths(paths: &[PathRule]) -> Result<(), IdentityError> {
    if !(1..=32).contains(&paths.len()) || paths.iter().any(|rule| matches!(rule, PathRule::Tree)) {
        return Err(IdentityError::InvalidInput);
    }
    Ok(())
}

fn admit_paths(
    scope: &WorkScopeDefinition,
    manifest: &SourceManifest,
    revision: i64,
    paths: &[PathRule],
) -> Result<(), IdentityError> {
    if revision != scope.revision {
        return Err(IdentityError::RevisionMismatch {
            current: scope.revision,
        });
    }
    if !paths
        .iter()
        .all(|path| scope.permissions.permits_claim(manifest, path))
    {
        return Err(IdentityError::AccessRequestRequired);
    }
    Ok(())
}

async fn active_claim(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    task: TaskId,
    run: RunId,
    now: DateTime<Utc>,
) -> Result<Option<ClaimRow>, IdentityError> {
    Ok(sqlx::query_as::<_, ClaimRow>(
        "SELECT * FROM path_claims
        WHERE workspace_id=$1 AND task_id=$2 AND run_id=$3 AND state='active' AND expires_at>$4",
    )
    .bind(workspace.0)
    .bind(task.0)
    .bind(run.0)
    .bind(now)
    .fetch_optional(&mut **tx)
    .await?)
}

pub(crate) async fn fence_scope(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
    scope: Uuid,
) -> Result<(), IdentityError> {
    bind_index(tx, Some(checkout)).await?;
    let rows = sqlx::query_as::<_, ClaimRow>(
        "UPDATE path_claims SET state = CASE WHEN expires_at <= clock_timestamp() THEN 'expired' ELSE 'fenced' END
         WHERE workspace_id=$1 AND checkout_id=$2 AND scope_id=$3 AND state='active' RETURNING *",
    )
    .bind(workspace.0).bind(checkout).bind(scope).fetch_all(&mut **tx).await?;
    handoffs::expire_waiting_scope(tx, workspace, checkout, scope).await?;
    for row in rows {
        handoffs::expire_waiters(tx, workspace, checkout, row.id).await?;
        publish(tx, &row, "path.released").await?;
    }
    bind_index(tx, None).await
}

pub(super) async fn fence_run(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRow,
    run: RunId,
) -> Result<(), IdentityError> {
    let Some(checkout) = task.checkout_id else {
        return Ok(());
    };
    lock_policy(tx, checkout).await?;
    bind_index(tx, Some(checkout)).await?;
    let closed = sqlx::query_as::<_, ClaimRow>(
        "UPDATE path_claims SET state = CASE WHEN expires_at <= clock_timestamp() THEN 'expired' ELSE 'fenced' END
         WHERE workspace_id=$1 AND checkout_id=$2 AND task_id=$3 AND run_id=$4 AND state='active' RETURNING *",
    )
    .bind(task.workspace_id).bind(checkout).bind(task.id).bind(run.0).fetch_optional(&mut **tx).await?;
    let workspace = WorkspaceId(task.workspace_id);
    handoffs::expire_waiting_run(tx, workspace, checkout, run).await?;
    if let Some(row) = closed {
        handoffs::expire_waiters(tx, workspace, checkout, row.id).await?;
        publish(tx, &row, "path.released").await?;
    }
    bind_index(tx, None).await
}

pub(crate) async fn fence_grant(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
    grant: Uuid,
) -> Result<(), IdentityError> {
    bind_index(tx, Some(checkout)).await?;
    let rows = sqlx::query_as::<_, ClaimRow>(
        "UPDATE path_claims c SET state = CASE WHEN c.expires_at <= clock_timestamp() THEN 'expired' ELSE 'fenced' END
         FROM work_scope_revisions s
         WHERE c.workspace_id=$1 AND c.checkout_id=$2 AND c.state='active' AND s.grant_id=$3
           AND (s.workspace_id,s.scope_id,s.revision)=(c.workspace_id,c.scope_id,c.scope_revision)
         RETURNING c.*",
    )
    .bind(workspace.0).bind(checkout).bind(grant).fetch_all(&mut **tx).await?;
    handoffs::expire_waiting_grant(tx, workspace, checkout, grant).await?;
    for row in rows {
        handoffs::expire_waiters(tx, workspace, checkout, row.id).await?;
        publish(tx, &row, "path.released").await?;
    }
    bind_index(tx, None).await
}

async fn bind_index(
    tx: &mut Transaction<'_, Postgres>,
    checkout: Option<Uuid>,
) -> Result<(), IdentityError> {
    sqlx::query("SELECT set_config('office.claim_index_context',$1,true)")
        .bind(checkout.map_or_else(String::new, |id| id.to_string()))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub(super) async fn publication_claim(
    tx: &mut Transaction<'_, Postgres>,
    scope: &WorkScopeDefinition,
    run_id: RunId,
    input: &ApplyCheckoutChanges,
) -> Result<(i64, DateTime<Utc>), IdentityError> {
    let now = clock(tx).await?;
    let workspace = scope.workspace_id;
    let paths = input.claims();
    bind_index(tx, Some(scope.checkout_id)).await?;
    let current = active_claim(tx, workspace, scope.task_id, run_id, now).await?;
    let result = match (current, input.claim_fence) {
        (Some(claim), Some(fence))
            if claim.fence == fence
                && claim.scope_id == scope.scope_id
                && claim.scope_revision == scope.revision
                && claim.worker() == scope.worker
                && paths
                    .iter()
                    .all(|path| claim.paths.0.iter().any(|rule| rule.covers(path))) =>
        {
            (fence, claim.expires_at)
        }
        (None, None) => reserve_publication_paths(tx, scope, run_id, input, paths, now).await?,
        _ => return Err(IdentityError::ClaimFenceStale),
    };
    bind_index(tx, None).await?;
    Ok(result)
}

async fn reserve_publication_paths(
    tx: &mut Transaction<'_, Postgres>,
    scope: &WorkScopeDefinition,
    run_id: RunId,
    input: &ApplyCheckoutChanges,
    paths: Vec<PathRule>,
    now: DateTime<Utc>,
) -> Result<(i64, DateTime<Utc>), IdentityError> {
    let workspace = scope.workspace_id.0;
    let claim = ClaimPaths {
        workspace_id: scope.workspace_id,
        task_id: scope.task_id,
        run_id,
        scope_revision: scope.revision,
        expected_fence: None,
        handoff_id: None,
        paths,
        ttl_seconds: 10,
        idempotency_key: input.idempotency_key,
    };
    handoffs::verify_reservation(tx, scope, &claim, now).await?;
    let conflict: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM path_claims WHERE workspace_id=$1 AND checkout_id=$2
         AND state='active' AND expires_at>$3 AND bounds && office_claim_ranges($4))",
    )
    .bind(workspace)
    .bind(scope.checkout_id)
    .bind(now)
    .bind(Json(&claim.paths))
    .fetch_one(&mut **tx)
    .await?;
    if conflict {
        return Err(IdentityError::PathClaimed);
    }
    sqlx::query(
        "UPDATE path_claims SET state='expired' WHERE workspace_id=$1 AND checkout_id=$2
         AND run_id=$3 AND state='active' AND expires_at<=$4",
    )
    .bind(workspace)
    .bind(scope.checkout_id)
    .bind(run_id.0)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    let expires = (now + Duration::seconds(10)).min(scope.expires_at);
    let fence = sqlx::query_scalar(
        "INSERT INTO path_claims (id,workspace_id,checkout_id,task_id,run_id,scope_id,scope_revision,
          worker_user_id,worker_agent_id,paths,started_at,expires_at)
          VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) RETURNING fence",
    )
    .bind(Uuid::new_v4()).bind(workspace).bind(scope.checkout_id).bind(scope.task_id.0).bind(run_id.0)
    .bind(scope.scope_id).bind(scope.revision).bind(scope.worker.user_id.0)
    .bind(scope.worker.agent_id.ok_or(IdentityError::InvalidRole)?.0)
    .bind(Json(&claim.paths)).bind(now).bind(expires).fetch_one(&mut **tx).await?;
    Ok((fence, expires))
}

pub(super) async fn release_publication_claim(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout_id: Uuid,
    fence: i64,
) -> Result<(), IdentityError> {
    bind_index(tx, Some(checkout_id)).await?;
    sqlx::query(
        "UPDATE path_claims SET state='released' WHERE workspace_id=$1 AND checkout_id=$2
         AND fence=$3 AND state='active'",
    )
    .bind(workspace.0)
    .bind(checkout_id)
    .bind(fence)
    .execute(&mut **tx)
    .await?;
    bind_index(tx, None).await
}

async fn publish(
    tx: &mut Transaction<'_, Postgres>,
    row: &ClaimRow,
    event: &str,
) -> Result<i64, IdentityError> {
    append_workspace_event(tx, WorkspaceId(row.workspace_id), row.scope_id, event, json!({
        "claim_id": row.id, "task_id": row.task_id, "run_id": row.run_id,
        "checkout_id": row.checkout_id, "claim_fence": row.fence, "scope_revision": row.scope_revision,
        "state": row.state, "expires_at": row.expires_at,
    }), None).await
}

async fn record(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    idem: &Idempotency,
    row: ClaimRow,
    event: &str,
    now: DateTime<Utc>,
) -> Result<PathClaimReceipt, IdentityError> {
    let sequence = publish(tx, &row, event).await?;
    let scope = row.scope_id;
    let receipt = PathClaimReceipt {
        claim: row.into_claim(now)?,
        event_cursor: save_cursor(tx, context, sequence).await?,
    };
    idem.record(tx, scope, &receipt).await?;
    Ok(receipt)
}

async fn clock(tx: &mut Transaction<'_, Postgres>) -> Result<DateTime<Utc>, IdentityError> {
    Ok(sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?)
}

fn ttl(seconds: u16) -> Result<Duration, IdentityError> {
    if !(5..=120).contains(&seconds) {
        return Err(IdentityError::InvalidInput);
    }
    Ok(Duration::seconds(i64::from(seconds)))
}
