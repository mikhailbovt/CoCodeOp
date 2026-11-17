use chrono::{DateTime, Utc};
use office_domain::{
    Action, AgentId, BearerDigest, ChannelId, GrantRight, MessageId, TeamId, UserId, Visibility,
    WorkspaceId,
};
use office_protocol::{
    AclGrant, Channel, ChannelAudience, ChannelPage, CreateChannel, ListChannels, Message,
    NoteLink, ReadThread, Recipient, SendMessage, ThreadPage,
};
use serde_json::json;
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use crate::{
    Store,
    data_scope::{authorize_scope, create_scope, subject},
    events::{Idempotency, append_event},
    identity::{IdentityError, WorkspaceContext, valid_name, workspace_context},
};

pub(crate) mod questions;

#[derive(sqlx::FromRow)]
struct ChannelRow {
    id: Uuid,
    name: String,
    kind: String,
    team_id: Option<Uuid>,
    owner_user_id: Uuid,
    author_agent_id: Option<Uuid>,
    recipient_user_id: Option<Uuid>,
    recipient_agent_id: Option<Uuid>,
    created_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct MessageRow {
    id: Uuid,
    thread_id: Uuid,
    reply_to: Option<Uuid>,
    author_user_id: Uuid,
    author_agent_id: Option<Uuid>,
    body: String,
    created_at: DateTime<Utc>,
    mentions: Json<Vec<Recipient>>,
    notes: Json<Vec<NoteLink>>,
}

impl Store {
    pub async fn create_channel(
        &self,
        digest: &BearerDigest,
        input: CreateChannel,
    ) -> Result<Channel, IdentityError> {
        let name = valid_name(&input.name)?;
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "channel.create",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }

        let mut grants = Vec::new();
        let (kind, visibility, recipient) = match input.audience {
            ChannelAudience::Workspace => ("workspace", Visibility::Workspace, None),
            ChannelAudience::Team { team_id } => ("team", Visibility::Team { team_id }, None),
            ChannelAudience::Direct { recipient } => {
                if recipient.user_id == context.access.member
                    && recipient.agent_id == context.agent_id()
                {
                    return Err(IdentityError::InvalidInput);
                }
                grants.push(AclGrant {
                    user_id: recipient.user_id,
                    agent_id: recipient.agent_id,
                    right: GrantRight::Write,
                });
                ("direct", Visibility::ExplicitAcl, Some(recipient))
            }
        };
        let channel_id = ChannelId(Uuid::new_v4());
        create_scope(&mut tx, context, channel_id.0, visibility, &grants).await?;
        let created_at = sqlx::query_scalar(
            "INSERT INTO channels (id, workspace_id, name, kind, recipient_user_id, recipient_agent_id)
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING created_at",
        ).bind(channel_id.0).bind(input.workspace_id.0).bind(name).bind(kind)
         .bind(recipient.map(|value| value.user_id.0)).bind(recipient.and_then(|value| value.agent_id.map(|id| id.0)))
         .fetch_one(&mut *tx).await?;
        let channel = Channel {
            workspace_id: input.workspace_id,
            channel_id,
            name: name.into(),
            audience: input.audience,
            author: context.subject,
            created_at,
        };
        append_event(
            &mut tx,
            context,
            channel_id.0,
            "channel.created",
            json!({"channel_id": channel_id}),
            None,
        )
        .await?;
        idem.record(&mut tx, channel_id.0, &channel).await?;
        tx.commit().await?;
        Ok(channel)
    }

    pub async fn list_channels(
        &self,
        digest: &BearerDigest,
        input: ListChannels,
    ) -> Result<ChannelPage, IdentityError> {
        let limit = page_limit(input.limit)?;
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let mut rows = sqlx::query_as::<_, ChannelRow>(
            "SELECT c.id, c.name, c.kind, c.recipient_user_id, c.recipient_agent_id, c.created_at,
                    scope.team_id, scope.owner_user_id, scope.author_agent_id
             FROM channels c JOIN data_scopes scope ON scope.workspace_id = c.workspace_id AND scope.id = c.id
             WHERE c.workspace_id = $1 AND ($2::uuid IS NULL OR c.id > $2) ORDER BY c.id LIMIT $3",
        ).bind(input.workspace_id.0).bind(input.after.map(|id| id.0)).bind(i64::from(limit) + 1)
         .fetch_all(&mut *tx).await?;
        let more = rows.len() > usize::from(limit);
        rows.truncate(usize::from(limit));
        let mut channels = Vec::with_capacity(rows.len());
        for row in rows {
            authorize_scope(&mut tx, context, row.id, Action::ReadData).await?;
            let audience = match (row.kind.as_str(), row.team_id, row.recipient_user_id) {
                ("workspace", None, None) => ChannelAudience::Workspace,
                ("team", Some(team), None) => ChannelAudience::Team {
                    team_id: TeamId(team),
                },
                ("direct", None, Some(user)) => ChannelAudience::Direct {
                    recipient: Recipient {
                        user_id: UserId(user),
                        agent_id: row.recipient_agent_id.map(AgentId),
                    },
                },
                _ => return Err(IdentityError::InvalidRole),
            };
            channels.push(Channel {
                workspace_id: input.workspace_id,
                channel_id: ChannelId(row.id),
                name: row.name,
                created_at: row.created_at,
                author: subject(UserId(row.owner_user_id), row.author_agent_id),
                audience,
            });
        }
        let next_after = channels
            .last()
            .filter(|_| more)
            .map(|channel| channel.channel_id);
        tx.commit().await?;
        Ok(ChannelPage {
            channels,
            next_after,
        })
    }

    pub async fn send_message(
        &self,
        digest: &BearerDigest,
        input: SendMessage,
    ) -> Result<Message, IdentityError> {
        if input.body.trim().is_empty()
            || input.body.len() > 4096
            || input.body.contains('\0')
            || input.mentions.len() > 16
            || input.notes.len() > 16
            || input.notes.iter().any(|link| link.revision < 1)
        {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "message.send",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(mut previous) = idem.replay::<Message>(&mut tx).await? {
            previous.notes =
                visible_notes(&mut tx, input.workspace_id, previous.message_id).await?;
            tx.commit().await?;
            return Ok(previous);
        }
        let channel = input.channel_id;
        let message = post_message(&mut tx, context, input).await?;
        idem.record(&mut tx, channel.0, &message).await?;
        tx.commit().await?;
        Ok(message)
    }

    pub async fn read_thread(
        &self,
        digest: &BearerDigest,
        input: ReadThread,
    ) -> Result<ThreadPage, IdentityError> {
        let limit = page_limit(input.limit)?;
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        authorize_channel(&mut tx, context, input.channel_id, Action::ReadData).await?;
        let thread = input.thread_id.map(|id| id.0);
        if let Some(thread) = thread {
            let root = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM messages WHERE workspace_id = $1 AND channel_id = $2
                                AND id = $3 AND reply_to IS NULL)",
            )
            .bind(input.workspace_id.0)
            .bind(input.channel_id.0)
            .bind(thread)
            .fetch_one(&mut *tx)
            .await?;
            if !root {
                return Err(IdentityError::Forbidden);
            }
        }
        let after = if let Some(after) = input.after {
            Some(sqlx::query_scalar::<_, DateTime<Utc>>(
                "SELECT created_at FROM messages WHERE workspace_id = $1 AND channel_id = $2 AND id = $3
                   AND (($4::uuid IS NULL AND reply_to IS NULL) OR thread_id = $4)",
            ).bind(input.workspace_id.0).bind(input.channel_id.0).bind(after.0).bind(thread)
             .fetch_optional(&mut *tx).await?.ok_or(IdentityError::CursorExpired)?)
        } else {
            None
        };
        let mut rows = sqlx::query_as::<_, MessageRow>(
            "WITH visible AS MATERIALIZED (
                 SELECT id, workspace_id, thread_id, reply_to, author_user_id, author_agent_id, body, created_at
                 FROM messages WHERE workspace_id = $1 AND channel_id = $2
                   AND (($3::uuid IS NULL AND reply_to IS NULL) OR thread_id = $3)
                   AND ($4::timestamptz IS NULL OR (created_at, id) > ($4, $5::uuid))
                 ORDER BY created_at, id LIMIT $6
             )
             SELECT m.*,
               coalesce((SELECT jsonb_agg(jsonb_build_object('user_id', p.user_id, 'agent_id', p.agent_id) ORDER BY p.ordinal)
                 FROM message_mentions p WHERE p.workspace_id = m.workspace_id AND p.message_id = m.id), '[]') AS mentions,
               coalesce((SELECT jsonb_agg(jsonb_build_object('note_id', link.note_id, 'revision', link.revision) ORDER BY link.ordinal)
                 FROM message_note_links link WHERE link.workspace_id = m.workspace_id AND link.message_id = m.id), '[]') AS notes
             FROM visible m ORDER BY m.created_at, m.id",
        ).bind(input.workspace_id.0).bind(input.channel_id.0).bind(thread).bind(after)
         .bind(input.after.map(|id| id.0)).bind(i64::from(limit) + 1).fetch_all(&mut *tx).await?;
        let more = rows.len() > usize::from(limit);
        rows.truncate(usize::from(limit));
        let next_after = rows.last().filter(|_| more).map(|row| MessageId(row.id));
        tx.commit().await?;
        Ok(ThreadPage {
            next_after,
            messages: rows
                .into_iter()
                .map(|row| Message {
                    workspace_id: input.workspace_id,
                    channel_id: input.channel_id,
                    message_id: MessageId(row.id),
                    thread_id: MessageId(row.thread_id),
                    reply_to: row.reply_to.map(MessageId),
                    author: subject(UserId(row.author_user_id), row.author_agent_id),
                    body: row.body,
                    created_at: row.created_at,
                    mentions: row.mentions.0,
                    notes: row.notes.0,
                })
                .collect(),
        })
    }
    pub async fn read_thread_resource(
        &self,
        digest: &BearerDigest,
        workspace: WorkspaceId,
        thread: MessageId,
    ) -> Result<ThreadPage, IdentityError> {
        let mut tx = self.pool.begin().await?;
        workspace_context(&mut tx, digest, workspace).await?;
        let channel = sqlx::query_scalar::<_, Uuid>(
            "SELECT channel_id FROM messages WHERE workspace_id = $1 AND id = $2 AND reply_to IS NULL",
        ).bind(workspace.0).bind(thread.0).fetch_optional(&mut *tx).await?
         .ok_or(IdentityError::Forbidden)?;
        tx.commit().await?;
        self.read_thread(
            digest,
            ReadThread {
                workspace_id: workspace,
                channel_id: ChannelId(channel),
                thread_id: Some(thread),
                after: None,
                limit: None,
            },
        )
        .await
    }
}

async fn post_message(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    input: SendMessage,
) -> Result<Message, IdentityError> {
    authorize_channel(tx, context, input.channel_id, Action::PostMessage).await?;
    reserve_message_rate(tx, context).await?;
    let message_id = MessageId(Uuid::new_v4());
    let thread_id = if let Some(parent) = input.reply_to {
        MessageId(sqlx::query_scalar(
            "SELECT thread_id FROM messages WHERE workspace_id = $1 AND channel_id = $2 AND id = $3",
        ).bind(input.workspace_id.0).bind(input.channel_id.0).bind(parent.0)
         .fetch_optional(&mut **tx).await?.ok_or(IdentityError::Forbidden)?)
    } else {
        message_id
    };
    let created_at = sqlx::query_scalar(
        "INSERT INTO messages (id, workspace_id, channel_id, thread_id, reply_to,
                               author_user_id, author_agent_id, body)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING created_at",
    )
    .bind(message_id.0)
    .bind(input.workspace_id.0)
    .bind(input.channel_id.0)
    .bind(thread_id.0)
    .bind(input.reply_to.map(|id| id.0))
    .bind(context.access.member.0)
    .bind(context.agent_id().map(|id| id.0))
    .bind(&input.body)
    .fetch_one(&mut **tx)
    .await?;

    for (ordinal, target) in input.mentions.iter().enumerate() {
        sqlx::query(
            "INSERT INTO message_mentions (workspace_id, message_id, user_id, agent_id, ordinal)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(input.workspace_id.0)
        .bind(message_id.0)
        .bind(target.user_id.0)
        .bind(target.agent_id.map(|id| id.0))
        .bind(ordinal as i16)
        .execute(&mut **tx)
        .await
        .map_err(validate_reference)?;
    }
    for (ordinal, link) in input.notes.iter().enumerate() {
        authorize_scope(tx, context, link.note_id.0, Action::ReadData).await?;
        let inserted = sqlx::query(
            "INSERT INTO message_note_links (workspace_id, message_id, note_id, revision, ordinal)
             SELECT workspace_id, $2, note_id, revision, $5 FROM note_revisions
             WHERE workspace_id = $1 AND note_id = $3 AND revision = $4",
        )
        .bind(input.workspace_id.0)
        .bind(message_id.0)
        .bind(link.note_id.0)
        .bind(link.revision)
        .bind(ordinal as i16)
        .execute(&mut **tx)
        .await
        .map_err(validate_reference)?;
        if inserted.rows_affected() != 1 {
            return Err(IdentityError::Forbidden);
        }
    }

    let position =
        json!({"channel_id": input.channel_id, "message_id": message_id, "thread_id": thread_id});
    append_event(
        tx,
        context,
        input.channel_id.0,
        "message.posted",
        position.clone(),
        None,
    )
    .await?;
    for mention in &input.mentions {
        append_event(
            tx,
            context,
            input.channel_id.0,
            "message.mentioned",
            position.clone(),
            Some(*mention),
        )
        .await?;
    }
    Ok(Message {
        workspace_id: input.workspace_id,
        channel_id: input.channel_id,
        message_id,
        thread_id,
        reply_to: input.reply_to,
        author: context.subject,
        body: input.body,
        created_at,
        mentions: input.mentions,
        notes: input.notes,
    })
}

async fn reserve_message_rate(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
) -> Result<(), IdentityError> {
    let workspace = context.access.workspace.0;
    let workspace_limit = 1200;
    let owner_limit = 240;
    let agent_limit = 120;
    let changed = sqlx::query(
        "WITH caps(kind,subject_id,rate) AS (
            VALUES ('workspace',$1::uuid,$4::integer),('owner',$2::uuid,$5::integer),('agent',$3::uuid,$6::integer)
         )
         INSERT INTO message_rate_windows (workspace_id,kind,subject_id,minute,messages)
           SELECT $1,kind,subject_id,floor(extract(epoch FROM statement_timestamp())/60)::bigint,1
           FROM caps WHERE subject_id IS NOT NULL
           ORDER BY CASE kind WHEN 'workspace' THEN 0 WHEN 'owner' THEN 1 ELSE 2 END
         ON CONFLICT(workspace_id,kind,subject_id) DO UPDATE SET
           messages=CASE WHEN excluded.minute>message_rate_windows.minute THEN 1 ELSE message_rate_windows.messages+1 END,
           minute=excluded.minute
         WHERE excluded.minute>message_rate_windows.minute OR
           (excluded.minute=message_rate_windows.minute AND message_rate_windows.messages<
             (SELECT rate FROM caps WHERE caps.kind=excluded.kind AND caps.subject_id=excluded.subject_id))",
    )
    .bind(workspace)
    .bind(context.access.member.0)
    .bind(context.agent_id().map(|agent| agent.0))
    .bind(workspace_limit)
    .bind(owner_limit)
    .bind(agent_limit)
    .execute(&mut **tx)
    .await?;
    if changed.rows_affected() != if context.agent_id().is_some() { 3 } else { 2 } {
        return Err(IdentityError::MessageRateLimited);
    }
    Ok(())
}

async fn authorize_channel(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    channel: ChannelId,
    action: Action,
) -> Result<(), IdentityError> {
    authorize_scope(tx, context, channel.0, action).await?;
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM channels WHERE workspace_id = $1 AND id = $2)",
    )
    .bind(context.access.workspace.0)
    .bind(channel.0)
    .fetch_one(&mut **tx)
    .await?;
    exists.then_some(()).ok_or(IdentityError::Forbidden)
}

async fn visible_notes(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    message: MessageId,
) -> Result<Vec<NoteLink>, IdentityError> {
    let columns = sqlx::query_as::<_, (Uuid, i64)>(
        "SELECT note_id, revision FROM message_note_links WHERE workspace_id = $1 AND message_id = $2 ORDER BY ordinal",
    ).bind(workspace.0).bind(message.0).fetch_all(&mut **tx).await?;
    Ok(columns
        .into_iter()
        .map(|(id, revision)| NoteLink {
            note_id: office_domain::NoteId(id),
            revision,
        })
        .collect())
}

fn page_limit(limit: Option<u16>) -> Result<u16, IdentityError> {
    let value = limit.unwrap_or(25);
    (1..=50)
        .contains(&value)
        .then_some(value)
        .ok_or(IdentityError::InvalidInput)
}

fn validate_reference(error: sqlx::Error) -> IdentityError {
    if error
        .as_database_error()
        .is_some_and(|cause| cause.is_foreign_key_violation() || cause.is_unique_violation())
    {
        IdentityError::InvalidInput
    } else {
        error.into()
    }
}
