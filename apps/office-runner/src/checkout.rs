use std::{fs::File, io::Read, path::Path};

use office_checkout::{CheckoutInspection, inspect_checkout, persist_scope_deployment};
use office_domain::SourceManifest;
use office_protocol::{
    AcknowledgeScope, CheckoutEnrollment, CheckoutState, NextScopeDeployment, ProjectCheckout,
    WorkScope,
};
use serde_json::json;

use super::{
    CheckoutCommand,
    transport::{client, confidential_url, endpoint, print_json, read_token, request},
};

mod coordinator;
mod doctor;

#[tokio::main]
pub(super) async fn run(command: CheckoutCommand, json: bool) -> Result<(), String> {
    match command {
        CheckoutCommand::Serve(options) => coordinator::serve(options, json).await?,
        CheckoutCommand::Doctor(options) => doctor::run(options, json).await?,
        CheckoutCommand::CommandManifest { file } => {
            crate::service::snapshot_commands::describe(&file, json)?
        }
        CheckoutCommand::Inspect { root, manifest } => {
            let file = File::open(manifest).map_err(|_| "Could not open the manifest file")?;
            let manifest = serde_json::from_reader(file.take(512 * 1024))
                .map_err(|_| "Could not read the manifest as JSON")?;
            let CheckoutInspection { proof, entries } = inspect(&root, &manifest)?;
            if json {
                print_json(&json!({
                    "baseline": proof.baseline,
                    "entries": entries,
                    "root": {
                        "host_fingerprint": proof.host_fingerprint,
                        "directory_id": proof.directory_id,
                        "ancestors": proof.ancestors,
                    },
                }))?;
            } else {
                for entry in &entries {
                    println!("{}  {}", entry.sha256, entry.path.as_str());
                }
                println!(
                    "{} files, {} bytes, baseline {}",
                    proof.baseline.file_count,
                    proof.baseline.total_bytes,
                    proof.baseline.manifest_sha256
                );
            }
        }
        CheckoutCommand::Enroll {
            checkout,
            root,
            token_file,
            url,
            expected_source_hash,
        } => {
            let url = confidential_url(&url)?;
            let token = read_token(&token_file)?;
            let client = client()?;
            let get = endpoint(&url, &format!("v1/runner-checkouts/{checkout}"))?;
            let pending: ProjectCheckout = request(client.get(get), &token).await?;
            let manifest = pending
                .manifest
                .as_ref()
                .ok_or("The server did not authorize this runner to read the manifest")?;
            let proof = inspect(&root, manifest)?.proof;
            if expected_source_hash.is_some_and(|hash| hash != proof.baseline.manifest_sha256) {
                return Err("Selected sources changed since the approved preview".into());
            }
            let expected_revision = match pending.state {
                CheckoutState::PendingRegistration => pending.revision,
                CheckoutState::Registered => pending.revision - 1,
            };
            let enroll = endpoint(&url, "v1/runner-checkouts/enroll")?;
            let receipt: ProjectCheckout = request(
                client.post(enroll).json(&CheckoutEnrollment {
                    checkout_id: checkout,
                    expected_revision,
                    proof,
                }),
                &token,
            )
            .await?;
            if json {
                print_json(&receipt)?;
            } else {
                println!(
                    "{} · {:?} · revision {}",
                    receipt.checkout_id, receipt.state, receipt.revision
                );
            }
        }
        CheckoutCommand::SyncScopes {
            checkout,
            root,
            state_dir,
            token_file,
            url,
            limit,
        } => {
            let url = confidential_url(&url)?;
            let token = read_token(&token_file)?;
            let client = client()?;
            let pending_url =
                endpoint(&url, &format!("v1/runner-checkouts/{checkout}/next-scope"))?;
            let ack_url = endpoint(&url, "v1/runner-checkouts/acknowledge-scope")?;
            let mut receipts = Vec::new();
            for _ in 0..limit {
                let batch: NextScopeDeployment =
                    request(client.get(pending_url.clone()), &token).await?;
                let Some(deployment) = batch.pending else {
                    break;
                };
                if batch.checkout_id != checkout || deployment.scope.checkout_id != checkout {
                    return Err("Deployment does not match the requested checkout".into());
                }
                let deployment_sha256 =
                    persist_scope_deployment(&root, &state_dir, &batch.root, &deployment)
                        .map_err(|error| error.to_string())?;
                let receipt: WorkScope = request(
                    client.post(ack_url.clone()).json(&AcknowledgeScope {
                        checkout_id: checkout,
                        coordinator_epoch: None,
                        policy_sequence: deployment.policy_sequence,
                        scope_id: deployment.scope.scope_id,
                        scope_revision: deployment.scope.revision,
                        deployment_sha256,
                    }),
                    &token,
                )
                .await?;
                receipts.push(receipt);
            }
            if json {
                print_json(&receipts)?;
            } else {
                for receipt in &receipts {
                    println!(
                        "{} · revision {} · {:?}",
                        receipt.definition.scope_id, receipt.definition.revision, receipt.state
                    );
                }
                println!("Acknowledged {} policy revisions", receipts.len());
            }
        }
    }
    Ok(())
}

fn inspect(root: &Path, manifest: &SourceManifest) -> Result<CheckoutInspection, String> {
    inspect_checkout(root, manifest).map_err(|error| error.to_string())
}
