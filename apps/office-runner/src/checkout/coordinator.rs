use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use office_checkout::{
    CheckoutCoordinator, CheckoutPublisher, CheckoutReadError, PublicationStateError, SealError,
};
use office_domain::{ErrorCode, SourceManifest};
use office_protocol::{
    AcknowledgeScope, CheckoutCommandOrder, CheckoutCommandOutcome, CheckoutCoordinatorLease,
    CheckoutCoordinatorRef, CheckoutPublicationRecovery, CheckoutPublicationSession,
    CheckoutReleaseOrder, CheckoutSnapshotOrder, CheckoutSnapshotOutcome, CheckoutWriteOffer,
    CheckoutWriteOutcome, FileEncoding, GatewayFile, GatewayQuery, GatewayReadOrder,
    GatewayReadOutcome, MAX_GATEWAY_READ_BYTES, NextScopeDeployment, ProjectCheckout,
    ReportCheckoutCommand, ReportCheckoutRelease, ReportCheckoutSnapshot, ReportCheckoutWrite,
    ReportGatewayRead, SnapshotCheckoutCommand, StartCheckoutCoordinator, WorkScope,
};
use reqwest::{Client, Url};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::{
    signal::unix::{SignalKind, signal},
    time::{interval_at, sleep},
};
use uuid::Uuid;

use crate::{
    CheckoutServiceOptions,
    service::snapshot_commands::CommandHost,
    transport::{
        ControlError, client, confidential_url, endpoint, print_json, read_token, request,
        request_typed,
    },
};

const HEARTBEAT: Duration = Duration::from_secs(4);
const POLL: Duration = Duration::from_millis(650);

struct Service {
    client: Client,
    token: String,
    base: Url,
    next_scope: Url,
    manifest: SourceManifest,
}

impl Service {
    async fn snapshot_one(
        &self,
        local: &Arc<CheckoutCoordinator>,
        proof: &CheckoutCoordinatorRef,
        state_dir: &Path,
    ) -> Result<(), String> {
        let url = endpoint(&self.base, "v1/runner-checkouts/coordinator/next-snapshot")?;
        let order: Option<CheckoutSnapshotOrder> =
            request(self.client.post(url).json(proof), &self.token).await?;
        let Some(order) = order else {
            return Ok(());
        };
        if order.checkout_id != proof.checkout_id {
            return Err("Snapshot order does not match the leased checkout".into());
        }
        let snapshot_id = order.snapshot_id;
        let local = Arc::clone(local);
        let manifest = self.manifest.clone();
        let state_dir = state_dir.to_path_buf();
        let sealed = self
            .with_live_checkout(proof, move || {
                local.seal_source(&state_dir, &manifest, order)
            })
            .await?;
        let outcome = match sealed {
            Ok(proof) => CheckoutSnapshotOutcome::Ready { proof },
            Err(SealError::SourceChanged) => CheckoutSnapshotOutcome::SourceChanged,
            Err(error) => {
                eprintln!("Could not capture checkout snapshot {snapshot_id}: {error}");
                CheckoutSnapshotOutcome::StorageUnavailable
            }
        };
        let url = endpoint(
            &self.base,
            "v1/runner-checkouts/coordinator/snapshot-receipt",
        )?;
        request(
            self.client.post(url).json(&ReportCheckoutSnapshot {
                coordinator: proof.clone(),
                snapshot_id,
                outcome,
            }),
            &self.token,
        )
        .await
    }

    async fn release_one(
        &self,
        local: &Arc<CheckoutCoordinator>,
        proof: &CheckoutCoordinatorRef,
        state_dir: &Path,
    ) -> Result<(), String> {
        let url = endpoint(&self.base, "v1/runner-checkouts/coordinator/next-release")?;
        let order: Option<CheckoutReleaseOrder> =
            request(self.client.post(url).json(proof), &self.token).await?;
        let Some(order) = order else {
            return Ok(());
        };
        if order.checkout_id != proof.checkout_id {
            return Err("Accepted checkpoint order does not match the leased checkout".into());
        }
        let release_id = order.release_id;
        let local = Arc::clone(local);
        let manifest = self.manifest.clone();
        let state_dir = state_dir.to_path_buf();
        let outcome = self
            .with_live_checkout(proof, move || {
                local.accept_snapshot(&state_dir, &manifest, order)
            })
            .await?
            .map_err(|error| {
                format!("Accepted checkpoint {release_id} requires reconciliation: {error}")
            })?;
        let url = endpoint(
            &self.base,
            "v1/runner-checkouts/coordinator/release-receipt",
        )?;
        request(
            self.client.post(url).json(&ReportCheckoutRelease {
                coordinator: proof.clone(),
                release_id,
                outcome,
            }),
            &self.token,
        )
        .await
    }

    async fn with_live_checkout<T: Send + 'static>(
        &self,
        proof: &CheckoutCoordinatorRef,
        action: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, String> {
        let mut operation = tokio::task::spawn_blocking(action);
        let mut renew = interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
        loop {
            tokio::select! {
                result = &mut operation => return result.map_err(|_| "Checkout checkpoint worker stopped".into()),
                _ = renew.tick() => { self.coordinator("renew", proof).await?; }
            }
        }
    }

    async fn coordinator(
        &self,
        action: &str,
        input: &impl Serialize,
    ) -> Result<CheckoutCoordinatorLease, String> {
        let url = endpoint(
            &self.base,
            &format!("v1/runner-checkouts/coordinator/{action}"),
        )?;
        request(self.client.post(url).json(input), &self.token).await
    }

    async fn scopes(&self) -> Result<NextScopeDeployment, String> {
        request(self.client.get(self.next_scope.clone()), &self.token).await
    }

    async fn sync_one(
        &self,
        local: &CheckoutCoordinator,
        lease: &CheckoutCoordinatorRef,
    ) -> Result<bool, String> {
        let batch = self.scopes().await?;
        let Some(deployment) = batch.pending else {
            return Ok(false);
        };
        if batch.checkout_id != lease.checkout_id
            || deployment.scope.checkout_id != lease.checkout_id
        {
            return Err("Scope deployment does not match the leased checkout".into());
        }
        let deployment_sha256 = local
            .persist_scope_deployment(&deployment)
            .map_err(|error| error.to_string())?;
        let input = AcknowledgeScope {
            checkout_id: lease.checkout_id,
            coordinator_epoch: Some(lease.epoch),
            policy_sequence: deployment.policy_sequence,
            scope_id: deployment.scope.scope_id,
            scope_revision: deployment.scope.revision,
            deployment_sha256,
        };
        let url = endpoint(&self.base, "v1/runner-checkouts/acknowledge-scope")?;
        match request_typed::<WorkScope>(self.client.post(url).json(&input), &self.token).await {
            Ok(_) | Err(ControlError::Rejected(ErrorCode::PublicationPending, _)) => Ok(true),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn recover_writes(
        &self,
        publisher: Option<&CheckoutPublisher<'_>>,
        session: &CheckoutPublicationSession,
    ) -> Result<(), String> {
        let url = endpoint(
            &self.base,
            "v1/runner-checkouts/coordinator/recover-publications",
        )?;
        let recovery: CheckoutPublicationRecovery =
            request(self.client.post(url).json(session), &self.token).await?;
        let Some(publisher) = publisher else {
            return if recovery.head.is_none() {
                Ok(())
            } else {
                Err("The checkout journal is unavailable".into())
            };
        };
        let Some(head) = recovery.head else {
            return publisher
                .require_no_pending()
                .map_err(|error| error.to_string());
        };
        let change = recovery.outstanding.as_ref().map(|order| order.change_id);
        match publisher.recover(&head, recovery.outstanding.as_ref()) {
            Ok(None) if recovery.blocked => {
                self.confirm_write(
                    publisher,
                    session,
                    None,
                    CheckoutWriteOutcome::Restored { head },
                )
                .await
            }
            Ok(None) => Ok(()),
            Ok(Some(outcome)) => {
                self.confirm_write(publisher, session, change, outcome)
                    .await
            }
            Err(error) => {
                self.confirm_write(
                    publisher,
                    session,
                    change,
                    CheckoutWriteOutcome::ReconcileRequired,
                )
                .await?;
                Err(error.to_string())
            }
        }
    }

    async fn publish_one(
        &self,
        publisher: &CheckoutPublisher<'_>,
        session: &CheckoutPublicationSession,
    ) -> Result<bool, String> {
        let mut url = endpoint(
            &self.base,
            "v1/runner-checkouts/coordinator/next-publication",
        )?;
        url.set_query(Some("wait=true"));
        let started = Instant::now();
        let offer: Option<CheckoutWriteOffer> =
            request(self.client.post(url).json(session), &self.token).await?;
        let Some(offer) = offer else {
            return Ok(false);
        };
        let order = offer.order;
        // Include transport time in the allowance without comparing host wall clocks.
        let remaining = (order.permit_until - offer.checked_at)
            .to_std()
            .unwrap_or_default();
        let deadline = started + remaining.min(Duration::from_secs(15));
        if order.epoch != session.coordinator.epoch {
            return Err("Publication order does not match the leased checkout epoch".into());
        }
        let change = Some(order.change_id);
        match publisher.apply(&order, deadline) {
            Ok(outcome) => {
                self.confirm_write(publisher, session, change, outcome)
                    .await?
            }
            Err(error) => {
                self.confirm_write(
                    publisher,
                    session,
                    change,
                    CheckoutWriteOutcome::ReconcileRequired,
                )
                .await?;
                return Err(error.to_string());
            }
        }
        Ok(true)
    }

    async fn confirm_write(
        &self,
        publisher: &CheckoutPublisher<'_>,
        session: &CheckoutPublicationSession,
        change_id: Option<Uuid>,
        outcome: CheckoutWriteOutcome,
    ) -> Result<(), String> {
        let url = endpoint(
            &self.base,
            "v1/runner-checkouts/coordinator/publication-receipt",
        )?;
        let applied = matches!(outcome, CheckoutWriteOutcome::Applied { .. });
        let input = ReportCheckoutWrite {
            session: session.clone(),
            change_id,
            outcome,
        };
        request::<()>(self.client.post(url).json(&input), &self.token).await?;
        if applied && let Some(id) = change_id {
            publisher
                .acknowledge(id)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    async fn command_one(
        &self,
        local: &CheckoutCoordinator,
        proof: &CheckoutCoordinatorRef,
        state_dir: &Path,
        commands: &mut Option<CommandHost>,
    ) -> Result<(), String> {
        let started = Instant::now();
        let next = endpoint(&self.base, "v1/runner-checkouts/coordinator/next-command")?;
        let order: Option<CheckoutCommandOrder> =
            request(self.client.post(next).json(proof), &self.token).await?;
        if let Some(host) = commands.as_mut()
            && let Some(active) = host.active_command()
        {
            let result = if order
                .as_ref()
                .is_some_and(|item| item.command_id == active && !item.revoked)
            {
                host.tick().await?
            } else {
                host.stop().await?
            };
            if let Some((id, result)) = result {
                self.command_receipt(proof, id, result).await?;
                host.retire(id).await?;
            }
            return Ok(());
        }
        let Some(order) = order else {
            return Ok(());
        };
        if order.scope.checkout_id != proof.checkout_id {
            return Err("The checkout command refers to another coordinator".into());
        }
        if order.epoch != proof.epoch || order.previous_snapshot.is_some() {
            let outcome = if order.previous_snapshot.is_some() {
                if commands.is_none() {
                    *commands = Some(CommandHost::open(state_dir, None).await?);
                }
                commands
                    .as_ref()
                    .ok_or("The command recovery host is unavailable")?
                    .recover(&order)
                    .await?
            } else {
                CheckoutCommandOutcome::Interrupted
            };
            self.command_receipt(proof, order.command_id, outcome)
                .await?;
            if let Some(host) = commands {
                host.retire(order.command_id).await?;
            }
            return Ok(());
        }
        if order.revoked {
            return self
                .command_receipt(proof, order.command_id, CheckoutCommandOutcome::Interrupted)
                .await;
        }
        let Some(host) = commands else {
            return self
                .command_receipt(
                    proof,
                    order.command_id,
                    CheckoutCommandOutcome::NotConfigured,
                )
                .await;
        };
        let remaining = (order.permit_until - order.checked_at)
            .to_std()
            .unwrap_or_default();
        let deadline = started + remaining.min(Duration::from_secs(15));
        if Instant::now() >= deadline {
            return self
                .command_receipt(proof, order.command_id, CheckoutCommandOutcome::Interrupted)
                .await;
        }
        let prepared = match host.prepare(local, state_dir, &self.manifest, &order)? {
            Ok(value) => value,
            Err(outcome) => {
                self.command_receipt(proof, order.command_id, outcome)
                    .await?;
                return host.retire(order.command_id).await;
            }
        };
        if Instant::now() >= deadline {
            self.command_receipt(proof, order.command_id, CheckoutCommandOutcome::Interrupted)
                .await?;
            return host.retire(order.command_id).await;
        }
        let url = endpoint(
            &self.base,
            "v1/runner-checkouts/coordinator/command-snapshot",
        )?;
        let snapshot = SnapshotCheckoutCommand {
            coordinator: proof.clone(),
            command_id: prepared.id,
            snapshot: prepared.proof.clone(),
        };
        match request_typed::<()>(self.client.post(url).json(&snapshot), &self.token).await {
            Ok(()) => {}
            Err(ControlError::Rejected(ErrorCode::RunLeaseStale, _)) => {
                self.command_receipt(proof, order.command_id, CheckoutCommandOutcome::Interrupted)
                    .await?;
                return host.retire(order.command_id).await;
            }
            Err(error) => return Err(error.to_string()),
        }
        if let Some(outcome) = host.start(prepared, deadline).await? {
            self.command_receipt(proof, order.command_id, outcome)
                .await?;
            host.retire(order.command_id).await?;
        }
        Ok(())
    }

    async fn command_receipt(
        &self,
        proof: &CheckoutCoordinatorRef,
        command_id: Uuid,
        outcome: CheckoutCommandOutcome,
    ) -> Result<(), String> {
        let url = endpoint(
            &self.base,
            "v1/runner-checkouts/coordinator/command-receipt",
        )?;
        let report = ReportCheckoutCommand {
            coordinator: proof.clone(),
            command_id,
            outcome,
        };
        request(self.client.post(url).json(&report), &self.token).await
    }

    async fn read_one(
        &self,
        local: &CheckoutCoordinator,
        lease: &CheckoutCoordinatorRef,
    ) -> Result<(), String> {
        let url = endpoint(&self.base, "v1/runner-checkouts/coordinator/next-file-read")?;
        let order: Option<GatewayReadOrder> =
            request(self.client.post(url).json(lease), &self.token).await?;
        let Some(order) = order else {
            return Ok(());
        };
        if order.epoch != lease.epoch
            || order.scope.checkout_id != lease.checkout_id
            || !order.query.is_valid()
        {
            return Err("Read order does not match the leased checkout epoch".into());
        }
        let access = &order.scope.permissions.read;
        let result = match order.query {
            GatewayQuery::File { path } => local
                .read_file(&self.manifest, access, &path, MAX_GATEWAY_READ_BYTES)
                .map(encode_file),
            GatewayQuery::Search {
                needle,
                under,
                limit,
            } => local.search_code(
                &self.manifest,
                access,
                under.as_ref(),
                &needle,
                usize::from(limit),
            ),
            GatewayQuery::Status {
                under,
                after,
                limit,
            } => local.checkout_status(
                &self.manifest,
                access,
                under.as_ref(),
                after.as_ref(),
                usize::from(limit),
            ),
        };
        let outcome = match result {
            Ok(value) => value,
            Err(CheckoutReadError::Forbidden) => GatewayReadOutcome::Forbidden,
            Err(CheckoutReadError::Missing) => GatewayReadOutcome::Missing,
            Err(CheckoutReadError::Limit) => GatewayReadOutcome::TooLarge,
            Err(CheckoutReadError::Changed) => GatewayReadOutcome::Changed,
            Err(CheckoutReadError::Unavailable) => GatewayReadOutcome::Unavailable,
        };
        let url = endpoint(
            &self.base,
            "v1/runner-checkouts/coordinator/file-read-receipt",
        )?;
        request(
            self.client.post(url).json(&ReportGatewayRead {
                coordinator: lease.clone(),
                read_id: order.read_id,
                outcome,
            }),
            &self.token,
        )
        .await
    }
}

fn encode_file(bytes: Vec<u8>) -> GatewayReadOutcome {
    let sha256 = hex::encode(Sha256::digest(&bytes));
    let size_bytes = bytes.len() as u64;
    let (encoding, content) = match std::str::from_utf8(&bytes) {
        Ok(text) if !text.contains('\0') => (FileEncoding::Utf8, text.to_owned()),
        _ => (FileEncoding::Base64, STANDARD.encode(bytes)),
    };
    GatewayReadOutcome::Success {
        file: GatewayFile {
            sha256,
            size_bytes,
            encoding,
            content,
        },
    }
}

pub(super) async fn serve(options: CheckoutServiceOptions, json: bool) -> Result<(), String> {
    let base = confidential_url(&options.connection.url)?;
    let client = client()?;
    let token = read_token(&options.connection.token_file)?;
    let url = endpoint(&base, &format!("v1/runner-checkouts/{}", options.checkout))?;
    let checkout: ProjectCheckout = request(client.get(url), &token).await?;
    let manifest = checkout
        .manifest
        .ok_or("The checkout host cannot read its registered source manifest")?;
    let service = Service {
        client,
        token,
        manifest,
        next_scope: endpoint(
            &base,
            &format!("v1/runner-checkouts/{}/next-scope", options.checkout),
        )?,
        base,
    };
    let mut term =
        signal(SignalKind::terminate()).map_err(|_| "Could not subscribe to shutdown signals")?;
    let mut interrupt =
        signal(SignalKind::interrupt()).map_err(|_| "Could not subscribe to interrupt signals")?;
    let root = service.scopes().await?.root;
    let local = Arc::new(
        CheckoutCoordinator::open(&options.root, &options.state_dir, &root)
            .map_err(|error| error.to_string())?,
    );
    let publisher = match CheckoutPublisher::open(&local, options.checkout, &service.manifest) {
        Ok(publisher) => Some(publisher),
        Err(PublicationStateError::DifferentMount) => None,
        Err(error) => return Err(error.to_string()),
    };
    let mut commands = match options.commands_file.as_deref() {
        Some(file) => Some(CommandHost::open(&options.state_dir, Some(file)).await?),
        None => None,
    };
    let lease = service
        .coordinator(
            "start",
            &StartCheckoutCoordinator {
                checkout_id: options.checkout,
                instance_key: Uuid::new_v4(),
                root,
            },
        )
        .await?;
    local
        .persist_epoch(&lease)
        .map_err(|error| error.to_string())?;
    let proof = CheckoutCoordinatorRef {
        checkout_id: options.checkout,
        epoch: lease.epoch,
    };
    let mut write_session = CheckoutPublicationSession {
        coordinator: proof.clone(),
        state_id: publisher.as_ref().map(CheckoutPublisher::state_id),
        reconcile: options.reconcile,
    };
    service
        .recover_writes(publisher.as_ref(), &write_session)
        .await?;
    write_session.reconcile = false;
    let ready = service.coordinator("renew", &proof).await?;
    if json {
        print_json(&ready)?;
    } else {
        println!(
            "Serving checkout {} with coordinator epoch {}",
            ready.checkout_id, ready.epoch
        );
    }
    tokio::select! {
        _ = term.recv() => (),
        _ = interrupt.recv() => (),
        result = run(&service, &local, publisher.as_ref(), &write_session,
            &options.state_dir, &mut commands) => return result,
    }
    if let Some(host) = &mut commands
        && let Some((id, outcome)) = host.stop().await?
    {
        service.command_receipt(&proof, id, outcome).await?;
        host.retire(id).await?;
    }
    service.coordinator("stop", &proof).await?;
    Ok(())
}

async fn run(
    service: &Service,
    local: &Arc<CheckoutCoordinator>,
    publisher: Option<&CheckoutPublisher<'_>>,
    session: &CheckoutPublicationSession,
    state_dir: &Path,
    commands: &mut Option<CommandHost>,
) -> Result<(), String> {
    let proof = &session.coordinator;
    let mut last_renewal = Instant::now();
    loop {
        if last_renewal.elapsed() >= HEARTBEAT {
            service.coordinator("renew", proof).await?;
            last_renewal = Instant::now();
        }
        service.release_one(local, proof, state_dir).await?;
        service.snapshot_one(local, proof, state_dir).await?;
        service
            .command_one(local, proof, state_dir, commands)
            .await?;
        if !service.sync_one(local, proof).await? {
            service.read_one(local, proof).await?;
        }
        let started = Instant::now();
        if let Some(publisher) = publisher
            && service.publish_one(publisher, session).await?
        {
            continue;
        }
        sleep(POLL.saturating_sub(started.elapsed())).await;
    }
}
