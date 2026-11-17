use office_domain::{BearerDigest, RunId, TaskId};
use office_protocol::{
    ActivationReview, ChangeTask, GetTaskResult, ReviewActivationResult, TaskChange, TaskMutation,
    TaskResult,
};
use sqlx::{Postgres, Transaction};

use crate::{
    Store,
    identity::IdentityError,
    tasks::{commands::change_task_for, load_task, results::task_result_for},
};

use super::super::start_effects::credentials::{RunAccess, authenticate_run};

impl Store {
    pub async fn get_activation_review(
        &self,
        digest: &BearerDigest,
    ) -> Result<ActivationReview, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let run = authenticate_run(&mut tx, digest).await?;
        let (result, _) = read_target_result(&mut tx, &run).await?;
        let (row, scope) = load_task(&mut tx, run.context, result.task_id, false).await?;
        let task = row.into_task(&mut tx, scope).await?;
        tx.commit().await?;
        Ok(ActivationReview { task, result })
    }

    pub async fn review_activation_result(
        &self,
        digest: &BearerDigest,
        input: ReviewActivationResult,
    ) -> Result<TaskMutation, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let run = authenticate_run(&mut tx, digest).await?;
        let (result, revision) = read_target_result(&mut tx, &run).await?;
        if result.binding.result_sha256 != input.result_sha256 {
            return Err(IdentityError::RevisionConflict);
        }
        let mutation = change_task_for(
            &mut tx,
            run.context,
            ChangeTask {
                workspace_id: result.workspace_id,
                task_id: result.task_id,
                expected_revision: revision,
                idempotency_key: input.idempotency_key,
                change: TaskChange::Review {
                    run_id: result.run_id,
                    decision: input.decision,
                    reason: input.reason,
                    binding: Some(result.binding),
                },
            },
        )
        .await?;
        tx.commit().await?;
        Ok(mutation)
    }
}

async fn read_target_result(
    tx: &mut Transaction<'_, Postgres>,
    run: &RunAccess,
) -> Result<(TaskResult, i64), IdentityError> {
    let workspace = run.context.access.workspace;
    let (task, revision, result_run) = sqlx::query_as::<_, (uuid::Uuid, i64, uuid::Uuid)>(
        "SELECT e.scope_id,(e.payload->>'revision')::bigint,(e.payload->>'run_id')::uuid
         FROM activated_executions a JOIN activation_events src ON (src.workspace_id,src.job_id)=(a.workspace_id,a.job_id)
         JOIN events e ON (e.workspace_id,e.id)=(src.workspace_id,src.event_id)
         WHERE (a.workspace_id,a.job_id)=($1,office_activation_job($1,$2)) AND a.trigger='task.review_requested'",
    ).bind(workspace.0).bind(run.job_id).fetch_optional(&mut **tx).await?.ok_or(IdentityError::Forbidden)?;
    let result = task_result_for(
        tx,
        run.context,
        GetTaskResult {
            workspace_id: workspace,
            task_id: TaskId(task),
            revision: None,
        },
        Some(RunId(result_run)),
    )
    .await?;
    Ok((result, revision))
}
