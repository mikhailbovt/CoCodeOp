use chrono::{DateTime, Utc};
use office_domain::{Action, AgentId, BearerDigest, RunId, TaskId, UserId, WorkspaceId};
use office_protocol::{
    Artifact, ArtifactChunkReceipt, ArtifactCleanup, ArtifactProvenance, ArtifactUpload,
    BeginArtifactUpload, CleanupArtifacts, FinishArtifactUpload, GetArtifact, MAX_ARTIFACT_BYTES,
    Recipient, artifact_chunk_count, artifact_chunk_length, is_sha256,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::{
    Store,
    data_scope::{authorize_scope, create_scope},
    events::{Idempotency, append_event},
    identity::{IdentityError, WorkspaceContext, workspace_context},
};

#[derive(sqlx::FromRow)]
struct Version {
    workspace_id: Uuid,
    upload_id: Uuid,
    artifact_id: Uuid,
    revision: i64,
    owner_user_id: Uuid,
    author_agent_id: Option<Uuid>,
    name: String,
    mime: String,
    size_bytes: i64,
    sha256: String,
    task_id: Option<Uuid>,
    run_id: Option<Uuid>,
    state: String,
    upload_until: DateTime<Utc>,
    finalized_at: Option<DateTime<Utc>>,
    expires_at: Option<DateTime<Utc>>,
    observed_at: DateTime<Utc>,
}

impl Version {
    fn upload(&self) -> ArtifactUpload {
        ArtifactUpload {
            workspace_id: WorkspaceId(self.workspace_id),
            artifact_id: self.artifact_id,
            upload_id: self.upload_id,
            revision: self.revision,
            chunk_bytes: office_protocol::ARTIFACT_CHUNK_BYTES,
            chunk_count: artifact_chunk_count(self.size_bytes),
            upload_until: self.upload_until,
        }
    }

    fn published(&self) -> bool {
        self.state == "ready" && self.expires_at.is_some_and(|time| time > self.observed_at)
    }

    async fn view(
        self,
        tx: &mut Transaction<'_, Postgres>,
        context: WorkspaceContext,
    ) -> Result<Artifact, IdentityError> {
        let provenance = if let Some(task) = self.task_id {
            match authorize_scope(tx, context, task, Action::ReadData).await {
                Ok(_) => Some(ArtifactProvenance {
                    task_id: TaskId(task),
                    run_id: self.run_id.map(RunId),
                }),
                Err(IdentityError::Forbidden) => None,
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        Ok(Artifact {
            available: self.published(),
            workspace_id: WorkspaceId(self.workspace_id),
            artifact_id: self.artifact_id,
            revision: self.revision,
            owner_user_id: UserId(self.owner_user_id),
            author: Recipient {
                user_id: UserId(self.owner_user_id),
                agent_id: self.author_agent_id.map(AgentId),
            },
            name: self.name,
            mime: self.mime.parse().map_err(|_| IdentityError::InvalidRole)?,
            size_bytes: self.size_bytes,
            sha256: self.sha256,
            chunk_count: artifact_chunk_count(self.size_bytes),
            provenance,
            finalized_at: self.finalized_at.ok_or(IdentityError::ArtifactIncomplete)?,
            expires_at: self.expires_at.ok_or(IdentityError::ArtifactIncomplete)?,
        })
    }
}

impl Store {
    pub async fn begin_artifact(
        &self,
        digest: &BearerDigest,
        input: BeginArtifactUpload,
    ) -> Result<ArtifactUpload, IdentityError> {
        let name = artifact_name(&input.name)?;
        if !(1..=MAX_ARTIFACT_BYTES).contains(&input.size_bytes)
            || !is_sha256(&input.sha256)
            || !(1..=365).contains(&input.retention_days)
            || input.expected_revision < 0
            || input.artifact_id.is_none() && input.expected_revision != 0
            || input.artifact_id.is_some()
                && (input.visibility.is_some() || !input.grants.is_empty())
        {
            return Err(IdentityError::InvalidInput);
        }
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let idem = Idempotency::lock(
            &mut tx,
            context,
            "artifact.begin",
            input.idempotency_key,
            &input,
        )
        .await?;
        if let Some(previous) = idem.replay(&mut tx).await? {
            tx.commit().await?;
            return Ok(previous);
        }
        validate_provenance(&mut tx, context, input.provenance).await?;
        lock_usage(&mut tx, context).await?;
        let workspace = context.access.workspace.0;
        let owner = context.access.member.0;
        let agent = context.agent_id().map(|id| id.0);
        let artifact = if let Some(artifact) = input.artifact_id {
            let revision = lock_artifact(&mut tx, context, artifact).await?;
            if revision != input.expected_revision {
                return Err(IdentityError::RevisionMismatch { current: revision });
            }
            artifact
        } else {
            let artifact = Uuid::new_v4();
            create_scope(
                &mut tx,
                context,
                artifact,
                input.visibility.unwrap_or_default(),
                &input.grants,
            )
            .await?;
            sqlx::query("INSERT INTO artifacts (workspace_id,id,owner_user_id,owner_agent_id) VALUES ($1,$2,$3,$4)")
                .bind(workspace).bind(artifact).bind(owner).bind(agent).execute(&mut *tx).await?;
            artifact
        };
        prune(&mut tx, context).await?;
        if input.artifact_id.is_some() {
            let pending: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM artifact_versions WHERE (workspace_id,artifact_id)=($1,$2) AND state='uploading')",
            ).bind(workspace).bind(artifact).fetch_one(&mut *tx).await?;
            if pending {
                return Err(IdentityError::ArtifactUploadOpen);
            }
        }
        require_workspace_storage(&mut tx, workspace, input.size_bytes).await?;
        let reserved = sqlx::query(
            "UPDATE artifact_storage_usage SET occupied_bytes=occupied_bytes+$3,occupied_revisions=occupied_revisions+1
             WHERE (workspace_id,owner_user_id)=($1,$2) AND occupied_bytes+$3<=536870912 AND occupied_revisions<256",
        ).bind(workspace).bind(owner).bind(input.size_bytes).execute(&mut *tx).await?;
        if reserved.rows_affected() != 1 {
            return Err(IdentityError::ArtifactQuotaExceeded);
        }
        let version = sqlx::query_as::<_, Version>(
            "INSERT INTO artifact_versions (workspace_id,upload_id,artifact_id,revision,owner_user_id,author_agent_id,
                name,mime,size_bytes,sha256,retention_days,task_id,run_id)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) RETURNING *,clock_timestamp() AS observed_at",
        ).bind(workspace).bind(Uuid::new_v4()).bind(artifact).bind(input.expected_revision + 1).bind(owner).bind(agent)
         .bind(name).bind(input.mime.as_str()).bind(input.size_bytes).bind(&input.sha256).bind(i32::from(input.retention_days))
         .bind(input.provenance.map(|source| source.task_id.0))
         .bind(input.provenance.and_then(|source| source.run_id.map(|run| run.0)))
         .fetch_one(&mut *tx).await?;
        let result = version.upload();
        idem.record(&mut tx, artifact, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn put_artifact_chunk(
        &self,
        digest: &BearerDigest,
        workspace: WorkspaceId,
        upload: Uuid,
        index: i32,
        content: &[u8],
    ) -> Result<ArtifactChunkReceipt, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, workspace).await?;
        let version = owned_upload(&mut tx, context, upload, true).await?;
        if artifact_chunk_length(version.size_bytes, index) != Some(content.len()) {
            return Err(IdentityError::InvalidInput);
        }
        let replay = sqlx::query_scalar::<_, bool>(
            "SELECT content=$4 FROM artifact_chunks WHERE (workspace_id,upload_id,ordinal)=($1,$2,$3)",
        ).bind(workspace.0).bind(upload).bind(index).bind(content).fetch_optional(&mut *tx).await?;
        match replay {
            Some(true) => (),
            Some(false) => return Err(IdentityError::IdempotencyConflict),
            None if version.state != "uploading" || version.upload_until <= version.observed_at => {
                return Err(IdentityError::ArtifactUnavailable);
            }
            None => {
                sqlx::query("INSERT INTO artifact_chunks (workspace_id,upload_id,ordinal,content) VALUES ($1,$2,$3,$4)")
                    .bind(workspace.0).bind(upload).bind(index).bind(content).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(ArtifactChunkReceipt {
            index,
            size_bytes: content.len(),
            sha256: hex::encode(Sha256::digest(content)),
        })
    }

    pub async fn finish_artifact(
        &self,
        digest: &BearerDigest,
        input: FinishArtifactUpload,
    ) -> Result<Artifact, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let workspace = input.workspace_id.0;
        let version = owned_upload(&mut tx, context, input.upload_id, false).await?;
        let revision = lock_artifact(&mut tx, context, version.artifact_id).await?;
        let version = owned_upload(&mut tx, context, input.upload_id, true).await?;
        let version = if version.state == "uploading" {
            if version.upload_until <= version.observed_at {
                return Err(IdentityError::ArtifactUnavailable);
            }
            if version.revision != revision + 1 {
                return Err(IdentityError::RevisionMismatch { current: revision });
            }
            verify_uploaded_bytes(&mut tx, &version).await?;
            let published = sqlx::query_as::<_, Version>(
                "UPDATE artifact_versions SET state='ready',finalized_at=statement_timestamp(),
                   expires_at=statement_timestamp()+make_interval(days=>retention_days)
                 WHERE (workspace_id,upload_id)=($1,$2) AND state='uploading' AND upload_until>statement_timestamp()
                 RETURNING *,clock_timestamp() AS observed_at",
            ).bind(workspace).bind(input.upload_id).fetch_optional(&mut *tx).await?.ok_or(IdentityError::ArtifactUnavailable)?;
            sqlx::query("UPDATE artifacts SET revision=$3 WHERE (workspace_id,id)=($1,$2)")
                .bind(workspace)
                .bind(version.artifact_id)
                .bind(version.revision)
                .execute(&mut *tx)
                .await?;
            append_event(&mut tx, context, version.artifact_id, "artifact.finalized",
                json!({"artifact_id":version.artifact_id,"revision":version.revision,"sha256":version.sha256,"size_bytes":version.size_bytes}),
                None).await?;
            published
        } else {
            version
        };
        let result = version.view(&mut tx, context).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn get_artifact(
        &self,
        digest: &BearerDigest,
        input: GetArtifact,
    ) -> Result<Artifact, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let version = read_version(&mut tx, context, input).await?;
        let result = version.view(&mut tx, context).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn read_artifact_chunk(
        &self,
        digest: &BearerDigest,
        input: GetArtifact,
        index: i32,
    ) -> Result<Vec<u8>, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        let version = read_version(&mut tx, context, input).await?;
        if !version.published() {
            return Err(IdentityError::ArtifactUnavailable);
        }
        if artifact_chunk_length(version.size_bytes, index).is_none() {
            return Err(IdentityError::InvalidInput);
        }
        let content = sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT c.content FROM artifact_chunks c JOIN artifact_versions v USING (workspace_id,upload_id)
             WHERE (c.workspace_id,c.upload_id,c.ordinal)=($1,$2,$3) AND v.state='ready' AND v.expires_at>clock_timestamp()",
        ).bind(version.workspace_id).bind(version.upload_id).bind(index).fetch_optional(&mut *tx).await?
         .ok_or(IdentityError::ArtifactUnavailable)?;
        tx.commit().await?;
        Ok(content)
    }

    pub async fn cleanup_artifacts(
        &self,
        digest: &BearerDigest,
        input: CleanupArtifacts,
    ) -> Result<ArtifactCleanup, IdentityError> {
        let mut tx = self.pool.begin().await?;
        let context = workspace_context(&mut tx, digest, input.workspace_id).await?;
        if context.agent_id().is_some() {
            return Err(IdentityError::Forbidden);
        }
        lock_usage(&mut tx, context).await?;
        let result = prune(&mut tx, context).await?;
        tx.commit().await?;
        Ok(result)
    }
}

fn artifact_name(name: &str) -> Result<&str, IdentityError> {
    let name = name.trim();
    if !(1..=160).contains(&name.len())
        || matches!(name, "." | "..")
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
    {
        return Err(IdentityError::InvalidInput);
    }
    Ok(name)
}

async fn validate_provenance(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    provenance: Option<ArtifactProvenance>,
) -> Result<(), IdentityError> {
    if let Some(source) = provenance {
        authorize_scope(tx, context, source.task_id.0, Action::ReadData).await?;
        let valid: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM tasks t WHERE (t.workspace_id,t.id)=($1,$2) AND ($3::uuid IS NULL OR EXISTS (
               SELECT 1 FROM runs r WHERE (r.workspace_id,r.task_id,r.id)=(t.workspace_id,t.id,$3)
                 AND r.executor_user_id=$4 AND ($5::uuid IS NULL OR r.executor_agent_id=$5))))",
        ).bind(context.access.workspace.0).bind(source.task_id.0).bind(source.run_id.map(|run| run.0))
         .bind(context.access.member.0).bind(context.agent_id().map(|agent| agent.0)).fetch_one(&mut **tx).await?;
        if !valid {
            return Err(IdentityError::Forbidden);
        }
    }
    Ok(())
}

async fn lock_artifact(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    artifact: Uuid,
) -> Result<i64, IdentityError> {
    authorize_scope(tx, context, artifact, Action::WriteData).await?;
    sqlx::query_scalar(
        "SELECT revision FROM artifacts WHERE (workspace_id,id)=($1,$2)
        AND office_owns_data(owner_user_id,coalesce(owner_agent_id,owner_user_id)) FOR UPDATE",
    )
    .bind(context.access.workspace.0)
    .bind(artifact)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(IdentityError::Forbidden)
}

async fn owned_upload(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    upload: Uuid,
    lock: bool,
) -> Result<Version, IdentityError> {
    let select = "SELECT v.*,clock_timestamp() AS observed_at FROM artifact_versions v WHERE (workspace_id,upload_id)=($1,$2)
               AND office_owns_data(owner_user_id,coalesce(author_agent_id,owner_user_id))";
    let statement = if lock {
        format!("{select} FOR UPDATE")
    } else {
        select.into()
    };
    sqlx::query_as(&statement)
        .bind(context.access.workspace.0)
        .bind(upload)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(IdentityError::Forbidden)
}

async fn read_version(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
    input: GetArtifact,
) -> Result<Version, IdentityError> {
    if input.revision.is_some_and(|revision| revision < 1) {
        return Err(IdentityError::InvalidInput);
    }
    authorize_scope(tx, context, input.artifact_id, Action::ReadData).await?;
    sqlx::query_as(
        "SELECT v.*,clock_timestamp() AS observed_at FROM artifact_versions v JOIN artifacts a
          ON (a.workspace_id,a.id)=(v.workspace_id,v.artifact_id)
         WHERE (v.workspace_id,v.artifact_id,v.revision)=($1,$2,coalesce($3,a.revision)) AND v.state<>'uploading'",
    ).bind(input.workspace_id.0).bind(input.artifact_id).bind(input.revision)
     .fetch_optional(&mut **tx).await?.ok_or(IdentityError::ArtifactUnavailable)
}

async fn verify_uploaded_bytes(
    tx: &mut Transaction<'_, Postgres>,
    version: &Version,
) -> Result<(), IdentityError> {
    let mut hash = Sha256::new();
    let mut next = 0;
    let expected = artifact_chunk_count(version.size_bytes);
    while next < expected {
        let batch = sqlx::query_as::<_, (i32, Vec<u8>)>(
            "SELECT ordinal,content FROM artifact_chunks WHERE (workspace_id,upload_id)=($1,$2) AND ordinal>=$3
             ORDER BY ordinal LIMIT 4",
        ).bind(version.workspace_id).bind(version.upload_id).bind(next).fetch_all(&mut **tx).await?;
        if batch.is_empty() {
            return Err(IdentityError::ArtifactIncomplete);
        }
        for (index, bytes) in batch {
            if index != next
                || artifact_chunk_length(version.size_bytes, index) != Some(bytes.len())
            {
                return Err(IdentityError::ArtifactIncomplete);
            }
            hash.update(bytes);
            next += 1;
        }
    }
    if hex::encode(hash.finalize()) != version.sha256 {
        return Err(IdentityError::ArtifactDigestMismatch);
    }
    Ok(())
}

async fn lock_usage(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
) -> Result<(), IdentityError> {
    let workspace = context.access.workspace.0;
    let owner = context.access.member.0;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!("artifact-storage/{workspace}"))
        .execute(&mut **tx)
        .await?;
    sqlx::query("INSERT INTO artifact_storage_usage (workspace_id,owner_user_id) VALUES ($1,$2) ON CONFLICT DO NOTHING")
        .bind(workspace).bind(owner).execute(&mut **tx).await?;
    sqlx::query("SELECT 1 FROM artifact_storage_usage WHERE (workspace_id,owner_user_id)=($1,$2) FOR UPDATE")
        .bind(workspace).bind(owner).fetch_one(&mut **tx).await?;
    Ok(())
}

async fn require_workspace_storage(
    tx: &mut Transaction<'_, Postgres>,
    workspace: Uuid,
    bytes: i64,
) -> Result<(), IdentityError> {
    sqlx::query("SELECT set_config('office.artifact_quota_workspace',$1,true)")
        .bind(workspace.to_string())
        .execute(&mut **tx)
        .await?;
    let available: bool = sqlx::query_scalar(
        "SELECT coalesce(sum(occupied_bytes),0)+$2::bigint<=8589934592
             AND coalesce(sum(occupied_revisions),0)<4096
         FROM artifact_storage_usage WHERE workspace_id=$1",
    )
    .bind(workspace)
    .bind(bytes)
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query("SELECT set_config('office.artifact_quota_workspace','',true)")
        .execute(&mut **tx)
        .await?;
    if !available {
        return Err(IdentityError::ArtifactQuotaExceeded);
    }
    Ok(())
}

async fn prune(
    tx: &mut Transaction<'_, Postgres>,
    context: WorkspaceContext,
) -> Result<ArtifactCleanup, IdentityError> {
    let workspace = context.access.workspace.0;
    let owner = context.access.member.0;
    let abandoned = sqlx::query_scalar::<_, i64>(
        "DELETE FROM artifact_versions WHERE (workspace_id,owner_user_id)=($1,$2)
         AND state='uploading' AND upload_until<=clock_timestamp()
         AND office_owns_data(owner_user_id,coalesce(author_agent_id,owner_user_id)) RETURNING size_bytes",
    ).bind(workspace).bind(owner).fetch_all(&mut **tx).await?;
    let expired = sqlx::query_as::<_, (Uuid, i64)>(
        "UPDATE artifact_versions SET state='expired' WHERE (workspace_id,owner_user_id)=($1,$2)
         AND state='ready' AND expires_at<=clock_timestamp()
         AND office_owns_data(owner_user_id,coalesce(author_agent_id,owner_user_id)) RETURNING upload_id,size_bytes",
    ).bind(workspace).bind(owner).fetch_all(&mut **tx).await?;
    if !expired.is_empty() {
        let ids: Vec<_> = expired.iter().map(|(id, _)| *id).collect();
        sqlx::query("DELETE FROM artifact_chunks WHERE workspace_id=$1 AND upload_id=ANY($2)")
            .bind(workspace)
            .bind(&ids)
            .execute(&mut **tx)
            .await?;
    }
    let freed_bytes: i64 = abandoned
        .iter()
        .chain(expired.iter().map(|(_, size)| size))
        .sum();
    let freed_revisions = (abandoned.len() + expired.len()) as i32;
    if freed_revisions > 0 {
        sqlx::query("UPDATE artifact_storage_usage SET occupied_bytes=occupied_bytes-$3,occupied_revisions=occupied_revisions-$4
            WHERE (workspace_id,owner_user_id)=($1,$2)")
            .bind(workspace).bind(owner).bind(freed_bytes).bind(freed_revisions).execute(&mut **tx).await?;
    }
    Ok(ArtifactCleanup {
        abandoned_uploads: abandoned.len() as i64,
        expired_revisions: expired.len() as i64,
        freed_bytes,
    })
}
