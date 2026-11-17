use office_domain::BearerDigest;
use office_protocol::{RequestRunInput, Run, RunInputIntent};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::super::start_effects::credentials::authenticate_run;
use super::can_pause;
use crate::{Store, identity::IdentityError, tasks::task_text};

impl Store {
    pub async fn request_run_input(
        &self,
        digest: &BearerDigest,
        input: RequestRunInput,
    ) -> Result<RunInputIntent, IdentityError> {
        let question = task_text(&input.question, 3000)?;
        let mut tx = self.pool.begin().await?;
        let run = authenticate_run(&mut tx, digest).await?;
        let workspace = run.context.access.workspace.0;
        let agent_id = run.context.agent_id().ok_or(IdentityError::Forbidden)?;
        if !can_pause(&mut tx, workspace, run.task.id, question).await? {
            return Err(IdentityError::InvalidTransition);
        }
        sqlx::query("INSERT INTO managed_input_intents
            (workspace_id,task_id,run_id,job_id,requester_user_id,requester_agent_id,idempotency_key,question)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT (workspace_id,run_id) DO NOTHING")
            .bind(workspace).bind(run.task.id).bind(run.run_id.0).bind(run.job_id).bind(run.context.access.member.0)
            .bind(agent_id.0).bind(input.idempotency_key).bind(question).execute(&mut *tx).await?;
        let (key, recorded) = sqlx::query_as::<_, (Uuid, String)>(
            "SELECT idempotency_key,question FROM managed_input_intents WHERE (workspace_id,run_id)=($1,$2)",
        ).bind(workspace).bind(run.run_id.0).fetch_one(&mut *tx).await?;
        if key != input.idempotency_key || recorded != question {
            return Err(IdentityError::IdempotencyConflict);
        }
        tx.commit().await?;
        Ok(RunInputIntent {
            run_id: run.run_id,
            question: recorded,
        })
    }
}

pub(in crate::tasks::execution) async fn recorded_intent(
    tx: &mut Transaction<'_, Postgres>,
    run: &Run,
) -> Result<Option<String>, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT question FROM managed_input_intents WHERE (workspace_id,run_id)=($1,$2)",
    )
    .bind(run.workspace_id.0)
    .bind(run.run_id.0)
    .fetch_optional(&mut **tx)
    .await?)
}
