use chrono::{DateTime, Utc};
use office_domain::{Action, BearerDigest, Resource, RunnerId, UserId, Visibility, WorkspaceId};
use office_protocol::{
    AdvertiseRunner, ExecutionBudget, ExecutionBudgetReceipt, GetExecutionBudget, GetRunnerProfile,
    ManagedRuntime, RunnerProfile, SetExecutionBudget, valid_capabilities,
};
use serde_json::json;
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use crate::{
    Store,
    checkouts::checkout_runner,
    data_scope::create_scope,
    events::{Idempotency, append_event},
    identity::{IdentityError, require_permission, workspace_context},
    inbox::save_cursor,
};

#[derive(sqlx::FromRow)]
struct ProfileRow {
    runner_id: Uuid,
    workspace_id: Uuid,
    owner_user_id: Uuid,
    revision: i64,
    capacity: i16,
    runtime_template_ids: Vec<Uuid>,
    hosted_checkout_ids: Vec<Uuid>,
    capabilities: Vec<String>,
    expires_at: DateTime<Utc>,
}

impl From<ProfileRow> for RunnerProfile {
    fn from(profile: ProfileRow) -> Self {
        Self {
            runner_id: RunnerId(profile.runner_id),
            workspace_id: WorkspaceId(profile.workspace_id),
            owner_user_id: UserId(profile.owner_user_id),
            revision: profile.revision,
            capacity: profile.capacity as u16,
            runtime_template_ids: profile.runtime_template_ids,
            hosted_checkout_ids: profile.hosted_checkout_ids,
            capabilities: profile.capabilities,
            expires_at: profile.expires_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct BudgetRow {
    id: Uuid,
    workspace_id: Uuid,
    owner_user_id: Uuid,
    revision: i64,
    limit_units: i64,
    reserved_units: i64,
    charged_units: i64,
}

impl From<BudgetRow> for ExecutionBudget {
    fn from(budget: BudgetRow) -> Self {
        Self {
            budget_id: budget.id,
            workspace_id: WorkspaceId(budget.workspace_id),
            owner_user_id: UserId(budget.owner_user_id),
            revision: budget.revision,
            limit_units: budget.limit_units,
            reserved_units: budget.reserved_units,
            charged_units: budget.charged_units,
        }
    }
}

impl Store {
    pub async fn advertise_runner(
        &self,
        digest: &BearerDigest,
        mut input: AdvertiseRunner,
    ) -> Result<RunnerProfile, IdentityError> {
        if !(0..i64::MAX).contains(&input.expected_revision)
            || input.capacity > 64
            || !(10..=120).contains(&input.ttl_seconds)
            || input.runtime_template_ids.len() > 64
            || input.hosted_checkout_ids.len() > 32
            || !valid_capabilities(&input.capabilities, 32)
        {
            return Err(IdentityError::InvalidInput);
        }
        input.runtime_template_ids.sort_unstable();
        input.hosted_checkout_ids.sort_unstable();
        if input
            .runtime_template_ids
            .windows(2)
            .any(|pair| pair[0] == pair[1])
            || input
                .hosted_checkout_ids
                .windows(2)
                .any(|pair| pair[0] == pair[1])
        {
            return Err(IdentityError::InvalidInput);
        }
        input.capabilities.sort_unstable();
        let mut tx = self.pool.begin().await?;
        let runner = checkout_runner(&mut tx, digest).await?;
        lock_runner(&mut tx, runner.workspace_id, runner.runner_id).await?;
        let current = profile(&mut tx, runner.workspace_id, runner.runner_id).await?;
        check_revision(
            input.expected_revision,
            current.map_or(0, |current| current.revision),
        )?;
        let templates = sqlx::query_scalar::<_, Json<ManagedRuntime>>(
            "SELECT managed FROM runtime_templates WHERE workspace_id=$1 AND owner_user_id=$2
                 AND id=ANY($3) AND kind='managed'",
        )
        .bind(runner.workspace_id.0)
        .bind(runner.owner_user_id.0)
        .bind(&input.runtime_template_ids)
        .fetch_all(&mut *tx)
        .await?;
        if templates.len() != input.runtime_template_ids.len() {
            return Err(IdentityError::Forbidden);
        }
        let hosted = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM project_checkouts WHERE workspace_id=$1 AND owner_user_id=$2
                AND host_runner_id=$3 AND state='registered' AND id=ANY($4)",
        )
        .bind(runner.workspace_id.0)
        .bind(runner.owner_user_id.0)
        .bind(runner.runner_id.0)
        .bind(&input.hosted_checkout_ids)
        .fetch_one(&mut *tx)
        .await?;
        if hosted != input.hosted_checkout_ids.len() as i64 {
            return Err(IdentityError::Forbidden);
        }
        if templates.iter().any(|template| {
            !template
                .0
                .required_capabilities
                .iter()
                .all(|label| input.capabilities.binary_search(label).is_ok())
        }) {
            return Err(IdentityError::InvalidInput);
        }
        let output = sqlx::query_as::<_, ProfileRow>(
            "INSERT INTO runner_execution_profiles
                (runner_id,workspace_id,owner_user_id,capacity,runtime_template_ids,capabilities,expires_at,hosted_checkout_ids)
             VALUES ($1,$2,$3,$4,$5,$6,clock_timestamp()+make_interval(secs=>$7),$8)
             ON CONFLICT (runner_id) DO UPDATE SET capacity=excluded.capacity,
                runtime_template_ids=excluded.runtime_template_ids,capabilities=excluded.capabilities,
                hosted_checkout_ids=excluded.hosted_checkout_ids,
                expires_at=excluded.expires_at,revision=runner_execution_profiles.revision+1 RETURNING *",
        )
        .bind(runner.runner_id.0).bind(runner.workspace_id.0).bind(runner.owner_user_id.0)
        .bind(input.capacity as i16).bind(input.runtime_template_ids).bind(input.capabilities)
        .bind(f64::from(input.ttl_seconds)).bind(input.hosted_checkout_ids).fetch_one(&mut *tx).await?;
        sqlx::query(
            "DELETE FROM checkout_execution_presence WHERE workspace_id=$1 AND runner_id=$2",
        )
        .bind(runner.workspace_id.0)
        .bind(runner.runner_id.0)
        .execute(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO checkout_execution_presence (workspace_id,checkout_id,owner_user_id,runner_id,expires_at)
             SELECT $1, checkout, $2, $3, $4 FROM unnest($5::uuid[]) AS checkout")
            .bind(runner.workspace_id.0).bind(runner.owner_user_id.0).bind(runner.runner_id.0)
            .bind(output.expires_at).bind(&output.hosted_checkout_ids).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(output.into())
    }

    pub async fn runner_profile(
        &self,
        digest: &BearerDigest,
    ) -> Result<Option<RunnerProfile>, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let runner = checkout_runner(&mut tx, digest).await?;
        let result = profile(&mut tx, runner.workspace_id, runner.runner_id)
            .await?
            .map(Into::into);
        tx.commit().await?;
        Ok(result)
    }

    pub async fn runner_runtime(
        &self,
        digest: &BearerDigest,
        template: Uuid,
    ) -> Result<ManagedRuntime, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let runner = checkout_runner(&mut tx, digest).await?;
        let runtime = sqlx::query_scalar::<_, Json<ManagedRuntime>>(
            "SELECT managed FROM runtime_templates
             WHERE workspace_id=$1 AND owner_user_id=$2 AND id=$3 AND kind='managed'",
        )
        .bind(runner.workspace_id.0)
        .bind(runner.owner_user_id.0)
        .bind(template)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(IdentityError::Forbidden)?;
        tx.commit().await?;
        Ok(runtime.0)
    }

    pub async fn get_runner_profile(
        &self,
        digest: &BearerDigest,
        input: GetRunnerProfile,
    ) -> Result<RunnerProfile, IdentityError> {
        let mut tx = self.pool.begin().await?;
        workspace_context(&mut tx, digest, input.workspace_id).await?;
        let output = profile(&mut tx, input.workspace_id, input.runner_id)
            .await?
            .ok_or(IdentityError::Forbidden)?;
        tx.commit().await?;
        Ok(output.into())
    }

    pub async fn set_execution_budget(
        &self,
        digest: &BearerDigest,
        input: SetExecutionBudget,
    ) -> Result<ExecutionBudgetReceipt, IdentityError> {
        if !(0..i64::MAX).contains(&input.expected_revision)
            || !(0..=1_000_000_000_000).contains(&input.limit_units)
        {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let owner = context.access.member;
        require_permission(
            context.actor,
            Action::ManageExecutionBudget,
            Resource::Account(owner),
        )?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "execution_budget.set",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        lock_budget(&mut tx, input.workspace_id, owner).await?;
        let current = budget(&mut tx, input.workspace_id, owner).await?;
        check_revision(
            input.expected_revision,
            current.as_ref().map_or(0, |row| row.revision),
        )?;
        let row = match current {
            Some(current) => {
                if input.limit_units < current.reserved_units + current.charged_units {
                    return Err(IdentityError::InvalidTransition);
                }
                sqlx::query_as::<_, BudgetRow>(
                    "UPDATE execution_budgets SET limit_units=$3, revision=revision+1
                   WHERE workspace_id=$1 AND owner_user_id=$2 RETURNING *",
                )
                .bind(input.workspace_id.0)
                .bind(owner.0)
                .bind(input.limit_units)
                .fetch_one(&mut *tx)
                .await?
            }
            None => {
                let id = Uuid::new_v4();
                create_scope(&mut tx, context, id, Visibility::OwnerPrivate, &[]).await?;
                sqlx::query_as::<_, BudgetRow>(
                    "INSERT INTO execution_budgets (id,workspace_id,owner_user_id,limit_units)
                   VALUES ($1,$2,$3,$4) RETURNING *",
                )
                .bind(id)
                .bind(input.workspace_id.0)
                .bind(owner.0)
                .bind(input.limit_units)
                .fetch_one(&mut *tx)
                .await?
            }
        };
        let sequence = append_event(
            &mut tx,
            context,
            row.id,
            "execution_budget.changed",
            json!({"budget_id": row.id, "revision": row.revision, "limit_units": row.limit_units}),
            None,
        )
        .await?;
        let scoped = row.id;
        let receipt = ExecutionBudgetReceipt {
            budget: row.into(),
            event_cursor: save_cursor(&mut tx, context, sequence).await?,
        };
        idem.record(&mut tx, scoped, &receipt).await?;
        tx.commit().await?;
        Ok(receipt)
    }

    pub async fn get_execution_budget(
        &self,
        digest: &BearerDigest,
        input: GetExecutionBudget,
    ) -> Result<Option<ExecutionBudget>, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let owner = context.access.member;
        require_permission(
            context.actor,
            Action::ManageExecutionBudget,
            Resource::Account(owner),
        )?;
        let result = budget(&mut tx, input.workspace_id, owner)
            .await?
            .map(Into::into);
        tx.commit().await?;
        Ok(result)
    }
}

async fn profile(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    runner: RunnerId,
) -> Result<Option<ProfileRow>, IdentityError> {
    Ok(sqlx::query_as(
        "SELECT * FROM runner_execution_profiles WHERE workspace_id=$1 AND runner_id=$2",
    )
    .bind(workspace.0)
    .bind(runner.0)
    .fetch_optional(&mut **tx)
    .await?)
}

async fn budget(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    owner: UserId,
) -> Result<Option<BudgetRow>, IdentityError> {
    Ok(
        sqlx::query_as(
            "SELECT * FROM execution_budgets WHERE workspace_id=$1 AND owner_user_id=$2",
        )
        .bind(workspace.0)
        .bind(owner.0)
        .fetch_optional(&mut **tx)
        .await?,
    )
}

pub(crate) async fn lock_runner(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    runner: RunnerId,
) -> Result<(), IdentityError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("runner-execution/{}/{}", workspace.0, runner.0))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub(crate) async fn ready_profile(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    runner: RunnerId,
) -> Result<Option<RunnerProfile>, IdentityError> {
    Ok(sqlx::query_as::<_, ProfileRow>(
        "SELECT * FROM runner_execution_profiles WHERE workspace_id=$1 AND runner_id=$2
         AND capacity>0 AND expires_at>clock_timestamp()",
    )
    .bind(workspace.0)
    .bind(runner.0)
    .fetch_optional(&mut **tx)
    .await?
    .map(Into::into))
}

pub(crate) async fn lock_budget(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    owner: UserId,
) -> Result<(), IdentityError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("execution-budget/{}/{}", workspace.0, owner.0))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn check_revision(expected: i64, current: i64) -> Result<(), IdentityError> {
    if expected != current {
        return Err(IdentityError::RevisionMismatch { current });
    }
    Ok(())
}
