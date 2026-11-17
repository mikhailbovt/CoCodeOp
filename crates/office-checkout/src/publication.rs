use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read, Write},
    time::Instant,
};

use office_domain::{SourceManifest, SourcePath};
use office_protocol::{
    CheckoutChange, CheckoutContentHead, CheckoutFileDelta, CheckoutPublicationError,
    CheckoutWriteOrder, CheckoutWriteOutcome, MAX_PUBLICATION_BYTES, MAX_SOURCE_BYTES,
    MAX_SOURCE_FILES, SourceEntry,
};
use rustix::{
    fd::OwnedFd,
    fs::{self, AtFlags, Mode, OFlags, RenameFlags, StatxFlags},
    io::Errno,
    process::geteuid,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    CheckoutCoordinator, PolicyStateError,
    linux::{
        InspectionError, copy_checked_source, descendant_resolution, open_source_file,
        snapshot_source, source_baseline,
    },
    policy_state::{persist_record, verify_file},
};

const PENDING: &str = "publication-pending.json";
const STATE_ID: &str = "publication-state.json";
const MAX_RECORD_BYTES: u64 = 96 * 1024;

pub struct CheckoutPublisher<'a> {
    local: &'a CheckoutCoordinator,
    checkout_id: Uuid,
    state_id: Uuid,
    manifest: &'a SourceManifest,
}

#[derive(Debug, thiserror::Error)]
pub enum PublicationStateError {
    #[error("the checkout journal and worktree require reconciliation")]
    Reconcile,
    #[error("the checkout and its write journal require the same Linux mount")]
    DifferentMount,
    #[error(transparent)]
    State(#[from] PolicyStateError),
    #[error(transparent)]
    Inspect(#[from] InspectionError),
    #[error("could not access checkout publication state: {0}")]
    Os(#[from] Errno),
    #[error("could not persist checkout publication state: {0}")]
    Io(#[from] io::Error),
    #[error("could not encode checkout publication state")]
    Encoding(#[from] serde_json::Error),
}

#[derive(Deserialize, Serialize)]
struct JournalOwner {
    checkout_id: Uuid,
    state_id: Uuid,
}

#[derive(Deserialize, Serialize)]
struct Journal {
    checkout_id: Uuid,
    change_id: Uuid,
    epoch: i64,
    previous: CheckoutContentHead,
    next: CheckoutContentHead,
    steps: Vec<Step>,
}

#[derive(Deserialize, Serialize)]
struct Step {
    path: SourcePath,
    parent: DirectoryIdentity,
    before: Option<FileVersion>,
    after: Option<FileVersion>,
}

#[derive(PartialEq, Eq, Deserialize, Serialize)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
struct FileVersion {
    sha256: String,
    size_bytes: u64,
    mode: u32,
}

struct Content {
    version: FileVersion,
    bytes: Vec<u8>,
}

struct PlannedStep {
    path: SourcePath,
    parent: DirectoryIdentity,
    before: Option<Content>,
    after: Option<Content>,
}

#[derive(Debug)]
enum PrepareError {
    Reject(CheckoutPublicationError),
    State(PublicationStateError),
}

impl From<PublicationStateError> for PrepareError {
    fn from(error: PublicationStateError) -> Self {
        Self::State(error)
    }
}

impl CheckoutPublisher<'_> {
    pub fn open<'a>(
        local: &'a CheckoutCoordinator,
        checkout_id: Uuid,
        manifest: &'a SourceManifest,
    ) -> Result<CheckoutPublisher<'a>, PublicationStateError> {
        if mount(&local.root)? != mount(&local.state)? {
            return Err(PublicationStateError::DifferentMount);
        }
        let state_id = if let Some(owner) = read_record::<JournalOwner>(&local.state, STATE_ID)? {
            if owner.checkout_id != checkout_id {
                return Err(PublicationStateError::Reconcile);
            }
            owner.state_id
        } else {
            if read_record::<Journal>(&local.state, PENDING)?.is_some() {
                return Err(PublicationStateError::Reconcile);
            }
            let state_id = Uuid::new_v4();
            write_record(
                &local.state,
                STATE_ID,
                &JournalOwner {
                    checkout_id,
                    state_id,
                },
            )?;
            state_id
        };
        Ok(CheckoutPublisher {
            local,
            checkout_id,
            state_id,
            manifest,
        })
    }

    pub fn state_id(&self) -> Uuid {
        self.state_id
    }

    pub fn require_no_pending(&self) -> Result<(), PublicationStateError> {
        if self.pending()?.is_some() {
            return Err(PublicationStateError::Reconcile);
        }
        Ok(())
    }

    pub fn apply(
        &self,
        order: &CheckoutWriteOrder,
        deadline: Instant,
    ) -> Result<CheckoutWriteOutcome, PublicationStateError> {
        if let Some(journal) = self.pending()? {
            return self.finish_order(order, &journal);
        }
        if Instant::now() >= deadline {
            return Ok(rejected(CheckoutPublicationError::InterruptedBeforeStart));
        }
        let journal = match self.prepare(order, deadline) {
            Ok(journal) => journal,
            Err(PrepareError::Reject(reason)) => return Ok(rejected(reason)),
            Err(PrepareError::State(error)) => return Err(error),
        };
        self.finish_order(order, &journal)
    }

    /// An outstanding order without a local intent may have been authorized by core
    /// just before the host stopped. It must obtain a fresh authorization to start.
    pub fn recover(
        &self,
        head: &CheckoutContentHead,
        order: Option<&CheckoutWriteOrder>,
    ) -> Result<Option<CheckoutWriteOutcome>, PublicationStateError> {
        match (self.pending()?, order) {
            (Some(journal), Some(order)) => self.finish_order(order, &journal).map(Some),
            (Some(journal), None) => {
                if journal.next != *head {
                    return Err(PublicationStateError::Reconcile);
                }
                self.verify_head(head)?;
                self.acknowledge(journal.change_id)?;
                Ok(None)
            }
            (None, Some(_)) => {
                self.verify_head(head)?;
                Ok(Some(rejected(
                    CheckoutPublicationError::InterruptedBeforeStart,
                )))
            }
            (None, None) => {
                self.verify_head(head)?;
                Ok(None)
            }
        }
    }

    pub fn acknowledge(&self, change_id: Uuid) -> Result<(), PublicationStateError> {
        let Some(journal) = self.pending()? else {
            return Ok(());
        };
        if journal.change_id != change_id || self.receipt(&journal)?.is_none() {
            return Err(PublicationStateError::Reconcile);
        }
        write_record(&self.local.state, &history_name(change_id), &journal)?;
        for (index, step) in journal.steps.iter().enumerate() {
            let slot = slot_name(change_id, index);
            if let Some(content) = observe(&self.local.state, &slot).map_err(to_state)? {
                if step.before.as_ref() != Some(&content.version) {
                    return Err(PublicationStateError::Reconcile);
                }
                fs::unlinkat(&self.local.state, slot.as_str(), AtFlags::empty())?;
            }
            if let Some(before) = &step.before {
                let backup = backup_name(change_id, index);
                if let Some(content) = observe(&self.local.state, &backup).map_err(to_state)? {
                    if content.version.sha256 != before.sha256 {
                        return Err(PublicationStateError::Reconcile);
                    }
                    fs::unlinkat(&self.local.state, backup.as_str(), AtFlags::empty())?;
                }
            }
        }
        fs::fsync(&self.local.state)?;
        fs::unlinkat(&self.local.state, PENDING, AtFlags::empty())?;
        fs::fsync(&self.local.state)?;
        Ok(())
    }

    fn pending(&self) -> Result<Option<Journal>, PublicationStateError> {
        let journal = read_record::<Journal>(&self.local.state, PENDING)?;
        if journal
            .as_ref()
            .is_some_and(|item| item.checkout_id != self.checkout_id)
        {
            return Err(PublicationStateError::Reconcile);
        }
        Ok(journal)
    }

    fn verify_head(&self, head: &CheckoutContentHead) -> Result<(), PublicationStateError> {
        if snapshot_source(&self.local.root, self.manifest)?
            .0
            .manifest_sha256
            != head.source_sha256
        {
            return Err(PublicationStateError::Reconcile);
        }
        Ok(())
    }

    fn prepare(
        &self,
        order: &CheckoutWriteOrder,
        deadline: Instant,
    ) -> Result<Journal, PrepareError> {
        if order.scope.checkout_id != self.checkout_id
            || order.scope.revision != order.input.scope_revision
            || order.input.base_checkout_revision > order.previous.checkout_revision
            || !order.input.is_valid()
            || !order.input.is_permitted(self.manifest, &order.scope)
        {
            return Err(PrepareError::Reject(CheckoutPublicationError::Forbidden));
        }
        if !order.input.includes_scope_dependencies(&order.scope) {
            return Err(PrepareError::Reject(CheckoutPublicationError::ContextStale));
        }
        let planned = self.plan(order)?;
        if let Some(original) = order.input.compensates_change_id {
            self.verify_reversal(original, &planned)?;
        }
        let (baseline, files) = snapshot_source(&self.local.root, self.manifest)
            .map_err(PublicationStateError::from)?;
        if baseline.manifest_sha256 != order.previous.source_sha256 {
            return Err(PublicationStateError::Reconcile.into());
        }
        let next = CheckoutContentHead {
            checkout_revision: order
                .previous
                .checkout_revision
                .checked_add(1)
                .ok_or(PublicationStateError::Reconcile)?,
            source_sha256: self.after_baseline(files, &planned)?,
        };
        let mut steps = Vec::with_capacity(planned.len());
        for (index, step) in planned.into_iter().enumerate() {
            if let Some(before) = &step.before {
                let backup = backup_name(order.change_id, index);
                persist_record(&self.local.state, &backup, &before.bytes)
                    .map_err(PublicationStateError::from)?;
            }
            if let Some(after) = &step.after {
                self.stage(order.change_id, index, after)?;
            }
            steps.push(Step {
                path: step.path,
                parent: step.parent,
                before: step.before.map(|before| before.version),
                after: step.after.map(|after| after.version),
            });
        }
        if Instant::now() >= deadline {
            return Err(PrepareError::Reject(
                CheckoutPublicationError::InterruptedBeforeStart,
            ));
        }
        let journal = Journal {
            checkout_id: self.checkout_id,
            change_id: order.change_id,
            epoch: order.epoch,
            previous: order.previous.clone(),
            next,
            steps,
        };
        write_record(&self.local.state, PENDING, &journal)?;
        Ok(journal)
    }

    fn verify_reversal(&self, original: Uuid, planned: &[PlannedStep]) -> Result<(), PrepareError> {
        let journal = read_record::<Journal>(&self.local.state, &history_name(original))?
            .ok_or(PrepareError::Reject(CheckoutPublicationError::ContextStale))?;
        if journal.checkout_id != self.checkout_id
            || journal.change_id != original
            || journal.steps.len() != planned.len()
        {
            return Err(PrepareError::Reject(CheckoutPublicationError::ContextStale));
        }
        for step in planned {
            let prior = journal
                .steps
                .iter()
                .find(|prior| prior.path == step.path)
                .ok_or(PrepareError::Reject(CheckoutPublicationError::ContextStale))?;
            let actual = step.before.as_ref().map(|value| &value.version);
            let reversed = step.after.as_ref().map(|value| &value.version);
            if fingerprint(actual) != fingerprint(prior.after.as_ref())
                || fingerprint(reversed) != fingerprint(prior.before.as_ref())
            {
                return Err(PrepareError::Reject(
                    CheckoutPublicationError::ContentConflict,
                ));
            }
        }
        Ok(())
    }

    fn plan(&self, order: &CheckoutWriteOrder) -> Result<Vec<PlannedStep>, PrepareError> {
        for dependency in &order.input.read_dependencies {
            let (parent, name, _) = open_parent(&self.local.root, &dependency.path)?;
            let mut sha = Sha256::new();
            match crate::linux::copy_source_file(&parent, name, MAX_SOURCE_BYTES, &mut sha) {
                Ok(_) if hex::encode(sha.finalize()) == dependency.sha256 => (),
                Ok(_)
                | Err(InspectionError::Path(Errno::NOENT) | InspectionError::SourceChanged) => {
                    return Err(PrepareError::Reject(CheckoutPublicationError::ContextStale));
                }
                Err(error) => return Err(PublicationStateError::from(error).into()),
            }
        }
        let mut planned = Vec::new();
        for change in &order.input.operations {
            match change {
                CheckoutChange::Create {
                    path,
                    file,
                    executable,
                } => {
                    let mut step = self.capture(path, None)?;
                    step.after = Some(content(
                        file.bytes().ok_or(PublicationStateError::Reconcile)?,
                        if *executable { 0o755 } else { 0o644 },
                    ));
                    planned.push(step);
                }
                CheckoutChange::Replace {
                    path,
                    before_sha256,
                    file,
                    executable,
                } => {
                    let mut step = self.capture(path, Some(before_sha256))?;
                    let prior = &step
                        .before
                        .as_ref()
                        .ok_or(PublicationStateError::Reconcile)?
                        .version;
                    let mode = executable.map_or(prior.mode, |value| {
                        if value {
                            prior.mode | 0o100
                        } else {
                            prior.mode & !0o111
                        }
                    });
                    step.after = Some(content(
                        file.bytes().ok_or(PublicationStateError::Reconcile)?,
                        mode,
                    ));
                    planned.push(step);
                }
                CheckoutChange::Delete {
                    path,
                    before_sha256,
                } => {
                    planned.push(self.capture(path, Some(before_sha256))?);
                }
                CheckoutChange::Rename {
                    source,
                    target,
                    before_sha256,
                } => {
                    let before = self.capture(source, Some(before_sha256))?;
                    let mut after = self.capture(target, None)?;
                    let source = before
                        .before
                        .as_ref()
                        .ok_or(PublicationStateError::Reconcile)?;
                    after.after = Some(Content {
                        version: source.version.clone(),
                        bytes: source.bytes.clone(),
                    });
                    planned.extend([before, after]);
                }
            }
        }
        Ok(planned)
    }

    fn capture(
        &self,
        path: &SourcePath,
        expected: Option<&str>,
    ) -> Result<PlannedStep, PrepareError> {
        let (parent_fd, name, parent) = open_parent(&self.local.root, path)?;
        let before = observe(&parent_fd, name)?;
        if before.as_ref().map(|file| file.version.sha256.as_str()) != expected {
            return Err(PrepareError::Reject(
                CheckoutPublicationError::ContentConflict,
            ));
        }
        Ok(PlannedStep {
            path: path.clone(),
            parent,
            before,
            after: None,
        })
    }

    fn after_baseline(
        &self,
        files: Vec<SourceEntry>,
        planned: &[PlannedStep],
    ) -> Result<String, PrepareError> {
        let mut files: BTreeMap<_, _> = files
            .into_iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect();
        for step in planned {
            let visible = files.remove(&step.path);
            if let Some(before) = &step.before {
                if !visible.is_some_and(|entry| {
                    entry.sha256 == before.version.sha256
                        && entry.size_bytes == before.version.size_bytes
                        && entry.executable == (before.version.mode & 0o111 != 0)
                }) {
                    return Err(PublicationStateError::Reconcile.into());
                }
            } else if visible.is_some() {
                return Err(PublicationStateError::Reconcile.into());
            }
            if let Some(after) = &step.after {
                files.insert(
                    step.path.clone(),
                    SourceEntry {
                        path: step.path.clone(),
                        sha256: after.version.sha256.clone(),
                        size_bytes: after.version.size_bytes,
                        executable: after.version.mode & 0o111 != 0,
                    },
                );
            }
        }
        let files: Vec<_> = files.into_values().collect();
        let total = files.iter().map(|entry| entry.size_bytes).sum();
        if total > MAX_SOURCE_BYTES || files.len() > MAX_SOURCE_FILES as usize {
            return Err(PrepareError::Reject(CheckoutPublicationError::TooLarge));
        }
        Ok(source_baseline(self.manifest, &files, total)
            .map_err(PublicationStateError::from)?
            .manifest_sha256)
    }

    fn stage(
        &self,
        id: Uuid,
        index: usize,
        content: &Content,
    ) -> Result<(), PublicationStateError> {
        let name = slot_name(id, index);
        let fd = fs::openat(
            &self.local.state,
            name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?;
        let mut file = File::from(fd);
        file.write_all(&content.bytes)?;
        fs::fchmod(&file, Mode::from_bits_truncate(content.version.mode))?;
        file.sync_all()?;
        fs::fsync(&self.local.state)?;
        Ok(())
    }

    fn finish_order(
        &self,
        order: &CheckoutWriteOrder,
        journal: &Journal,
    ) -> Result<CheckoutWriteOutcome, PublicationStateError> {
        if journal.change_id != order.change_id
            || journal.previous != order.previous
            || journal.epoch != order.epoch
        {
            return Err(PublicationStateError::Reconcile);
        }
        if let Some(outcome) = self.receipt(journal)? {
            self.verify_head(&journal.next)?;
            return Ok(outcome);
        }
        // Check every step before finishing any unfinished step. A third value is
        // evidence of a writer outside the gateway and requires operator inspection.
        for (index, step) in journal.steps.iter().enumerate() {
            self.needs_step(journal.change_id, index, step)?;
        }
        for (index, step) in journal.steps.iter().enumerate() {
            self.apply_step(journal.change_id, index, step)?;
        }
        self.verify_head(&journal.next)?;
        let outcome = CheckoutWriteOutcome::Applied {
            head: journal.next.clone(),
            files: journal
                .steps
                .iter()
                .map(|step| CheckoutFileDelta {
                    path: step.path.clone(),
                    before_sha256: step.before.as_ref().map(|file| file.sha256.clone()),
                    after_sha256: step.after.as_ref().map(|file| file.sha256.clone()),
                })
                .collect(),
        };
        write_record(
            &self.local.state,
            &receipt_name(journal.change_id),
            &outcome,
        )?;
        Ok(outcome)
    }

    fn receipt(
        &self,
        journal: &Journal,
    ) -> Result<Option<CheckoutWriteOutcome>, PublicationStateError> {
        let result = read_record(&self.local.state, &receipt_name(journal.change_id))?;
        match &result {
            None => Ok(None),
            Some(CheckoutWriteOutcome::Applied { head, .. }) if head == &journal.next => Ok(result),
            _ => Err(PublicationStateError::Reconcile),
        }
    }

    fn needs_step(
        &self,
        id: Uuid,
        index: usize,
        step: &Step,
    ) -> Result<bool, PublicationStateError> {
        let (parent, name, identity) =
            open_parent(&self.local.root, &step.path).map_err(to_state)?;
        if identity != step.parent {
            return Err(PublicationStateError::Reconcile);
        }
        let current = observe(&parent, name)
            .map_err(to_state)?
            .map(|file| file.version);
        let slot = observe(&self.local.state, &slot_name(id, index))
            .map_err(to_state)?
            .map(|file| file.version);
        if current == step.after && slot == step.before {
            return Ok(false);
        }
        if current == step.before && slot == step.after {
            return Ok(true);
        }
        Err(PublicationStateError::Reconcile)
    }

    fn apply_step(&self, id: Uuid, index: usize, step: &Step) -> Result<(), PublicationStateError> {
        let (parent, name, identity) =
            open_parent(&self.local.root, &step.path).map_err(to_state)?;
        if identity != step.parent {
            return Err(PublicationStateError::Reconcile);
        }
        let slot = slot_name(id, index);
        if self.needs_step(id, index, step)? {
            match (&step.before, &step.after) {
                (None, Some(_)) => fs::renameat_with(
                    &self.local.state,
                    slot.as_str(),
                    &parent,
                    name,
                    RenameFlags::NOREPLACE,
                )?,
                (Some(_), None) => fs::renameat_with(
                    &parent,
                    name,
                    &self.local.state,
                    slot.as_str(),
                    RenameFlags::NOREPLACE,
                )?,
                (Some(_), Some(_)) => fs::renameat_with(
                    &self.local.state,
                    slot.as_str(),
                    &parent,
                    name,
                    RenameFlags::EXCHANGE,
                )?,
                (None, None) => return Err(PublicationStateError::Reconcile),
            }
        }
        fs::fsync(&parent)?;
        fs::fsync(&self.local.state)?;
        if self.needs_step(id, index, step)? {
            return Err(PublicationStateError::Reconcile);
        }
        Ok(())
    }
}

fn mount(fd: &OwnedFd) -> Result<u64, PublicationStateError> {
    let stat = fs::statx(fd, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)?;
    if stat.stx_mask & StatxFlags::MNT_ID.bits() == 0 {
        return Err(PublicationStateError::DifferentMount);
    }
    Ok(stat.stx_mnt_id)
}

fn open_parent<'a>(
    root: &OwnedFd,
    path: &'a SourcePath,
) -> Result<(OwnedFd, &'a str, DirectoryIdentity), PrepareError> {
    let (prefix, name) = path
        .as_str()
        .rsplit_once('/')
        .unwrap_or(("", path.as_str()));
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY;
    let mut directory =
        fs::openat(root, ".", flags, Mode::empty()).map_err(PublicationStateError::from)?;
    let mut stat = fs::fstat(&directory).map_err(PublicationStateError::from)?;
    for component in prefix.split('/').filter(|component| !component.is_empty()) {
        assert_private_writer(stat.st_uid, stat.st_mode)?;
        directory = match fs::openat2(
            &directory,
            component,
            flags,
            Mode::empty(),
            descendant_resolution(),
        ) {
            Ok(child) => child,
            Err(Errno::NOENT) => {
                return Err(PrepareError::Reject(
                    CheckoutPublicationError::ContentConflict,
                ));
            }
            Err(Errno::LOOP | Errno::XDEV | Errno::NOTDIR) => {
                return Err(PrepareError::Reject(CheckoutPublicationError::Forbidden));
            }
            Err(error) => return Err(PublicationStateError::from(error).into()),
        };
        stat = fs::fstat(&directory).map_err(PublicationStateError::from)?;
    }
    assert_private_writer(stat.st_uid, stat.st_mode)?;
    Ok((
        directory,
        name,
        DirectoryIdentity {
            device: stat.st_dev,
            inode: stat.st_ino,
        },
    ))
}

fn assert_private_writer(owner: u32, mode: u32) -> Result<(), PrepareError> {
    if owner != geteuid().as_raw() || mode & 0o022 != 0 {
        return Err(PrepareError::Reject(CheckoutPublicationError::Forbidden));
    }
    Ok(())
}

fn observe(directory: &OwnedFd, name: &str) -> Result<Option<Content>, PrepareError> {
    let (fd, stat, size) = match open_source_file(directory, name) {
        Ok(entry) => entry,
        Err(InspectionError::Path(Errno::NOENT)) => return Ok(None),
        Err(
            InspectionError::Path(Errno::LOOP | Errno::XDEV | Errno::NOTDIR)
            | InspectionError::UnsupportedEntry,
        ) => {
            return Err(PrepareError::Reject(CheckoutPublicationError::Forbidden));
        }
        Err(error) => return Err(PublicationStateError::from(error).into()),
    };
    assert_private_writer(stat.st_uid, stat.st_mode)?;
    if stat.st_mode & 0o7000 != 0 {
        return Err(PrepareError::Reject(CheckoutPublicationError::Forbidden));
    }
    if size > MAX_PUBLICATION_BYTES as u64 {
        return Err(PrepareError::Reject(CheckoutPublicationError::TooLarge));
    }
    let mut bytes = Vec::with_capacity(size as usize);
    copy_checked_source(&fd, &stat, size, &mut bytes).map_err(PublicationStateError::from)?;
    Ok(Some(content(bytes, stat.st_mode & 0o777)))
}

fn content(bytes: Vec<u8>, mode: u32) -> Content {
    let version = FileVersion {
        sha256: hex::encode(Sha256::digest(&bytes)),
        size_bytes: bytes.len() as u64,
        mode,
    };
    Content { bytes, version }
}

fn to_state(error: PrepareError) -> PublicationStateError {
    match error {
        PrepareError::Reject(_) => PublicationStateError::Reconcile,
        PrepareError::State(error) => error,
    }
}

fn rejected(reason: CheckoutPublicationError) -> CheckoutWriteOutcome {
    CheckoutWriteOutcome::Rejected { reason }
}

fn slot_name(id: Uuid, index: usize) -> String {
    format!("publication-{id}-{index}.slot")
}

fn backup_name(id: Uuid, index: usize) -> String {
    format!("publication-{id}-{index}.before")
}

fn history_name(id: Uuid) -> String {
    format!("publication-{id}.history.json")
}

fn fingerprint(version: Option<&FileVersion>) -> Option<(&str, u64, bool)> {
    version.map(|value| {
        (
            value.sha256.as_str(),
            value.size_bytes,
            value.mode & 0o100 != 0,
        )
    })
}

fn receipt_name(id: Uuid) -> String {
    format!("publication-{id}.receipt.json")
}

fn write_record(
    state: &OwnedFd,
    name: &str,
    record: &impl Serialize,
) -> Result<(), PublicationStateError> {
    persist_record(state, name, &serde_json::to_vec(record)?)?;
    Ok(())
}

pub(super) fn read_record<T: DeserializeOwned>(
    state: &OwnedFd,
    name: &str,
) -> Result<Option<T>, PublicationStateError> {
    let fd = match fs::openat(
        state,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    verify_file(&fd)?;
    let mut bytes = Vec::new();
    File::from(fd)
        .take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(PublicationStateError::Reconcile);
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}

pub(super) fn verify_journal(
    local: &CheckoutCoordinator,
    checkout: Uuid,
    state_id: Uuid,
) -> Result<(), PublicationStateError> {
    let owner: JournalOwner =
        read_record(&local.state, STATE_ID)?.ok_or(PublicationStateError::Reconcile)?;
    if owner.checkout_id != checkout
        || owner.state_id != state_id
        || read_record::<Journal>(&local.state, PENDING)?.is_some()
    {
        return Err(PublicationStateError::Reconcile);
    }
    if mount(&local.root)? != mount(&local.state)? {
        return Err(PublicationStateError::DifferentMount);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
