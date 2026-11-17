use chrono::{DateTime, Utc};
use office_domain::{Action, BearerDigest, InstanceId, Resource};
use office_protocol::{AgentIdentity, AgentInstance, AttachAgentInstance};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::{
    IdentityError,
    agent::{agent_actor, agent_binding, load_agent},
    require_permission, valid_name,
};
use crate::Store;

impl Store {
    pub async fn attach_instance(
        &self,
        credential: &BearerDigest,
        input: AttachAgentInstance,
    ) -> Result<AgentInstance, IdentityError> {
        let label = valid_name(&input.label)?;
        let mut tx = self.pool.begin().await?;
        let actor = bind_agent(&mut tx, credential).await?;
        let instance = Uuid::new_v4();
        let inserted = sqlx::query_scalar::<_, DateTime<Utc>>(
            "INSERT INTO agent_instances (id, workspace_id, agent_id, owner_user_id, credential_id, attachment_key, label)
             VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (credential_id, attachment_key) DO NOTHING
             RETURNING expires_at",
        )
        .bind(instance).bind(actor.workspace_id.0).bind(actor.agent_id.0).bind(actor.owner_user_id.0)
        .bind(actor.credential_id.0).bind(input.idempotency_key).bind(label)
        .fetch_optional(&mut *tx).await?;
        let (instance, label, expires_at) = if let Some(expires_at) = inserted {
            (instance, label.to_owned(), expires_at)
        } else {
            let previous = sqlx::query_as::<_, (Uuid, String, DateTime<Utc>)>(
                "SELECT id, label, expires_at FROM agent_instances WHERE credential_id = $1 AND attachment_key = $2
                 AND expires_at > transaction_timestamp()",
            )
            .bind(actor.credential_id.0).bind(input.idempotency_key).fetch_optional(&mut *tx).await?
            .ok_or(IdentityError::Forbidden)?;
            if previous.1 != label {
                return Err(IdentityError::IdempotencyConflict);
            }
            previous
        };
        tx.commit().await?;
        Ok(as_instance(&actor, InstanceId(instance), label, expires_at))
    }

    pub async fn get_instance(
        &self,
        credential: &BearerDigest,
        instance_id: InstanceId,
    ) -> Result<AgentInstance, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let actor = bind_agent(&mut tx, credential).await?;
        let (label, expires_at) = sqlx::query_as::<_, (String, DateTime<Utc>)>(
            "SELECT label, expires_at FROM agent_instances WHERE id = $1 AND credential_id = $2
             AND workspace_id = $3 AND agent_id = $4 AND expires_at > transaction_timestamp()",
        )
        .bind(instance_id.0)
        .bind(actor.credential_id.0)
        .bind(actor.workspace_id.0)
        .bind(actor.agent_id.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(IdentityError::Forbidden)?;
        tx.commit().await?;
        Ok(as_instance(&actor, instance_id, label, expires_at))
    }
}

async fn bind_agent(
    tx: &mut Transaction<'_, Postgres>,
    credential: &BearerDigest,
) -> Result<AgentIdentity, IdentityError> {
    let binding = agent_binding(tx, credential).await?;
    let actor = load_agent(tx, binding).await?;
    require_permission(
        agent_actor(&actor),
        Action::AccessInstance,
        Resource::Instance {
            workspace: actor.workspace_id,
            agent: actor.agent_id,
            credential: actor.credential_id,
        },
    )?;
    sqlx::query("SELECT set_config('office.credential_id', $1, true)")
        .bind(actor.credential_id.0.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(actor)
}

fn as_instance(
    actor: &AgentIdentity,
    id: InstanceId,
    label: String,
    expires_at: DateTime<Utc>,
) -> AgentInstance {
    AgentInstance {
        instance_id: id,
        label,
        expires_at,
        agent_id: actor.agent_id,
        workspace_id: actor.workspace_id,
        credential_id: actor.credential_id,
    }
}
