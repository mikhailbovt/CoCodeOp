use chrono::{DateTime, Utc};
use office_domain::{
    Action, BearerDigest, Resource, RunnerId, SourceManifest, UserId, WorkspaceId,
};
use office_protocol::{
    CheckoutEnrollment, CheckoutMode, CheckoutReceipt, CheckoutState, CreateProject, GetCheckout,
    GetProject, MAX_ROOT_ANCESTORS, MAX_SOURCE_BYTES, MAX_SOURCE_FILES, Project, ProjectCheckout,
    ProjectReceipt, RequestCheckout, RootProof, RunnerIdentity, SourceBaseline, WriteStrategy,
};
use serde_json::json;
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use crate::{
    Store,
    data_scope::{authorize_scope, create_scope},
    events::{Idempotency, append_event, append_workspace_event},
    identity::{
        IdentityError, require_permission, runner::authenticate_runner_in, valid_name,
        workspace_context,
    },
    inbox::save_cursor,
};

pub(crate) mod areas;
pub(crate) mod coordinator;
pub(crate) mod grants;

#[derive(sqlx::FromRow)]
struct ProjectRow {
    id: Uuid,
    workspace_id: Uuid,
    owner_user_id: Uuid,
    name: String,
    created_at: DateTime<Utc>,
}

impl From<ProjectRow> for Project {
    fn from(row: ProjectRow) -> Self {
        Self {
            project_id: row.id,
            workspace_id: WorkspaceId(row.workspace_id),
            owner_user_id: UserId(row.owner_user_id),
            name: row.name,
            created_at: row.created_at,
        }
    }
}

#[derive(sqlx::FromRow)]
pub(crate) struct CheckoutRow {
    pub(crate) id: Uuid,
    workspace_id: Uuid,
    pub(crate) project_id: Uuid,
    pub(crate) owner_user_id: Uuid,
    pub(crate) host_runner_id: Uuid,
    pub(crate) state: String,
    revision: i64,
    pub(crate) manifest: Json<SourceManifest>,
    pub(crate) root_ref: Option<Uuid>,
    pub(crate) baseline: Option<Json<SourceBaseline>>,
}

impl CheckoutRow {
    fn into_checkout(self, include_manifest: bool) -> Result<ProjectCheckout, IdentityError> {
        let state = match self.state.as_str() {
            "pending_registration" => CheckoutState::PendingRegistration,
            "registered" => CheckoutState::Registered,
            _ => return Err(IdentityError::InvalidRole),
        };
        Ok(ProjectCheckout {
            checkout_id: self.id,
            workspace_id: WorkspaceId(self.workspace_id),
            project_id: self.project_id,
            host_runner_id: RunnerId(self.host_runner_id),
            mode: CheckoutMode::Shared,
            write_strategy: WriteStrategy::Mediated,
            state,
            revision: self.revision,
            root_ref: self.root_ref,
            manifest: include_manifest.then_some(self.manifest.0),
            baseline: if include_manifest {
                self.baseline.map(|value| value.0)
            } else {
                None
            },
        })
    }
}

impl Store {
    pub async fn create_project(
        &self,
        digest: &BearerDigest,
        input: CreateProject,
    ) -> Result<ProjectReceipt, IdentityError> {
        let name = valid_name(&input.name)?;
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        require_permission(
            context.actor,
            Action::CreateProject,
            Resource::Account(context.access.member),
        )?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "project.create",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let id = Uuid::new_v4();
        create_scope(&mut tx, context, id, input.visibility, &input.grants).await?;
        let row = sqlx::query_as::<_, ProjectRow>(
            "INSERT INTO projects (id, workspace_id, owner_user_id, name) VALUES ($1, $2, $3, $4) RETURNING *",
        ).bind(id).bind(input.workspace_id.0).bind(context.access.member.0).bind(name).fetch_one(&mut *tx).await?;
        let sequence = append_event(
            &mut tx,
            context,
            id,
            "project.created",
            json!({"project_id": id}),
            None,
        )
        .await?;
        let result = ProjectReceipt {
            project: row.into(),
            event_cursor: save_cursor(&mut tx, context, sequence).await?,
        };
        idem.record(&mut tx, id, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn get_project(
        &self,
        digest: &BearerDigest,
        input: GetProject,
    ) -> Result<Project, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        authorize_scope(&mut tx, context, input.project_id, Action::ReadData).await?;
        let row = load_project(&mut tx, input.workspace_id, input.project_id).await?;
        tx.commit().await?;
        Ok(row.into())
    }

    pub async fn request_checkout(
        &self,
        digest: &BearerDigest,
        input: RequestCheckout,
    ) -> Result<CheckoutReceipt, IdentityError> {
        if !input.manifest.is_valid() {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let project = load_project(&mut tx, input.workspace_id, input.project_id).await?;
        require_permission(
            context.actor,
            Action::RegisterCheckout,
            Resource::Account(UserId(project.owner_user_id)),
        )?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "checkout.request",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let runner = sqlx::query_scalar::<_, bool>(
            "SELECT true FROM runners WHERE workspace_id = $1 AND id = $2 AND owner_user_id = $3
               AND disabled_at IS NULL FOR SHARE",
        )
        .bind(input.workspace_id.0)
        .bind(input.host_runner_id.0)
        .bind(project.owner_user_id)
        .fetch_optional(&mut *tx)
        .await?;
        if runner.is_none() {
            return Err(IdentityError::Forbidden);
        }
        let id = Uuid::new_v4();
        let row = sqlx::query_as::<_, CheckoutRow>(
            "INSERT INTO project_checkouts (id, workspace_id, project_id, owner_user_id, host_runner_id, manifest)
             VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT (workspace_id, project_id) DO NOTHING RETURNING *",
        ).bind(id).bind(input.workspace_id.0).bind(input.project_id).bind(project.owner_user_id)
         .bind(input.host_runner_id.0).bind(Json(input.manifest)).fetch_optional(&mut *tx).await?
         .ok_or(IdentityError::InvalidTransition)?;
        let sequence = append_event(
            &mut tx,
            context,
            input.project_id,
            "checkout.requested",
            json!({"checkout_id": id, "host_runner_id": input.host_runner_id}),
            None,
        )
        .await?;
        let result = CheckoutReceipt {
            checkout: row.into_checkout(true)?,
            event_cursor: save_cursor(&mut tx, context, sequence).await?,
        };
        idem.record(&mut tx, input.project_id, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn get_checkout(
        &self,
        digest: &BearerDigest,
        input: GetCheckout,
    ) -> Result<ProjectCheckout, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let row = load_checkout(&mut tx, input.workspace_id, input.checkout_id, false).await?;
        authorize_scope(&mut tx, context, row.project_id, Action::ReadData).await?;
        let is_owner = context.access.member.0 == row.owner_user_id && context.agent_id().is_none();
        let result = row.into_checkout(is_owner)?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn get_runner_checkout(
        &self,
        digest: &BearerDigest,
        checkout: Uuid,
    ) -> Result<ProjectCheckout, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let runner = checkout_runner(&mut tx, digest).await?;
        let result = load_checkout(&mut tx, runner.workspace_id, checkout, false)
            .await?
            .into_checkout(true)?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn enroll_checkout(
        &self,
        digest: &BearerDigest,
        input: CheckoutEnrollment,
    ) -> Result<ProjectCheckout, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let runner = checkout_runner(&mut tx, digest).await?;
        validate_proof(&input.proof)?;
        let workspace = runner.workspace_id;
        let row = load_checkout(&mut tx, workspace, input.checkout_id, true).await?;
        sqlx::query("SELECT set_config('office.checkout_host_fingerprint', $1, true)")
            .bind(&input.proof.host_fingerprint)
            .execute(&mut *tx)
            .await?;
        if let Some(root) = row.root_ref {
            let same = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM checkout_roots WHERE workspace_id = $1 AND id = $2
                   AND host_runner_id = $3 AND directory_id = $4 AND ancestor_ids = $5)",
            )
            .bind(workspace.0)
            .bind(root)
            .bind(runner.runner_id.0)
            .bind(&input.proof.directory_id)
            .bind(&input.proof.ancestors)
            .fetch_one(&mut *tx)
            .await?;
            if !same
                || row.baseline.as_ref().map(|baseline| &baseline.0) != Some(&input.proof.baseline)
                || input.expected_revision.checked_add(1) != Some(row.revision)
            {
                return Err(IdentityError::RevisionMismatch {
                    current: row.revision,
                });
            }
            let result = row.into_checkout(true)?;
            tx.commit().await?;
            return Ok(result);
        }
        if row.revision != input.expected_revision {
            return Err(IdentityError::RevisionMismatch {
                current: row.revision,
            });
        }
        let root = reserve_root(&mut tx, &runner, row.id, &input.proof).await?;
        let row = sqlx::query_as::<_, CheckoutRow>(
            "UPDATE project_checkouts SET state = 'registered', revision = revision + 1, root_ref = $3, baseline = $4
             WHERE workspace_id = $1 AND id = $2 RETURNING *",
        ).bind(workspace.0).bind(row.id).bind(root).bind(Json(input.proof.baseline)).fetch_one(&mut *tx).await?;
        append_workspace_event(&mut tx, workspace, row.project_id, "checkout.registered",
            json!({"checkout_id": row.id, "host_runner_id": runner.runner_id, "revision": row.revision}), None).await?;
        let result = row.into_checkout(true)?;
        tx.commit().await?;
        Ok(result)
    }
}

async fn reserve_root(
    tx: &mut Transaction<'_, Postgres>,
    runner: &RunnerIdentity,
    checkout: Uuid,
    proof: &RootProof,
) -> Result<Uuid, IdentityError> {
    // Serialize by the physical host, including different owners, credentials, and workspaces.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("checkout-host/{}", proof.host_fingerprint))
        .execute(&mut **tx)
        .await?;
    let overlap = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM checkout_roots WHERE host_fingerprint = $1
          AND (directory_id = $2 OR directory_id = ANY($3) OR $2 = ANY(ancestor_ids)))",
    )
    .bind(&proof.host_fingerprint)
    .bind(&proof.directory_id)
    .bind(&proof.ancestors)
    .fetch_one(&mut **tx)
    .await?;
    if overlap {
        return Err(IdentityError::CheckoutRootOverlap);
    }
    let root = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO checkout_roots (id, workspace_id, checkout_id, host_runner_id, host_fingerprint, directory_id, ancestor_ids)
         VALUES ($1,$2,$3,$4,$5,$6,$7)",
    ).bind(root).bind(runner.workspace_id.0).bind(checkout).bind(runner.runner_id.0).bind(&proof.host_fingerprint)
     .bind(&proof.directory_id).bind(&proof.ancestors).execute(&mut **tx).await?;
    Ok(root)
}

pub(crate) async fn checkout_runner(
    tx: &mut Transaction<'_, Postgres>,
    digest: &BearerDigest,
) -> Result<RunnerIdentity, IdentityError> {
    let runner = authenticate_runner_in(tx, digest).await?;
    sqlx::query("SELECT set_config('office.workspace_id', $1, true), set_config('office.checkout_runner_id', $2, true)")
        .bind(runner.workspace_id.0.to_string()).bind(runner.runner_id.0.to_string()).execute(&mut **tx).await?;
    Ok(runner)
}

async fn load_project(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    project: Uuid,
) -> Result<ProjectRow, IdentityError> {
    sqlx::query_as::<_, ProjectRow>("SELECT * FROM projects WHERE workspace_id = $1 AND id = $2")
        .bind(workspace.0)
        .bind(project)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(IdentityError::Forbidden)
}

pub(crate) async fn load_checkout(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
    lock: bool,
) -> Result<CheckoutRow, IdentityError> {
    let query = if lock {
        "SELECT * FROM project_checkouts WHERE workspace_id = $1 AND id = $2 FOR NO KEY UPDATE"
    } else {
        "SELECT * FROM project_checkouts WHERE workspace_id = $1 AND id = $2"
    };
    sqlx::query_as::<_, CheckoutRow>(query)
        .bind(workspace.0)
        .bind(checkout)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(IdentityError::Forbidden)
}

fn validate_proof(proof: &RootProof) -> Result<(), IdentityError> {
    if !valid_hash(&proof.host_fingerprint)
        || !valid_hash(&proof.directory_id)
        || !(1..=MAX_ROOT_ANCESTORS).contains(&proof.ancestors.len())
        || proof.ancestors.iter().enumerate().any(|(index, ancestor)| {
            !valid_hash(ancestor)
                || ancestor == &proof.directory_id
                || proof.ancestors[..index].contains(ancestor)
        })
        || !valid_hash(&proof.baseline.manifest_sha256)
        || proof.baseline.file_count > MAX_SOURCE_FILES
        || proof.baseline.total_bytes > MAX_SOURCE_BYTES
        || (proof.baseline.file_count == 0 && proof.baseline.total_bytes != 0)
    {
        return Err(IdentityError::InvalidInput);
    }
    Ok(())
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

pub(crate) async fn lock_policy(
    tx: &mut Transaction<'_, Postgres>,
    checkout: Uuid,
) -> Result<(), IdentityError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("checkout-policy/{checkout}"))
        .execute(&mut **tx)
        .await?;
    Ok(())
}
