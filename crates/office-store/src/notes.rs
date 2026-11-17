use chrono::{DateTime, Utc};
use office_domain::{Action, BearerDigest, NoteId, UserId};
use office_protocol::{GetNote, NoteChange, NoteRevision, UpsertNote};
use serde_json::json;
use uuid::Uuid;

use crate::{
    Store,
    data_scope::{authorize_scope, create_scope, subject},
    events::{Idempotency, append_event},
    identity::{IdentityError, valid_name, workspace_context},
};

#[derive(sqlx::FromRow)]
struct RevisionRow {
    revision: i64,
    title: String,
    body: String,
    editor_user_id: Uuid,
    editor_agent_id: Option<Uuid>,
    created_at: DateTime<Utc>,
}

impl Store {
    pub async fn upsert_note(
        &self,
        digest: &BearerDigest,
        input: UpsertNote,
    ) -> Result<NoteRevision, IdentityError> {
        let title = valid_name(&input.title)?;
        if input.body.trim().is_empty() || input.body.len() > 8192 || input.body.contains('\0') {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "notes.upsert",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }

        let (note_id, revision, event_type) = match input.change {
            NoteChange::Create { visibility, grants } => {
                let id = Uuid::new_v4();
                create_scope(&mut tx, context, id, visibility, &grants).await?;
                sqlx::query(
                    "INSERT INTO notes (id, workspace_id, current_revision) VALUES ($1, $2, 1)",
                )
                .bind(id)
                .bind(input.workspace_id.0)
                .execute(&mut *tx)
                .await?;
                (NoteId(id), 1, "note.created")
            }
            NoteChange::Revise {
                note_id,
                expected_revision,
            } => {
                if !(1..i64::MAX).contains(&expected_revision) {
                    return Err(IdentityError::InvalidInput);
                }
                authorize_scope(&mut tx, context, note_id.0, Action::WriteData).await?;
                let update = sqlx::query(
                    "UPDATE notes SET current_revision = current_revision + 1
                     WHERE workspace_id = $1 AND id = $2 AND current_revision = $3",
                )
                .bind(input.workspace_id.0)
                .bind(note_id.0)
                .bind(expected_revision)
                .execute(&mut *tx)
                .await?;
                if update.rows_affected() != 1 {
                    return Err(IdentityError::RevisionConflict);
                }
                (note_id, expected_revision + 1, "note.revised")
            }
        };

        let created_at: DateTime<Utc> = sqlx::query_scalar(
            "INSERT INTO note_revisions (workspace_id, note_id, revision, editor_user_id, editor_agent_id, title, body)
             VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING created_at",
        ).bind(input.workspace_id.0).bind(note_id.0).bind(revision).bind(context.access.member.0)
         .bind(context.agent_id().map(|id| id.0)).bind(title).bind(&input.body)
         .fetch_one(&mut *tx).await?;
        let scope = authorize_scope(&mut tx, context, note_id.0, Action::ReadData).await?;
        let result = NoteRevision {
            workspace_id: input.workspace_id,
            note_id,
            revision,
            visibility: scope.visibility,
            owner_user_id: scope.owner,
            author: scope.author,
            editor: context.subject,
            title: title.to_owned(),
            body: input.body,
            created_at,
        };
        append_event(
            &mut tx,
            context,
            note_id.0,
            event_type,
            json!({ "note_id": note_id, "revision": revision }),
            None,
        )
        .await?;
        idem.record(&mut tx, note_id.0, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn get_note(
        &self,
        digest: &BearerDigest,
        input: GetNote,
    ) -> Result<NoteRevision, IdentityError> {
        if input.revision.is_some_and(|revision| revision < 1) {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let scope = authorize_scope(&mut tx, context, input.note_id.0, Action::ReadData).await?;
        let row = sqlx::query_as::<_, RevisionRow>(
            "SELECT r.revision, r.title, r.body, r.editor_user_id, r.editor_agent_id, r.created_at
             FROM notes n JOIN note_revisions r ON r.workspace_id = n.workspace_id AND r.note_id = n.id
             WHERE n.workspace_id = $1 AND n.id = $2 AND r.revision = coalesce($3, n.current_revision)",
        ).bind(input.workspace_id.0).bind(input.note_id.0).bind(input.revision)
         .fetch_optional(&mut *tx).await?.ok_or(IdentityError::Forbidden)?;
        tx.commit().await?;
        Ok(NoteRevision {
            workspace_id: input.workspace_id,
            note_id: input.note_id,
            visibility: scope.visibility,
            owner_user_id: scope.owner,
            author: scope.author,
            editor: subject(UserId(row.editor_user_id), row.editor_agent_id),
            revision: row.revision,
            title: row.title,
            body: row.body,
            created_at: row.created_at,
        })
    }
}
