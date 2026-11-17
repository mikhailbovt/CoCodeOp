use std::{
    fs::File,
    io::{self, Seek, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use office_protocol::{CheckoutReleaseOrder, SourceEntry, is_sha256};
use rustix::fs::{CWD, RenameFlags, renameat_with};
use tempfile::TempDir;
use uuid::Uuid;

use super::sealed_snapshot::SealError;

const EMPTY_OID: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const ACCEPTED: &str = "refs/cocodeop/accepted";

pub(super) struct CheckpointRepository {
    path: PathBuf,
}

impl CheckpointRepository {
    pub(super) fn open(state: &Path) -> Result<Self, SealError> {
        let path = state.join("checkpoints.git");
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => (),
            Ok(_) => return Err(SealError::StorageUnavailable),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let temp = TempDir::new_in(state)?;
                let mut init = git();
                init.current_dir(temp.path()).args([
                    "init",
                    "--bare",
                    "--object-format=sha256",
                    "--template=",
                    "--quiet",
                ]);
                output(init, "init")?;
                File::open(temp.path())?.sync_all()?;
                renameat_with(CWD, temp.path(), CWD, &path, RenameFlags::NOREPLACE)?;
                File::open(state)?.sync_all()?;
            }
            Err(error) => return Err(error.into()),
        }
        let repository = Self { path };
        let mut inspect = repository.command();
        inspect.args(["rev-parse", "--show-object-format", "--is-bare-repository"]);
        if output(inspect, "rev-parse")? != "sha256\ntrue\n" {
            return Err(SealError::StorageUnavailable);
        }
        Ok(repository)
    }

    pub(super) fn write_tree(
        &self,
        staging: &Path,
        index: &Path,
        entries: &[SourceEntry],
    ) -> Result<String, SealError> {
        let mut paths =
            tempfile::tempfile_in(staging.parent().ok_or(SealError::StorageUnavailable)?)?;
        for file in entries {
            writeln!(paths, "./{}", file.path.as_str())?;
        }
        paths.rewind()?;
        let mut blobs = self.command();
        blobs.current_dir(staging).stdin(Stdio::from(paths)).args([
            "hash-object",
            "-w",
            "--no-filters",
            "--stdin-paths",
        ]);
        let output_blobs = output(blobs, "hash-object")?;
        let ids: Vec<_> = output_blobs.lines().collect();
        if ids.len() != entries.len() || ids.iter().any(|id| !is_sha256(id)) {
            return Err(SealError::StorageUnavailable);
        }
        let mut entries_input =
            tempfile::tempfile_in(staging.parent().ok_or(SealError::StorageUnavailable)?)?;
        for (file, id) in entries.iter().zip(ids) {
            let mode = if file.executable { "100755" } else { "100644" };
            write!(entries_input, "{mode} {id}\t{}\0", file.path.as_str())?;
        }
        entries_input.rewind()?;
        let mut update = self.command();
        update
            .env("GIT_INDEX_FILE", index)
            .stdin(Stdio::from(entries_input))
            .args(["update-index", "-z", "--index-info"]);
        output(update, "update-index")?;
        let mut write = self.command();
        write.env("GIT_INDEX_FILE", index).arg("write-tree");
        oid(write, "write-tree")
    }

    pub(super) fn commit_snapshot(&self, snapshot: Uuid, tree: &str) -> Result<String, SealError> {
        let mut commit = self.command();
        commit
            .args([
                "commit-tree",
                tree,
                "-m",
                &format!("CoCodeOp WIP snapshot {snapshot}"),
            ])
            .env("GIT_AUTHOR_DATE", "@1 +0000")
            .env("GIT_COMMITTER_DATE", "@1 +0000");
        oid(commit, "commit-tree")
    }

    pub(super) fn commit_release(&self, order: &CheckoutReleaseOrder) -> Result<String, SealError> {
        let mut commit = self.command();
        let message = format!(
            "CoCodeOp accepted checkpoint {}\n\nSnapshot {}\nAuthorization SHA-256 {}",
            order.release_id, order.snapshot_id, order.authorization_sha256
        );
        commit
            .args(["commit-tree", &order.tree_sha256, "-m", &message])
            .env("GIT_AUTHOR_DATE", "@1 +0000")
            .env("GIT_COMMITTER_DATE", "@1 +0000");
        if let Some(parent) = &order.expected_commit_sha256 {
            commit.args(["-p", parent]);
        }
        oid(commit, "commit-tree")
    }

    pub(super) fn accepted_ref(&self) -> Result<Option<String>, SealError> {
        let mut read = self.command();
        read.args(["rev-parse", "--verify", "--quiet", ACCEPTED]);
        let result = read.output().map_err(|_| SealError::Git("read-accepted"))?;
        match result.status.code() {
            Some(0) => {
                let digest = String::from_utf8(result.stdout)
                    .map_err(|_| SealError::Git("read-accepted"))?;
                let digest = digest.trim();
                if !is_sha256(digest) {
                    return Err(SealError::Git("read-accepted"));
                }
                Ok(Some(digest.into()))
            }
            Some(1) => Ok(None),
            _ => Err(SealError::Git("read-accepted")),
        }
    }

    pub(super) fn advance_accepted(
        &self,
        commit: &str,
        expected: Option<&str>,
    ) -> Result<bool, SealError> {
        let mut update = self.command();
        update.args([
            "update-ref",
            "--no-deref",
            ACCEPTED,
            commit,
            expected.unwrap_or(EMPTY_OID),
        ]);
        let updated = output(update, "update-accepted");
        let observed = self.accepted_ref()?;
        if observed.as_deref() == Some(commit) {
            return Ok(true);
        }
        if observed.as_deref() != expected {
            return Ok(false);
        }
        updated?;
        Err(SealError::Git("verify-accepted"))
    }

    pub(super) fn record_snapshot(
        &self,
        snapshot: Uuid,
        commit: &str,
        tree: &str,
    ) -> Result<(), SealError> {
        let mut inspect = self.command();
        inspect.args(["rev-parse", "--verify", &format!("{commit}^{{tree}}")]);
        if oid(inspect, "verify-tree")? != tree {
            return Err(SealError::StorageUnavailable);
        }
        let reference = format!("refs/cocodeop/wip/{snapshot}");
        let mut update = self.command();
        update.args(["update-ref", "--no-deref", &reference, commit, EMPTY_OID]);
        if output(update, "update-ref").is_err() {
            let mut read = self.command();
            read.args(["rev-parse", "--verify", &reference]);
            if oid(read, "verify-ref")? != commit {
                return Err(SealError::StorageUnavailable);
            }
        }
        Ok(())
    }

    fn command(&self) -> Command {
        let mut command = git();
        command.current_dir(&self.path).env("GIT_DIR", &self.path);
        command
    }
}

fn git() -> Command {
    let mut command = Command::new("/usr/bin/git");
    command
        .env_clear()
        .env("LANG", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "CoCodeOp")
        .env("GIT_AUTHOR_EMAIL", "checkpoint@cocodeop.local")
        .env("GIT_COMMITTER_NAME", "CoCodeOp")
        .env("GIT_COMMITTER_EMAIL", "checkpoint@cocodeop.local")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsync=all",
            "-c",
            "gc.auto=0",
        ]);
    command
}

fn output(mut command: Command, operation: &'static str) -> Result<String, SealError> {
    let result = command.output().map_err(|_| SealError::Git(operation))?;
    if !result.status.success() {
        return Err(SealError::Git(operation));
    }
    String::from_utf8(result.stdout).map_err(|_| SealError::Git(operation))
}

fn oid(command: Command, operation: &'static str) -> Result<String, SealError> {
    let result = output(command, operation)?;
    let digest = result.trim();
    if !is_sha256(digest) {
        return Err(SealError::Git(operation));
    }
    Ok(digest.into())
}
