use std::{
    fs::File,
    io::{self, Write},
    path::Path,
};

use office_domain::{PathRule, SourceManifest, SourcePath};
use office_protocol::{
    CheckoutCommandOrder, CommandSnapshot, CommandTemplateSpec, MAX_SOURCE_BYTES, SourceEntry,
};
use rustix::{
    fd::OwnedFd,
    fs::{self, Mode, OFlags},
    io::Errno,
};
use sha2::{Digest, Sha256};

use crate::{
    CheckoutCoordinator,
    linux::{
        InspectionError, copy_source_file, descendant_resolution, open_root, snapshot_source,
        source_baseline,
    },
};

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("the approved source inputs have changed or are unavailable")]
    SourceUnavailable,
    #[error(transparent)]
    Inspect(#[from] InspectionError),
    #[error("could not create the command snapshot: {0}")]
    Os(#[from] Errno),
    #[error("could not persist the command snapshot: {0}")]
    Io(#[from] io::Error),
}

impl CheckoutCoordinator {
    /// The coordinator calls this under its publication barrier and mounts the result read-only.
    pub fn command_snapshot(
        &self,
        state_dir: &Path,
        manifest: &SourceManifest,
        order: &CheckoutCommandOrder,
        destination: &Path,
    ) -> Result<CommandSnapshot, SnapshotError> {
        if !order.scope.permissions.permits_command(
            order.template_id,
            manifest,
            &order.spec.source_inputs,
        ) {
            return Err(SnapshotError::SourceUnavailable);
        }
        let sealed = order
            .source_snapshot_id
            .map(|snapshot| {
                open_root(
                    &crate::sealed_snapshot::snapshot_root(state_dir, snapshot).join("source"),
                )
                .map(|(root, _, _)| root)
            })
            .transpose()?;
        let source = sealed.as_ref().unwrap_or(&self.root);
        if snapshot_source(source, manifest)?.0.manifest_sha256 != order.head.source_sha256 {
            return Err(SnapshotError::SourceUnavailable);
        }
        let selected = SourceManifest {
            include: order.spec.source_inputs.clone(),
            exclude: manifest.exclude.clone(),
        };
        let directory = new_snapshot_directory(destination)?;
        let baseline = if selected.include.is_empty() {
            source_baseline(&selected, &[], 0)?
        } else {
            let (baseline, entries) = snapshot_source(source, &selected)?;
            for input in &selected.include {
                if let PathRule::File { path } = input
                    && !entries.iter().any(|entry| &entry.path == path)
                {
                    return Err(SnapshotError::SourceUnavailable);
                }
            }
            for entry in entries {
                copy_entry(source, &directory, &entry, false)?;
            }
            baseline
        };
        fs::fsync(&directory)?;
        if snapshot_source(source, manifest)?.0.manifest_sha256 != order.head.source_sha256 {
            return Err(SnapshotError::SourceUnavailable);
        }
        Ok(CommandSnapshot {
            checkout_revision: order.head.checkout_revision,
            source_snapshot_id: order.source_snapshot_id,
            sha256: baseline.manifest_sha256,
            file_count: baseline.file_count,
            total_bytes: baseline.total_bytes,
            environment: None,
        })
    }
}

pub fn stage_command_copy(
    snapshot: &Path,
    writable: &Path,
    spec: &CommandTemplateSpec,
) -> Result<(), SnapshotError> {
    let (source, _, _) = open_root(snapshot)?;
    let destination = new_snapshot_directory(writable)?;
    if !spec.source_inputs.is_empty() {
        let manifest = SourceManifest {
            include: spec.source_inputs.clone(),
            exclude: Vec::new(),
        };
        let (_, entries) = snapshot_source(&source, &manifest)?;
        for entry in entries {
            copy_entry(&source, &destination, &entry, true)?;
        }
    }
    fs::fsync(&destination)?;
    Ok(())
}

pub(super) fn new_snapshot_directory(destination: &Path) -> Result<OwnedFd, SnapshotError> {
    std::fs::create_dir(destination)?;
    let (root, _, _) = open_root(destination)?;
    Ok(fs::openat(
        &root,
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

pub(super) fn copy_entry(
    source: &OwnedFd,
    root: &OwnedFd,
    entry: &SourceEntry,
    writable: bool,
) -> Result<(), SnapshotError> {
    let (parent, name) = snapshot_parent(root, &entry.path)?;
    let target = fs::openat(
        &parent,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?;
    let mut writer = DigestWriter {
        file: File::from(target),
        sha: Sha256::new(),
    };
    let (size, executable) =
        copy_source_file(source, entry.path.as_str(), MAX_SOURCE_BYTES, &mut writer)?;
    if entry.size_bytes != size
        || entry.executable != executable
        || hex::encode(writer.sha.finalize()) != entry.sha256
    {
        return Err(SnapshotError::SourceUnavailable);
    }
    let mut mode = Mode::RUSR;
    if executable {
        mode |= Mode::XUSR;
    }
    if writable {
        mode |= Mode::WUSR;
    }
    fs::fchmod(&writer.file, mode)?;
    writer.file.sync_all()?;
    fs::fsync(&parent)?;
    Ok(())
}

fn snapshot_parent<'a>(
    root: &OwnedFd,
    path: &'a SourcePath,
) -> Result<(OwnedFd, &'a str), SnapshotError> {
    let (prefix, name) = path
        .as_str()
        .rsplit_once('/')
        .unwrap_or(("", path.as_str()));
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let mut parent = fs::openat(root, ".", flags, Mode::empty())?;
    for component in prefix.split('/').filter(|component| !component.is_empty()) {
        match fs::mkdirat(&parent, component, Mode::RWXU) {
            Ok(()) => fs::fsync(&parent)?,
            Err(Errno::EXIST) => (),
            Err(error) => return Err(error.into()),
        }
        parent = fs::openat2(
            &parent,
            component,
            flags,
            Mode::empty(),
            descendant_resolution(),
        )?;
    }
    Ok((parent, name))
}

struct DigestWriter {
    file: File,
    sha: Sha256,
}

impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let size = self.file.write(bytes)?;
        self.sha.update(&bytes[..size]);
        Ok(size)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
