use std::collections::HashSet;

use chrono::{DateTime, Duration, Utc};
use office_domain::{
    Action, AgentId, AgentRole, BearerDigest, PathAccess, Resource, SourceManifest, UserId,
    Visibility, WorkPermissions, WorkspaceId,
};
use office_protocol::{
    CheckoutGrant, CheckoutGrantReceipt, CommandArgument, CommandTemplateReceipt,
    CommandTemplateSpec, CreateCommandTemplate, GetCheckoutGrant, IssueCheckoutGrant, Recipient,
    RevokeCheckoutGrant,
};
use serde_json::json;
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use super::{CheckoutRow, load_checkout, lock_policy};
use crate::{
    Store,
    data_scope::{authorize_scope, create_directed_scope, create_scope},
    events::{Idempotency, append_event},
    identity::{
        IdentityError, WorkspaceContext, active_principal, bind_subject, require_permission,
        valid_name, workspace_context,
    },
    inbox::save_cursor,
};

#[derive(sqlx::FromRow)]
struct GrantRow {
    id: Uuid,
    workspace_id: Uuid,
    checkout_id: Uuid,
    owner_user_id: Uuid,
    recipient_user_id: Uuid,
    recipient_agent_id: Uuid,
    permissions: Json<WorkPermissions>,
    expires_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
    revision: i64,
}

impl From<GrantRow> for CheckoutGrant {
    fn from(row: GrantRow) -> Self {
        Self {
            grant_id: row.id,
            workspace_id: WorkspaceId(row.workspace_id),
            checkout_id: row.checkout_id,
            owner_user_id: UserId(row.owner_user_id),
            recipient: Recipient {
                user_id: UserId(row.recipient_user_id),
                agent_id: Some(AgentId(row.recipient_agent_id)),
            },
            permissions: row.permissions.0,
            expires_at: row.expires_at,
            revoked_at: row.revoked_at,
            revision: row.revision,
        }
    }
}

impl Store {
    pub async fn issue_checkout_grant(
        &self,
        digest: &BearerDigest,
        input: IssueCheckoutGrant,
    ) -> Result<CheckoutGrantReceipt, IdentityError> {
        let agent = input
            .recipient
            .agent_id
            .ok_or(IdentityError::InvalidInput)?;
        if !input.permissions.is_valid() {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let checkout =
            owned_checkout(&mut tx, context, input.checkout_id, input.expected_revision).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "checkout.grant_issue",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        lock_policy(&mut tx, checkout.id).await?;
        let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *tx)
            .await?;
        if input.expires_at <= now || input.expires_at > now + Duration::days(30) {
            return Err(IdentityError::InvalidInput);
        }
        if !input.permissions.within_manifest(&checkout.manifest.0) {
            return Err(IdentityError::AccessRequestRequired);
        }
        if active_principal(&mut tx, context, input.recipient).await? != Some(AgentRole::Lead) {
            return Err(IdentityError::Forbidden);
        }
        bind_subject(&mut tx, input.recipient).await?;
        let visible = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM projects WHERE workspace_id = $1 AND id = $2)",
        )
        .bind(input.workspace_id.0)
        .bind(checkout.project_id)
        .fetch_one(&mut *tx)
        .await?;
        bind_subject(&mut tx, context.recipient()).await?;
        if !visible {
            return Err(IdentityError::Forbidden);
        }
        verify_commands(&mut tx, context, &checkout, &input.permissions).await?;

        let id = Uuid::new_v4();
        create_directed_scope(&mut tx, context, id, input.recipient).await?;
        let grant = sqlx::query_as::<_, GrantRow>(
            "INSERT INTO checkout_grants (id, workspace_id, checkout_id, owner_user_id,
                recipient_user_id, recipient_agent_id, permissions, expires_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8) RETURNING *",
        )
        .bind(id)
        .bind(input.workspace_id.0)
        .bind(checkout.id)
        .bind(checkout.owner_user_id)
        .bind(input.recipient.user_id.0)
        .bind(agent.0)
        .bind(Json(input.permissions))
        .bind(input.expires_at)
        .fetch_one(&mut *tx)
        .await?;
        let result = record_grant(
            &mut tx,
            context,
            &idem,
            grant.into(),
            "checkout.grant_issued",
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn get_checkout_grant(
        &self,
        digest: &BearerDigest,
        input: GetCheckoutGrant,
    ) -> Result<CheckoutGrant, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        authorize_scope(&mut tx, context, input.grant_id, Action::ReadData).await?;
        let answer = read_grant(&mut tx, input.workspace_id, input.grant_id)
            .await?
            .into();
        tx.commit().await?;
        Ok(answer)
    }

    pub async fn revoke_checkout_grant(
        &self,
        digest: &BearerDigest,
        input: RevokeCheckoutGrant,
    ) -> Result<CheckoutGrantReceipt, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let grant = read_grant(&mut tx, input.workspace_id, input.grant_id).await?;
        require_permission(
            context.actor,
            Action::RegisterCheckout,
            Resource::Account(UserId(grant.owner_user_id)),
        )?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "checkout.grant_revoke",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        lock_policy(&mut tx, grant.checkout_id).await?;
        let grant = sqlx::query_as::<_, GrantRow>(
            "UPDATE checkout_grants SET revoked_at = clock_timestamp(), revision = revision + 1
             WHERE workspace_id = $1 AND id = $2 AND revision = $3 AND revoked_at IS NULL RETURNING *",
        ).bind(input.workspace_id.0).bind(input.grant_id).bind(input.expected_revision).fetch_optional(&mut *tx).await?;
        let Some(grant) = grant else {
            let current = read_grant(&mut tx, input.workspace_id, input.grant_id).await?;
            if current.revision != input.expected_revision {
                return Err(IdentityError::RevisionMismatch {
                    current: current.revision,
                });
            }
            return Err(IdentityError::InvalidTransition);
        };
        crate::tasks::claims::fence_grant(&mut tx, input.workspace_id, grant.checkout_id, grant.id)
            .await?;
        let result = record_grant(
            &mut tx,
            context,
            &idem,
            grant.into(),
            "checkout.grant_revoked",
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn create_command_template(
        &self,
        digest: &BearerDigest,
        mut input: CreateCommandTemplate,
    ) -> Result<CommandTemplateReceipt, IdentityError> {
        input.template.name = valid_name(&input.template.name)?.to_owned();
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let checkout =
            owned_checkout(&mut tx, context, input.checkout_id, input.expected_revision).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "command_template.create",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        let id = Uuid::new_v4();
        validate_template(id, &input.template, &checkout.manifest.0)?;
        create_scope(&mut tx, context, id, Visibility::OwnerPrivate, &[]).await?;
        sqlx::query("INSERT INTO command_templates (id, workspace_id, checkout_id, owner_user_id, spec) VALUES ($1,$2,$3,$4,$5)")
            .bind(id).bind(input.workspace_id.0).bind(input.checkout_id).bind(checkout.owner_user_id)
            .bind(Json(&input.template)).execute(&mut *tx).await?;
        let seq = append_event(
            &mut tx,
            context,
            id,
            "checkout.command_template_added",
            json!({"checkout_id": input.checkout_id, "template_id": id}),
            None,
        )
        .await?;
        let result = CommandTemplateReceipt {
            template_id: id,
            workspace_id: input.workspace_id,
            checkout_id: input.checkout_id,
            template: input.template,
            event_cursor: save_cursor(&mut tx, context, seq).await?,
        };
        idem.record(&mut tx, id, &result).await?;
        tx.commit().await?;
        Ok(result)
    }
}

pub(super) async fn owned_checkout(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    checkout: Uuid,
    expected_revision: i64,
) -> Result<CheckoutRow, IdentityError> {
    let row = load_checkout(tx, context.access.workspace, checkout, false).await?;
    require_permission(
        context.actor,
        Action::RegisterCheckout,
        Resource::Account(UserId(row.owner_user_id)),
    )?;
    if row.revision != expected_revision {
        return Err(IdentityError::RevisionMismatch {
            current: row.revision,
        });
    }
    if row.state != "registered" {
        return Err(IdentityError::InvalidTransition);
    }
    Ok(row)
}

async fn read_grant(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<GrantRow, IdentityError> {
    sqlx::query_as::<_, GrantRow>(
        "SELECT * FROM checkout_grants WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.0)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(IdentityError::Forbidden)
}

async fn record_grant(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    idem: &Idempotency,
    grant: CheckoutGrant,
    event: &str,
) -> Result<CheckoutGrantReceipt, IdentityError> {
    let seq = append_event(
        tx,
        context,
        grant.grant_id,
        event,
        json!({"checkout_id": grant.checkout_id,
        "grant_id": grant.grant_id, "recipient": grant.recipient, "revision": grant.revision}),
        None,
    )
    .await?;
    let result = CheckoutGrantReceipt {
        grant,
        event_cursor: save_cursor(tx, context, seq).await?,
    };
    idem.record(tx, result.grant.grant_id, &result).await?;
    Ok(result)
}

pub(crate) async fn verify_commands(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    checkout: &CheckoutRow,
    permissions: &WorkPermissions,
) -> Result<(), IdentityError> {
    let specs = sqlx::query_as::<_, (Uuid, Json<CommandTemplateSpec>)>(
        "SELECT id, spec FROM command_templates WHERE workspace_id = $1 AND checkout_id = $2 AND id = ANY($3)",
    ).bind(context.access.workspace.0).bind(checkout.id).bind(&permissions.command_template_ids).fetch_all(&mut **tx).await?;
    if specs.len() != permissions.command_template_ids.len() {
        return Err(IdentityError::Forbidden);
    }
    if specs.into_iter().any(|(id, spec)| {
        !permissions.permits_command(id, &checkout.manifest.0, &spec.0.source_inputs)
    }) {
        return Err(IdentityError::AccessRequestRequired);
    }
    Ok(())
}

pub(crate) async fn checkout_grant(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<CheckoutGrant, IdentityError> {
    Ok(read_grant(tx, workspace, id).await?.into())
}

pub(crate) async fn bind_grant_validation(
    tx: &mut Transaction<'_, Postgres>,
    id: Option<Uuid>,
) -> Result<(), IdentityError> {
    sqlx::query("SELECT set_config('office.verifying_scope_grant_id', $1, true)")
        .bind(id.map_or_else(String::new, |id| id.to_string()))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn validate_template(
    id: Uuid,
    spec: &CommandTemplateSpec,
    manifest: &SourceManifest,
) -> Result<(), IdentityError> {
    if !identifier(&spec.executable)
        || !(1..=1800).contains(&spec.timeout_seconds)
        || spec.arguments.len() > 32
        || spec.source_inputs.len() > 128
        || spec.scratch_outputs.len() > 128
        || spec.scratch_outputs.iter().any(|rule| {
            rule.path()
                .is_some_and(office_domain::SourcePath::is_reserved)
        })
    {
        return Err(IdentityError::InvalidInput);
    }
    let mut keys = HashSet::new();
    for argument in &spec.arguments {
        let valid = match argument {
            CommandArgument::Literal { value } => valid_argument(value),
            CommandArgument::Choice { key, values } => {
                identifier(key)
                    && keys.insert(key)
                    && (1..=16).contains(&values.len())
                    && values.iter().all(|value| valid_argument(value))
            }
            CommandArgument::Integer {
                key,
                minimum,
                maximum,
            } => identifier(key) && keys.insert(key) && minimum <= maximum,
        };
        if !valid {
            return Err(IdentityError::InvalidInput);
        }
    }
    let source = WorkPermissions {
        read: PathAccess {
            allow: manifest.include.clone(),
            deny: manifest.exclude.clone(),
        },
        command_template_ids: vec![id],
        ..WorkPermissions::default()
    };
    if !source.permits_command(id, manifest, &spec.source_inputs) {
        return Err(IdentityError::AccessRequestRequired);
    }
    Ok(())
}

fn identifier(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_argument(value: &str) -> bool {
    value.len() <= 512 && !value.chars().any(char::is_control)
}
