use chrono::{DateTime, Utc};
use office_domain::{AgentId, BearerDigest, RunId, UserId, WorkspaceId};
use office_protocol::{
    AgentReportedCheck, ArtifactFormat, GetTaskResult, Recipient, ResultArtifact, ResultBinding,
    ResultReport, ResultReview, ReviewDecision, Run, TaskResult, TaskResultContent,
};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use super::{TaskRow, load_task, task_text};
use crate::{
    Store,
    events::append_workspace_event,
    identity::{IdentityError, WorkspaceContext, workspace_context},
};

pub(super) async fn notify_reviewer(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRow,
    sender: Recipient,
    run: &Run,
    revision: i64,
) -> Result<(), IdentityError> {
    let reviewer = match task.reviewer_user_id {
        Some(owner) => Recipient {
            user_id: UserId(owner),
            agent_id: task.reviewer_agent_id.map(AgentId),
        },
        None => Recipient {
            user_id: UserId(task.accountable_user_id),
            agent_id: task.accountable_agent_id.map(AgentId),
        },
    };
    if reviewer.agent_id.is_some() && reviewer != run.executor {
        append_workspace_event(tx, WorkspaceId(task.workspace_id), task.id, "task.review_requested",
            serde_json::json!({"task_id":task.id,"run_id":run.run_id,"revision":revision,"executor":run.executor}),
            Some((sender,reviewer))).await?;
    }
    Ok(())
}

const MAX_REVIEW_RETURNS: i64 = 3;

#[derive(sqlx::FromRow)]
struct ResultRow {
    id: Uuid,
    workspace_id: Uuid,
    task_id: Uuid,
    run_id: Uuid,
    revision: i64,
    author_user_id: Uuid,
    author_agent_id: Option<Uuid>,
    definition: Json<TaskResultContent>,
    explicit_binding: bool,
    result_sha256: String,
    criteria_sha256: String,
    submitted_at: DateTime<Utc>,
}

impl ResultRow {
    fn binding(&self) -> ResultBinding {
        ResultBinding {
            result_revision: self.revision,
            result_sha256: self.result_sha256.clone(),
            criteria_revision: self.definition.0.criteria_revision,
            criteria_sha256: self.criteria_sha256.clone(),
        }
    }
}

impl Store {
    pub async fn get_task_result(
        &self,
        digest: &BearerDigest,
        input: GetTaskResult,
    ) -> Result<TaskResult, IdentityError> {
        if input.revision.is_some_and(|revision| revision < 1) {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let result = task_result_for(&mut tx, context, input, None).await?;
        tx.commit().await?;
        Ok(result)
    }
}

pub(in crate::tasks) async fn task_result_for(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    input: GetTaskResult,
    exact_run: Option<RunId>,
) -> Result<TaskResult, IdentityError> {
    if input.workspace_id != context.access.workspace {
        return Err(IdentityError::Forbidden);
    }
    load_task(tx, context, input.task_id, false).await?;
    let row = sqlx::query_as::<_, ResultRow>(
        "SELECT * FROM task_result_revisions
        WHERE workspace_id=$1 AND task_id=$2 AND ($3::bigint IS NULL OR revision=$3)
        AND ($4::uuid IS NULL OR run_id=$4)
        ORDER BY revision DESC LIMIT 1",
    )
    .bind(input.workspace_id.0)
    .bind(input.task_id.0)
    .bind(input.revision)
    .bind(exact_run.map(|run| run.0))
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(IdentityError::InvalidTransition)?;
    let stored_review = sqlx::query_as::<_, (Uuid, Option<Uuid>, String, String, DateTime<Utc>)>(
        "SELECT reviewer_user_id,reviewer_agent_id,decision,reason,reviewed_at FROM task_result_reviews
         WHERE workspace_id=$1 AND task_id=$2 AND result_id=$3",
    ).bind(row.workspace_id).bind(row.task_id).bind(row.id).fetch_optional(&mut **tx).await?;
    let review = stored_review
        .map(|(user, agent, decision, reason, reviewed_at)| {
            let decision = match decision.as_str() {
                "accept" => ReviewDecision::Accept,
                "request_changes" => ReviewDecision::RequestChanges,
                _ => return Err(IdentityError::InvalidRole),
            };
            Ok(ResultReview {
                reviewer: Recipient {
                    user_id: UserId(user),
                    agent_id: agent.map(AgentId),
                },
                decision,
                reason,
                reviewed_at,
            })
        })
        .transpose()?;
    let binding = row.binding();
    let result = TaskResult {
        result_id: row.id,
        workspace_id: input.workspace_id,
        task_id: input.task_id,
        run_id: RunId(row.run_id),
        author: Recipient {
            user_id: UserId(row.author_user_id),
            agent_id: row.author_agent_id.map(AgentId),
        },
        binding,
        content: row.definition.0,
        review,
        submitted_at: row.submitted_at,
    };
    Ok(result)
}

pub(super) async fn capture(
    tx: &mut Transaction<'_, Postgres>,
    row: &TaskRow,
    run: &Run,
    summary: &str,
    report: Option<&ResultReport>,
) -> Result<(), IdentityError> {
    let (artifacts, agent_reported_checks) = match report {
        Some(report) => validate_report(report)?,
        None => (Vec::new(), Vec::new()),
    };
    let definition = TaskResultContent {
        summary: summary.into(),
        criteria_revision: row.criteria_revision,
        acceptance_criteria: row.criteria.0.clone(),
        artifacts,
        agent_reported_checks,
    };
    sqlx::query(
        "INSERT INTO task_result_revisions (id,workspace_id,task_id,run_id,revision,
        author_user_id,author_agent_id,definition,explicit_binding)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
    )
    .bind(Uuid::new_v4())
    .bind(row.workspace_id)
    .bind(row.id)
    .bind(run.run_id.0)
    .bind(run.attempt_number)
    .bind(run.executor.user_id.0)
    .bind(run.executor.agent_id.map(|id| id.0))
    .bind(Json(definition))
    .bind(report.is_some() || run.lease.is_some())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(super) async fn review(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: &TaskRow,
    run: &Run,
    decision: ReviewDecision,
    reason: &str,
    binding: Option<&ResultBinding>,
) -> Result<bool, IdentityError> {
    let result = sqlx::query_as::<_, ResultRow>(
        "SELECT * FROM task_result_revisions
        WHERE workspace_id=$1 AND task_id=$2 AND run_id=$3",
    )
    .bind(task.workspace_id)
    .bind(task.id)
    .bind(run.run_id.0)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(IdentityError::InvalidTransition)?;
    if result.definition.0.criteria_revision != task.criteria_revision
        || result.definition.0.acceptance_criteria != task.criteria.0
    {
        return Err(IdentityError::RevisionMismatch {
            current: task.criteria_revision,
        });
    }
    if result.explicit_binding && binding.is_none() {
        return Err(IdentityError::InvalidInput);
    }
    if binding.is_some_and(|binding| *binding != result.binding()) {
        return Err(IdentityError::RevisionMismatch {
            current: result.revision,
        });
    }
    sqlx::query("SELECT set_config('office.review_result_context',$1,true)")
        .bind(result.id.to_string())
        .execute(&mut **tx)
        .await?;
    let stored = sqlx::query(
        "INSERT INTO task_result_reviews
        (result_id,workspace_id,task_id,reviewer_user_id,reviewer_agent_id,decision,reason)
        VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING",
    )
    .bind(result.id)
    .bind(task.workspace_id)
    .bind(task.id)
    .bind(context.access.member.0)
    .bind(context.agent_id().map(|id| id.0))
    .bind(match decision {
        ReviewDecision::Accept => "accept",
        ReviewDecision::RequestChanges => "request_changes",
    })
    .bind(reason)
    .execute(&mut **tx)
    .await?;
    if stored.rows_affected() != 1 {
        return Err(IdentityError::InvalidTransition);
    }
    if decision == ReviewDecision::Accept {
        return Ok(false);
    }
    let returns = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM task_result_reviews review
        JOIN task_result_revisions result ON (result.workspace_id,result.task_id,result.id)=
             (review.workspace_id,review.task_id,review.result_id)
        WHERE review.workspace_id=$1 AND review.task_id=$2 AND review.decision='request_changes'
           AND result.revision>$3",
    )
    .bind(task.workspace_id)
    .bind(task.id)
    .bind(task.review_cycle_after)
    .fetch_one(&mut **tx)
    .await?;
    Ok(returns >= MAX_REVIEW_RETURNS)
}

fn validate_report(
    report: &ResultReport,
) -> Result<(Vec<ResultArtifact>, Vec<AgentReportedCheck>), IdentityError> {
    if report.artifacts.len() > 8
        || report.agent_reported_checks.len() > 8
        || (report.artifacts.is_empty() && report.agent_reported_checks.is_empty())
    {
        return Err(IdentityError::InvalidInput);
    }
    let mut artifacts: Vec<ResultArtifact> = Vec::with_capacity(report.artifacts.len());
    let mut attachment_bytes = 0;
    for artifact in &report.artifacts {
        let name = task_text(&artifact.name, 128)?;
        attachment_bytes += artifact.content.len();
        if artifact.content.is_empty()
            || artifact.content.len() > 8192
            || attachment_bytes > 10_240
            || artifact.content.contains('\0')
            || artifacts.iter().any(|item| item.name == name)
            || (matches!(artifact.format, ArtifactFormat::Json)
                && serde_json::from_str::<serde_json::Value>(&artifact.content).is_err())
        {
            return Err(IdentityError::InvalidInput);
        }
        let content_sha256 = hex::encode(Sha256::digest(artifact.content.as_bytes()));
        artifacts.push(ResultArtifact {
            name: name.into(),
            format: artifact.format,
            content: artifact.content.clone(),
            content_sha256,
        });
    }
    let mut checks: Vec<AgentReportedCheck> =
        Vec::with_capacity(report.agent_reported_checks.len());
    for check in &report.agent_reported_checks {
        let name = task_text(&check.name, 128)?;
        let notes = check.notes.trim();
        if notes.len() > 1024 || notes.contains('\0') || checks.iter().any(|item| item.name == name)
        {
            return Err(IdentityError::InvalidInput);
        }
        checks.push(AgentReportedCheck {
            name: name.into(),
            outcome: check.outcome,
            notes: notes.into(),
        });
    }
    Ok((artifacts, checks))
}
