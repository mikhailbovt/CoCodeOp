use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use office_domain::SourceManifest;
use office_protocol::{
    CheckoutReleaseOrder, CheckoutReleaseOutcome, CheckoutSnapshotOrder, CheckoutSnapshotProof,
    is_sha256,
};
use rustix::{
    fs::{CWD, RenameFlags, renameat_with},
    io::Errno,
};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use uuid::Uuid;

use crate::{
    CheckoutCoordinator, SnapshotError,
    checkpoint_repository::CheckpointRepository,
    linux::{InspectionError, open_root, snapshot_source},
    snapshot::{copy_entry, new_snapshot_directory},
};

const RECORD: &str = "capture.json";

#[derive(Debug, thiserror::Error)]
pub enum SealError {
    #[error("the authoritative source does not match the requested checkout head")]
    SourceChanged,
    #[error("the local checkpoint storage is unavailable")]
    StorageUnavailable,
    #[error("Git checkpoint operation {0} failed")]
    Git(&'static str),
    #[error(transparent)]
    Inspect(#[from] InspectionError),
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    #[error(transparent)]
    Os(#[from] Errno),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Encoding(#[from] serde_json::Error),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    order: CheckoutSnapshotOrder,
    proof: CheckoutSnapshotProof,
}

impl CheckoutCoordinator {
    pub fn accept_snapshot(
        &self,
        state_dir: &Path,
        manifest: &SourceManifest,
        order: CheckoutReleaseOrder,
    ) -> Result<CheckoutReleaseOutcome, SealError> {
        if !is_sha256(&order.tree_sha256)
            || !is_sha256(&order.authorization_sha256)
            || order
                .expected_commit_sha256
                .as_deref()
                .is_some_and(|hash| !is_sha256(hash))
        {
            return Err(SealError::StorageUnavailable);
        }
        let location = snapshot_root(state_dir, order.snapshot_id);
        let local = load_record(&location)?.ok_or(SealError::StorageUnavailable)?;
        if local.order.checkout_id != order.checkout_id
            || local.order.snapshot_id != order.snapshot_id
            || local.order.head != order.head
            || local.proof.git_tree_sha256 != order.tree_sha256
        {
            return Err(SealError::StorageUnavailable);
        }
        verify_sealed(&location, manifest, &local)?;
        let repo = CheckpointRepository::open(state_dir)?;
        let observed = repo.accepted_ref()?;
        let commit = repo.commit_release(&order)?;
        let success = CheckoutReleaseOutcome::Accepted {
            commit_sha256: commit.clone(),
            tree_sha256: order.tree_sha256.clone(),
        };
        if observed.as_deref() == Some(commit.as_str()) {
            return Ok(success);
        }
        if observed != order.expected_commit_sha256 {
            return Ok(CheckoutReleaseOutcome::RefConflict);
        }
        if snapshot_source(&self.root, manifest)?.0 != local.proof.baseline {
            return Ok(CheckoutReleaseOutcome::SourceChanged);
        }
        if repo.advance_accepted(&commit, order.expected_commit_sha256.as_deref())? {
            Ok(success)
        } else {
            Ok(CheckoutReleaseOutcome::RefConflict)
        }
    }

    /// The caller holds the checkout lock and processes publications and captures serially.
    pub fn seal_source(
        &self,
        state_dir: &Path,
        manifest: &SourceManifest,
        order: CheckoutSnapshotOrder,
    ) -> Result<CheckoutSnapshotProof, SealError> {
        let archive = state_dir.join("snapshots");
        match std::fs::DirBuilder::new().mode(0o700).create(&archive) {
            Ok(()) => File::open(state_dir)?.sync_all()?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error.into()),
        }
        open_root(&archive)?;
        let destination = snapshot_root(state_dir, order.snapshot_id);
        if let Some(existing) = load_record(&destination)? {
            if existing.order != order {
                return Err(SealError::StorageUnavailable);
            }
            verify_sealed(&destination, manifest, &existing)?;
            let repository = CheckpointRepository::open(state_dir)?;
            repository.record_snapshot(
                order.snapshot_id,
                &existing.proof.git_commit_sha256,
                &existing.proof.git_tree_sha256,
            )?;
            return Ok(existing.proof);
        }
        let (baseline, entries) = snapshot_source(&self.root, manifest)?;
        if baseline.manifest_sha256 != order.head.source_sha256 {
            return Err(SealError::SourceChanged);
        }
        let staging = TempDir::new_in(&archive)?;
        let source = staging.path().join("source");
        let directory = new_snapshot_directory(&source)?;
        for entry in &entries {
            copy_entry(&self.root, &directory, entry, false)?;
        }
        rustix::fs::fsync(&directory)?;
        if snapshot_source(&self.root, manifest)?.0 != baseline {
            return Err(SealError::SourceChanged);
        }
        let repository = CheckpointRepository::open(state_dir)?;
        let index = staging.path().join("index");
        let tree = repository.write_tree(&source, &index, &entries)?;
        let commit = repository.commit_snapshot(order.snapshot_id, &tree)?;
        if index.is_file() {
            std::fs::remove_file(index)?;
        }
        let record = Record {
            order,
            proof: CheckoutSnapshotProof {
                baseline,
                git_tree_sha256: tree,
                git_commit_sha256: commit,
            },
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(staging.path().join(RECORD))?;
        file.write_all(&serde_json::to_vec(&record)?)?;
        file.sync_all()?;
        File::open(staging.path())?.sync_all()?;
        renameat_with(
            CWD,
            staging.path(),
            CWD,
            &destination,
            RenameFlags::NOREPLACE,
        )?;
        File::open(&archive)?.sync_all()?;
        repository.record_snapshot(
            record.order.snapshot_id,
            &record.proof.git_commit_sha256,
            &record.proof.git_tree_sha256,
        )?;
        Ok(record.proof)
    }
}

pub(super) fn snapshot_root(state_dir: &Path, snapshot: Uuid) -> PathBuf {
    state_dir.join("snapshots").join(snapshot.to_string())
}

fn verify_sealed(root: &Path, manifest: &SourceManifest, record: &Record) -> Result<(), SealError> {
    let (source, _, _) = open_root(&root.join("source"))?;
    if snapshot_source(&source, manifest)?.0 != record.proof.baseline
        || record.proof.baseline.manifest_sha256 != record.order.head.source_sha256
    {
        return Err(SealError::StorageUnavailable);
    }
    Ok(())
}

fn load_record(root: &Path) -> Result<Option<Record>, SealError> {
    let file = match File::open(root.join(RECORD)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > 8192 {
        return Err(SealError::StorageUnavailable);
    }
    let mut bytes = Vec::new();
    file.take(8193).read_to_end(&mut bytes)?;
    if bytes.len() > 8192 {
        return Err(SealError::StorageUnavailable);
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}
