use std::{fs::File, io::Read, path::Path};

use office_protocol::{
    AdvertiseRunner, AuthorizeExecutionStart, ClaimExecution, CompleteExecution,
    ExecutionCompletionReceipt, ExecutionLease, ExecutionStartGrant, ExecutionStartReceipt,
    IssueRunCredential, ObserveExecutionStart, ReconcileExecutionStart, RenewExecutionLease, Run,
    RunCredentialReceipt, RunnerProfile,
};
use serde::{Serialize, de::DeserializeOwned};

use super::{
    SchedulerCommand,
    transport::{client, confidential_url, endpoint, print_json, read_token, request},
};

#[tokio::main]
pub(super) async fn run(command: SchedulerCommand, json: bool) -> Result<(), String> {
    let connection = match &command {
        SchedulerCommand::Advertise { connection, .. }
        | SchedulerCommand::Claim { connection, .. }
        | SchedulerCommand::Renew { connection, .. }
        | SchedulerCommand::Profile(connection) => connection,
        SchedulerCommand::AuthorizeStart(args)
        | SchedulerCommand::ReadStart(args)
        | SchedulerCommand::ObserveStart(args)
        | SchedulerCommand::ReconcileStart(args)
        | SchedulerCommand::Complete(args)
        | SchedulerCommand::IssueRunCredential(args) => &args.connection,
    };
    let base = confidential_url(&connection.url)?;
    let token = read_token(&connection.token_file)?;
    let client = client()?;
    let profile = match command {
        SchedulerCommand::Advertise { file, .. } => {
            let payload: AdvertiseRunner = read_request(&file)?;
            let url = endpoint(&base, "v1/runner-scheduler/profile")?;
            Some(request::<RunnerProfile>(client.post(url).json(&payload), &token).await?)
        }
        SchedulerCommand::Profile(_) => {
            let url = endpoint(&base, "v1/runner-scheduler/profile")?;
            request::<Option<RunnerProfile>>(client.get(url), &token).await?
        }
        SchedulerCommand::Claim { claim_key, .. } => {
            let url = endpoint(&base, "v1/runner-scheduler/claim")?;
            let lease: Option<ExecutionLease> = request(
                client.post(url).json(&ClaimExecution {
                    claim_key,
                    analysis_only: false,
                    checkout_gateway_templates: None,
                }),
                &token,
            )
            .await?;
            return if json {
                print_json(&lease)
            } else if let Some(lease) = lease {
                print_run(&lease.run)
            } else {
                println!("No compatible execution is ready");
                Ok(())
            };
        }
        SchedulerCommand::Renew { file, .. } => {
            let payload: RenewExecutionLease = read_request(&file)?;
            let url = endpoint(&base, "v1/runner-scheduler/renew")?;
            let run: Run = request(client.post(url).json(&payload), &token).await?;
            return if json {
                print_json(&run)
            } else {
                print_run(&run)
            };
        }
        SchedulerCommand::AuthorizeStart(args) => {
            let input: AuthorizeExecutionStart = read_request(&args.file)?;
            let url = endpoint(&base, "v1/runner-scheduler/start/authorize")?;
            let grant: ExecutionStartGrant = request(client.post(url).json(&input), &token).await?;
            return if json {
                print_json(&grant)
            } else {
                print_effect(&grant.execution)
            };
        }
        SchedulerCommand::ReadStart(args) => {
            let input: RenewExecutionLease = read_request(&args.file)?;
            let receipt: Option<ExecutionStartReceipt> = request(
                client
                    .post(endpoint(&base, "v1/runner-scheduler/start/read")?)
                    .json(&input),
                &token,
            )
            .await?;
            return if json {
                print_json(&receipt)
            } else if let Some(receipt) = receipt {
                print_effect(&receipt)
            } else {
                println!("No process start was authorized for this attempt");
                Ok(())
            };
        }
        SchedulerCommand::ObserveStart(args) => {
            let input: ObserveExecutionStart = read_request(&args.file)?;
            return send_effect(&client, &base, &token, "observe", &input, json).await;
        }
        SchedulerCommand::ReconcileStart(args) => {
            let input: ReconcileExecutionStart = read_request(&args.file)?;
            return send_effect(&client, &base, &token, "reconcile", &input, json).await;
        }
        SchedulerCommand::Complete(args) => {
            let input: CompleteExecution = read_request(&args.file)?;
            let url = endpoint(&base, "v1/runner-scheduler/complete")?;
            let receipt: ExecutionCompletionReceipt =
                request(client.post(url).json(&input), &token).await?;
            return if json {
                print_json(&receipt)
            } else {
                print_effect(&receipt.execution)?;
                println!(
                    "Process exit {} · task revision {}",
                    receipt.exit_code, receipt.task_revision
                );
                Ok(())
            };
        }
        SchedulerCommand::IssueRunCredential(args) => {
            let input: IssueRunCredential = read_request(&args.file)?;
            let url = endpoint(&base, "v1/runner-scheduler/run-credential")?;
            let receipt: RunCredentialReceipt =
                request(client.post(url).json(&input), &token).await?;
            return if json {
                print_json(&receipt)
            } else {
                println!(
                    "Issued run credential {} for run {}",
                    receipt.credential_id, receipt.run_id.0
                );
                Ok(())
            };
        }
    };
    if json {
        print_json(&profile)?;
    } else if let Some(profile) = profile {
        println!(
            "Runner {} revision {} capacity {} expires {}",
            profile.runner_id.0, profile.revision, profile.capacity, profile.expires_at
        );
    } else {
        println!("Execution profile is pending advertisement");
    }
    Ok(())
}

async fn send_effect(
    client: &reqwest::Client,
    base: &reqwest::Url,
    token: &str,
    operation: &str,
    input: &impl Serialize,
    json: bool,
) -> Result<(), String> {
    let url = endpoint(base, &format!("v1/runner-scheduler/start/{operation}"))?;
    let receipt: ExecutionStartReceipt = request(client.post(url).json(input), token).await?;
    if json {
        print_json(&receipt)
    } else {
        print_effect(&receipt)
    }
}

fn print_effect(receipt: &ExecutionStartReceipt) -> Result<(), String> {
    print_run(&receipt.run)?;
    println!(
        "Start effect {} {:?}",
        receipt.effect.effect_id, receipt.effect.state
    );
    Ok(())
}

fn read_request<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    const MAX_REQUEST_BYTES: usize = 160 * 1024;
    let file = File::open(path).map_err(|_| "Could not open the runner request file")?;
    let mut bytes = Vec::new();
    file.take(MAX_REQUEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Could not read the runner request file")?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err("The runner request exceeds the size limit".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "Could not read the runner request as JSON".into())
}

fn print_run(run: &Run) -> Result<(), String> {
    let lease = run
        .lease
        .as_ref()
        .ok_or("The server response did not contain the requested lease")?;
    println!(
        "Run {} {:?} fence {} expires {}",
        run.run_id.0, run.status, lease.run_fence, lease.lease_until
    );
    Ok(())
}
