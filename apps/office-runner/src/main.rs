use std::{net::SocketAddr, path::PathBuf, process::ExitCode};

use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};
use uuid::Uuid;

#[cfg(target_os = "linux")]
mod checkout;
#[cfg(target_os = "linux")]
mod inference;
#[cfg(target_os = "linux")]
mod scheduler;
#[cfg(target_os = "linux")]
mod service;
#[cfg(target_os = "linux")]
mod transport;
#[cfg(target_os = "linux")]
mod worker_agent;
#[cfg(target_os = "linux")]
mod worker_mcp;

#[derive(Parser)]
#[command(version, about = "CoCodeOp owner-host runner")]
struct Args {
    #[arg(long, global = true, help = "Emit JSON")]
    json: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Verify this host's runner configuration, images, and backend executables.
    Doctor(DoctorOptions),
    /// Run owner-approved jobs on this Linux host.
    Serve(ServeOptions),
    /// Print the manifest digest to register for an owner-approved runtime.
    RuntimeManifest {
        #[arg(long)]
        file: PathBuf,
    },
    /// Print the digest of an owner-selected inference profile without credentials.
    InferenceProfile {
        #[arg(long)]
        file: PathBuf,
    },
    /// Bridge the isolated agent's loopback API to its private inference socket.
    WorkerInference {
        #[arg(long, default_value = "/run/cocodeop/inference.sock")]
        socket: PathBuf,
        #[arg(long, default_value = "127.0.0.1:8417")]
        listen: SocketAddr,
    },
    /// Connect a worker's stdio MCP client to its sandbox-local socket.
    WorkerMcp {
        #[arg(long, default_value = "/run/cocodeop/mcp.sock")]
        socket: PathBuf,
    },
    /// Execute an approved agent client inside the isolated worker container.
    WorkerAgent(WorkerAgentOptions),
    /// Advertise execution capabilities to the coordinator.
    Scheduler {
        #[command(subcommand)]
        command: SchedulerCommand,
    },
    /// Inspect or enroll a shared checkout on this host.
    Checkout {
        #[command(subcommand)]
        command: CheckoutCommand,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum WorkerBackend {
    Codex,
    Opencode,
    Jsonl,
}

#[derive(ClapArgs)]
struct WorkerAgentOptions {
    #[arg(long, value_enum)]
    backend: WorkerBackend,
    #[arg(long, requires = "backend")]
    bridge: Option<PathBuf>,
    #[arg(long, requires = "bridge", allow_hyphen_values = true)]
    bridge_arg: Vec<String>,
    #[arg(
        long,
        help = "Print the capabilities of this backend inside a CoCodeOp container"
    )]
    capabilities: bool,
}

#[derive(ClapArgs)]
struct RunnerConnection {
    #[arg(long)]
    token_file: PathBuf,
    #[arg(long, default_value = "http://127.0.0.1:8400")]
    url: String,
}

#[derive(ClapArgs)]
struct DoctorOptions {
    #[arg(long)]
    file: PathBuf,
    #[arg(long)]
    state_dir: PathBuf,
    #[command(flatten)]
    connection: RunnerConnection,
}

#[derive(ClapArgs)]
struct ServeOptions {
    #[arg(long)]
    file: PathBuf,
    #[arg(long)]
    state_dir: PathBuf,
    #[arg(long, help = "Process one assignment and return its receipt")]
    once: bool,
    #[command(flatten)]
    connection: RunnerConnection,
}

#[derive(ClapArgs)]
struct CheckoutServiceOptions {
    #[arg(long)]
    checkout: Uuid,
    #[arg(long)]
    root: PathBuf,
    #[arg(long)]
    state_dir: PathBuf,
    #[arg(long)]
    commands_file: Option<PathBuf>,
    #[arg(
        long,
        help = "Verify an owner-repaired checkout against its existing durable journal"
    )]
    reconcile: bool,
    #[command(flatten)]
    connection: RunnerConnection,
}

#[derive(ClapArgs)]
struct SchedulerRequest {
    #[arg(long)]
    file: PathBuf,
    #[command(flatten)]
    connection: RunnerConnection,
}

#[derive(Subcommand)]
enum SchedulerCommand {
    /// Publish the runner's current capacity and managed runtime allowlist.
    Advertise {
        #[arg(long)]
        file: PathBuf,
        #[command(flatten)]
        connection: RunnerConnection,
    },
    /// Read the execution profile registered with this runner credential.
    Profile(RunnerConnection),
    /// Lease one compatible pending execution with a caller-generated key.
    Claim {
        #[arg(long)]
        claim_key: Uuid,
        #[command(flatten)]
        connection: RunnerConnection,
    },
    /// Extend the current lease for the exact run and fence.
    Renew {
        #[arg(long)]
        file: PathBuf,
        #[command(flatten)]
        connection: RunnerConnection,
    },
    /// Record start authorization against the current lease and a durable effect ID.
    AuthorizeStart(SchedulerRequest),
    /// Read the exact prior start effect, including after a lease expires.
    ReadStart(SchedulerRequest),
    /// Record the external process identity observed by the assigned runner.
    ObserveStart(SchedulerRequest),
    /// Resolve an effect after the assigned runner verifies the process outcome.
    ReconcileStart(SchedulerRequest),
    /// Publish the observed process exit and a result under the live run fence.
    Complete(SchedulerRequest),
    /// Bind a locally generated worker bearer digest to this exact run fence.
    IssueRunCredential(SchedulerRequest),
}

#[derive(Subcommand)]
enum CheckoutCommand {
    /// Hold the registered checkout and continuously apply its authorized policy stream.
    Serve(CheckoutServiceOptions),
    /// Verify the active coordinator, local journal and worker isolation.
    Doctor(CheckoutDoctorOptions),
    /// Hash the exact command template receipt reviewed by the checkout owner.
    CommandManifest {
        #[arg(long)]
        file: PathBuf,
    },
    /// Preview actual selected source files without modifying the checkout.
    Inspect {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        manifest: PathBuf,
    },
    /// Enroll the host's directory in an owner-approved checkout registration.
    Enroll {
        #[arg(long)]
        checkout: Uuid,
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        token_file: PathBuf,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: String,
        #[arg(long)]
        expected_source_hash: Option<String>,
    },
    /// Install and acknowledge pending permission revisions on this checkout host.
    SyncScopes {
        #[arg(long)]
        checkout: Uuid,
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        token_file: PathBuf,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: String,
        #[arg(long, default_value_t = 32, value_parser = clap::value_parser!(u32).range(1..=128))]
        limit: u32,
    },
}

#[derive(clap::Args)]
struct CheckoutDoctorOptions {
    #[arg(long)]
    checkout: Uuid,
    #[arg(long)]
    root: PathBuf,
    #[arg(long)]
    state_dir: PathBuf,
    #[arg(long)]
    scope_task: Option<Uuid>,
    /// Use this installed local image ID to exercise the actual worker OCI mounts.
    #[arg(long)]
    probe_image: Option<String>,
    #[command(flatten)]
    connection: RunnerConnection,
}

fn main() -> ExitCode {
    let args = Args::parse();
    if let Some(command) = args.command {
        #[cfg(target_os = "linux")]
        let result = match command {
            Command::Doctor(options) => service::doctor(options, args.json),
            Command::Serve(options) => service::run(options, args.json),
            Command::RuntimeManifest { file } => service::describe_manifest(&file, args.json),
            Command::InferenceProfile { file } => service::describe_inference(&file, args.json),
            Command::WorkerInference { socket, listen } => inference::worker_http(&socket, listen),
            Command::WorkerMcp { socket } => worker_mcp::stdio(&socket),
            Command::WorkerAgent(options) => worker_agent::run(options),
            Command::Checkout { command } => checkout::run(command, args.json),
            Command::Scheduler { command } => scheduler::run(command, args.json),
        };
        #[cfg(not(target_os = "linux"))]
        let result: Result<(), String> = {
            let _ = command;
            Err("Runner operations require Linux".into())
        };
        if let Err(error) = result {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }
    let version = env!("CARGO_PKG_VERSION");
    if args.json {
        println!(r#"{{"version":"{version}"}}"#);
    } else {
        println!("office-runner {version}");
    }
    ExitCode::SUCCESS
}
