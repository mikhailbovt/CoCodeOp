use chrono::{DateTime, Utc};
use office_domain::{AgentId, AgentRole, BearerDigest, Visibility, WorkspaceId};
use office_protocol::{
    ActivationPolicy, ActivationTrigger, GetActivationPolicy, ManagedActivation, Recipient,
    SetActivationPolicy, packaged_backend,
};
use serde_json::json;
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use super::select_managed_runtime;
use crate::{
    Store,
    data_scope::create_scope,
    events::{Idempotency, append_event},
    identity::{IdentityError, WorkspaceContext, active_principal, workspace_context},
};

mod dispatch;
mod review;
mod run_context;
pub(super) use dispatch::{admit, validate_activation};
pub(super) use run_context::{missing_activation_effect, run_activation};

#[derive(sqlx::FromRow)]
struct PolicyRow {
    id: Uuid,
    revision: i64,
    managed: Option<Json<ManagedActivation>>,
    updated_at: DateTime<Utc>,
}

fn policy_view(workspace: WorkspaceId, agent: AgentId, row: Option<PolicyRow>) -> ActivationPolicy {
    let (revision, managed, updated_at) = row.map_or((0, None, None), |value| {
        (
            value.revision,
            value.managed.map(|plan| plan.0),
            Some(value.updated_at),
        )
    });
    ActivationPolicy {
        workspace_id: workspace,
        agent_id: agent,
        revision,
        managed,
        updated_at,
    }
}

impl Store {
    pub async fn get_activation_policy(
        &self,
        digest: &BearerDigest,
        input: GetActivationPolicy,
    ) -> Result<ActivationPolicy, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        if context
            .agent_id()
            .is_some_and(|agent| agent != input.agent_id)
        {
            return Err(IdentityError::Forbidden);
        }
        owned_agent(&mut tx, context, input.agent_id).await?;
        let row = sqlx::query_as::<_, PolicyRow>(
            "SELECT id,revision,managed,updated_at FROM activation_policies WHERE workspace_id=$1 AND agent_id=$2",
        ).bind(input.workspace_id.0).bind(input.agent_id.0).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        Ok(policy_view(input.workspace_id, input.agent_id, row))
    }

    pub async fn set_activation_policy(
        &self,
        digest: &BearerDigest,
        input: SetActivationPolicy,
    ) -> Result<ActivationPolicy, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        if context.agent_id().is_some() {
            return Err(IdentityError::Forbidden);
        }
        let role = owned_agent(&mut tx, context, input.agent_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "activation.configure",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(format!(
                "activation-policy/{}/{}",
                input.workspace_id.0, input.agent_id.0
            ))
            .execute(&mut *tx)
            .await?;
        let previous = sqlx::query_as::<_, PolicyRow>(
            "SELECT id,revision,managed,updated_at FROM activation_policies WHERE workspace_id=$1 AND agent_id=$2 FOR UPDATE",
        ).bind(input.workspace_id.0).bind(input.agent_id.0).fetch_optional(&mut *tx).await?;
        let revision = previous.as_ref().map_or(0, |row| row.revision);
        if revision != input.expected_revision {
            return Err(IdentityError::RevisionMismatch { current: revision });
        }
        if let Some(managed) = &input.managed {
            validate_plan(&mut tx, context, role, managed).await?;
        }
        let id = match previous {
            Some(row) if row.managed.as_ref().map(|plan| &plan.0) == input.managed.as_ref() => {
                let id = row.id;
                let result = policy_view(input.workspace_id, input.agent_id, Some(row));
                idem.record(&mut tx, id, &result).await?;
                tx.commit().await?;
                return Ok(result);
            }
            Some(row) => row.id,
            None => {
                let id = Uuid::new_v4();
                create_scope(&mut tx, context, id, Visibility::OwnerPrivate, &[]).await?;
                id
            }
        };
        let result = sqlx::query_as::<_, PolicyRow>(
            "INSERT INTO activation_policies (id,workspace_id,owner_user_id,agent_id,revision,managed,start_after_sequence)
             VALUES ($1,$2,$3,$4,1,$5,coalesce((SELECT last_sequence FROM workspace_event_counters WHERE workspace_id=$2),0))
             ON CONFLICT (workspace_id,agent_id) DO UPDATE SET managed=excluded.managed,
                 revision=activation_policies.revision+1,start_after_sequence=excluded.start_after_sequence,updated_at=clock_timestamp()
             RETURNING id,revision,managed,updated_at",
        ).bind(id).bind(input.workspace_id.0).bind(context.access.member.0).bind(input.agent_id.0)
         .bind(input.managed.map(Json)).fetch_one(&mut *tx).await?;
        let result = policy_view(input.workspace_id, input.agent_id, Some(result));
        append_event(
            &mut tx,
            context,
            id,
            "activation.configured",
            json!({"agent_id":input.agent_id,
            "revision":result.revision,"managed":result.managed.is_some()}),
            None,
        )
        .await?;
        idem.record(&mut tx, id, &result).await?;
        tx.commit().await?;
        Ok(result)
    }
}

async fn owned_agent(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    agent_id: AgentId,
) -> Result<AgentRole, IdentityError> {
    active_principal(
        tx,
        context,
        Recipient {
            user_id: context.access.member,
            agent_id: Some(agent_id),
        },
    )
    .await?
    .ok_or(IdentityError::Forbidden)
}

async fn validate_plan(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    role: AgentRole,
    plan: &ManagedActivation,
) -> Result<(), IdentityError> {
    if !(1..=4).contains(&plan.triggers.len())
        || plan
            .triggers
            .iter()
            .enumerate()
            .any(|(index, trigger)| plan.triggers[..index].contains(trigger))
        || plan.triggers.contains(&ActivationTrigger::Assignment) && role == AgentRole::Reviewer
        || plan.triggers.contains(&ActivationTrigger::ReviewRequest) && role == AgentRole::Worker
        || !(1..=30).contains(&plan.debounce_seconds)
        || !(1..=300).contains(&plan.cooldown_seconds)
        || !(1..=8).contains(&plan.max_turns_per_thread)
        || !(1..=64).contains(&plan.max_turns_per_hour)
    {
        return Err(IdentityError::InvalidInput);
    }
    let runner: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM runners WHERE workspace_id=$1 AND id=$2 AND owner_user_id=$3 AND disabled_at IS NULL)",
    ).bind(context.access.workspace.0).bind(plan.runner_id.0).bind(context.access.member.0).fetch_one(&mut **tx).await?;
    if !runner {
        return Err(IdentityError::Forbidden);
    }
    let runtime = select_managed_runtime(
        tx,
        context.access.workspace,
        context.access.member,
        plan.runtime_template_id,
    )
    .await?;
    if packaged_backend(&runtime.adapter_id).is_none() {
        return Err(IdentityError::InvalidInput);
    }
    Ok(())
}
