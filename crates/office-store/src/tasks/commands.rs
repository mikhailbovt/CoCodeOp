use office_domain::{
    Action, Actor, BearerDigest, Resource, RunStatus, TaskAuthority, TaskId, TaskStatus, TeamId,
    TeamRole, UserId,
};
use office_protocol::{
    ChangeTask, Recipient, ReviewDecision, ReviewPolicy, Run, TaskChange, TaskMutation,
};
use sqlx::{Postgres, Transaction, types::Json};

use super::{
    TaskRow, claims, delegation, dependencies, execution, load_task, public_task_reference,
    record_change, results, runs, scopes, task_criteria, task_text,
};
use crate::{
    Store,
    data_scope::subject,
    events::Idempotency,
    identity::{
        IdentityError, WorkspaceContext, bind_subject, require_permission, workspace_context,
    },
    teams::member_role,
};

impl Store {
    pub async fn change_task(
        &self,
        digest: &BearerDigest,
        input: ChangeTask,
    ) -> Result<TaskMutation, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let result = change_task_for(&mut tx, context, input).await?;
        tx.commit().await?;
        Ok(result)
    }
}

pub(in crate::tasks) async fn change_task_for(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    input: ChangeTask,
) -> Result<TaskMutation, IdentityError> {
    if input.workspace_id != context.access.workspace
        || !(1..i64::MAX).contains(&input.expected_revision)
    {
        return Err(IdentityError::InvalidInput);
    }
    let idem = Idempotency::lock(tx, context, "task.change", input.idempotency_key, &input).await?;
    if let Some(previous) = idem.replay(tx).await? {
        return Ok(previous);
    }
    if matches!(&input.change, TaskChange::AddDependency { .. }) {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("task-dependencies/{}", input.workspace_id.0))
            .execute(&mut **tx)
            .await?;
    }
    let (row, scope) = load_task(tx, context, input.task_id, true).await?;
    if row.revision != input.expected_revision {
        return Err(IdentityError::RevisionMismatch {
            current: row.revision,
        });
    }
    if row.awaiting_owner
        && !matches!(
            input.change,
            TaskChange::Cancel { .. }
                | TaskChange::AcknowledgeCancel { .. }
                | TaskChange::Fail { .. }
        )
    {
        return Err(IdentityError::InvalidTransition);
    }
    if row.review_escalated
        && !matches!(
            input.change,
            TaskChange::Cancel { .. } | TaskChange::ResolveReviewEscalation { .. }
        )
    {
        return Err(IdentityError::InvalidTransition);
    }
    if let Some(team) = row.team_id.map(TeamId)
        && !matches!(
            input.change,
            TaskChange::AcknowledgeCancel { .. } | TaskChange::ResolveReviewEscalation { .. }
        )
    {
        let role = member_role(tx, input.workspace_id, team, context.recipient()).await?;
        if role.is_none()
            || (matches!(input.change, TaskChange::Claim { .. }) && !worker_role(role))
        {
            return Err(IdentityError::Forbidden);
        }
    }
    let previous_run = runs::latest(tx, input.workspace_id, input.task_id).await?;
    if previous_run
        .as_ref()
        .is_some_and(|run| run.lease.is_some() && run.status != RunStatus::Starting)
        && matches!(
            input.change,
            TaskChange::Progress { .. }
                | TaskChange::RequestHelp { .. }
                | TaskChange::Resume
                | TaskChange::Submit { .. }
                | TaskChange::Fail { .. }
                | TaskChange::AcknowledgeCancel { .. }
        )
    {
        return Err(IdentityError::RunLeaseStale);
    }
    let reviewer = match row.review_policy() {
        ReviewPolicy::Accountable => None,
        ReviewPolicy::ExplicitReviewer { reviewer } => Some(reviewer),
    };
    let assignee = row.assignee_user_id.map(|id| Recipient {
        user_id: UserId(id),
        agent_id: row.assignee_agent_id.map(office_domain::AgentId),
    });
    require_permission(
        context.actor,
        action(&input.change),
        Resource::Task(TaskAuthority {
            workspace: context.access,
            accountable_owner: UserId(row.accountable_user_id),
            accountable: subject(UserId(row.accountable_user_id), row.accountable_agent_id),
            assignee: assignee.map(principal),
            executor: previous_run.as_ref().map(|run| principal(run.executor)),
            explicit_reviewer: reviewer.map(principal),
        }),
    )?;
    let status = row
        .status()?
        .transition(input.change.transition())
        .ok_or(IdentityError::InvalidTransition)?;
    if matches!(
        input.change,
        TaskChange::Claim { .. } | TaskChange::Assign { .. } | TaskChange::ReviseCriteria { .. }
    ) {
        execution::ensure_idle(tx, &row).await?;
    }
    scopes::authorize_task_change(tx, context, &row, &input.change).await?;
    delegation::check_task_command(tx, context, &row, &input.change).await?;
    let effect = apply(tx, context, &row, &input.change, assignee, previous_run).await?;
    if let Some(run) = &effect.run
        && run.status != RunStatus::Running
    {
        claims::fence_run(tx, &row, run.run_id).await?;
    }
    let status = if effect.review_escalated == Some(true) {
        TaskStatus::Blocked
    } else {
        status
    };
    let row = sqlx::query_as::<_, TaskRow>(
        "UPDATE tasks SET status = $3, revision = revision + 1, updated_at = clock_timestamp(),
              assignee_user_id = $4, assignee_agent_id = $5,
              awaiting_owner = awaiting_owner AND $3 NOT IN ('cancelled', 'failed'),
              criteria = coalesce($6, criteria),
              criteria_revision = criteria_revision + CASE WHEN $6::jsonb IS NULL THEN 0 ELSE 1 END,
              review_escalated = coalesce($7, review_escalated) AND $3 <> 'cancelled',
              review_cycle_after = coalesce($8, review_cycle_after)
         WHERE workspace_id = $1 AND id = $2 RETURNING *",
    )
    .bind(input.workspace_id.0)
    .bind(input.task_id.0)
    .bind(status.as_str())
    .bind(effect.assignee.map(|target| target.user_id.0))
    .bind(
        effect
            .assignee
            .and_then(|target| target.agent_id.map(|id| id.0)),
    )
    .bind(effect.criteria.as_ref().map(Json))
    .bind(effect.review_escalated)
    .bind(effect.review_cycle_after)
    .fetch_one(&mut **tx)
    .await?;
    if effect.event == "task.result_submitted"
        && let Some(run) = effect.run.as_ref()
    {
        results::notify_reviewer(tx, &row, context.recipient(), run, row.revision).await?;
    }
    let task = row.into_task(tx, scope).await?;
    let mutation = record_change(
        tx,
        context,
        &idem,
        task,
        effect.run,
        effect.event,
        effect.detail,
    )
    .await?;
    Ok(mutation)
}

fn principal(recipient: Recipient) -> office_domain::Subject {
    subject(recipient.user_id, recipient.agent_id.map(|id| id.0))
}

fn action(change: &TaskChange) -> Action {
    match change {
        TaskChange::Claim { .. } => Action::ClaimTask,
        TaskChange::Progress { .. }
        | TaskChange::RequestHelp { .. }
        | TaskChange::Submit { .. }
        | TaskChange::Fail { .. }
        | TaskChange::AcknowledgeCancel { .. } => Action::UpdateRun,
        TaskChange::Review { .. } => Action::ReviewTask,
        _ => Action::ManageTask,
    }
}

struct Effect<'a> {
    assignee: Option<Recipient>,
    run: Option<Run>,
    event: &'static str,
    detail: Option<&'a str>,
    criteria: Option<Vec<&'a str>>,
    review_escalated: Option<bool>,
    review_cycle_after: Option<i64>,
}

async fn apply<'a>(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    row: &TaskRow,
    change: &'a TaskChange,
    mut assignee: Option<Recipient>,
    previous: Option<Run>,
) -> Result<Effect<'a>, IdentityError> {
    let mut detail = None;
    let mut run = None;
    let mut criteria = None;
    let mut review_escalated = None;
    let mut review_cycle_after = None;
    let task = TaskId(row.id);
    if matches!(
        change,
        TaskChange::Progress { .. }
            | TaskChange::RequestHelp { .. }
            | TaskChange::Resume
            | TaskChange::Submit { .. }
    ) && let Some(run) = &previous
    {
        crate::execution_control::require_live_run(tx, context.access.workspace, run.run_id)
            .await?;
    }
    let event = match change {
        TaskChange::AddDependency { prerequisite_id } => {
            add_dependency(tx, context, task, *prerequisite_id).await?;
            "task.dependency_added"
        }
        TaskChange::ReviseCriteria {
            expected_criteria_revision,
            acceptance_criteria,
        } => {
            if *expected_criteria_revision != row.criteria_revision {
                return Err(IdentityError::RevisionMismatch {
                    current: row.criteria_revision,
                });
            }
            criteria = Some(task_criteria(acceptance_criteria)?);
            "task.criteria_revised"
        }
        TaskChange::ResolveReviewEscalation { reason } => {
            if context.actor != Actor::Human(UserId(row.accountable_user_id)) {
                return Err(IdentityError::Forbidden);
            }
            if !row.review_escalated {
                return Err(IdentityError::InvalidTransition);
            }
            let latest = current(previous, &[RunStatus::Succeeded])?;
            review_escalated = Some(false);
            review_cycle_after = Some(latest.attempt_number);
            detail = Some(task_text(reason, 4096)?);
            "task.review_escalation_resolved"
        }
        TaskChange::Prepare => {
            runnable(tx, context, row).await?;
            assignee = None;
            "task.ready"
        }
        TaskChange::Assign { agent_id } => {
            let exists = sqlx::query_scalar::<_, bool>(
                "SELECT true FROM agents WHERE workspace_id = $1 AND id = $2 AND owner_user_id = $3
                 AND disabled_at IS NULL AND role IN ('lead', 'worker') FOR SHARE",
            )
            .bind(row.workspace_id)
            .bind(agent_id.0)
            .bind(row.accountable_user_id)
            .fetch_optional(&mut **tx)
            .await?;
            if exists.is_none() {
                return Err(IdentityError::Forbidden);
            }
            let target = Recipient {
                user_id: UserId(row.accountable_user_id),
                agent_id: Some(*agent_id),
            };
            if let Some(team) = row.team_id.map(TeamId) {
                bind_subject(tx, target).await?;
                let role = member_role(tx, context.access.workspace, team, target).await?;
                bind_subject(tx, context.recipient()).await?;
                if !worker_role(role) {
                    return Err(IdentityError::Forbidden);
                }
            }
            assignee = Some(target);
            "task.assigned"
        }
        TaskChange::Claim {
            instance_id,
            runtime_template_id,
        } => {
            runnable(tx, context, row).await?;
            let previous_attempt = previous.as_ref().map_or(0, |run| run.attempt_number);
            let active = runs::start(
                tx,
                context,
                task,
                *instance_id,
                *runtime_template_id,
                previous_attempt,
            )
            .await?;
            assignee = Some(active.executor);
            run = Some(active);
            "task.claimed"
        }
        TaskChange::Progress { message } => {
            detail = Some(task_text(message, 4096)?);
            run = Some(current(previous, &[RunStatus::Running])?);
            "task.progress"
        }
        TaskChange::RequestHelp {
            message,
            wait_seconds,
        } => {
            let seconds = wait_seconds.unwrap_or(900);
            if !(1..=86_400).contains(&seconds) {
                return Err(IdentityError::InvalidInput);
            }
            detail = Some(task_text(message, 4096)?);
            let active = current(previous, &[RunStatus::Running])?;
            run = Some(
                runs::set_status(tx, &active, RunStatus::WaitingInput, None, Some(seconds)).await?,
            );
            "task.help_requested"
        }
        TaskChange::Resume => {
            let active = current(previous, &[RunStatus::WaitingInput])?;
            run = Some(runs::set_status(tx, &active, RunStatus::Running, None, None).await?);
            "task.resumed"
        }
        TaskChange::Submit { summary, report } => {
            let active = current(previous, &[RunStatus::Running])?;
            let summary = task_text(summary, 4096)?;
            let submitted =
                runs::set_status(tx, &active, RunStatus::Succeeded, Some(summary), None).await?;
            results::capture(tx, row, &submitted, summary, report.as_ref()).await?;
            run = Some(submitted);
            "task.result_submitted"
        }
        TaskChange::Review {
            run_id,
            decision,
            reason,
            binding,
        } => {
            let submitted = current(previous, &[RunStatus::Succeeded])?;
            if submitted.run_id != *run_id {
                return Err(IdentityError::InvalidTransition);
            }
            if matches!(row.review_policy(), ReviewPolicy::ExplicitReviewer { .. })
                && principal(submitted.executor) == context.subject
            {
                return Err(IdentityError::Forbidden);
            }
            let reason = task_text(reason, 4096)?;
            let escalated = results::review(
                tx,
                context,
                row,
                &submitted,
                *decision,
                reason,
                binding.as_ref(),
            )
            .await?;
            detail = Some(reason);
            run = Some(submitted);
            match decision {
                ReviewDecision::Accept => "task.accepted",
                ReviewDecision::RequestChanges if escalated => {
                    review_escalated = Some(true);
                    "task.review_escalated"
                }
                ReviewDecision::RequestChanges => "task.changes_requested",
            }
        }
        TaskChange::Fail { reason } => {
            let active = current(previous, &[RunStatus::Running, RunStatus::WaitingInput])?;
            let reason = task_text(reason, 4096)?;
            run = Some(runs::set_status(tx, &active, RunStatus::Failed, Some(reason), None).await?);
            "task.failed"
        }
        TaskChange::Cancel { reason } => {
            let reason = task_text(reason, 4096)?;
            detail = Some(reason);
            run = execution::cancel_task_queue(tx, context, row, reason).await?;
            if let Some(active) = previous.filter(|run| {
                run.lease.is_none()
                    && matches!(run.status, RunStatus::Running | RunStatus::WaitingInput)
            }) {
                run = Some(
                    runs::set_status(tx, &active, RunStatus::CancelRequested, None, None).await?,
                );
            }
            "task.cancelled"
        }
        TaskChange::AcknowledgeCancel { run_id } => {
            let active = current(previous, &[RunStatus::CancelRequested])?;
            if active.run_id != *run_id {
                return Err(IdentityError::InvalidTransition);
            }
            run = Some(runs::set_status(tx, &active, RunStatus::Cancelled, None, None).await?);
            "run.cancel_acknowledged"
        }
    };
    Ok(Effect {
        assignee,
        run,
        event,
        detail,
        criteria,
        review_escalated,
        review_cycle_after,
    })
}

fn worker_role(role: Option<TeamRole>) -> bool {
    matches!(role, Some(TeamRole::Lead | TeamRole::Worker))
}

fn current(previous: Option<Run>, allowed: &[RunStatus]) -> Result<Run, IdentityError> {
    previous
        .filter(|run| allowed.contains(&run.status))
        .ok_or(IdentityError::InvalidTransition)
}

async fn runnable(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: &TaskRow,
) -> Result<(), IdentityError> {
    let dependencies = dependencies(tx, context.access.workspace, TaskId(task.id)).await?;
    if dependencies
        .iter()
        .any(|item| item.status != TaskStatus::Done)
    {
        return Err(IdentityError::DependencyNotReady);
    }
    Ok(())
}

async fn add_dependency(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: TaskId,
    prerequisite: TaskId,
) -> Result<(), IdentityError> {
    public_task_reference(tx, context, prerequisite).await?;
    let workspace = context.access.workspace.0;
    let cycle = sqlx::query_scalar::<_, bool>(
        "WITH RECURSIVE ancestors(id) AS (
             SELECT $3::uuid
             UNION
             SELECT d.prerequisite_id FROM task_dependencies d JOIN ancestors a ON a.id = d.task_id
             WHERE d.workspace_id = $1
         ) SELECT EXISTS (SELECT 1 FROM ancestors WHERE id = $2)",
    )
    .bind(workspace)
    .bind(task.0)
    .bind(prerequisite.0)
    .fetch_one(&mut **tx)
    .await?;
    if cycle {
        return Err(IdentityError::DependencyCycle);
    }
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM task_dependencies WHERE workspace_id = $1 AND task_id = $2",
    )
    .bind(workspace)
    .bind(task.0)
    .fetch_one(&mut **tx)
    .await?;
    if count >= 32 {
        return Err(IdentityError::InvalidInput);
    }
    let inserted = sqlx::query(
        "INSERT INTO task_dependencies (workspace_id, task_id, prerequisite_id)
         VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(workspace)
    .bind(task.0)
    .bind(prerequisite.0)
    .execute(&mut **tx)
    .await?;
    if inserted.rows_affected() != 1 {
        return Err(IdentityError::InvalidInput);
    }
    Ok(())
}
