use office_domain::{
    Action, AgentRole, BearerDigest, PathAccess, PathRule, Resource, UserId, Visibility,
    WorkPermissions, WorkspaceId,
};
use office_protocol::{Area, AreaPolicy, AreaReceipt, DefineArea, GetArea, Recipient};
use serde_json::json;
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use super::{grants::owned_checkout, lock_policy};
use crate::{
    Store,
    data_scope::{authorize_scope, create_scope},
    events::{Idempotency, append_event},
    identity::{
        IdentityError, active_principal, require_permission, valid_name, workspace_context,
    },
    inbox::save_cursor,
};

#[derive(sqlx::FromRow)]
struct AreaRow {
    id: Uuid,
    workspace_id: Uuid,
    checkout_id: Uuid,
    owner_user_id: Uuid,
    revision: i64,
    name: String,
    paths: Json<PathRule>,
    policy: Json<AreaPolicy>,
}

impl From<AreaRow> for Area {
    fn from(row: AreaRow) -> Self {
        Self {
            area_id: row.id,
            workspace_id: WorkspaceId(row.workspace_id),
            checkout_id: row.checkout_id,
            owner_user_id: UserId(row.owner_user_id),
            revision: row.revision,
            name: row.name,
            paths: row.paths.0,
            policy: row.policy.0,
        }
    }
}

impl Store {
    pub async fn define_area(
        &self,
        digest: &BearerDigest,
        input: DefineArea,
    ) -> Result<AreaReceipt, IdentityError> {
        let name = valid_name(&input.name)?;
        if input.area_id.is_some() != input.expected_area_revision.is_some()
            || input
                .expected_area_revision
                .is_some_and(|version| !(1..i64::MAX).contains(&version))
            || input
                .paths
                .path()
                .is_some_and(office_domain::SourcePath::is_reserved)
        {
            return Err(IdentityError::InvalidInput);
        }
        let people = input.policy.approvers();
        if (matches!(input.policy, AreaPolicy::Owned { lead } if lead.agent_id.is_none()))
            || !(1..=8).contains(&people.len())
            || people
                .iter()
                .enumerate()
                .any(|(i, person)| people[..i].contains(person))
        {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let checkout = owned_checkout(
            &mut tx,
            context,
            input.checkout_id,
            input.expected_checkout_revision,
        )
        .await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "checkout.define_area",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        lock_policy(&mut tx, input.checkout_id).await?;
        let envelope = WorkPermissions {
            read: PathAccess {
                allow: vec![input.paths.clone()],
                deny: vec![],
            },
            ..WorkPermissions::default()
        };
        if !envelope.within_manifest(&checkout.manifest.0) {
            return Err(IdentityError::AccessRequestRequired);
        }
        for person in people {
            let role = active_principal(&mut tx, context, *person).await?;
            if person.agent_id.is_some() && role != Some(AgentRole::Lead) {
                return Err(IdentityError::Forbidden);
            }
        }
        let areas = list_areas(&mut tx, input.workspace_id, input.checkout_id).await?;
        if areas
            .iter()
            .any(|area| Some(area.id) != input.area_id && area.paths.0.intersects(&input.paths))
            || (input.area_id.is_none() && areas.len() >= 128)
        {
            return Err(IdentityError::InvalidInput);
        }
        let row = if let Some(id) = input.area_id {
            let previous = areas
                .iter()
                .find(|area| area.id == id)
                .ok_or(IdentityError::Forbidden)?;
            if input.expected_area_revision != Some(previous.revision) {
                return Err(IdentityError::RevisionMismatch {
                    current: previous.revision,
                });
            }
            sqlx::query_as::<_, AreaRow>(
                "UPDATE checkout_areas SET paths=$3, policy=$4, name=$5, revision=revision+1
                WHERE workspace_id=$1 AND id=$2 RETURNING *",
            )
            .bind(input.workspace_id.0)
            .bind(id)
            .bind(Json(input.paths))
            .bind(Json(input.policy))
            .bind(name)
            .fetch_one(&mut *tx)
            .await?
        } else {
            let id = Uuid::new_v4();
            create_scope(&mut tx, context, id, Visibility::OwnerPrivate, &[]).await?;
            sqlx::query_as::<_, AreaRow>("INSERT INTO checkout_areas (id,workspace_id,checkout_id,owner_user_id,name,paths,policy)
                VALUES ($1,$2,$3,$4,$5,$6,$7) RETURNING *").bind(id).bind(input.workspace_id.0).bind(input.checkout_id)
                .bind(checkout.owner_user_id).bind(name).bind(Json(input.paths)).bind(Json(input.policy)).fetch_one(&mut *tx).await?
        };
        let scope = row.id;
        let seq = append_event(
            &mut tx,
            context,
            scope,
            "checkout.area_changed",
            json!({
                "checkout_id": input.checkout_id, "area_id": scope, "revision": row.revision,
            }),
            None,
        )
        .await?;
        let result = AreaReceipt {
            area: row.into(),
            event_cursor: save_cursor(&mut tx, context, seq).await?,
        };
        idem.record(&mut tx, scope, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn get_area(
        &self,
        digest: &BearerDigest,
        input: GetArea,
    ) -> Result<Area, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        authorize_scope(&mut tx, context, input.area_id, Action::ReadData).await?;
        let row = sqlx::query_as::<_, AreaRow>(
            "SELECT * FROM checkout_areas WHERE workspace_id=$1 AND id=$2",
        )
        .bind(input.workspace_id.0)
        .bind(input.area_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(IdentityError::Forbidden)?;
        require_permission(
            context.actor,
            Action::RegisterCheckout,
            Resource::Account(UserId(row.owner_user_id)),
        )?;
        tx.commit().await?;
        Ok(row.into())
    }
}

pub(crate) async fn required_area_approvals(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
    lead: Recipient,
    permissions: &WorkPermissions,
) -> Result<Vec<Area>, IdentityError> {
    let rows = policy_areas(tx, workspace, checkout).await?;
    Ok(rows
        .into_iter()
        .filter(|area| {
            permissions.can_write_in(&area.paths.0)
                && !matches!(area.policy.0, AreaPolicy::Owned { lead: owner } if owner == lead)
        })
        .map(Area::from)
        .collect())
}

pub(crate) async fn policy_area(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
    area: Uuid,
) -> Result<Area, IdentityError> {
    policy_areas(tx, workspace, checkout)
        .await?
        .into_iter()
        .find(|row| row.id == area)
        .map(Area::from)
        .ok_or(IdentityError::Forbidden)
}

async fn policy_areas(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
) -> Result<Vec<AreaRow>, IdentityError> {
    sqlx::query("SELECT set_config('office.validating_checkout_areas',$1,true)")
        .bind(checkout.to_string())
        .execute(&mut **tx)
        .await?;
    let rows = list_areas(tx, workspace, checkout).await?;
    sqlx::query("SELECT set_config('office.validating_checkout_areas','',true)")
        .execute(&mut **tx)
        .await?;
    Ok(rows)
}

pub(crate) async fn notification_areas(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
) -> Result<Vec<(Uuid, i64, PathRule)>, IdentityError> {
    Ok(policy_areas(tx, workspace, checkout)
        .await?
        .into_iter()
        .map(|row| (row.id, row.revision, row.paths.0))
        .collect())
}

async fn list_areas(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    checkout: Uuid,
) -> Result<Vec<AreaRow>, IdentityError> {
    Ok(sqlx::query_as::<_, AreaRow>(
        "SELECT * FROM checkout_areas WHERE workspace_id=$1 AND checkout_id=$2 ORDER BY id",
    )
    .bind(workspace.0)
    .bind(checkout)
    .fetch_all(&mut **tx)
    .await?)
}
