use chrono::{DateTime, Utc};
use office_domain::{
    Action, AgentId, BearerDigest, Resource, TaskId, TaskStatus, TeamId, TeamState, UserId,
    Visibility, WorkspaceId,
};
use office_protocol::{
    ContextTask, ContextTasks, CreateTask, GetTask, ListTasks, Recipient, ReviewPolicy, Run, Task,
    TaskBlockReason, TaskDependency, TaskExecution, TaskMutation, TaskPage, TaskView,
};
use serde_json::json;
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use crate::{
    Store,
    data_scope::{Scope, authorize_scope, create_scope, subject},
    events::{Idempotency, append_event},
    identity::{IdentityError, WorkspaceContext, require_permission, workspace_context},
    inbox::save_cursor,
    teams::load_team,
};

mod checkout_releases;
mod checkout_reviews;
mod checkpoint_snapshots;
pub(crate) mod claims;
mod commands;
mod delegation;
mod execution;
mod explain;
mod gateway;
mod handoffs;
mod managed_workers;
pub(crate) mod publications;
mod results;
mod runs;
mod scopes;
mod snapshot_commands;

#[derive(sqlx::FromRow)]
struct TaskRow {
    id: Uuid,
    workspace_id: Uuid,
    parent_task_id: Option<Uuid>,
    team_id: Option<Uuid>,
    delegation_grant_id: Option<Uuid>,
    awaiting_owner: bool,
    review_escalated: bool,
    review_cycle_after: i64,
    accountable_user_id: Uuid,
    accountable_agent_id: Option<Uuid>,
    assignee_user_id: Option<Uuid>,
    assignee_agent_id: Option<Uuid>,
    objective: String,
    criteria: Json<Vec<String>>,
    criteria_revision: i64,
    status: String,
    revision: i64,
    execution_kind: String,
    checkout_id: Option<Uuid>,
    work_scope_id: Option<Uuid>,
    work_scope_revision: Option<i64>,
    reviewer_user_id: Option<Uuid>,
    reviewer_agent_id: Option<Uuid>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TaskRow {
    fn status(&self) -> Result<TaskStatus, IdentityError> {
        self.status.parse().map_err(|_| IdentityError::InvalidRole)
    }

    fn review_policy(&self) -> ReviewPolicy {
        match self.reviewer_user_id {
            Some(user) => ReviewPolicy::ExplicitReviewer {
                reviewer: Recipient {
                    user_id: UserId(user),
                    agent_id: self.reviewer_agent_id.map(AgentId),
                },
            },
            None => ReviewPolicy::Accountable,
        }
    }

    async fn into_task(
        self,
        tx: &mut Transaction<'_, Postgres>,
        scope: Scope,
    ) -> Result<Task, IdentityError> {
        let status = if self.awaiting_owner {
            TaskStatus::Blocked
        } else {
            self.status()?
        };
        let review_policy = self.review_policy();
        let execution = match self.execution_kind.as_str() {
            "coordination" => TaskExecution::Coordination,
            "coding" => {
                let active = scopes::task_scope_reference(tx, &self).await?;
                TaskExecution::Coding {
                    checkout_id: self.checkout_id,
                    work_scope_id: active.map(|(id, _)| id).or(self.work_scope_id),
                    work_scope_revision: active
                        .map(|(_, version)| version)
                        .or(self.work_scope_revision),
                }
            }
            _ => return Err(IdentityError::InvalidRole),
        };
        let assignee = self.assignee_user_id.map(|id| Recipient {
            user_id: UserId(id),
            agent_id: self.assignee_agent_id.map(AgentId),
        });
        let dependencies =
            dependencies(tx, WorkspaceId(self.workspace_id), TaskId(self.id)).await?;
        Ok(Task {
            task_id: TaskId(self.id),
            workspace_id: WorkspaceId(self.workspace_id),
            owner_user_id: scope.owner,
            author: scope.author,
            accountable: subject(UserId(self.accountable_user_id), self.accountable_agent_id),
            assignee,
            parent_task_id: self.parent_task_id.map(TaskId),
            team_id: self.team_id.map(TeamId),
            delegation_grant_id: self.delegation_grant_id,
            status,
            block_reason: self
                .awaiting_owner
                .then_some(TaskBlockReason::AwaitingOwner)
                .or(self
                    .review_escalated
                    .then_some(TaskBlockReason::ReviewEscalation)),
            revision: self.revision,
            objective: self.objective,
            acceptance_criteria: self.criteria.0,
            acceptance_criteria_revision: self.criteria_revision,
            execution,
            review_policy,
            dependencies,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

impl Store {
    pub async fn create_task(
        &self,
        digest: &BearerDigest,
        input: CreateTask,
    ) -> Result<TaskMutation, IdentityError> {
        let objective = task_text(&input.objective, 4096)?;
        let criteria = task_criteria(&input.acceptance_criteria)?;
        let (execution, checkout, scope, scope_revision) = match input.execution {
            TaskExecution::Coordination => ("coordination", None, None, None),
            TaskExecution::Coding {
                checkout_id,
                work_scope_id,
                work_scope_revision,
            } => {
                if work_scope_id.is_some() != work_scope_revision.is_some()
                    || work_scope_id.is_some() && checkout_id.is_none()
                    || work_scope_revision.is_some_and(|rev| rev < 1)
                {
                    return Err(IdentityError::InvalidInput);
                }
                ("coding", checkout_id, work_scope_id, work_scope_revision)
            }
        };
        let reviewer = match input.review_policy {
            ReviewPolicy::Accountable => None,
            ReviewPolicy::ExplicitReviewer { reviewer } => Some(reviewer),
        };
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        require_permission(
            context.actor,
            Action::CreateTask,
            Resource::Workspace(context.access),
        )?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "task.create",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        if let Some(parent) = input.parent_task_id {
            public_task_reference(&mut tx, context, parent).await?;
        }
        let team_id = match input.visibility {
            Visibility::Team { team_id } => {
                if input.team_id.is_some_and(|specified| specified != team_id) {
                    return Err(IdentityError::InvalidInput);
                }
                Some(team_id)
            }
            _ => input.team_id,
        };
        let awaiting_owner = if let Some(team) = team_id {
            load_team(&mut tx, context, team, Action::ManageTeam, true)
                .await?
                .state
                == TeamState::AwaitingOwner
        } else {
            false
        };
        let task_id = TaskId(Uuid::new_v4());
        create_scope(&mut tx, context, task_id.0, input.visibility, &input.grants).await?;
        let row = sqlx::query_as::<_, TaskRow>(
            "INSERT INTO tasks (id, workspace_id, parent_task_id, accountable_user_id, accountable_agent_id,
                                objective, criteria, execution_kind, checkout_id, work_scope_id, work_scope_revision,
                                reviewer_user_id, reviewer_agent_id, team_id, awaiting_owner)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) RETURNING *",
        ).bind(task_id.0).bind(input.workspace_id.0).bind(input.parent_task_id.map(|id| id.0))
         .bind(context.access.member.0).bind(context.agent_id().map(|id| id.0))
         .bind(objective).bind(Json(criteria)).bind(execution).bind(checkout).bind(scope).bind(scope_revision)
         .bind(reviewer.map(|recipient| recipient.user_id.0))
         .bind(reviewer.and_then(|recipient| recipient.agent_id.map(|id| id.0)))
         .bind(team_id.map(|id| id.0)).bind(awaiting_owner)
         .fetch_one(&mut *tx).await.map_err(reference_error)?;
        let scope = authorize_scope(&mut tx, context, task_id.0, Action::ReadData).await?;
        let task = row.into_task(&mut tx, scope).await?;
        let result =
            record_change(&mut tx, context, &idem, task, None, "task.created", None).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn get_task(
        &self,
        digest: &BearerDigest,
        input: GetTask,
    ) -> Result<TaskView, IdentityError> {
        let limit = page_limit(input.run_limit)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let (row, scope) = load_task(&mut tx, context, input.task_id, false).await?;
        let task = row.into_task(&mut tx, scope).await?;
        let (runs, next_run_after) = runs::history(
            &mut tx,
            input.workspace_id,
            input.task_id,
            input.runs_after,
            limit,
        )
        .await?;
        tx.commit().await?;
        Ok(TaskView {
            task,
            runs,
            next_run_after,
        })
    }

    pub async fn list_tasks(
        &self,
        digest: &BearerDigest,
        input: ListTasks,
    ) -> Result<TaskPage, IdentityError> {
        let limit = page_limit(input.limit)?;
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let mut rows = sqlx::query_as::<_, TaskRow>(
            "SELECT * FROM tasks WHERE workspace_id = $1
             AND ($2::text IS NULL OR CASE WHEN awaiting_owner THEN 'blocked' ELSE status END = $2)
             AND ($3::uuid IS NULL OR id > $3) ORDER BY id LIMIT $4",
        )
        .bind(input.workspace_id.0)
        .bind(input.status.map(TaskStatus::as_str))
        .bind(input.after.map(|id| id.0))
        .bind(i64::from(limit) + 1)
        .fetch_all(&mut *tx)
        .await?;
        let more = rows.len() > usize::from(limit);
        rows.truncate(usize::from(limit));
        let mut tasks = Vec::with_capacity(rows.len());
        for row in rows {
            let scope = authorize_scope(&mut tx, context, row.id, Action::ReadData).await?;
            tasks.push(row.into_task(&mut tx, scope).await?);
        }
        let next_after = tasks.last().filter(|_| more).map(|task| task.task_id);
        tx.commit().await?;
        Ok(TaskPage { tasks, next_after })
    }
}

pub(crate) async fn context_tasks(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    after: Option<TaskId>,
    limit: u16,
) -> Result<ContextTasks, IdentityError> {
    let mut rows = sqlx::query_as::<_, (Uuid, i64, String, String)>(
        "SELECT id, revision, CASE WHEN awaiting_owner THEN 'blocked' ELSE status END, objective
         FROM tasks WHERE workspace_id=$1 AND ($2::uuid IS NULL OR id>$2)
           AND status NOT IN ('done','cancelled','failed')
           AND ((assignee_user_id=$3 AND assignee_agent_id IS NOT DISTINCT FROM $4)
             OR (accountable_user_id=$3 AND accountable_agent_id IS NOT DISTINCT FROM $4)
             OR (status='in_review' AND reviewer_user_id=$3 AND reviewer_agent_id IS NOT DISTINCT FROM $4))
         ORDER BY id LIMIT $5",
    )
    .bind(context.access.workspace.0)
    .bind(after.map(|id| id.0))
    .bind(context.access.member.0)
    .bind(context.agent_id().map(|agent| agent.0))
    .bind(i64::from(limit) + 1)
    .fetch_all(&mut **tx)
    .await?;
    let more = rows.len() > usize::from(limit);
    rows.truncate(usize::from(limit));
    let next_after = rows.last().filter(|_| more).map(|row| TaskId(row.0));
    let mut items = Vec::with_capacity(rows.len());
    for (id, revision, status, objective) in rows {
        authorize_scope(tx, context, id, Action::ReadData).await?;
        items.push(ContextTask {
            task_id: TaskId(id),
            revision,
            status: status.parse().map_err(|_| IdentityError::InvalidRole)?,
            objective,
        });
    }
    Ok(ContextTasks { items, next_after })
}

async fn load_task(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: TaskId,
    lock: bool,
) -> Result<(TaskRow, Scope), IdentityError> {
    let scope = authorize_scope(tx, context, task.0, Action::ReadData).await?;
    let statement = if lock {
        "SELECT * FROM tasks WHERE workspace_id = $1 AND id = $2 FOR NO KEY UPDATE"
    } else {
        "SELECT * FROM tasks WHERE workspace_id = $1 AND id = $2"
    };
    let row = sqlx::query_as::<_, TaskRow>(statement)
        .bind(context.access.workspace.0)
        .bind(task.0)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(IdentityError::Forbidden)?;
    Ok((row, scope))
}

async fn public_task_reference(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    task: TaskId,
) -> Result<(), IdentityError> {
    let (_, scope) = load_task(tx, context, task, false).await?;
    if scope.visibility == Visibility::Workspace {
        Ok(())
    } else {
        Err(IdentityError::Forbidden)
    }
}

async fn dependencies(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    task: TaskId,
) -> Result<Vec<TaskDependency>, IdentityError> {
    let rows = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT p.id, CASE WHEN p.awaiting_owner THEN 'blocked' ELSE p.status END FROM task_dependencies d JOIN tasks p
         ON p.workspace_id = d.workspace_id AND p.id = d.prerequisite_id
         WHERE d.workspace_id = $1 AND d.task_id = $2 ORDER BY p.id",
    )
    .bind(workspace.0)
    .bind(task.0)
    .fetch_all(&mut **tx)
    .await?;
    rows.into_iter()
        .map(|(id, status)| {
            Ok(TaskDependency {
                task_id: TaskId(id),
                status: status.parse().map_err(|_| IdentityError::InvalidRole)?,
            })
        })
        .collect()
}

async fn record_change(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    idem: &Idempotency,
    task: Task,
    run: Option<Run>,
    event: &str,
    detail: Option<&str>,
) -> Result<TaskMutation, IdentityError> {
    let sequence = append_event(
        tx, context, task.task_id.0, event,
        json!({"task_id": task.task_id, "status": task.status, "revision": task.revision,
               "actor": context.subject, "run_id": run.as_ref().map(|value| value.run_id), "detail": detail}),
        None,
    ).await?;
    let sequence = if event == "task.assigned"
        && let Some(recipient) = task.assignee.filter(|target| target.agent_id.is_some())
    {
        append_event(
            tx,
            context,
            task.task_id.0,
            "task.assignment_requested",
            json!({"task_id":task.task_id,"revision":task.revision}),
            Some(recipient),
        )
        .await?
    } else {
        sequence
    };
    let event_cursor = save_cursor(tx, context, sequence).await?;
    let result = TaskMutation {
        task,
        run,
        event_cursor,
    };
    idem.record(tx, result.task.task_id.0, &result).await?;
    Ok(result)
}

pub(crate) fn task_text(text: &str, limit: usize) -> Result<&str, IdentityError> {
    let text = text.trim();
    if text.is_empty() || text.len() > limit || text.contains('\0') {
        return Err(IdentityError::InvalidInput);
    }
    Ok(text)
}

fn task_criteria(criteria: &[String]) -> Result<Vec<&str>, IdentityError> {
    if !(1..=16).contains(&criteria.len()) {
        return Err(IdentityError::InvalidInput);
    }
    criteria.iter().map(|text| task_text(text, 512)).collect()
}

fn reference_error(error: sqlx::Error) -> IdentityError {
    if error
        .as_database_error()
        .is_some_and(|cause| cause.is_foreign_key_violation())
    {
        IdentityError::InvalidInput
    } else {
        error.into()
    }
}

fn page_limit(limit: Option<u16>) -> Result<u16, IdentityError> {
    let limit = limit.unwrap_or(25);
    (1..=50)
        .contains(&limit)
        .then_some(limit)
        .ok_or(IdentityError::InvalidInput)
}
