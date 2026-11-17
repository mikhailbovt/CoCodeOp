use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::{Args, Subcommand};
use office_domain::WorkspaceId;
use office_protocol::{
    ARTIFACT_CHUNK_BYTES, Artifact, ArtifactChunkReceipt, ArtifactCleanup, ArtifactUpload,
    BeginArtifactUpload, CleanupArtifacts, FinishArtifactUpload, GetArtifact, MAX_ARTIFACT_BYTES,
    artifact_chunk_length,
};
use reqwest::{Client, Method, Url, header};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{identity, print_receipt, server_client, server_url};

#[derive(Subcommand)]
pub(crate) enum ArtifactCommand {
    /// Publish a local file with revision metadata from JSON.
    Upload {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        content: PathBuf,
        #[command(flatten)]
        connection: Connection,
    },
    /// Inspect metadata for an artifact or a fixed revision.
    Get {
        #[command(flatten)]
        artifact: ArtifactArgs,
        #[command(flatten)]
        connection: Connection,
    },
    /// Download and verify an artifact revision into a new local file.
    Download {
        #[command(flatten)]
        artifact: ArtifactArgs,
        #[arg(long)]
        out: PathBuf,
        #[command(flatten)]
        connection: Connection,
    },
    /// Collect the current owner's expired revisions and abandoned uploads.
    Cleanup {
        #[arg(long)]
        workspace: Uuid,
        #[command(flatten)]
        connection: Connection,
    },
}

#[derive(Args)]
pub(crate) struct Connection {
    #[arg(long, default_value = "http://127.0.0.1:8400")]
    url: Url,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub(crate) struct ArtifactArgs {
    #[arg(long)]
    workspace: Uuid,
    #[arg(long)]
    id: Uuid,
    #[arg(long)]
    revision: Option<i64>,
}

impl From<ArtifactArgs> for GetArtifact {
    fn from(args: ArtifactArgs) -> Self {
        Self {
            workspace_id: WorkspaceId(args.workspace),
            artifact_id: args.id,
            revision: args.revision,
        }
    }
}

pub(crate) async fn run(command: ArtifactCommand) -> ExitCode {
    let (result, json) = match command {
        ArtifactCommand::Upload {
            file,
            content,
            connection,
        } => (
            publish(connection.url, &file, &content).await,
            connection.json,
        ),
        ArtifactCommand::Get {
            artifact,
            connection,
        } => (
            identity::authenticated_post(
                connection.url,
                "/v1/artifacts/read",
                &GetArtifact::from(artifact),
            )
            .await,
            connection.json,
        ),
        ArtifactCommand::Download {
            artifact,
            out,
            connection,
        } => (
            download(connection.url, GetArtifact::from(artifact), &out).await,
            connection.json,
        ),
        ArtifactCommand::Cleanup {
            workspace,
            connection,
        } => {
            return print_receipt(
                identity::authenticated_post::<ArtifactCleanup>(
                    connection.url,
                    "/v1/artifacts/cleanup",
                    &CleanupArtifacts {
                        workspace_id: WorkspaceId(workspace),
                    },
                )
                .await,
                connection.json,
                |result| {
                    format!(
                        "Collected {} uploads and {} revisions, freed {} bytes",
                        result.abandoned_uploads, result.expired_revisions, result.freed_bytes
                    )
                },
            );
        }
    };
    print_receipt(result, json, |item| {
        format!(
            "{} ({}) revision {}  {} bytes  {}",
            item.name, item.artifact_id, item.revision, item.size_bytes, item.sha256
        )
    })
}

struct Transfer {
    url: Url,
    client: Client,
    token: String,
}

impl Transfer {
    fn new(url: Url) -> Result<Self, String> {
        identity::require_confidential(&url)?;
        Ok(Self {
            url,
            client: server_client()?,
            token: identity::user_token()?,
        })
    }

    fn request(&self, method: Method, path: &str) -> Result<reqwest::RequestBuilder, String> {
        Ok(self.client.request(method, server_url(&self.url, path)?))
    }

    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        input: &impl serde::Serialize,
    ) -> Result<T, String> {
        identity::reply(self.request(Method::POST, path)?.json(input), &self.token).await
    }
}

async fn publish(url: Url, metadata: &Path, content: &Path) -> Result<Artifact, String> {
    let mut source = File::open(content).map_err(|_| "could not open the artifact source file")?;
    if !source
        .metadata()
        .map_err(|_| "could not inspect the artifact source file")?
        .is_file()
    {
        return Err("the artifact source must be a regular file".into());
    }
    let mut buffer = vec![0; ARTIFACT_CHUNK_BYTES];
    let (size, digest) = measure(&mut source, &mut buffer)?;
    let bytes = tokio::fs::read(metadata)
        .await
        .map_err(|_| "could not read artifact metadata")?;
    let mut value: Value =
        serde_json::from_slice(&bytes).map_err(|_| "invalid artifact metadata JSON")?;
    let fields = value
        .as_object_mut()
        .ok_or("artifact metadata must be a JSON object")?;
    fields.entry("size_bytes").or_insert(json!(size));
    fields.entry("sha256").or_insert(json!(digest));
    let input: BeginArtifactUpload =
        serde_json::from_value(value).map_err(|_| "invalid artifact metadata")?;
    if input.size_bytes != size || input.sha256 != digest {
        return Err("the source does not match the declared length or SHA-256".into());
    }
    source
        .seek(SeekFrom::Start(0))
        .map_err(|_| "could not rewind the artifact source")?;
    let remote = Transfer::new(url)?;
    let upload: ArtifactUpload = remote.post("/v1/artifacts/begin", &input).await?;
    if upload.workspace_id != input.workspace_id
        || upload.chunk_bytes != ARTIFACT_CHUNK_BYTES
        || upload.chunk_count != office_protocol::artifact_chunk_count(size)
        || input.artifact_id.is_some_and(|id| id != upload.artifact_id)
        || input.expected_revision.checked_add(1) != Some(upload.revision)
    {
        return Err("the server returned an inconsistent artifact upload".into());
    }
    for index in 0..upload.chunk_count {
        let length = artifact_chunk_length(size, index).ok_or("invalid artifact chunk size")?;
        let chunk = &mut buffer[..length];
        source
            .read_exact(chunk)
            .map_err(|_| "the artifact source changed during upload")?;
        let address = format!(
            "/v1/artifacts/upload/{}/{}/{index}",
            upload.workspace_id.0, upload.upload_id
        );
        let request = remote
            .request(Method::PUT, &address)?
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(chunk.to_vec());
        let receipt: ArtifactChunkReceipt = identity::reply(request, &remote.token).await?;
        if receipt.index != index
            || receipt.size_bytes != length
            || receipt.sha256 != hex::encode(Sha256::digest(chunk))
        {
            return Err("the server returned an inconsistent chunk receipt".into());
        }
    }
    if source
        .read(&mut buffer[..1])
        .map_err(|_| "could not recheck the artifact source")?
        != 0
    {
        return Err("the artifact source grew during upload".into());
    }
    let result: Artifact = remote
        .post(
            "/v1/artifacts/finalize",
            &FinishArtifactUpload {
                workspace_id: upload.workspace_id,
                upload_id: upload.upload_id,
            },
        )
        .await?;
    if result.artifact_id != upload.artifact_id
        || result.revision != upload.revision
        || result.sha256 != digest
        || result.size_bytes != size
    {
        return Err("the server returned an inconsistent artifact revision".into());
    }
    Ok(result)
}

fn measure(source: &mut File, buffer: &mut [u8]) -> Result<(i64, String), String> {
    let mut size = 0;
    let mut sha256 = Sha256::new();
    loop {
        let read = source
            .read(buffer)
            .map_err(|_| "could not read the artifact source file")?;
        if read == 0 {
            break;
        }
        size += read as i64;
        if size > MAX_ARTIFACT_BYTES {
            return Err("artifacts may contain up to 64 MiB".into());
        }
        sha256.update(&buffer[..read]);
    }
    if size == 0 {
        return Err("the artifact is empty".into());
    }
    Ok((size, hex::encode(sha256.finalize())))
}

async fn download(url: Url, input: GetArtifact, out: &Path) -> Result<Artifact, String> {
    let remote = Transfer::new(url)?;
    let artifact: Artifact = remote.post("/v1/artifacts/read", &input).await?;
    if artifact.workspace_id != input.workspace_id
        || artifact.artifact_id != input.artifact_id
        || input
            .revision
            .is_some_and(|revision| artifact.revision != revision)
        || !(1..=MAX_ARTIFACT_BYTES).contains(&artifact.size_bytes)
        || artifact.chunk_count != office_protocol::artifact_chunk_count(artifact.size_bytes)
        || !office_protocol::is_sha256(&artifact.sha256)
    {
        return Err("the server returned inconsistent artifact metadata".into());
    }
    if !artifact.available {
        return Err("the requested artifact revision has expired".into());
    }
    let directory = out
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(directory)
        .map_err(|_| "could not create a local temporary file")?;
    let mut hash = Sha256::new();
    for index in 0..artifact.chunk_count {
        let length = artifact_chunk_length(artifact.size_bytes, index)
            .ok_or("invalid artifact chunk size")?;
        let address = format!(
            "/v1/artifacts/content/{}/{}/{}/{index}",
            artifact.workspace_id.0, artifact.artifact_id, artifact.revision
        );
        let mut response = remote
            .request(Method::GET, &address)?
            .bearer_auth(&remote.token)
            .send()
            .await
            .map_err(|_| "could not reach the CoCodeOp artifact endpoint")?;
        if !response.status().is_success()
            || response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                != Some("application/octet-stream")
        {
            return Err(format!(
                "artifact chunk {index} is unavailable (HTTP {})",
                response.status().as_u16()
            ));
        }
        let mut received = 0;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "could not receive the artifact chunk")?
        {
            received += chunk.len();
            if received > length {
                return Err("artifact chunk exceeds its declared length".into());
            }
            hash.update(&chunk);
            temporary
                .write_all(&chunk)
                .map_err(|_| "could not write the local artifact")?;
        }
        if received != length {
            return Err("artifact chunk ended before its declared length".into());
        }
    }
    if hex::encode(hash.finalize()) != artifact.sha256 {
        return Err("downloaded bytes do not match the artifact SHA-256".into());
    }
    temporary
        .as_file()
        .sync_all()
        .map_err(|_| "could not sync the local artifact")?;
    temporary
        .persist_noclobber(out)
        .map_err(|_| "could not publish the verified file at the requested new path")?;
    Ok(artifact)
}
