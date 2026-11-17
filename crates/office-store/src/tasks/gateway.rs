use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::STANDARD};
use office_domain::{BearerDigest, RunnerId, SourceManifest, SourcePath, TaskId, WorkspaceId};
use office_protocol::{
    CheckoutCoordinatorRef, FileEncoding, GatewayOrigin, GatewayQuery, GatewayReadOrder,
    GatewayReadOutcome, GatewayReadReceipt, GatewaySearchReceipt, GatewayStatusReceipt,
    GetCheckoutStatus, MAX_GATEWAY_READ_BYTES, MAX_GATEWAY_SNIPPET_BYTES, ReadCheckoutFile,
    ReportGatewayRead, SearchCheckoutCode, WorkScopeDefinition,
};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction, types::Json};
use uuid::Uuid;

use super::{
    delegation,
    execution::start_effects::credentials::{RunAccess, authenticate_run},
    load_task,
    scopes::lock_worker_scope,
};
use crate::{
    Store,
    checkouts::{checkout_runner, coordinator::require_epoch, load_checkout, lock_policy},
    identity::IdentityError,
};

#[derive(sqlx::FromRow)]
struct ReadRow {
    id: Uuid,
    checkout_id: Uuid,
    epoch: i64,
    host_runner_id: Uuid,
    scope_id: Uuid,
    scope_revision: i64,
    path: Option<String>,
    scope_definition: Json<WorkScopeDefinition>,
    query_definition: Option<Json<GatewayQuery>>,
    outcome: Option<Json<GatewayReadOutcome>>,
}

impl ReadRow {
    fn origin(&self) -> GatewayOrigin {
        GatewayOrigin {
            read_id: self.id,
            checkout_id: self.checkout_id,
            host_runner_id: RunnerId(self.host_runner_id),
            coordinator_epoch: self.epoch,
            scope_revision: self.scope_revision,
        }
    }
}

fn restore_query(
    path: Option<String>,
    definition: Option<Json<GatewayQuery>>,
) -> Result<GatewayQuery, IdentityError> {
    match (path, definition) {
        (Some(path), None) => Ok(GatewayQuery::File {
            path: path.parse().map_err(|_| IdentityError::InvalidRole)?,
        }),
        (None, Some(Json(query))) => Ok(query),
        _ => Err(IdentityError::InvalidRole),
    }
}

fn verify_query_outcome(
    query: &GatewayQuery,
    manifest: &SourceManifest,
    scope: &WorkScopeDefinition,
    outcome: &GatewayReadOutcome,
) -> Result<(), IdentityError> {
    let visible = |under: Option<&SourcePath>, path: &SourcePath| {
        under.is_none_or(|prefix| prefix != path && prefix.contains(path))
            && manifest.allows(path)
            && scope.permissions.read.allows(path)
    };
    let good_hash = |sha: &str| {
        sha.len() == 64
            && sha
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    };
    let valid = match (query, outcome) {
        (GatewayQuery::File { .. }, GatewayReadOutcome::Success { .. }) => true,
        (
            GatewayQuery::Search {
                under,
                needle,
                limit,
            },
            GatewayReadOutcome::Search { matches, .. },
        ) => {
            matches.len() <= usize::from(*limit)
                && matches.iter().all(|entry| {
                    entry.line_number > 0
                        && good_hash(&entry.file_sha256)
                        && entry.snippet.len() <= MAX_GATEWAY_SNIPPET_BYTES
                        && !entry.snippet.contains('\0')
                        && entry.snippet.contains(needle)
                        && visible(under.as_ref(), &entry.path)
                })
                && matches.windows(2).all(|pair| {
                    (&pair[0].path, pair[0].line_number) < (&pair[1].path, pair[1].line_number)
                })
        }
        (
            GatewayQuery::Status {
                under,
                after,
                limit,
            },
            GatewayReadOutcome::Status { files, next_after },
        ) => {
            files.len() <= usize::from(*limit)
                && files.iter().all(|entry| {
                    (match entry.sha256.as_deref() {
                        Some(hash) => {
                            entry.size_bytes <= office_protocol::MAX_GATEWAY_STATUS_HASH_BYTES
                                && good_hash(hash)
                        }
                        None => entry.size_bytes > office_protocol::MAX_GATEWAY_STATUS_HASH_BYTES,
                    }) && visible(under.as_ref(), &entry.path)
                        && after.as_ref().is_none_or(|cursor| &entry.path > cursor)
                })
                && files.windows(2).all(|pair| pair[0].path < pair[1].path)
                && next_after
                    .as_ref()
                    .is_none_or(|cursor| files.last().is_some_and(|entry| &entry.path == cursor))
        }
        (
            _,
            GatewayReadOutcome::Forbidden
            | GatewayReadOutcome::Missing
            | GatewayReadOutcome::TooLarge
            | GatewayReadOutcome::Changed
            | GatewayReadOutcome::Unavailable,
        ) => true,
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(IdentityError::InvalidInput)
    }
}

impl Store {
    pub async fn read_checkout_file(
        &self,
        digest: &BearerDigest,
        input: ReadCheckoutFile,
    ) -> Result<GatewayReadReceipt, IdentityError> {
        let query = GatewayQuery::File {
            path: input.path.clone(),
        };
        let (origin, outcome) = self
            .checkout_query(digest, input.scope_revision, input.idempotency_key, &query)
            .await?;
        let GatewayReadOutcome::Success { file } = outcome else {
            return Err(IdentityError::InvalidRole);
        };
        Ok(GatewayReadReceipt {
            origin,
            path: input.path,
            file,
        })
    }

    pub async fn search_checkout_code(
        &self,
        digest: &BearerDigest,
        input: SearchCheckoutCode,
    ) -> Result<GatewaySearchReceipt, IdentityError> {
        let query = GatewayQuery::Search {
            needle: input.needle,
            under: input.under,
            limit: input.limit.unwrap_or(16),
        };
        let (origin, outcome) = self
            .checkout_query(digest, input.scope_revision, input.idempotency_key, &query)
            .await?;
        let GatewayReadOutcome::Search { matches, truncated } = outcome else {
            return Err(IdentityError::InvalidRole);
        };
        Ok(GatewaySearchReceipt {
            origin,
            matches,
            truncated,
        })
    }

    pub async fn checkout_status(
        &self,
        digest: &BearerDigest,
        input: GetCheckoutStatus,
    ) -> Result<GatewayStatusReceipt, IdentityError> {
        let query = GatewayQuery::Status {
            under: input.under,
            after: input.after,
            limit: input.limit.unwrap_or(16),
        };
        let (origin, outcome) = self
            .checkout_query(digest, input.scope_revision, input.idempotency_key, &query)
            .await?;
        let GatewayReadOutcome::Status { files, next_after } = outcome else {
            return Err(IdentityError::InvalidRole);
        };
        Ok(GatewayStatusReceipt {
            origin,
            files,
            next_after,
        })
    }

    async fn checkout_query(
        &self,
        digest: &BearerDigest,
        revision: i64,
        key: Uuid,
        query: &GatewayQuery,
    ) -> Result<(GatewayOrigin, GatewayReadOutcome), IdentityError> {
        if revision < 1 || !query.is_valid() {
            return Err(IdentityError::InvalidInput);
        }
        let order = self.request_read(digest, revision, key, query).await?;
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            let mut tx = self.pool.begin().await?;
            let access = authenticate_run(&mut tx, digest).await?;
            let scope = authorize_read(&mut tx, &access, revision, query).await?;
            let row = live_read(&mut tx, scope.workspace_id, order).await?;
            if row.scope_id != scope.scope_id || row.scope_revision != revision {
                return Err(IdentityError::ScopeStale);
            }
            require_epoch(
                &mut tx,
                scope.workspace_id,
                scope.checkout_id,
                Some(row.epoch),
                true,
            )
            .await?;
            tx.commit().await?;
            let origin = row.origin();
            if let Some(Json(outcome)) = row.outcome {
                return match outcome {
                    GatewayReadOutcome::Forbidden => Err(IdentityError::PathForbidden),
                    GatewayReadOutcome::Missing => Err(IdentityError::FileNotFound),
                    GatewayReadOutcome::TooLarge => Err(IdentityError::InvalidInput),
                    GatewayReadOutcome::Changed => Err(IdentityError::ContentConflict),
                    GatewayReadOutcome::Unavailable => Err(IdentityError::CheckoutOffline),
                    _ => Ok((origin, outcome)),
                };
            }
            if Instant::now() >= deadline {
                return Err(IdentityError::CheckoutOffline);
            }
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
    }

    async fn request_read(
        &self,
        digest: &BearerDigest,
        revision: i64,
        key: Uuid,
        query: &GatewayQuery,
    ) -> Result<Uuid, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let access = authenticate_run(&mut tx, digest).await?;
        let scope = authorize_read(&mut tx, &access, revision, query).await?;
        let existing = sqlx::query_as::<_, ReadRow>(
            "SELECT * FROM checkout_file_reads WHERE workspace_id=$1 AND run_id=$2 AND idempotency_key=$3",
        )
        .bind(scope.workspace_id.0).bind(access.run_id.0).bind(key)
        .fetch_optional(&mut *tx).await?;
        if let Some(row) = existing {
            if row.scope_id != scope.scope_id
                || row.scope_revision != revision
                || &restore_query(row.path, row.query_definition)? != query
            {
                return Err(IdentityError::IdempotencyConflict);
            }
            tx.commit().await?;
            return Ok(row.id);
        }
        let (epoch, host_runner) = sqlx::query_as::<_, (i64, Uuid)>(
            "SELECT epoch,host_runner_id FROM checkout_coordinator_sessions
             WHERE workspace_id=$1 AND checkout_id=$2 AND stopped_at IS NULL
             AND lease_until>clock_timestamp() AND ready_at IS NOT NULL",
        )
        .bind(scope.workspace_id.0)
        .bind(scope.checkout_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(IdentityError::CheckoutOffline)?;
        let live = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM checkout_file_reads WHERE workspace_id=$1 AND run_id=$2 AND expires_at>clock_timestamp()",
        ).bind(scope.workspace_id.0).bind(access.run_id.0).fetch_one(&mut *tx).await?;
        if live >= 16 {
            return Err(IdentityError::InvalidInput);
        }
        let read_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO checkout_file_reads (id,workspace_id,checkout_id,epoch,host_runner_id,task_id,run_id,
                executor_user_id,worker_agent_id,scope_id,scope_revision,scope_definition,path,idempotency_key,query_definition,expires_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,clock_timestamp()+interval '15 seconds')",
        ).bind(read_id).bind(scope.workspace_id.0).bind(scope.checkout_id).bind(epoch).bind(host_runner)
          .bind(access.task.id).bind(access.run_id.0).bind(access.task.assignee_user_id).bind(access.task.assignee_agent_id)
          .bind(scope.scope_id).bind(scope.revision).bind(Json(scope)).bind(query.path().map(SourcePath::as_str)).bind(key)
          .bind(query.path().is_none().then_some(Json(query)))
          .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(read_id)
    }

    pub async fn next_checkout_read(
        &self,
        digest: &BearerDigest,
        proof: CheckoutCoordinatorRef,
    ) -> Result<Option<GatewayReadOrder>, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let runner = checkout_runner(&mut tx, digest).await?;
        lock_policy(&mut tx, proof.checkout_id).await?;
        require_epoch(
            &mut tx,
            runner.workspace_id,
            proof.checkout_id,
            Some(proof.epoch),
            true,
        )
        .await?;
        sqlx::query("DELETE FROM checkout_file_reads WHERE id IN (
            SELECT id FROM checkout_file_reads WHERE workspace_id=$1 AND checkout_id=$2 AND expires_at<clock_timestamp()-interval '1 minute'
            ORDER BY expires_at LIMIT 32)")
            .bind(runner.workspace_id.0).bind(proof.checkout_id).execute(&mut *tx).await?;
        let row = sqlx::query_as::<_, ReadRow>(
            "SELECT r.* FROM checkout_file_reads r
             JOIN work_scope_revisions s ON (s.workspace_id,s.scope_id,s.revision)=(r.workspace_id,r.scope_id,r.scope_revision)
             WHERE r.workspace_id=$1 AND r.checkout_id=$2 AND r.epoch=$3 AND r.outcome IS NULL
             AND r.expires_at>clock_timestamp() AND s.state='active' AND s.operation='apply' AND s.expires_at>clock_timestamp()
             AND NOT EXISTS (SELECT 1 FROM work_scope_revisions pending
                 WHERE pending.workspace_id=r.workspace_id AND pending.checkout_id=r.checkout_id AND pending.state='pending_activation')
             ORDER BY r.created_at,r.id LIMIT 1",
        ).bind(runner.workspace_id.0).bind(proof.checkout_id).bind(proof.epoch).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        row.map(|row| {
            Ok(GatewayReadOrder {
                read_id: row.id,
                epoch: proof.epoch,
                scope: row.scope_definition.0,
                query: restore_query(row.path, row.query_definition)?,
            })
        })
        .transpose()
    }

    pub async fn report_checkout_read(
        &self,
        digest: &BearerDigest,
        input: ReportGatewayRead,
    ) -> Result<(), IdentityError> {
        if let GatewayReadOutcome::Success { file } = &input.outcome {
            if file.content.len() > MAX_GATEWAY_READ_BYTES as usize * 2 {
                return Err(IdentityError::InvalidInput);
            }
            let bytes = match file.encoding {
                FileEncoding::Utf8 if !file.content.contains('\0') => {
                    file.content.as_bytes().to_vec()
                }
                FileEncoding::Utf8 => return Err(IdentityError::InvalidInput),
                FileEncoding::Base64 => STANDARD
                    .decode(&file.content)
                    .map_err(|_| IdentityError::InvalidInput)?,
            };
            if bytes.len() as u64 != file.size_bytes
                || file.size_bytes > MAX_GATEWAY_READ_BYTES
                || hex::encode(Sha256::digest(bytes)) != file.sha256
            {
                return Err(IdentityError::InvalidInput);
            }
        }
        let mut tx = self.pool.begin().await?;
        let runner = checkout_runner(&mut tx, digest).await?;
        let proof = input.coordinator;
        lock_policy(&mut tx, proof.checkout_id).await?;
        require_epoch(
            &mut tx,
            runner.workspace_id,
            proof.checkout_id,
            Some(proof.epoch),
            true,
        )
        .await?;
        let row = live_read(&mut tx, runner.workspace_id, input.read_id).await?;
        if row.checkout_id != proof.checkout_id
            || row.epoch != proof.epoch
            || row.host_runner_id != runner.runner_id.0
        {
            return Err(IdentityError::CoordinatorStale);
        }
        let checkout =
            load_checkout(&mut tx, runner.workspace_id, proof.checkout_id, false).await?;
        let query = restore_query(row.path, row.query_definition)?;
        verify_query_outcome(
            &query,
            &checkout.manifest.0,
            &row.scope_definition.0,
            &input.outcome,
        )?;
        if let Some(Json(previous)) = row.outcome {
            if previous != input.outcome {
                return Err(IdentityError::IdempotencyConflict);
            }
        } else {
            sqlx::query("UPDATE checkout_file_reads SET outcome=$2 WHERE id=$1")
                .bind(row.id)
                .bind(Json(input.outcome))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

async fn authorize_read(
    tx: &mut Transaction<'_, Postgres>,
    access: &RunAccess,
    revision: i64,
    query: &GatewayQuery,
) -> Result<WorkScopeDefinition, IdentityError> {
    let (scope, manifest) = lock_worker_scope(tx, access.context, &access.task).await?;
    if scope.revision != revision {
        return Err(IdentityError::ScopeStale);
    }
    if !query.admits(&manifest, &scope.permissions.read) {
        return Err(IdentityError::PathForbidden);
    }
    sqlx::query("SELECT set_config('office.gateway_checkout_id',$1,true)")
        .bind(scope.checkout_id.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(scope)
}

pub(super) async fn authorize_checkout_run(
    tx: &mut Transaction<'_, Postgres>,
    digest: &BearerDigest,
    revision: i64,
) -> Result<(RunAccess, WorkScopeDefinition, SourceManifest), IdentityError> {
    let access = authenticate_run(tx, digest).await?;
    if !access.gateway_required {
        return Err(IdentityError::AccessRequestRequired);
    }
    let (task, _) = load_task(tx, access.context, TaskId(access.task.id), true).await?;
    let (scope, manifest) = lock_worker_scope(tx, access.context, &task).await?;
    let access = authenticate_run(tx, digest).await?;
    delegation::require_active_task_grant(tx, access.context, &task).await?;
    if revision != scope.revision {
        return Err(IdentityError::ScopeStale);
    }
    sqlx::query("SELECT set_config('office.gateway_checkout_id',$1,true)")
        .bind(scope.checkout_id.to_string())
        .execute(&mut **tx)
        .await?;
    Ok((access, scope, manifest))
}

async fn live_read(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<ReadRow, IdentityError> {
    sqlx::query_as("SELECT * FROM checkout_file_reads WHERE workspace_id=$1 AND id=$2 AND expires_at>clock_timestamp()")
        .bind(workspace.0).bind(id).fetch_optional(&mut **tx).await?.ok_or(IdentityError::CheckoutOffline)
}
