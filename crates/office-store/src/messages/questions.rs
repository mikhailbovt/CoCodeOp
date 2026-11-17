use chrono::{DateTime, Utc};
use office_domain::{Action, AgentId, BearerDigest, ChannelId, MessageId, UserId, WorkspaceId};
use office_protocol::{
    AnswerQuestion, AskQuestion, GetQuestion, Message, Recipient, SendMessage, ThreadQuestion,
};
use serde_json::json;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::{
    Store,
    events::{Idempotency, append_event},
    identity::{
        IdentityError, WorkspaceContext, active_principal, bind_subject, workspace_context,
    },
    tasks::task_text,
};

use super::{authorize_channel, post_message};

#[derive(sqlx::FromRow)]
struct QuestionRow {
    workspace_id: Uuid,
    channel_id: Uuid,
    question_id: Uuid,
    question_event_id: Uuid,
    asker_user_id: Uuid,
    asker_agent_id: Option<Uuid>,
    recipient_user_id: Uuid,
    recipient_agent_id: Option<Uuid>,
    expires_at: DateTime<Utc>,
    answer_id: Option<Uuid>,
    answer_event_id: Option<Uuid>,
}

impl From<QuestionRow> for ThreadQuestion {
    fn from(row: QuestionRow) -> Self {
        Self {
            workspace_id: WorkspaceId(row.workspace_id),
            channel_id: ChannelId(row.channel_id),
            question_id: MessageId(row.question_id),
            question_event_id: row.question_event_id,
            asker: Recipient {
                user_id: UserId(row.asker_user_id),
                agent_id: row.asker_agent_id.map(AgentId),
            },
            recipient: Recipient {
                user_id: UserId(row.recipient_user_id),
                agent_id: row.recipient_agent_id.map(AgentId),
            },
            expires_at: row.expires_at,
            answer_id: row.answer_id.map(MessageId),
            answer_event_id: row.answer_event_id,
        }
    }
}

impl Store {
    pub async fn ask_question(
        &self,
        digest: &BearerDigest,
        input: AskQuestion,
    ) -> Result<ThreadQuestion, IdentityError> {
        let body = task_text(&input.body, 4096)?;
        let seconds = input.expires_in_seconds.unwrap_or(3600);
        if !(5..=86_400).contains(&seconds) {
            return Err(IdentityError::InvalidInput);
        }
        let recipient = Recipient {
            user_id: input.recipient_user_id,
            agent_id: input.recipient_agent_id,
        };
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        if context.recipient() == recipient {
            return Err(IdentityError::InvalidInput);
        }
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "message.question",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        active_principal(&mut tx, context, recipient).await?;
        bind_subject(&mut tx, recipient).await?;
        let visible: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM channels WHERE workspace_id=$1 AND id=$2)",
        )
        .bind(input.workspace_id.0)
        .bind(input.channel_id.0)
        .fetch_one(&mut *tx)
        .await?;
        bind_subject(&mut tx, context.recipient()).await?;
        if !visible {
            return Err(IdentityError::Forbidden);
        }
        let message = post_message(
            &mut tx,
            context,
            SendMessage {
                workspace_id: input.workspace_id,
                channel_id: input.channel_id,
                body: body.into(),
                idempotency_key: input.idempotency_key,
                reply_to: input.reply_to,
                mentions: vec![],
                notes: vec![],
            },
        )
        .await?;
        let event = addressed_event(
            &mut tx,
            context,
            &message,
            message.message_id,
            "message.question",
            recipient,
        )
        .await?;
        let row = sqlx::query_as::<_, QuestionRow>(
            "INSERT INTO thread_questions
              (workspace_id,channel_id,question_id,question_event_id,asker_user_id,asker_agent_id,recipient_user_id,recipient_agent_id,expires_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,clock_timestamp()+make_interval(secs=>$9::integer)) RETURNING *",
        ).bind(input.workspace_id.0).bind(input.channel_id.0).bind(message.message_id.0).bind(event)
         .bind(context.access.member.0).bind(context.agent_id().map(|agent| agent.0))
         .bind(recipient.user_id.0).bind(recipient.agent_id.map(|agent| agent.0)).bind(seconds as i32)
         .fetch_one(&mut *tx).await?;
        let result = ThreadQuestion::from(row);
        idem.record(&mut tx, input.channel_id.0, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn answer_question(
        &self,
        digest: &BearerDigest,
        input: AnswerQuestion,
    ) -> Result<ThreadQuestion, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let result = answer_bound_question(&mut tx, context, input).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn get_question(
        &self,
        digest: &BearerDigest,
        input: GetQuestion,
    ) -> Result<ThreadQuestion, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let row = sqlx::query_as::<_, QuestionRow>(
            "SELECT * FROM thread_questions WHERE workspace_id=$1 AND question_id=$2",
        )
        .bind(input.workspace_id.0)
        .bind(input.question_id.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(IdentityError::Forbidden)?;
        let question = ThreadQuestion::from(row);
        authorize_channel(&mut tx, context, question.channel_id, Action::ReadData).await?;
        tx.commit().await?;
        Ok(question)
    }
}

pub(crate) async fn answer_bound_question(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    input: AnswerQuestion,
) -> Result<ThreadQuestion, IdentityError> {
    if input.workspace_id != context.access.workspace {
        return Err(IdentityError::Forbidden);
    }
    let body = task_text(&input.body, 4096)?;
    let idem =
        Idempotency::lock(tx, context, "message.answer", input.idempotency_key, &input).await?;
    if let Some(previous) = idem.replay(tx).await? {
        return Ok(previous);
    }
    let row = sqlx::query_as::<_, QuestionRow>(
        "SELECT * FROM thread_questions WHERE workspace_id=$1 AND question_id=$2 AND recipient_user_id=$3
          AND recipient_agent_id IS NOT DISTINCT FROM $4 FOR NO KEY UPDATE",
    ).bind(context.access.workspace.0).bind(input.question_id.0).bind(context.access.member.0)
     .bind(context.agent_id().map(|agent| agent.0)).fetch_optional(&mut **tx).await?.ok_or(IdentityError::Forbidden)?;
    let question = ThreadQuestion::from(row);
    if question.answer_id.is_some() {
        return Err(IdentityError::InvalidTransition);
    }
    let message = post_message(
        tx,
        context,
        SendMessage {
            workspace_id: input.workspace_id,
            channel_id: question.channel_id,
            body: body.into(),
            idempotency_key: input.idempotency_key,
            reply_to: Some(input.question_id),
            mentions: vec![],
            notes: vec![],
        },
    )
    .await?;
    let event = addressed_event(
        tx,
        context,
        &message,
        input.question_id,
        "message.answer",
        question.asker,
    )
    .await?;
    let updated = sqlx::query_as::<_, QuestionRow>(
        "UPDATE thread_questions SET answer_id=$3, answer_event_id=$4
          WHERE workspace_id=$1 AND question_id=$2 AND answer_id IS NULL AND expires_at>clock_timestamp() RETURNING *",
    ).bind(input.workspace_id.0).bind(input.question_id.0).bind(message.message_id.0).bind(event)
     .fetch_optional(&mut **tx).await?.ok_or(IdentityError::InvalidTransition)?;
    let result = ThreadQuestion::from(updated);
    idem.record(tx, question.channel_id.0, &result).await?;
    Ok(result)
}

async fn addressed_event(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    message: &Message,
    question_id: MessageId,
    event_type: &str,
    recipient: Recipient,
) -> Result<Uuid, IdentityError> {
    let sequence = append_event(
        tx,
        context,
        message.channel_id.0,
        event_type,
        json!({"question_id":question_id,"channel_id":message.channel_id,
               "thread_id":message.thread_id,"message_id":message.message_id}),
        Some(recipient),
    )
    .await?;
    Ok(
        sqlx::query_scalar("SELECT id FROM events WHERE workspace_id=$1 AND sequence=$2")
            .bind(context.access.workspace.0)
            .bind(sequence)
            .fetch_one(&mut **tx)
            .await?,
    )
}
