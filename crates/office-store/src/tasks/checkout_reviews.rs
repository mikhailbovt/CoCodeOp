use chrono::{DateTime, Utc};
use office_domain::{
    Action, AgentId, BearerDigest, Resource, RunId, TaskAuthority, TaskId, TaskStatus, TeamId,
    UserId, WorkspaceId,
};
use office_protocol::{
    ApproveCheckoutReview, ChangeTask, CheckoutCommandOutcome, CheckoutContribution,
    CheckoutReview, CheckoutReviewContent, CheckoutReviewReceipt, GetCheckoutReview, GetTaskResult,
    Recipient, ReviewDecision, ReviewPolicy, TaskChange, TaskResult, VerifiedCheckoutCheck,
    is_sha256,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use super::{
    TaskRow, checkpoint_snapshots::ready_snapshot, commands::change_task_for, load_task,
    results::task_result_for, task_text,
};
use crate::{
    Store,
    data_scope::subject,
    events::{Idempotency, append_event},
    identity::{IdentityError, WorkspaceContext, require_permission, workspace_context},
    teams::member_role,
};

const MAX_CHECKS: usize = 8;
const MAX_REVIEW_CONTRIBUTIONS: usize = 1024;

#[derive(sqlx::FromRow)]
struct ContributionRow {
    id: Uuid,
    checkout_revision: i64,
    task_id: Uuid,
    run_id: Uuid,
    executor_user_id: Uuid,
    worker_agent_id: Uuid,
    delta_sha256: String,
}

impl From<ContributionRow> for CheckoutContribution {
    fn from(row: ContributionRow) -> Self {
        Self {
            change_id: row.id,
            checkout_revision: row.checkout_revision,
            task_id: TaskId(row.task_id),
            run_id: RunId(row.run_id),
            author: Recipient {
                user_id: UserId(row.executor_user_id),
                agent_id: Some(AgentId(row.worker_agent_id)),
            },
            delta_sha256: row.delta_sha256,
        }
    }
}

#[derive(sqlx::FromRow)]
struct CommandRow {
    id: Uuid,
    template_id: Uuid,
    run_id: Uuid,
    outcome: Json<CheckoutCommandOutcome>,
}

impl Store {
    pub async fn get_checkout_review(
        &self,
        digest: &BearerDigest,
        input: GetCheckoutReview,
    ) -> Result<CheckoutReview, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let (task, _) = load_task(&mut tx, context, input.task_id, false).await?;
        let result = reviewable_result(&mut tx, context, &task).await?;
        let review = review_content(
            &mut tx,
            &task,
            &result,
            input.snapshot_id,
            &input.command_ids,
        )
        .await?;
        tx.commit().await?;
        Ok(review)
    }

    pub async fn approve_checkout_review(
        &self,
        digest: &BearerDigest,
        input: ApproveCheckoutReview,
    ) -> Result<CheckoutReviewReceipt, IdentityError> {
        let reason = task_text(&input.reason, 4096)?;
        if !is_sha256(&input.revision_sha256) {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "checkout.review_snapshot",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let (task, _) = load_task(&mut tx, context, input.task_id, true).await?;
        let result = reviewable_result(&mut tx, context, &task).await?;
        let review = review_content(
            &mut tx,
            &task,
            &result,
            input.snapshot_id,
            &input.command_ids,
        )
        .await?;
        if review.revision_sha256 != input.revision_sha256 {
            return Err(IdentityError::RevisionConflict);
        }
        if matches!(task.status()?, TaskStatus::InReview) {
            change_task_for(
                &mut tx,
                context,
                ChangeTask {
                    workspace_id: input.workspace_id,
                    task_id: input.task_id,
                    expected_revision: task.revision,
                    idempotency_key: input.idempotency_key,
                    change: TaskChange::Review {
                        run_id: result.run_id,
                        decision: ReviewDecision::Accept,
                        reason: reason.into(),
                        binding: Some(result.binding),
                    },
                },
            )
            .await?;
        } else if result.review.is_none_or(|review| {
            review.reviewer != context.recipient() || review.decision != ReviewDecision::Accept
        }) {
            return Err(IdentityError::Forbidden);
        }
        let timing: DateTime<Utc> = sqlx::query_scalar(
            "INSERT INTO checkout_snapshot_reviews (id,workspace_id,checkout_id,snapshot_id,task_id,result_id,
              reviewer_user_id,reviewer_agent_id,approval_sha256,definition)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
             ON CONFLICT (workspace_id,snapshot_id,task_id,approval_sha256) DO NOTHING RETURNING approved_at",
        ).bind(Uuid::new_v4()).bind(input.workspace_id.0).bind(review.content.checkout_id).bind(input.snapshot_id)
         .bind(input.task_id.0).bind(result.result_id).bind(context.access.member.0)
         .bind(context.agent_id().map(|agent| agent.0)).bind(&review.revision_sha256).bind(Json(&review.content))
         .fetch_optional(&mut *tx).await?.ok_or(IdentityError::IdempotencyConflict)?;
        append_event(&mut tx, context, input.task_id.0, "task.snapshot_reviewed",
            json!({"task_id":input.task_id,"snapshot_id":input.snapshot_id,"result_id":result.result_id,
              "approval_sha256":review.revision_sha256}), None).await?;
        let receipt = CheckoutReviewReceipt {
            review,
            reviewer: context.recipient(),
            approved_at: timing,
        };
        idem.record(&mut tx, input.task_id.0, &receipt).await?;
        tx.commit().await?;
        Ok(receipt)
    }
}

async fn reviewable_result(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: &TaskRow,
) -> Result<TaskResult, IdentityError> {
    let result = task_result_for(
        tx,
        context,
        GetTaskResult {
            workspace_id: context.access.workspace,
            task_id: TaskId(task.id),
            revision: None,
        },
        None,
    )
    .await?;
    let explicit_reviewer = match task.review_policy() {
        ReviewPolicy::Accountable => None,
        ReviewPolicy::ExplicitReviewer { reviewer } => {
            Some(subject(reviewer.user_id, reviewer.agent_id.map(|id| id.0)))
        }
    };
    require_permission(
        context.actor,
        Action::ReviewTask,
        Resource::Task(TaskAuthority {
            workspace: context.access,
            accountable_owner: UserId(task.accountable_user_id),
            accountable: subject(UserId(task.accountable_user_id), task.accountable_agent_id),
            explicit_reviewer,
            executor: None,
            assignee: None,
        }),
    )?;
    if task.checkout_id.is_none()
        || task.awaiting_owner
        || task.review_escalated
        || !matches!(task.status()?, TaskStatus::InReview | TaskStatus::Done)
    {
        return Err(IdentityError::InvalidTransition);
    }
    if result.author == context.recipient() {
        return Err(IdentityError::Forbidden);
    }
    if let Some(team) = task.team_id
        && member_role(
            tx,
            context.access.workspace,
            TeamId(team),
            context.recipient(),
        )
        .await?
        .is_none()
    {
        return Err(IdentityError::Forbidden);
    }
    sqlx::query("SELECT set_config('office.checkout_review_task_id',$1,true)")
        .bind(task.id.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(result)
}

async fn review_content(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRow,
    result: &TaskResult,
    snapshot_id: Uuid,
    commands: &[Uuid],
) -> Result<CheckoutReview, IdentityError> {
    if !(1..=MAX_CHECKS).contains(&commands.len()) {
        return Err(IdentityError::InvalidInput);
    }
    let workspace = WorkspaceId(task.workspace_id);
    let checkout = task.checkout_id.ok_or(IdentityError::InvalidTransition)?;
    let (head, proof) = ready_snapshot(tx, workspace, checkout, snapshot_id).await?;
    let changes = sqlx::query_as::<_, ContributionRow>(
        "SELECT id,task_id,run_id,executor_user_id,worker_agent_id,
           (outcome->'head'->>'checkout_revision')::bigint AS checkout_revision,
           office_review_digest(outcome->'files') AS delta_sha256
         FROM checkout_publications WHERE (workspace_id,checkout_id,task_id)=($1,$2,$3) AND state='applied'
           AND (outcome->'head'->>'checkout_revision')::bigint<=$4
         ORDER BY checkout_revision,id LIMIT $5",
    ).bind(task.workspace_id).bind(checkout).bind(task.id).bind(head.checkout_revision)
     .bind(MAX_REVIEW_CONTRIBUTIONS as i64 + 1).fetch_all(&mut **tx).await?;
    if !(1..=MAX_REVIEW_CONTRIBUTIONS).contains(&changes.len()) {
        return Err(IdentityError::InvalidTransition);
    }
    let checks = verified_checks(
        tx,
        task,
        checkout,
        snapshot_id,
        result.author,
        &head.source_sha256,
        commands,
    )
    .await?;
    let content = CheckoutReviewContent {
        task_id: TaskId(task.id),
        task_revision: task.revision,
        result_run_id: result.run_id,
        result: result.binding.clone(),
        checkout_id: checkout,
        snapshot_id,
        head,
        git_tree_sha256: proof.git_tree_sha256,
        contributions: changes.into_iter().map(Into::into).collect(),
        checks,
    };
    let bytes = serde_json::to_vec(&content).map_err(|_| IdentityError::InvalidRole)?;
    Ok(CheckoutReview {
        revision_sha256: hex::encode(Sha256::digest(bytes)),
        content,
    })
}

async fn verified_checks(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRow,
    checkout: Uuid,
    snapshot: Uuid,
    author: Recipient,
    expected_source_sha256: &str,
    ids: &[Uuid],
) -> Result<Vec<VerifiedCheckoutCheck>, IdentityError> {
    let rows = sqlx::query_as::<_, CommandRow>(
        "SELECT id,template_id,run_id,outcome FROM checkout_commands WHERE (workspace_id,checkout_id,source_snapshot_id)=($1,$2,$3)
             AND state='finished' AND id=ANY($4) AND task_id<>$5 AND (executor_user_id,worker_agent_id) IS DISTINCT FROM ($6,$7)
             AND NOT coalesce((spec->>'writable_staging')::boolean,false) ORDER BY id",
    ).bind(task.workspace_id).bind(checkout).bind(snapshot).bind(ids).bind(task.id)
     .bind(author.user_id.0).bind(author.agent_id.map(|agent| agent.0)).fetch_all(&mut **tx).await?;
    if rows.len() != ids.len() {
        return Err(IdentityError::InvalidInput);
    }
    let mut checks = Vec::with_capacity(rows.len());
    for row in rows {
        let CheckoutCommandOutcome::Exited {
            snapshot: observed,
            exit_code: 0,
            stdout,
            stderr,
            log_truncated,
            ..
        } = row.outcome.0
        else {
            return Err(IdentityError::InvalidTransition);
        };
        if observed.source_snapshot_id != Some(snapshot)
            || observed.sha256 != expected_source_sha256
        {
            return Err(IdentityError::ContextStale);
        }
        let logs = serde_json::to_vec(&(stdout, stderr, log_truncated))
            .map_err(|_| IdentityError::InvalidRole)?;
        checks.push(VerifiedCheckoutCheck {
            command_id: row.id,
            template_id: row.template_id,
            run_id: RunId(row.run_id),
            source_snapshot_id: snapshot,
            source_sha256: observed.sha256,
            environment: observed
                .environment
                .ok_or(IdentityError::InvalidTransition)?,
            exit_code: 0,
            logs_sha256: hex::encode(Sha256::digest(logs)),
            log_truncated,
        });
    }
    Ok(checks)
}
