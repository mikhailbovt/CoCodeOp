use office_domain::{Action, WorkspaceId};
use office_protocol::Recipient;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use crate::{
    data_scope::authorize_scope,
    identity::{IdentityError, WorkspaceContext},
};

pub(crate) async fn append_event(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    scope: Uuid,
    event_type: &str,
    payload: Value,
    recipient: Option<Recipient>,
) -> Result<i64, IdentityError> {
    let address = recipient.map(|target| (context.recipient(), target));
    append_workspace_event(
        tx,
        context.access.workspace,
        scope,
        event_type,
        payload,
        address,
    )
    .await
}

pub(crate) async fn append_workspace_event(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    scope: Uuid,
    event_type: &str,
    payload: Value,
    address: Option<(Recipient, Recipient)>,
) -> Result<i64, IdentityError> {
    let workspace = workspace.0;
    let sender = address.map(|(sender, _)| sender);
    let recipient = address.map(|(_, recipient)| recipient);
    // Lock the workspace's counter until both the state mutation and the event commit.
    let sequence: i64 = sqlx::query_scalar(
        "INSERT INTO workspace_event_counters (workspace_id, last_sequence) VALUES ($1, 1)
         ON CONFLICT (workspace_id) DO UPDATE
         SET last_sequence = workspace_event_counters.last_sequence + 1 RETURNING last_sequence",
    )
    .bind(workspace)
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO events (workspace_id, sequence, id, scope_id, event_type, payload,
                            sender_user_id, sender_agent_id, recipient_user_id, recipient_agent_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(workspace)
    .bind(sequence)
    .bind(Uuid::new_v4())
    .bind(scope)
    .bind(event_type)
    .bind(Json(payload))
    .bind(sender.map(|item| item.user_id.0))
    .bind(sender.and_then(|item| item.agent_id.map(|id| id.0)))
    .bind(recipient.map(|item| item.user_id.0))
    .bind(recipient.and_then(|item| item.agent_id.map(|id| id.0)))
    .execute(&mut **tx)
    .await?;
    sqlx::query("INSERT INTO outbox (workspace_id, sequence) VALUES ($1, $2)")
        .bind(workspace)
        .bind(sequence)
        .execute(&mut **tx)
        .await?;
    Ok(sequence)
}

pub(crate) struct Idempotency {
    context: WorkspaceContext,
    operation: &'static str,
    key: Uuid,
    hash: [u8; 32],
}

impl Idempotency {
    pub(crate) async fn lock(
        tx: &mut Transaction<'_, Postgres>,
        context: WorkspaceContext,
        operation: &'static str,
        key: Uuid,
        request: &impl Serialize,
    ) -> Result<Self, IdentityError> {
        let request = serde_json::to_vec(request).map_err(|_| IdentityError::InvalidInput)?;
        let hash = Sha256::digest(request).into();
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!(
                "idem/{}/{}/{:?}/{operation}/{key}",
                context.access.workspace.0,
                context.access.member.0,
                context.agent_id()
            ))
            .execute(&mut **tx)
            .await?;
        Ok(Self {
            context,
            operation,
            key,
            hash,
        })
    }

    pub(crate) async fn replay<T: DeserializeOwned + Send + Unpin + 'static>(
        &self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<Option<T>, IdentityError> {
        let row = sqlx::query_as::<_, (Vec<u8>, Uuid, Json<T>)>(
            "SELECT request_hash, result_scope_id, result FROM idempotency_records
             WHERE workspace_id = $1 AND user_id = $2 AND agent_id IS NOT DISTINCT FROM $3
               AND operation = $4 AND idempotency_key = $5",
        )
        .bind(self.context.access.workspace.0)
        .bind(self.context.access.member.0)
        .bind(self.context.agent_id().map(|id| id.0))
        .bind(self.operation)
        .bind(self.key)
        .fetch_optional(&mut **tx)
        .await?;
        let Some((hash, scope, result)) = row else {
            return Ok(None);
        };
        authorize_scope(tx, self.context, scope, Action::ReadData).await?;
        if hash != self.hash {
            return Err(IdentityError::IdempotencyConflict);
        }
        Ok(Some(result.0))
    }

    pub(crate) async fn record<T: Serialize>(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        scope: Uuid,
        result: &T,
    ) -> Result<(), IdentityError> {
        let row = sqlx::query(
            "INSERT INTO idempotency_records
                (workspace_id, user_id, agent_id, operation, idempotency_key, request_hash, result_scope_id, result)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT DO NOTHING",
        ).bind(self.context.access.workspace.0).bind(self.context.access.member.0)
         .bind(self.context.agent_id().map(|id| id.0)).bind(self.operation).bind(self.key)
         .bind(self.hash.as_slice()).bind(scope).bind(Json(result)).execute(&mut **tx).await?;
        if row.rows_affected() == 1 {
            Ok(())
        } else {
            Err(IdentityError::IdempotencyConflict)
        }
    }
}
