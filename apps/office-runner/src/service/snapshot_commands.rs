use std::{
    fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use office_checkout::{CheckoutCoordinator, command_files, stage_command_copy};
use office_domain::SourceManifest;
use office_protocol::{
    CheckoutCommandOrder, CheckoutCommandOutcome, CommandEnvironment, CommandSnapshot,
    CommandTemplateReceipt, CommandTemplateSpec, MAX_COMMAND_LOG_BYTES, is_sha256,
};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::{
    config::Manifest,
    journal::{Journal, RunFiles, read_config},
    oci::{CommandMount, Container, Podman, RunSockets},
};
use crate::transport::print_json;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandConfig {
    templates: Vec<CommandImage>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandImage {
    template_id: Uuid,
    spec_sha256: String,
    image_id: String,
    entrypoint: String,
    memory_mib: u16,
    cpus: u8,
    pids_limit: u16,
}

impl CommandImage {
    fn manifest(&self, args: Vec<String>, timeout: u16) -> Manifest {
        Manifest {
            adapter_id: "checkout-command.v1".into(),
            image_id: self.image_id.clone(),
            entrypoint: self.entrypoint.clone(),
            args,
            timeout_seconds: timeout.max(5),
            memory_mib: self.memory_mib,
            cpus: self.cpus,
            pids_limit: self.pids_limit,
            run_mcp: false,
            checkout_gateway: false,
            provider_profile_sha256: None,
        }
    }

    fn matches(&self, order: &CheckoutCommandOrder) -> bool {
        self.template_id == order.template_id
            && Path::new(&self.entrypoint)
                .file_name()
                .is_some_and(|name| name == order.spec.executable.as_str())
            && order
                .spec
                .fingerprint()
                .is_ok_and(|digest| digest == self.spec_sha256)
    }
}

pub(crate) fn describe(file: &Path, json: bool) -> Result<(), String> {
    let receipt: CommandTemplateReceipt = read_config(file)?;
    let digest = receipt
        .template
        .fingerprint()
        .map_err(|_| "Could not encode the command template")?;
    if json {
        print_json(
            &json!({"template_id": receipt.template_id, "spec_sha256": digest,
            "executable": receipt.template.executable}),
        )
    } else {
        println!("{digest}");
        Ok(())
    }
}

pub(crate) struct CommandHost {
    config: CommandConfig,
    journal: Journal,
    podman: Podman,
    active: Option<Active>,
}

pub(crate) struct PreparedCommand {
    pub id: Uuid,
    pub proof: CommandSnapshot,
    files: RunFiles,
    snapshot: PathBuf,
    manifest: Manifest,
    timeout: Duration,
    spec: CommandTemplateSpec,
}

struct Active {
    id: Uuid,
    container_id: String,
    proof: CommandSnapshot,
    until: Instant,
    spec: CommandTemplateSpec,
}

impl CommandHost {
    pub async fn open(state: &Path, config: Option<&Path>) -> Result<Self, String> {
        let config: CommandConfig = match config {
            Some(file) => read_config(file)?,
            None => CommandConfig {
                templates: Vec::new(),
            },
        };
        if config.templates.len() > 32 {
            return Err("Configure at most 32 command templates on each checkout host".into());
        }
        for (index, template) in config.templates.iter().enumerate() {
            template.manifest(Vec::new(), 5).validate()?;
            if !is_sha256(&template.spec_sha256)
                || config.templates[..index]
                    .iter()
                    .any(|prior| prior.template_id == template.template_id)
            {
                return Err(
                    "Each command template requires one unique ID and SHA-256 of the reviewed spec"
                        .into(),
                );
            }
        }
        let path = state.join("commands");
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {
                fs::File::open(state)
                    .and_then(|dir| dir.sync_all())
                    .map_err(|_| "Could not sync the checkout command state directory")?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(_) => return Err("Could not create the checkout command state directory".into()),
        }
        let journal = Journal::open(&path)?;
        let podman = Podman::local(journal.root()).await?;
        Ok(Self {
            config,
            journal,
            podman,
            active: None,
        })
    }

    pub fn prepare(
        &self,
        local: &CheckoutCoordinator,
        state_dir: &Path,
        selected: &SourceManifest,
        order: &CheckoutCommandOrder,
    ) -> Result<Result<PreparedCommand, CheckoutCommandOutcome>, String> {
        let Some(config) = self
            .config
            .templates
            .iter()
            .find(|config| config.matches(order))
        else {
            return Ok(Err(CheckoutCommandOutcome::NotConfigured));
        };
        let timeout: u16 = order
            .spec
            .timeout_seconds
            .try_into()
            .map_err(|_| "The command template timeout is invalid")?;
        let manifest = config.manifest(order.argv.clone(), timeout);
        manifest.validate()?;
        let files = self.journal.prepare_command(
            order.command_id,
            &json!({
                "command_id": order.command_id, "template_id": order.template_id,
                "checkout_revision": order.head.checkout_revision,
            }),
        )?;
        let snapshot = files.task.with_file_name("snapshot");
        files.prepare_command_resources(&order.spec.resources)?;
        let mut proof = match local.command_snapshot(state_dir, selected, order, &snapshot) {
            Ok(proof) => proof,
            Err(_) => return Ok(Err(CheckoutCommandOutcome::SnapshotUnavailable)),
        };
        proof.environment = Some(CommandEnvironment {
            image_id: manifest.image_id.clone(),
            template_sha256: config.spec_sha256.clone(),
        });
        if order.spec.writable_staging
            && stage_command_copy(&snapshot, &files.scratch.join("source"), &order.spec).is_err()
        {
            return Ok(Err(CheckoutCommandOutcome::SnapshotUnavailable));
        }
        Ok(Ok(PreparedCommand {
            id: order.command_id,
            proof,
            files,
            snapshot,
            manifest,
            timeout: Duration::from_secs(u64::from(timeout)),
            spec: order.spec.clone(),
        }))
    }

    pub async fn start(
        &mut self,
        command: PreparedCommand,
        deadline: Instant,
    ) -> Result<Option<CheckoutCommandOutcome>, String> {
        let container = self
            .podman
            .create(
                command.id,
                &command.files.scratch,
                &command.files.task,
                &command.manifest,
                RunSockets::default(),
                Some(CommandMount {
                    snapshot: &command.snapshot,
                    writable_staging: command.spec.writable_staging,
                    resources: &command.spec.resources,
                }),
            )
            .await?;
        if Instant::now() >= deadline {
            self.podman.remove(&container).await?;
            return Ok(Some(CheckoutCommandOutcome::Interrupted));
        }
        self.podman.start(&container).await?;
        self.active = Some(Active {
            id: command.id,
            container_id: container.id,
            proof: command.proof,
            until: Instant::now() + command.timeout,
            spec: command.spec,
        });
        Ok(None)
    }

    pub fn active_command(&self) -> Option<Uuid> {
        self.active.as_ref().map(|active| active.id)
    }

    pub async fn tick(&mut self) -> Result<Option<(Uuid, CheckoutCommandOutcome)>, String> {
        let Some(active) = &self.active else {
            return Ok(None);
        };
        let mut container = self.podman.require(active.id).await?;
        if container.id != active.container_id {
            return Err("The running command does not match its authorized OCI container".into());
        }
        let timed_out = Instant::now() >= active.until;
        if container.state.running {
            if !timed_out {
                return Ok(None);
            }
            self.podman.stop(&container).await?;
            container = self.podman.require(active.id).await?;
            if container.id != active.container_id || !container.state.stopped() {
                return Err("The host has not verified that the timed-out command stopped".into());
            }
            let result = CheckoutCommandOutcome::Timeout {
                snapshot: active.proof.clone(),
            };
            return self.save_active(active.id, result);
        }
        if !container.state.stopped() {
            return Err("Podman did not confirm a terminal command state".into());
        }
        let result = if timed_out && container.state.exit_code == -1 {
            CheckoutCommandOutcome::Timeout {
                snapshot: active.proof.clone(),
            }
        } else {
            self.exited(&container, active.proof.clone(), active.id, &active.spec)
                .await?
        };
        self.save_active(active.id, result)
    }

    fn save_active(
        &mut self,
        id: Uuid,
        outcome: CheckoutCommandOutcome,
    ) -> Result<Option<(Uuid, CheckoutCommandOutcome)>, String> {
        self.journal.persist_command_result(id, &outcome)?;
        self.active = None;
        Ok(Some((id, outcome)))
    }

    pub async fn recover(
        &self,
        order: &CheckoutCommandOrder,
    ) -> Result<CheckoutCommandOutcome, String> {
        let restored = self.journal.command_result(order.command_id)?;
        let outcome = match self.podman.inspect(order.command_id).await? {
            None => restored.unwrap_or(CheckoutCommandOutcome::Interrupted),
            Some(container) if container.state.running => {
                self.podman.stop(&container).await?;
                let stopped = self.podman.require(order.command_id).await?;
                if stopped.id != container.id || !stopped.state.stopped() {
                    return Err("The checkout command has not reached a verified stop".into());
                }
                restored.unwrap_or(CheckoutCommandOutcome::Interrupted)
            }
            Some(container) if container.state.stopped() && container.state.started() => {
                match (restored, order.previous_snapshot.clone()) {
                    (Some(receipt), _) => receipt,
                    (None, Some(snapshot)) => {
                        self.exited(&container, snapshot, order.command_id, &order.spec)
                            .await?
                    }
                    (None, None) => CheckoutCommandOutcome::Unknown,
                }
            }
            Some(container) if !container.state.started() => {
                self.podman.remove(&container).await?;
                CheckoutCommandOutcome::Interrupted
            }
            Some(_) => return Err("The previous checkout command cannot be reconciled yet".into()),
        };
        self.journal
            .persist_command_result(order.command_id, &outcome)?;
        Ok(outcome)
    }

    pub async fn stop(&mut self) -> Result<Option<(Uuid, CheckoutCommandOutcome)>, String> {
        let Some(active) = &self.active else {
            return Ok(None);
        };
        let container = self.podman.require(active.id).await?;
        if container.id != active.container_id {
            return Err("The active command has an unexpected container identity".into());
        }
        if container.state.running {
            self.podman.stop(&container).await?;
            let stopped = self.podman.require(active.id).await?;
            if stopped.id != container.id || !stopped.state.stopped() {
                return Err("The checkout command did not reach a verified stop".into());
            }
            return self.save_active(active.id, CheckoutCommandOutcome::Interrupted);
        }
        if !container.state.stopped() {
            return Err("The checkout command could not be reconciled during shutdown".into());
        }
        let result = self
            .exited(&container, active.proof.clone(), active.id, &active.spec)
            .await?;
        self.save_active(active.id, result)
    }

    pub async fn retire(&self, id: Uuid) -> Result<(), String> {
        if let Some(container) = self.podman.inspect(id).await? {
            if !container.state.stopped() {
                return Err(
                    "The completed checkout command still has an active OCI container".into(),
                );
            }
            self.podman.remove(&container).await?;
        }
        let path = self.journal.root().join(format!("command-{id}"));
        match fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err("Could not retire the acknowledged command snapshot".into()),
        }
    }

    async fn exited(
        &self,
        container: &Container,
        snapshot: CommandSnapshot,
        id: Uuid,
        spec: &CommandTemplateSpec,
    ) -> Result<CheckoutCommandOutcome, String> {
        let Ok(exit_code) = u8::try_from(container.state.exit_code) else {
            return Ok(CheckoutCommandOutcome::Unknown);
        };
        let output = self.podman.logs(container).await?;
        let (stdout, stdout_cut) = log_text(&output.stdout);
        let (stderr, stderr_cut) = log_text(&output.stderr);
        let location = self.journal.root().join(format!("command-{id}"));
        let files = command_files(spec, &location.join("snapshot"), &location.join("scratch"));
        Ok(CheckoutCommandOutcome::Exited {
            snapshot,
            exit_code: i32::from(exit_code),
            stdout,
            stderr,
            log_truncated: stdout_cut || stderr_cut,
            outputs: files.outputs,
            staged_changes: files.staged_changes,
            files_truncated: files.truncated,
        })
    }
}

fn log_text(bytes: &[u8]) -> (String, bool) {
    let limit = MAX_COMMAND_LOG_BYTES / 2;
    let mut text = String::from_utf8_lossy(&bytes[..bytes.len().min(limit)]).into_owned();
    let cut = bytes.len() > limit || text.len() > limit;
    text.truncate(text.floor_char_boundary(text.len().min(limit)));
    (text, cut)
}
