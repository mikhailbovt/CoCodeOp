use std::{
    fs::File,
    io::{self, Read, Write},
    path::Path,
};

use office_domain::{PathAccess, SourceManifest, SourcePath};
use office_protocol::{CheckoutCoordinatorLease, RootIdentity, ScopeDeployment};
use rustix::{
    fd::OwnedFd,
    fs::{self, FileType, FlockOperation, Mode, OFlags, RenameFlags},
    io::Errno,
    process::geteuid,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::linux::{InspectionError, copy_source_file, host_fingerprint, open_root};

#[derive(Debug, thiserror::Error)]
pub enum PolicyStateError {
    #[error("the local directory does not match the registered checkout")]
    WrongCheckout,
    #[error("use an owner-only state directory outside the checkout and its ancestors")]
    UnsafeStateDirectory,
    #[error("another coordinator holds the exclusive lock on this checkout root")]
    CheckoutBusy,
    #[error("a local policy receipt conflicts with the server deployment")]
    DeploymentConflict,
    #[error(transparent)]
    Inspect(#[from] InspectionError),
    #[error("could not access local policy state: {0}")]
    Fs(#[from] Errno),
    #[error("could not persist local policy state: {0}")]
    Io(#[from] io::Error),
    #[error("could not encode the scope deployment")]
    Encoding(#[from] serde_json::Error),
}

/// Persist the exact server deployment before the coordinator acknowledges it.
/// Repeating a deployment verifies the same bytes so a lost response cannot apply it again.
pub fn persist_scope_deployment(
    root: &Path,
    state_dir: &Path,
    expected: &RootIdentity,
    deployment: &ScopeDeployment,
) -> Result<String, PolicyStateError> {
    CheckoutCoordinator::open(root, state_dir, expected)?.persist_scope_deployment(deployment)
}

pub struct CheckoutCoordinator {
    pub(super) root: OwnedFd,
    pub(super) state: OwnedFd,
}

impl CheckoutCoordinator {
    pub fn open(
        root: &Path,
        state_dir: &Path,
        expected: &RootIdentity,
    ) -> Result<Self, PolicyStateError> {
        let checkout = Self::bind(root, state_dir, expected)?;
        checkout.lock()?;
        Ok(checkout)
    }

    pub(super) fn bind(
        root: &Path,
        state_dir: &Path,
        expected: &RootIdentity,
    ) -> Result<Self, PolicyStateError> {
        let (checkout, checkout_id, ancestry) = open_root(root)?;
        if host_fingerprint()? != expected.host_fingerprint
            || checkout_id != expected.directory_id
            || ancestry != expected.ancestors
        {
            return Err(PolicyStateError::WrongCheckout);
        }
        let (state_fd, state_id, state_ancestry) = open_root(state_dir)?;
        let state_stat = fs::fstat(&state_fd)?;
        if state_id == checkout_id
            || ancestry.contains(&state_id)
            || state_ancestry.contains(&checkout_id)
            || state_stat.st_mode & 0o077 != 0
            || state_stat.st_uid != geteuid().as_raw()
        {
            return Err(PolicyStateError::UnsafeStateDirectory);
        }
        let root = fs::openat(
            &checkout,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let state = fs::openat(
            &state_fd,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self { root, state })
    }

    pub(super) fn lock(&self) -> Result<(), PolicyStateError> {
        fs::flock(&self.root, FlockOperation::NonBlockingLockExclusive).map_err(|error| {
            if error == Errno::WOULDBLOCK {
                PolicyStateError::CheckoutBusy
            } else {
                PolicyStateError::Fs(error)
            }
        })
    }

    pub fn read_file(
        &self,
        manifest: &SourceManifest,
        permissions: &PathAccess,
        path: &SourcePath,
        max_bytes: u64,
    ) -> Result<Vec<u8>, CheckoutReadError> {
        if !manifest.allows(path) || !permissions.allows(path) {
            return Err(CheckoutReadError::Forbidden);
        }
        let mut bytes = Vec::new();
        copy_source_file(&self.root, path.as_str(), max_bytes, &mut bytes).map_err(|error| {
            match error {
                InspectionError::Path(Errno::NOENT) => CheckoutReadError::Missing,
                InspectionError::Path(Errno::LOOP | Errno::XDEV | Errno::NOTDIR)
                | InspectionError::UnsupportedEntry => CheckoutReadError::Forbidden,
                InspectionError::Limit => CheckoutReadError::Limit,
                InspectionError::SourceChanged => CheckoutReadError::Changed,
                _ => CheckoutReadError::Unavailable,
            }
        })?;
        Ok(bytes)
    }

    pub fn persist_epoch(&self, lease: &CheckoutCoordinatorLease) -> Result<(), PolicyStateError> {
        let name = format!("coordinator-{}.json", lease.epoch);
        let bytes = serde_json::to_vec(lease)?;
        persist_record(&self.state, &name, &bytes)
    }

    pub fn persist_scope_deployment(
        &self,
        deployment: &ScopeDeployment,
    ) -> Result<String, PolicyStateError> {
        let state = &self.state;
        let lock = fs::openat(
            state,
            ".scope-lock",
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::RUSR | Mode::WUSR,
        )?;
        verify_file(&lock)?;
        fs::flock(&lock, FlockOperation::NonBlockingLockExclusive)?;
        let file_name = format!(
            "scope-{}-{}.json",
            deployment.scope.scope_id, deployment.scope.revision
        );
        let bytes = serde_json::to_vec(deployment)?;
        let sha256 = hex::encode(Sha256::digest(&bytes));

        persist_record(state, &file_name, &bytes)?;
        Ok(sha256)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutReadError {
    Forbidden,
    Missing,
    Limit,
    Changed,
    Unavailable,
}

pub(super) fn persist_record(
    state: &OwnedFd,
    file_name: &str,
    bytes: &[u8],
) -> Result<(), PolicyStateError> {
    match fs::openat(
        state,
        file_name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(existing) => {
            verify_file(&existing)?;
            let mut previous = Vec::new();
            let file = File::from(existing);
            (&file)
                .take(bytes.len() as u64 + 1)
                .read_to_end(&mut previous)?;
            if previous != bytes {
                return Err(PolicyStateError::DeploymentConflict);
            }
            file.sync_all()?;
        }
        Err(Errno::NOENT) => write_atomic(state, file_name, bytes)?,
        Err(error) => return Err(error.into()),
    }
    fs::fsync(state)?;
    Ok(())
}

pub(super) fn verify_file(fd: &OwnedFd) -> Result<(), PolicyStateError> {
    let stat = fs::fstat(fd)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_nlink != 1
        || stat.st_mode & 0o077 != 0
    {
        return Err(PolicyStateError::UnsafeStateDirectory);
    }
    Ok(())
}

fn write_atomic(state: &OwnedFd, file_name: &str, bytes: &[u8]) -> Result<(), PolicyStateError> {
    let staging = format!(".scope-{}.partial", Uuid::new_v4());
    let fd = fs::openat(
        state,
        staging.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?;
    let result = (|| {
        let mut file = File::from(fd);
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::renameat_with(
            state,
            staging.as_str(),
            state,
            file_name,
            RenameFlags::NOREPLACE,
        )?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::unlinkat(state, staging.as_str(), fs::AtFlags::empty());
    }
    result
}
