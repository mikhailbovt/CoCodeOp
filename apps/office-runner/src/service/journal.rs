use std::{
    fs::File,
    io::{Read, Write},
    os::fd::{AsRawFd, OwnedFd},
    path::{Path, PathBuf},
};

use office_protocol::{
    BackendEvidence, BackendState, CheckoutCommandOutcome, CommandResources, CompleteExecution,
    ExecutionLease, ExecutionStartReceipt, ExecutionTaskBrief, GENERIC_JSONL_ADAPTER,
    MAX_BACKEND_TRACE_BYTES, ProcessResult, ReconcileExecutionStart,
};
use rustix::{
    fs::{self, FileType, FlockOperation, Mode, OFlags},
    io::Errno,
    process::geteuid,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const MAX_JSON_BYTES: u64 = 192 * 1024;
const MAX_CHECKPOINT_BYTES: u64 = 512 * 1024;
const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const SAFE_FILE: OFlags = OFlags::NOFOLLOW
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK);
const PRIVATE_FILE: Mode = Mode::RUSR.union(Mode::WUSR);

pub(super) struct Journal {
    root: PathBuf,
    directory: OwnedFd,
    _lock: OwnedFd,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pending {
    pub claim_key: Uuid,
    pub assigned: Option<ExecutionLease>,
    pub grant: Option<ExecutionStartReceipt>,
    pub start_attempted: bool,
    pub completion: Option<CompleteExecution>,
    pub reconciliation: Option<ReconcileExecutionStart>,
}

impl Default for Pending {
    fn default() -> Self {
        Self {
            claim_key: Uuid::new_v4(),
            assigned: None,
            grant: None,
            start_attempted: false,
            completion: None,
            reconciliation: None,
        }
    }
}

pub(super) struct RunFiles {
    pub scratch: PathBuf,
    pub task: PathBuf,
    scratch_fd: OwnedFd,
    run_fd: OwnedFd,
}

impl RunFiles {
    pub fn prepare_command_resources(&self, resources: &CommandResources) -> Result<(), String> {
        for (enabled, directory) in [
            (resources.build_cache, "build-cache"),
            (resources.package_cache, "package-cache"),
            (resources.test_database, "test-db"),
        ] {
            if enabled {
                child_directory(&self.scratch_fd, directory)?;
            }
        }
        Ok(())
    }

    pub fn socket_mount(&self) -> PathBuf {
        self.task.with_file_name("mcp.sock")
    }

    pub fn socket_bind(&self) -> PathBuf {
        self.bind_socket("mcp.sock")
    }

    pub fn inference_mount(&self) -> PathBuf {
        self.task.with_file_name("inference.sock")
    }

    pub fn inference_bind(&self) -> PathBuf {
        self.bind_socket("inference.sock")
    }

    fn bind_socket(&self, file: &str) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}/{file}", self.run_fd.as_raw_fd()))
    }

    pub fn result(&self) -> Result<ProcessResult, String> {
        let fd = fs::openat(
            &self.scratch_fd,
            "result.json",
            OFlags::RDONLY | SAFE_FILE,
            Mode::empty(),
        )
        .map_err(|_| "The worker did not produce a regular result.json")?;
        validate_file(&fd, false)?;
        read_json(fd)
    }

    pub fn backend_evidence(&self, adapter: &str) -> Result<BackendEvidence, String> {
        let dir = fs::openat(&self.scratch_fd, "agent", DIRECTORY, Mode::empty())
            .map_err(|_| "The worker did not produce its agent evidence directory")?;
        validate_directory(&dir)?;
        let open = |name| {
            let fd = fs::openat(&dir, name, OFlags::RDONLY | SAFE_FILE, Mode::empty())
                .map_err(|_| "The worker omitted a regular backend evidence file")?;
            validate_file(&fd, true)?;
            Ok::<_, String>(fd)
        };
        let evidence: BackendEvidence = read_json(open("evidence.json")?)?;
        let mut log = Vec::new();
        File::from(open("events.jsonl")?)
            .take(MAX_BACKEND_TRACE_BYTES as u64 + 1)
            .read_to_end(&mut log)
            .map_err(|_| "Could not read the stopped worker's event log")?;
        if !evidence.valid()
            || evidence.backend != adapter
            || log.len() > MAX_BACKEND_TRACE_BYTES
            || log.last() != Some(&b'\n')
            || log.iter().filter(|byte| **byte == b'\n').count() != evidence.event_count as usize
            || hex::encode(Sha256::digest(log)) != evidence.events_sha256
        {
            return Err(
                "The stopped worker's event log does not match its backend evidence".into(),
            );
        }
        if evidence.terminal_state == BackendState::InputRequired {
            let checkpoint = if adapter == GENERIC_JSONL_ADAPTER {
                let fd = fs::openat(
                    &self.scratch_fd,
                    "checkpoint.json",
                    OFlags::RDONLY | SAFE_FILE,
                    Mode::empty(),
                )
                .map_err(|_| "The bridge omitted its pause checkpoint file")?;
                validate_file(&fd, true)?;
                fd
            } else {
                open("checkpoint.json")?
            };
            let mut bytes = Vec::new();
            File::from(checkpoint)
                .take(MAX_CHECKPOINT_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| "Could not read the pause checkpoint file")?;
            let actual = format!("sha256:{}", hex::encode(Sha256::digest(&bytes)));
            if bytes.is_empty()
                || bytes.len() as u64 > MAX_CHECKPOINT_BYTES
                || evidence.checkpoint_id.as_deref() != Some(actual.as_str())
            {
                return Err(
                    "The paused backend's checkpoint file does not match its evidence".into(),
                );
            }
        }
        Ok(evidence)
    }
}

impl Journal {
    pub fn open(path: &Path) -> Result<Self, String> {
        let directory = fs::open(path, DIRECTORY, Mode::empty())
            .map_err(|_| "Create a dedicated owner-only runner state directory first")?;
        validate_directory(&directory)?;
        let root = path
            .canonicalize()
            .map_err(|_| "Could not resolve the runner state directory")?;
        let lock = fs::openat(
            &directory,
            "controller.lock",
            OFlags::RDWR | OFlags::CREATE | SAFE_FILE,
            PRIVATE_FILE,
        )
        .map_err(|_| "Could not open the controller lock")?;
        validate_file(&lock, true)?;
        fs::flock(&lock, FlockOperation::NonBlockingLockExclusive)
            .map_err(|_| "Another controller is using this runner state directory")?;
        Ok(Self {
            root,
            directory,
            _lock: lock,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn load(&self) -> Result<Option<Pending>, String> {
        match fs::openat(
            &self.directory,
            "current.json",
            OFlags::RDONLY | SAFE_FILE,
            Mode::empty(),
        ) {
            Ok(fd) => {
                validate_file(&fd, true)?;
                read_json(fd).map(Some)
            }
            Err(Errno::NOENT) => Ok(None),
            Err(_) => Err("Could not open the controller journal".into()),
        }
    }

    pub fn save(&self, state: &Pending) -> Result<(), String> {
        write_json(&self.directory, "current.json", state)
    }

    pub fn clear(&self) -> Result<(), String> {
        fs::unlinkat(&self.directory, "current.json", fs::AtFlags::empty())
            .and_then(|_| fs::fsync(&self.directory))
            .map_err(|_| "Could not retire the controller journal".into())
    }

    pub fn prepare_run(&self, brief: &ExecutionTaskBrief, run: Uuid) -> Result<RunFiles, String> {
        self.prepare_files(brief, &format!("run-{run}"))
    }

    pub fn prepare_command(
        &self,
        id: Uuid,
        descriptor: &impl Serialize,
    ) -> Result<RunFiles, String> {
        self.prepare_files(descriptor, &format!("command-{id}"))
    }

    pub fn command_result(&self, id: Uuid) -> Result<Option<CheckoutCommandOutcome>, String> {
        let name = format!("command-{id}.json");
        let fd = match fs::openat(
            &self.directory,
            name.as_str(),
            OFlags::RDONLY | SAFE_FILE,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(_) => return Err("Could not open the local command receipt".into()),
        };
        validate_file(&fd, true)?;
        read_json(fd).map(Some)
    }

    pub fn persist_command_result(
        &self,
        id: Uuid,
        outcome: &CheckoutCommandOutcome,
    ) -> Result<(), String> {
        write_json(&self.directory, &format!("command-{id}.json"), outcome)
    }

    fn prepare_files(&self, descriptor: &impl Serialize, run: &str) -> Result<RunFiles, String> {
        let run_fd = child_directory(&self.directory, run)?;
        let scratch_fd = child_directory(&run_fd, "scratch")?;
        write_json(&run_fd, "task.json", descriptor)?;
        let base = self.root.join(run);
        Ok(RunFiles {
            task: base.join("task.json"),
            scratch: base.join("scratch"),
            scratch_fd,
            run_fd,
        })
    }
}

pub(super) fn read_config<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let fd = fs::open(path, OFlags::RDONLY | SAFE_FILE, Mode::empty())
        .map_err(|_| "Could not open the local runtime configuration")?;
    validate_file(&fd, false)?;
    read_json(fd)
}

fn child_directory(parent: &OwnedFd, name: &str) -> Result<OwnedFd, String> {
    match fs::mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
        Ok(()) => fs::fsync(parent).map_err(|_| "Could not sync the runner directory")?,
        Err(Errno::EXIST) => (),
        Err(_) => return Err("Could not create the per-run directory".into()),
    }
    let fd = fs::openat(parent, name, DIRECTORY, Mode::empty())
        .map_err(|_| "Could not open the per-run directory")?;
    validate_directory(&fd)?;
    Ok(fd)
}

fn validate_directory(fd: &OwnedFd) -> Result<(), String> {
    let stat = fs::fstat(fd).map_err(|_| "Could not inspect the runner directory")?;
    if stat.st_uid != geteuid().as_raw() || stat.st_mode & 0o077 != 0 {
        return Err("Runner state and scratch directories must be private to their owner".into());
    }
    Ok(())
}

fn validate_file(fd: &OwnedFd, private: bool) -> Result<(), String> {
    let stat = fs::fstat(fd).map_err(|_| "Could not inspect the runner file")?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_nlink != 1
        || stat.st_uid != geteuid().as_raw()
        || stat.st_mode & 0o022 != 0
        || (private && stat.st_mode & 0o077 != 0)
    {
        return Err(
            "The runner file must be an owner-controlled regular file without hard links".into(),
        );
    }
    Ok(())
}

fn read_json<T: DeserializeOwned>(fd: OwnedFd) -> Result<T, String> {
    let mut bytes = Vec::new();
    File::from(fd)
        .take(MAX_JSON_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read the runner file")?;
    if bytes.len() as u64 > MAX_JSON_BYTES {
        return Err("Runner JSON exceeds the configured size limit".into());
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| "The runner file does not match its JSON contract".into())
}

fn write_json(directory: &OwnedFd, name: &str, value: &impl Serialize) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|_| "Could not encode runner state")?;
    if bytes.len() as u64 > MAX_JSON_BYTES {
        return Err("Runner journal exceeds the configured size limit".into());
    }
    let staging = format!(".{}.partial", Uuid::new_v4());
    let fd = fs::openat(
        directory,
        staging.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | SAFE_FILE,
        PRIVATE_FILE,
    )
    .map_err(|_| "Could not stage runner state")?;
    let result = (|| {
        let mut file = File::from(fd);
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| "Could not sync the staged runner file")?;
        fs::renameat(directory, staging.as_str(), directory, name)
            .and_then(|_| fs::fsync(directory))
            .map_err(|_| "Could not publish the staged runner file")
    })();
    if result.is_err() {
        let _ = fs::unlinkat(directory, staging.as_str(), fs::AtFlags::empty());
    }
    result.map_err(str::to_owned)
}
