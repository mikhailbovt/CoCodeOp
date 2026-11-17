use office_domain::{BearerDigest, ChannelId, MessageId, RunId, TaskId};
use office_protocol::{
    ActivationSource, ActivationTrigger, AnswerActivationQuestion, AnswerQuestion, RunActivation,
    ThreadQuestion,
};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::{
    Store,
    identity::{IdentityError, WorkspaceContext},
    messages::questions::answer_bound_question,
    tasks::execution::start_effects::credentials::authenticate_run,
};

#[derive(sqlx::FromRow)]
struct MessageSource {
    event_id: Uuid,
    channel_id: Uuid,
    question_id: Uuid,
    message_id: Uuid,
    body: String,
    question_open: bool,
}

pub(in crate::tasks::execution) async fn run_activation(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    job: Uuid,
) -> Result<Option<RunActivation>, IdentityError> {
    let workspace = context.access.workspace.0;
    let selected = sqlx::query_as::<_, (String, Uuid, Uuid)>(
        "SELECT trigger,thread_id,job_id FROM activated_executions
         WHERE (workspace_id,job_id)=($1,office_activation_job($1,$2))",
    )
    .bind(workspace)
    .bind(job)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((event_type, thread_id, job)) = selected else {
        return Ok(None);
    };
    let trigger = match event_type.as_str() {
        "task.assignment_requested" => ActivationTrigger::Assignment,
        "message.question" => ActivationTrigger::Question,
        "task.review_requested" => ActivationTrigger::ReviewRequest,
        "message.answer" => ActivationTrigger::Answer,
        _ => return Err(IdentityError::InvalidRole),
    };
    let sources: Vec<_> = match trigger {
        ActivationTrigger::Question | ActivationTrigger::Answer => {
            sqlx::query_as::<_, MessageSource>(
                "SELECT e.id AS event_id,q.channel_id,q.question_id,m.id AS message_id,m.body,
                  (q.answer_id IS NULL AND q.expires_at>clock_timestamp()) AS question_open
                 FROM activation_events source JOIN events e ON (e.workspace_id,e.id)=(source.workspace_id,source.event_id)
                 JOIN thread_questions q ON q.workspace_id=e.workspace_id AND e.id=CASE WHEN $3 THEN q.question_event_id ELSE q.answer_event_id END
                 JOIN messages m ON m.workspace_id=q.workspace_id AND m.id=CASE WHEN $3 THEN q.question_id ELSE q.answer_id END
                 WHERE source.workspace_id=$1 AND source.job_id=$2 ORDER BY e.sequence",
            ).bind(workspace).bind(job).bind(trigger == ActivationTrigger::Question).fetch_all(&mut **tx).await?
             .into_iter().map(|row| ActivationSource::Message {
                 event_id: row.event_id, channel_id: ChannelId(row.channel_id), question_id: MessageId(row.question_id),
                 message_id: MessageId(row.message_id), body: row.body, question_open: row.question_open,
             }).collect()
        }
        ActivationTrigger::Assignment | ActivationTrigger::ReviewRequest => {
            sqlx::query_as::<_, (Uuid, Uuid, i64, Option<Uuid>)>(
                "SELECT e.id,e.scope_id,(e.payload->>'revision')::bigint,(e.payload->>'run_id')::uuid
                 FROM activation_events source JOIN events e ON (e.workspace_id,e.id)=(source.workspace_id,source.event_id)
                 WHERE source.workspace_id=$1 AND source.job_id=$2 ORDER BY e.sequence",
            ).bind(workspace).bind(job).fetch_all(&mut **tx).await?
             .into_iter().map(|(event_id, task, revision, run)| ActivationSource::Task {
                 event_id, task_id: TaskId(task), trigger_revision: revision, result_run_id: run.map(RunId),
             }).collect()
        }
    };
    if sources.is_empty() {
        return Err(IdentityError::Forbidden);
    }
    Ok(Some(RunActivation {
        trigger,
        thread_id,
        sources,
    }))
}

impl Store {
    pub async fn answer_activation_question(
        &self,
        digest: &BearerDigest,
        input: AnswerActivationQuestion,
    ) -> Result<ThreadQuestion, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let run = authenticate_run(&mut tx, digest).await?;
        let allowance = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM activated_executions a JOIN activation_events e ON (e.workspace_id,e.job_id)=(a.workspace_id,a.job_id)
             JOIN thread_questions q ON (q.workspace_id,q.question_event_id)=(e.workspace_id,e.event_id)
             WHERE a.workspace_id=$1 AND a.job_id=office_activation_job($1,$2) AND a.trigger='message.question' AND q.question_id=$3)",
        ).bind(run.context.access.workspace.0).bind(run.job_id).bind(input.question_id.0).fetch_one(&mut *tx).await?;
        if !allowance {
            return Err(IdentityError::Forbidden);
        }
        let result = answer_bound_question(
            &mut tx,
            run.context,
            AnswerQuestion {
                workspace_id: run.context.access.workspace,
                question_id: input.question_id,
                idempotency_key: input.idempotency_key,
                body: input.body,
            },
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }
}

pub(in crate::tasks::execution) async fn missing_activation_effect(
    tx: &mut Transaction<'_, Postgres>,
    run: &office_protocol::Run,
) -> Result<Option<&'static str>, IdentityError> {
    let lease = run.lease.as_ref().ok_or(IdentityError::RunLeaseStale)?;
    let activation = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT job_id,trigger FROM activated_executions
         WHERE workspace_id=$1 AND job_id=office_activation_job($1,$2) AND trigger IN ('message.question','task.review_requested')",
    ).bind(run.workspace_id.0).bind(lease.job_id).fetch_optional(&mut **tx).await?;
    let Some((origin, trigger)) = activation else {
        return Ok(None);
    };
    crate::identity::bind_subject(tx, run.executor).await?;
    let missing = if trigger == "message.question" {
        let pending = sqlx::query_as::<_, (i64, i64)>(
            "SELECT count(*),count(q.question_id) FILTER (WHERE q.answer_id IS NOT NULL)
             FROM activation_events e LEFT JOIN thread_questions q ON (q.workspace_id,q.question_event_id)=(e.workspace_id,e.event_id)
             WHERE e.workspace_id=$1 AND e.job_id=$2",
        ).bind(run.workspace_id.0).bind(origin).fetch_one(&mut **tx).await?;
        (pending.0 == 0 || pending.0 != pending.1)
            .then_some("The activation finished without a recorded answer to each bound question")
    } else {
        let reviewed = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM activation_events src JOIN events e ON (e.workspace_id,e.id)=(src.workspace_id,src.event_id)
             JOIN task_result_revisions result ON (result.workspace_id,result.task_id,result.run_id)=(e.workspace_id,e.scope_id,(e.payload->>'run_id')::uuid)
             JOIN task_result_reviews r ON (r.workspace_id,r.task_id,r.result_id)=(result.workspace_id,result.task_id,result.id)
             WHERE src.workspace_id=$1 AND src.job_id=$2 AND (r.reviewer_user_id,r.reviewer_agent_id)=(office_user_id(),office_agent_id()))",
        ).bind(run.workspace_id.0).bind(origin).fetch_one(&mut **tx).await?;
        (!reviewed)
            .then_some("The activation finished without a recorded review of the bound result")
    };
    crate::identity::bind_subject(
        tx,
        office_protocol::Recipient {
            user_id: run.executor.user_id,
            agent_id: None,
        },
    )
    .await?;
    Ok(missing)
}
