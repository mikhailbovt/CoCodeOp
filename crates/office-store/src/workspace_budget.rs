use office_domain::{BearerDigest, UserId, Visibility, WorkspaceId, WorkspaceRole};
use office_protocol::{GetExecutionBudget, SetWorkspaceBudget, WorkspaceBudget};
use serde_json::json;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::{
    Store,
    data_scope::create_scope,
    events::{Idempotency, append_event},
    execution_control::lock_admission,
    identity::{IdentityError, workspace_context},
};

#[derive(sqlx::FromRow)]
struct BudgetRow {
    revision: i64,
    limit_units: i64,
    member_user_ids: Vec<Uuid>,
}

impl Store {
    pub async fn set_workspace_budget(
        &self,
        digest: &BearerDigest,
        input: SetWorkspaceBudget,
    ) -> Result<WorkspaceBudget, IdentityError> {
        let workspace = input.workspace_id;
        let mut members = input
            .member_user_ids
            .iter()
            .map(|id| id.0)
            .collect::<Vec<_>>();
        members.sort_unstable();
        if members.len() > 64
            || members.windows(2).any(|pair| pair[0] == pair[1])
            || !(0..=1_000_000_000).contains(&input.limit_units)
        {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, workspace).await?;
        if context.agent_id().is_some() || !admin(context.access.role) {
            return Err(IdentityError::Forbidden);
        }
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "workspace_budget.set",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        lock_admission(&mut tx, workspace).await?;
        bind(&mut tx, "configuration").await?;
        let previous = load(&mut tx, workspace)
            .await?
            .map_or(0, |item| item.revision);
        if previous != input.expected_revision {
            return Err(IdentityError::RevisionMismatch { current: previous });
        }
        let eligible = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM workspace_memberships WHERE workspace_id=$1 AND user_id=ANY($2) AND revoked_at IS NULL",
        ).bind(workspace.0).bind(&members).fetch_one(&mut *tx).await?;
        if eligible != members.len() as i64 {
            return Err(IdentityError::Forbidden);
        }
        let available = usage(&mut tx, workspace).await?;
        if available.0 + available.1 > input.limit_units {
            return Err(IdentityError::BudgetExhausted);
        }
        let row = sqlx::query_as::<_, BudgetRow>(
            "INSERT INTO workspace_execution_budgets (workspace_id,revision,limit_units,member_user_ids)
             VALUES($1,1,$2,$3) ON CONFLICT(workspace_id) DO UPDATE SET
                revision=workspace_execution_budgets.revision+1,limit_units=excluded.limit_units,member_user_ids=excluded.member_user_ids
             RETURNING revision,limit_units,member_user_ids",
        ).bind(workspace.0).bind(input.limit_units).bind(members).fetch_one(&mut *tx).await?;
        let result = receipt(workspace, row, available);
        let scope = Uuid::new_v4();
        create_scope(&mut tx, context, scope, Visibility::Workspace, &[]).await?;
        append_event(
            &mut tx,
            context,
            scope,
            "execution.workspace_budget_changed",
            json!({
                "revision": result.revision, "limit_units": result.limit_units,
            }),
            None,
        )
        .await?;
        idem.record(&mut tx, scope, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn get_workspace_budget(
        &self,
        digest: &BearerDigest,
        input: GetExecutionBudget,
    ) -> Result<Option<WorkspaceBudget>, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let workspace = input.workspace_id;
        let context = workspace_context(&mut tx, digest, workspace).await?;
        if context.agent_id().is_some() {
            return Err(IdentityError::Forbidden);
        }
        bind(
            &mut tx,
            if admin(context.access.role) {
                "configuration"
            } else {
                "accounting"
            },
        )
        .await?;
        let result = match load(&mut tx, workspace).await? {
            Some(row) => Some(receipt(workspace, row, usage(&mut tx, workspace).await?)),
            None => None,
        };
        tx.commit().await?;
        Ok(result)
    }
}

/// The queue holds the same workspace admission lock as grant configuration.
pub(crate) async fn admission(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    owner: UserId,
    requested: i64,
) -> Result<bool, IdentityError> {
    let Some(row) = load(tx, workspace).await? else {
        return Ok(false);
    };
    if !row.member_user_ids.contains(&owner.0) {
        return Ok(false);
    }
    bind(tx, "accounting").await?;
    let (reserved, charged) = usage(tx, workspace).await?;
    bind(tx, "").await?;
    if reserved + charged + requested > row.limit_units {
        return Err(IdentityError::BudgetExhausted);
    }
    Ok(true)
}

fn admin(role: WorkspaceRole) -> bool {
    matches!(role, WorkspaceRole::Admin | WorkspaceRole::Owner)
}

fn receipt(workspace_id: WorkspaceId, row: BudgetRow, amounts: (i64, i64)) -> WorkspaceBudget {
    WorkspaceBudget {
        workspace_id,
        revision: row.revision,
        limit_units: row.limit_units,
        member_user_ids: row.member_user_ids.into_iter().map(UserId).collect(),
        reserved_units: amounts.0,
        charged_units: amounts.1,
    }
}

async fn load(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
) -> Result<Option<BudgetRow>, IdentityError> {
    Ok(sqlx::query_as("SELECT revision,limit_units,member_user_ids FROM workspace_execution_budgets WHERE workspace_id=$1")
        .bind(workspace.0).fetch_optional(&mut **tx).await?)
}

async fn usage(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
) -> Result<(i64, i64), IdentityError> {
    Ok(sqlx::query_as(
        "SELECT coalesce(sum(units) FILTER (WHERE released_at IS NULL),0)::bigint,
                coalesce(sum(charged_units),0)::bigint
         FROM execution_budget_reservations WHERE workspace_id=$1 AND uses_workspace_budget",
    )
    .bind(workspace.0)
    .fetch_one(&mut **tx)
    .await?)
}

async fn bind(tx: &mut Transaction<'_, Postgres>, context: &str) -> Result<(), IdentityError> {
    sqlx::query("SELECT set_config('office.workspace_budget_context',$1,true)")
        .bind(context)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
