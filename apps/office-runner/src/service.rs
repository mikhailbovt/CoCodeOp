use std::{
    path::Path,
    time::{Duration, Instant},
};

use office_domain::{BearerDigest, ErrorCode, RunStatus};
use office_protocol::{
    AdvertiseRunner, AuthorizeExecutionStart, ClaimExecution, CompleteExecution,
    ExecutionCompletionReceipt, ExecutionLease, ExecutionStartGrant, ExecutionStartReceipt,
    IssueRunCredential, ObserveExecutionStart, ProcessResult, ReconcileExecutionStart,
    RenewExecutionLease, Run, RunCredentialReceipt, RunnerProfile, StartEffectRef,
    StartEffectState, StopObservation, packaged_backend,
};
use reqwest::{Client, Url};
use serde::{Serialize, de::DeserializeOwned};
use tokio::{
    signal::unix::{SignalKind, signal},
    time::sleep,
};
use uuid::Uuid;

use super::{
    RunnerConnection, ServeOptions,
    inference::{ProviderPricing, ProviderProfile, ProviderSocket},
    transport::{
        ControlError, client, confidential_url, endpoint, print_json, read_token, request_typed,
    },
    worker_mcp::WorkerSocket,
};
use config::{Config, Manifest};
use journal::{Journal, Pending, RunFiles, read_config};
use oci::{Container, InferenceMount, Podman, RunSockets};

mod config;
mod doctor;
mod journal;
mod oci;
pub(super) mod snapshot_commands;
pub(super) use doctor::probe_checkout_isolation;
pub(super) use doctor::run as doctor;

const PROFILE_TTL: u16 = 90;
const PROFILE_REFRESH: Duration = Duration::from_secs(30);
const HEARTBEAT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(800);

pub(super) fn describe_manifest(file: &Path, json: bool) -> Result<(), String> {
    let manifest: Manifest = read_config(file)?;
    manifest.validate()?;
    let digest = manifest.digest()?;
    if json {
        print_json(&serde_json::json!({"adapter_id":manifest.adapter_id,"manifest_sha256":digest}))
    } else {
        println!("{digest}");
        Ok(())
    }
}

pub(super) fn describe_inference(file: &Path, json: bool) -> Result<(), String> {
    let profile: ProviderProfile = read_config(file)?;
    let digest = profile.digest()?;
    if json {
        print_json(&serde_json::json!({
            "name":profile.name, "model":profile.model, "api":profile.api.name(),
            "provider_profile_sha256":digest,
        }))
    } else {
        println!("{digest}");
        Ok(())
    }
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Report {
    Idle,
    LeaseExpired,
    Completed { receipt: ExecutionCompletionReceipt },
    Reconciled { receipt: ExecutionStartReceipt },
}

impl Report {
    fn print(&self, json: bool) -> Result<(), String> {
        if json {
            return print_json(self);
        }
        match self {
            Self::Idle => println!("No compatible execution is ready"),
            Self::LeaseExpired => println!("The lease ended before authorization"),
            Self::Completed { receipt } => println!(
                "Run {} {:?}",
                receipt.execution.run.run_id.0, receipt.execution.run.status
            ),
            Self::Reconciled { receipt } => {
                println!("Run {} {:?}", receipt.run.run_id.0, receipt.run.status)
            }
        }
        Ok(())
    }
}

struct Coordinator {
    client: Client,
    base: Url,
    token: String,
}

impl Coordinator {
    fn local(options: &RunnerConnection) -> Result<Self, String> {
        Ok(Self {
            client: client()?,
            base: confidential_url(&options.url)?,
            token: read_token(&options.token_file)?,
        })
    }

    async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        input: &impl Serialize,
    ) -> Result<T, ControlError> {
        let url = endpoint(&self.base, path)
            .map_err(|_| ControlError::Transport("Invalid scheduler endpoint"))?;
        request_typed(self.client.post(url).json(input), &self.token).await
    }

    async fn profile(&self) -> Result<Option<RunnerProfile>, String> {
        let url = endpoint(&self.base, "v1/runner-scheduler/profile")?;
        request_typed(self.client.get(url), &self.token)
            .await
            .map_err(|error| error.to_string())
    }
}

struct Controller {
    config: Config,
    journal: Journal,
    podman: Podman,
    office: Coordinator,
    last_profile: Option<(RunnerProfile, Instant)>,
}

#[tokio::main]
pub(super) async fn run(options: ServeOptions, json: bool) -> Result<(), String> {
    let config: Config = read_config(&options.file)?;
    config.validate()?;
    let journal = Journal::open(&options.state_dir)?;
    let podman = Podman::local(journal.root()).await?;
    let office = Coordinator::local(&options.connection)?;
    let mut controller = Controller {
        config,
        journal,
        podman,
        office,
        last_profile: None,
    };
    let mut term =
        signal(SignalKind::terminate()).map_err(|_| "Could not subscribe to shutdown signals")?;
    let shutdown = async {
        tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
    };
    tokio::pin!(shutdown);
    let mut last_error = None;
    loop {
        let result = tokio::select! {
            _ = &mut shutdown => {
                return controller.shutdown(json).await;
            }
            result = controller.cycle() => result,
        };
        match result {
            Ok(report) => {
                if options.once || !matches!(report, Report::Idle) {
                    report.print(json)?;
                }
                last_error = None;
                if options.once {
                    return Ok(());
                }
            }
            Err(error) => {
                if last_error.as_ref() != Some(&error) {
                    eprintln!("{error}");
                    last_error = Some(error.clone());
                }
                if let Some(mut pending) = controller.journal.load()?
                    && pending.grant.is_some()
                {
                    match controller.resume(&mut pending).await {
                        Ok(report) => {
                            report.print(json)?;
                            last_error = None;
                            if options.once {
                                return Ok(());
                            }
                        }
                        Err(recovery) => {
                            if options.once {
                                return Err(format!("{error}. Recovery is pending. {recovery}"));
                            }
                        }
                    }
                } else if options.once {
                    return Err(error);
                }
            }
        }
        tokio::select! { _ = &mut shutdown => return controller.shutdown(json).await, _ = sleep(POLL) => {} }
    }
}

impl Controller {
    async fn shutdown(&self, json: bool) -> Result<(), String> {
        if let Some(mut pending) = self.journal.load()?
            && pending.grant.is_some()
        {
            self.resume(&mut pending).await?.print(json)?;
        }
        Ok(())
    }

    async fn refresh_profile(&mut self) -> Result<(), String> {
        let previous = match &self.last_profile {
            Some((_, instant)) if instant.elapsed() < PROFILE_REFRESH => return Ok(()),
            Some((profile, _)) => profile.revision,
            None => self
                .office
                .profile()
                .await?
                .map_or(0, |profile| profile.revision),
        };
        let input = AdvertiseRunner {
            expected_revision: previous,
            capacity: 1,
            runtime_template_ids: self
                .config
                .runtimes
                .iter()
                .map(|runtime| runtime.template_id)
                .collect(),
            hosted_checkout_ids: Vec::new(),
            capabilities: self.config.capabilities.clone(),
            ttl_seconds: PROFILE_TTL,
        };
        let result = self
            .office
            .post("v1/runner-scheduler/profile", &input)
            .await;
        match result {
            Ok(profile) => {
                self.last_profile = Some((profile, Instant::now()));
                Ok(())
            }
            Err(error) => {
                self.last_profile = None;
                Err(error.to_string())
            }
        }
    }

    async fn cycle(&mut self) -> Result<Report, String> {
        let mut pending = match self.journal.load()? {
            Some(pending) => pending,
            None => {
                let state = Pending::default();
                self.journal.save(&state)?;
                state
            }
        };
        if pending.grant.is_some() {
            return self.resume(&mut pending).await;
        }
        self.refresh_profile().await?;
        if pending.assigned.is_none() {
            let checkout_templates = self
                .config
                .runtimes
                .iter()
                .filter(|runtime| runtime.manifest.checkout_gateway)
                .map(|runtime| runtime.template_id)
                .collect::<Vec<_>>();
            let response: Option<ExecutionLease> = self
                .office
                .post(
                    "v1/runner-scheduler/claim",
                    &ClaimExecution {
                        claim_key: pending.claim_key,
                        analysis_only: checkout_templates.is_empty(),
                        checkout_gateway_templates: Some(checkout_templates),
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            if response.is_none() {
                return Ok(Report::Idle);
            }
            pending.assigned = response;
            self.journal.save(&pending)?;
        }
        let assigned = pending
            .assigned
            .as_ref()
            .ok_or("The scheduler returned an empty execution lease")?;
        let proof = run_proof(&assigned.run)?;
        let manifest = self
            .config
            .runtimes
            .iter()
            .find(|runtime| Some(runtime.template_id) == assigned.run.runtime_template_id)
            .ok_or("The leased template is missing from the local runner configuration")?
            .manifest
            .clone();
        if assigned.checkout_id.is_some() && !manifest.checkout_gateway {
            return Err("This runtime manifest authorizes scratch-only execution".into());
        }
        manifest.verify(&assigned.managed_runtime)?;
        let input = AuthorizeExecutionStart {
            lease: proof.clone(),
            idempotency_key: pending.claim_key,
        };
        let grant: ExecutionStartGrant = match self
            .office
            .post("v1/runner-scheduler/start/authorize", &input)
            .await
        {
            Ok(grant) => grant,
            Err(ControlError::Rejected(ErrorCode::RunLeaseStale, _)) => {
                let receipt: Option<ExecutionStartReceipt> = self
                    .office
                    .post("v1/runner-scheduler/start/read", &proof)
                    .await
                    .map_err(|error| error.to_string())?;
                if let Some(receipt) = receipt {
                    pending.grant = Some(receipt);
                    self.journal.save(&pending)?;
                    return self.resume(&mut pending).await;
                }
                self.journal.clear()?;
                return Ok(Report::LeaseExpired);
            }
            Err(error) => return Err(error.to_string()),
        };
        let authorized = grant.execution.run.status == RunStatus::Starting
            && grant.execution.effect.state == StartEffectState::Authorized;
        let effect_id = grant.execution.effect.effect_id;
        let grant_run = grant.execution.run.run_id;
        pending.grant = Some(grant.execution);
        self.journal.save(&pending)?;
        if !authorized {
            return self.resume(&mut pending).await;
        }
        let files = self.journal.prepare_run(&grant.task, grant_run.0)?;
        let socket = if manifest.run_mcp {
            Some(self.worker_socket(&files, &proof, effect_id).await?)
        } else {
            None
        };
        let mount = socket.as_ref().map(|_| files.socket_mount());
        let installation = self.config.provider_for(&manifest)?;
        let pricing = installation.and_then(|entry| entry.profile.pricing.clone());
        let inference = installation
            .map(|profile| ProviderSocket::bind(&files.inference_bind(), profile))
            .transpose()?;
        let provider_socket = inference.as_ref().map(|_| files.inference_mount());
        let sockets = RunSockets {
            mcp: mount.as_deref(),
            inference: provider_socket.as_deref().zip(installation).map(
                |(socket, installation)| InferenceMount {
                    socket,
                    profile: &installation.profile,
                },
            ),
        };
        let created = self
            .podman
            .create(
                effect_id,
                &files.scratch,
                &files.task,
                &manifest,
                sockets,
                None,
            )
            .await?;
        self.heartbeat(&proof).await?;
        self.office
            .post::<ExecutionStartGrant>("v1/runner-scheduler/start/authorize", &input)
            .await
            .map_err(|error| error.to_string())?;
        pending.start_attempted = true;
        self.journal.save(&pending)?;
        self.podman.start(&created).await?;
        let container = self.podman.require(effect_id).await?;
        if container.id != created.id || !container.state.started() {
            return Err("Podman did not confirm the expected container start".into());
        }
        let effect = StartEffectRef {
            lease: proof.clone(),
            effect_id,
        };
        let observation = ObserveExecutionStart {
            effect,
            external_id: container.id.clone(),
        };
        self.office
            .post::<ExecutionStartReceipt>("v1/runner-scheduler/start/observe", &observation)
            .await
            .map_err(|error| error.to_string())?;
        let listening = socket.map(WorkerSocket::start);
        let inference_listening = inference.map(ProviderSocket::start);
        let exited = self.wait(&proof, effect_id, &container.id).await?;
        drop(inference_listening);
        drop(listening);
        pending.completion = Some(build_completion(
            observation.effect,
            &exited,
            &files,
            &manifest.adapter_id,
            pricing.as_ref(),
        )?);
        self.journal.save(&pending)?;
        self.resume(&mut pending).await
    }

    async fn worker_socket(
        &self,
        files: &RunFiles,
        proof: &RenewExecutionLease,
        effect_id: Uuid,
    ) -> Result<WorkerSocket, String> {
        let mut random = [0u8; 32];
        getrandom::fill(&mut random).map_err(|_| "Could not generate a secure run bearer")?;
        let bearer = format!("crun_{}", hex::encode(random));
        let digest =
            BearerDigest::of_run(&bearer).ok_or("Could not encode the run bearer digest")?;
        self.office
            .post::<RunCredentialReceipt>(
                "v1/runner-scheduler/run-credential",
                &IssueRunCredential {
                    effect: StartEffectRef {
                        lease: proof.clone(),
                        effect_id,
                    },
                    credential_digest: hex::encode(digest.as_bytes()),
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        let mcp = endpoint(&self.office.base, "run/mcp")?;
        WorkerSocket::bind(
            &files.socket_bind(),
            self.office.client.clone(),
            mcp,
            bearer,
        )
    }

    async fn heartbeat(&mut self, proof: &RenewExecutionLease) -> Result<(), String> {
        self.refresh_profile().await?;
        self.office
            .post::<Run>("v1/runner-scheduler/renew", proof)
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    async fn wait(
        &mut self,
        proof: &RenewExecutionLease,
        effect_id: Uuid,
        id: &str,
    ) -> Result<Container, String> {
        let mut last_heartbeat = Instant::now();
        loop {
            if last_heartbeat.elapsed() >= HEARTBEAT {
                self.heartbeat(proof).await?;
                last_heartbeat = Instant::now();
            }
            let container = self.podman.require(effect_id).await?;
            if container.id != id {
                return Err("The running container was replaced outside the controller".into());
            }
            if container.state.stopped() {
                return Ok(container);
            }
            if !container.state.running {
                return Err("Podman reported an unrecognized active state".into());
            }
            sleep(POLL).await;
        }
    }

    async fn resume(&self, pending: &mut Pending) -> Result<Report, String> {
        let grant = pending
            .grant
            .as_ref()
            .ok_or("Missing start effect in the controller journal")?;
        let effect_id = grant.effect.effect_id;
        if pending.reconciliation.is_none()
            && let Some(completion) = &pending.completion
        {
            match self
                .office
                .post("v1/runner-scheduler/complete", completion)
                .await
            {
                Ok(receipt) => {
                    self.retire(effect_id).await?;
                    return Ok(Report::Completed { receipt });
                }
                Err(
                    error @ ControlError::Rejected(
                        ErrorCode::RunLeaseStale
                        | ErrorCode::Forbidden
                        | ErrorCode::AccessRequestRequired
                        | ErrorCode::InvalidInput
                        | ErrorCode::InvalidTransition
                        | ErrorCode::DelegationInactive,
                        _,
                    ),
                ) => eprintln!("The controller is reconciling a rejected completion. {error}"),
                Err(error) => return Err(error.to_string()),
            }
        }
        if pending.reconciliation.is_none() {
            let observation = self
                .observe_stopped(effect_id, pending.start_attempted)
                .await?;
            let effect = StartEffectRef {
                lease: run_proof(&grant.run)?,
                effect_id,
            };
            pending.reconciliation = Some(ReconcileExecutionStart {
                effect,
                observation,
            });
            self.journal.save(pending)?;
        }
        let observation = pending
            .reconciliation
            .as_ref()
            .ok_or("The stop observation was lost from the controller journal")?;
        let receipt: ExecutionStartReceipt = self
            .office
            .post("v1/runner-scheduler/start/reconcile", observation)
            .await
            .map_err(|error| error.to_string())?;
        self.retire(effect_id).await?;
        Ok(Report::Reconciled { receipt })
    }

    async fn observe_stopped(
        &self,
        effect_id: Uuid,
        attempted_start: bool,
    ) -> Result<StopObservation, String> {
        let Some(mut container) = self.podman.inspect(effect_id).await? else {
            if attempted_start {
                return Err("The container disappeared after a start attempt. Its final state requires operator reconciliation".into());
            }
            return Ok(StopObservation::NeverStarted);
        };
        if container.state.running {
            let id = container.id.clone();
            self.podman.stop(&container).await?;
            container = self.podman.require(effect_id).await?;
            if container.id != id {
                return Err("The container identity changed while stopping".into());
            }
        }
        if container.state.started() && container.state.stopped() {
            return Ok(StopObservation::Stopped {
                external_id: container.id,
            });
        }
        if !container.state.started()
            && matches!(container.state.status.as_str(), "created" | "configured")
        {
            if attempted_start {
                self.podman.remove(&container).await?;
            }
            return Ok(StopObservation::NeverStarted);
        }
        Err("Podman could not verify a stopped or unstarted container".into())
    }

    async fn retire(&self, effect_id: Uuid) -> Result<(), String> {
        if let Some(container) = self.podman.inspect(effect_id).await? {
            self.podman.remove(&container).await?;
        }
        self.journal.clear()
    }
}

fn run_proof(run: &Run) -> Result<RenewExecutionLease, String> {
    let lease = run
        .lease
        .as_ref()
        .ok_or("The managed execution response is missing its lease")?;
    Ok(RenewExecutionLease {
        job_id: lease.job_id,
        run_id: run.run_id,
        run_fence: lease.run_fence,
    })
}

fn build_completion(
    effect: StartEffectRef,
    container: &Container,
    files: &RunFiles,
    adapter: &str,
    pricing: Option<&ProviderPricing>,
) -> Result<CompleteExecution, String> {
    if container.state.exit_code == -1 {
        return Err("Podman confirmed the stop without providing a process exit code".into());
    }
    let exit_code = u8::try_from(container.state.exit_code)
        .map_err(|_| "Podman returned an invalid process exit code")?;
    let evidence = packaged_backend(adapter).map(|_| files.backend_evidence(adapter));
    let mut result = match files.result() {
        Ok(result) if exit_code == 0 => result,
        Ok(result @ ProcessResult::Failed { .. }) if matches!(evidence, Some(Ok(_))) => result,
        Err(reason) if exit_code == 0 => ProcessResult::Failed { reason },
        _ => ProcessResult::Failed {
            reason: format!("The isolated process exited with status {exit_code}"),
        },
    };
    let verified = matches!(
        result,
        ProcessResult::Succeeded { .. } | ProcessResult::WaitingInput { .. }
    );
    let backend = match evidence {
        Some(Ok(mut evidence)) if result.matches_backend_state(evidence.terminal_state) => {
            if evidence.cost.is_none() {
                evidence.cost = pricing
                    .zip(evidence.usage)
                    .and_then(|(prices, usage)| prices.estimate(usage));
            }
            Some(evidence)
        }
        Some(Ok(_)) if verified => {
            result = ProcessResult::Failed {
                reason: "The backend evidence did not confirm the execution result".into(),
            };
            None
        }
        Some(Err(reason)) if verified => {
            result = ProcessResult::Failed { reason };
            None
        }
        None if matches!(result, ProcessResult::WaitingInput { .. }) => {
            result = ProcessResult::Failed {
                reason: "The runtime provided no verified input checkpoint".into(),
            };
            None
        }
        _ => None,
    };
    Ok(CompleteExecution {
        effect,
        external_id: container.id.clone(),
        idempotency_key: Uuid::new_v4(),
        exit_code,
        result,
        backend,
    })
}
