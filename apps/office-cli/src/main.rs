mod artifacts;
mod checkouts;
mod claims;
mod connection;
mod decisions;
mod delegation;
mod doctor;
mod identity;
mod inbox;
mod mcp_stdio;
mod messages;
mod notes;
mod runs;
mod scheduler;
mod tasks;
mod teams;
mod work_scopes;
mod workers;

use std::{path::PathBuf, process::ExitCode, time::Duration};

use clap::{Parser, Subcommand, ValueEnum};
use office_domain::{AgentId, AgentRole, CredentialId, RunnerId, WorkspaceId};
use office_protocol::{
    AgentIdentity, CredentialSubject, Health, Identity, Outcome, Readiness, Reply,
};
use reqwest::{Method, Url};
use schemars::schema_for;

#[derive(Parser)]
#[command(version, about = "CoCodeOp administration CLI")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create and track lead-managed workers.
    Workers {
        #[command(subcommand)]
        command: workers::WorkerCommand,
    },
    /// Publish, inspect, and download immutable artifact revisions.
    Artifacts {
        #[command(subcommand)]
        command: artifacts::ArtifactCommand,
    },
    /// Configure execution budgets and inspect runner availability.
    Scheduler {
        #[command(subcommand)]
        command: scheduler::SchedulerCommand,
    },
    /// Propose, decide, and inspect shared contracts.
    Decisions {
        #[command(subcommand)]
        command: decisions::DecisionCommand,
    },
    /// Coordinate exact paths and directory prefixes for a coding run.
    Claims {
        #[command(subcommand)]
        command: claims::ClaimsCommand,
    },
    /// Manage checkout permission envelopes and commands.
    Scope {
        #[command(subcommand)]
        command: work_scopes::ScopeCommand,
    },
    /// Register and inspect coordinated project checkouts.
    Checkout {
        #[command(subcommand)]
        command: checkouts::CheckoutCommand,
    },
    /// Manage teams and their registered members.
    Teams {
        #[command(subcommand)]
        command: teams::TeamCommand,
    },
    /// Create, inspect and coordinate tasks and execution attempts.
    #[command(visible_alias = "task")]
    Tasks {
        #[command(subcommand)]
        command: tasks::TaskCommand,
    },
    /// Inspect execution attempts and their coordination logs.
    Runs {
        #[command(subcommand)]
        command: runs::RunCommand,
    },
    /// Manage delegation grants and owner-scoped runtime templates.
    Delegation {
        #[command(subcommand)]
        command: delegation::DelegationCommand,
    },
    /// Search visible notes and messages.
    Search {
        query: String,
        #[arg(long)]
        workspace: uuid::Uuid,
        #[arg(long)]
        limit: Option<u16>,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Manage workspace channels and direct conversations.
    Channels {
        #[command(subcommand)]
        command: messages::ChannelCommand,
    },
    /// Post messages and read threads.
    Messages {
        #[command(subcommand)]
        command: messages::MessageCommand,
    },
    /// Read or acknowledge workspace deliveries.
    Inbox {
        #[command(subcommand)]
        command: inbox::InboxCommand,
    },
    /// Read the role workflow, inbox and active tasks for the current identity.
    Context(inbox::ContextArgs),
    /// Read workspace events and optionally follow new deliveries.
    Events {
        #[command(subcommand)]
        command: inbox::EventsCommand,
    },
    /// Create, revise or read workspace notes.
    Notes {
        #[command(subcommand)]
        command: notes::NoteCommand,
    },
    /// Register or inspect the identity of an owner-controlled runner.
    Runner {
        #[command(subcommand)]
        command: RunnerCommand,
    },
    /// Issue or revoke credentials you own.
    Credential {
        #[command(subcommand)]
        command: CredentialCommand,
    },
    /// Provision an agent identity in a workspace you can access.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Manage workspaces you can access.
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    /// Invite an independent workspace owner.
    User {
        #[command(subcommand)]
        command: UserCommand,
    },
    /// Initialize the first owner using the operator's local PostgreSQL connection.
    Bootstrap {
        #[arg(long)]
        user: String,
        #[arg(long)]
        workspace: String,
        #[arg(long, value_name = "PRIVATE_FILE")]
        token_file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Show the authenticated owner and their active workspace memberships.
    Whoami {
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Verify the connection, migrations, MCP agent and advertised runner.
    Doctor(doctor::DoctorOptions),
    /// Report execution, inbox, checkout and usage metrics for this owner.
    Metrics {
        #[arg(long)]
        workspace: uuid::Uuid,
        #[arg(long, default_value_t = 3600, value_parser = clap::value_parser!(u32).range(60..=604_800))]
        window_seconds: u32,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Verify an agent credential and save the local plugin's connection profile.
    Connect(connection::ConnectOptions),
    /// Run the CoCodeOp MCP connection used by local agent plugins.
    McpStdio,
    /// Check the server and the PostgreSQL schema it is serving.
    Status {
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Emit the JSON Schema for an API response.
    Schema { endpoint: Endpoint },
}

#[derive(Subcommand)]
enum CredentialCommand {
    Issue {
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long, requires = "workspace")]
        agent: Option<uuid::Uuid>,
        #[arg(long, requires = "workspace", conflicts_with = "agent")]
        runner: Option<uuid::Uuid>,
        #[arg(long)]
        workspace: Option<uuid::Uuid>,
        #[arg(long)]
        token_file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    Revoke {
        credential: uuid::Uuid,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum RunnerCommand {
    Register {
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        workspace: uuid::Uuid,
        #[arg(long)]
        name: String,
        #[arg(long)]
        token_file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    Whoami {
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum AgentCommand {
    /// Create an agent and its first agent-scoped credential.
    Create {
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        workspace: uuid::Uuid,
        #[arg(long)]
        name: String,
        #[arg(long)]
        role: AgentRole,
        #[arg(long)]
        token_file: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum WorkspaceCommand {
    /// Create a workspace owned by the current user.
    Create {
        name: String,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        idempotency_key: Option<uuid::Uuid>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum UserCommand {
    /// Create a single-use invitation and save the code in a private file.
    Invite {
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        workspace: uuid::Uuid,
        #[arg(long)]
        name: String,
        #[arg(long)]
        invitation_file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Accept an invitation and generate a separate local bearer credential.
    Join {
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        invitation_file: PathBuf,
        #[arg(long)]
        token_file: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, ValueEnum)]
enum Endpoint {
    Doctor,
    OperationsMetrics,
    CheckoutHostHealth,
    Health,
    Readiness,
    Identity,
    Agent,
    Runner,
    Note,
    Inbox,
    InboxReceipt,
    InboxDelivery,
    Context,
    Channels,
    Message,
    ThreadQuestion,
    Thread,
    Search,
    Task,
    TaskExplanation,
    RunLogs,
    TaskMutation,
    TaskPage,
    TaskResult,
    Team,
    TeamPage,
    TeamInvite,
    TeamInvitePage,
    Handoff,
    HandoffPage,
    Delegation,
    DelegationReceipt,
    RuntimeTemplate,
    Project,
    ProjectReceipt,
    Checkout,
    CheckoutReceipt,
    CheckoutGrant,
    CheckoutGrantReceipt,
    CommandTemplate,
    WorkScope,
    WorkScopeReceipt,
    Area,
    AreaReceipt,
    AccessRequest,
    AccessRequestReceipt,
    ScopeRevocationReceipt,
    PathClaimReceipt,
    PathClaimPage,
    PathHandoff,
    PathHandoffReceipt,
    Proposal,
    ProposalReceipt,
    Decision,
    AreaDecision,
    Artifact,
    ArtifactUpload,
    ArtifactChunkReceipt,
    ArtifactCleanup,
    ActivationPolicy,
    ActivationReview,
    ManagedInput,
    ManagedInputReceipt,
    ManagedWorker,
    ManagedWorkerStatus,
    ExecutionBudget,
    WorkspaceBudget,
    ExecutionControl,
    ExecutionBudgetReceipt,
    ExecutionJob,
    ExecutionReceipt,
    ExecutionLease,
    ExecutionStart,
    ExecutionStartGrant,
    ExecutionCompletion,
    RunCredential,
    RunContext,
    RunWorkContext,
    BackendEvent,
    BackendCapabilities,
    CheckoutCoordinator,
    CheckoutHead,
    CheckoutSnapshot,
    CheckoutReview,
    CheckoutReviewReceipt,
    CheckoutReleasePolicy,
    CheckoutRelease,
    GatewayRead,
    GatewaySearch,
    GatewayStatus,
    CheckoutChange,
    CheckoutCommand,
    Run,
    RunnerProfile,
}

#[tokio::main]
async fn main() -> ExitCode {
    match Args::parse().command {
        Command::Doctor(options) => doctor::run(options).await,
        Command::Metrics {
            workspace,
            window_seconds,
            url,
            json,
        } => print_receipt(
            identity::authenticated_post::<office_protocol::OperationsMetrics>(
                url,
                "/v1/operations/metrics",
                &office_protocol::ReadOperationsMetrics {
                    workspace_id: WorkspaceId(workspace),
                    window_seconds,
                },
            )
            .await,
            json,
            |metrics| {
                format!(
                    "{} queued · {} active · {} waiting · {} blocked · {} awaiting reconciliation\n{} addressed deliveries pending\nInput {} tokens · output {} tokens · {} attempts with unknown cost",
                    metrics.executions.queued_jobs,
                    metrics.executions.running_attempts,
                    metrics.executions.waiting_input_attempts,
                    metrics.executions.blocked_tasks,
                    metrics.executions.reconciling_jobs,
                    metrics.inbox.pending_addressed,
                    metrics.usage.input_tokens,
                    metrics.usage.output_tokens,
                    metrics.usage.unknown_cost_attempts,
                )
            },
        ),
        Command::Connect(options) => connection::run(options).await,
        Command::McpStdio => match mcp_stdio::serve().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        },
        Command::Runs { command } => runs::run(command).await,
        Command::Workers { command } => workers::run(command).await,
        Command::Artifacts { command } => artifacts::run(command).await,
        Command::Scheduler { command } => scheduler::run(command).await,
        Command::Decisions { command } => decisions::run(command).await,
        Command::Claims { command } => claims::run(command).await,
        Command::Scope { command } => work_scopes::run(command).await,
        Command::Checkout { command } => checkouts::run(command).await,
        Command::Teams { command } => teams::run(command).await,
        Command::Tasks { command } => tasks::run(command).await,
        Command::Delegation { command } => delegation::run(command).await,
        Command::Search {
            workspace,
            query,
            limit,
            url,
            json,
        } => print_receipt(
            identity::authenticated_post::<office_protocol::SearchResult>(
                url,
                "/v1/search",
                &office_protocol::Search {
                    workspace_id: WorkspaceId(workspace),
                    query,
                    limit,
                },
            )
            .await,
            json,
            |result| {
                result
                    .hits
                    .iter()
                    .map(|hit| hit.excerpt.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            },
        ),
        Command::Channels { command } => messages::run_channels(command).await,
        Command::Messages { command } => messages::run_messages(command).await,
        Command::Inbox { command } => inbox::run(command).await,
        Command::Context(args) => inbox::run_context(args).await,
        Command::Events { command } => inbox::run_events(command).await,
        Command::Notes { command } => notes::run(command).await,
        Command::Runner {
            command:
                RunnerCommand::Register {
                    url,
                    workspace,
                    name,
                    token_file,
                    json,
                },
        } => print_receipt(
            identity::register_runner(url, WorkspaceId(workspace), name, &token_file).await,
            json,
            |runner| format!("Registered {} ({})", runner.name, runner.runner_id.0),
        ),
        Command::Runner {
            command: RunnerCommand::Whoami { url, json },
        } => print_receipt(identity::whoami_runner(url).await, json, |runner| {
            format!("{} ({})", runner.name, runner.runner_id.0)
        }),
        Command::Credential {
            command:
                CredentialCommand::Issue {
                    url,
                    agent,
                    runner,
                    workspace,
                    token_file,
                    json,
                },
        } => {
            let subject = match (workspace, agent, runner) {
                (None, None, None) => CredentialSubject::User,
                (Some(workspace), Some(agent), None) => CredentialSubject::Agent {
                    agent_id: AgentId(agent),
                    workspace_id: WorkspaceId(workspace),
                },
                (Some(workspace), None, Some(runner)) => CredentialSubject::Runner {
                    runner_id: RunnerId(runner),
                    workspace_id: WorkspaceId(workspace),
                },
                _ => {
                    eprintln!("Choose an agent or runner when supplying a workspace");
                    return ExitCode::FAILURE;
                }
            };
            print_receipt(
                identity::issue_credential(url, subject, &token_file).await,
                json,
                |receipt| format!("Issued credential {}", receipt.credential_id.0),
            )
        }
        Command::Credential {
            command:
                CredentialCommand::Revoke {
                    credential,
                    url,
                    json,
                },
        } => print_receipt(
            identity::revoke_credential(url, CredentialId(credential)).await,
            json,
            |receipt| {
                format!(
                    "Revoked credential {} at {}",
                    receipt.credential_id.0, receipt.revoked_at
                )
            },
        ),
        Command::Agent {
            command:
                AgentCommand::Create {
                    url,
                    workspace,
                    name,
                    role,
                    token_file,
                    json,
                },
        } => {
            match identity::create_agent(url, WorkspaceId(workspace), name, role, &token_file).await
            {
                Ok(agent) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string(&agent).expect("agent is serializable")
                        );
                    } else {
                        println!(
                            "{} ({}) · {} · {}",
                            agent.display_name,
                            agent.agent_id.0,
                            agent.role.as_str(),
                            agent.workspace_name
                        );
                    }
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Workspace {
            command:
                WorkspaceCommand::Create {
                    name,
                    url,
                    idempotency_key,
                    json,
                },
        } => {
            let key = idempotency_key.unwrap_or_else(uuid::Uuid::new_v4);
            match identity::create_workspace(url, name, key).await {
                Ok(workspace) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string(&workspace).expect("workspace is serializable")
                        );
                    } else {
                        println!("{} ({})", workspace.name, workspace.workspace_id.0);
                    }
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::User {
            command:
                UserCommand::Invite {
                    url,
                    workspace,
                    name,
                    invitation_file,
                    json,
                },
        } => match identity::invite(url, WorkspaceId(workspace), name, &invitation_file).await {
            Ok(invitation) => {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string(&invitation).expect("invitation is serializable")
                    );
                } else {
                    println!(
                        "Invitation {} expires at {}",
                        invitation.invitation_id.0, invitation.expires_at
                    );
                }
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        },
        Command::User {
            command:
                UserCommand::Join {
                    url,
                    invitation_file,
                    token_file,
                    json,
                },
        } => print_identity(
            identity::join(url, &invitation_file, &token_file).await,
            json,
        ),
        Command::Bootstrap {
            user,
            workspace,
            token_file,
            json,
        } => print_identity(
            identity::bootstrap(&user, &workspace, &token_file)
                .await
                .map_err(|error| error.to_string()),
            json,
        ),
        Command::Whoami { url, json } => print_identity(identity::whoami(url).await, json),
        Command::Status { url, json } => match show_status(url, json).await {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::FAILURE,
            Err(message) => {
                eprintln!("{message}");
                ExitCode::FAILURE
            }
        },
        Command::Schema { endpoint } => {
            let schema = match endpoint {
                Endpoint::Health => schema_for!(Reply<Health>),
                Endpoint::Readiness => schema_for!(Reply<Readiness>),
                Endpoint::Doctor => schema_for!(office_protocol::DoctorReport),
                Endpoint::OperationsMetrics => {
                    schema_for!(Reply<office_protocol::OperationsMetrics>)
                }
                Endpoint::CheckoutHostHealth => {
                    schema_for!(Reply<office_protocol::CheckoutHostHealth>)
                }
                Endpoint::Identity => schema_for!(Reply<Identity>),
                Endpoint::Agent => schema_for!(Reply<AgentIdentity>),
                Endpoint::Runner => schema_for!(Reply<office_protocol::RunnerIdentity>),
                Endpoint::Note => schema_for!(Reply<office_protocol::NoteRevision>),
                Endpoint::Inbox => schema_for!(Reply<office_protocol::InboxPage>),
                Endpoint::InboxReceipt => schema_for!(Reply<office_protocol::InboxReceipt>),
                Endpoint::InboxDelivery => schema_for!(Reply<office_protocol::InboxDelivery>),
                Endpoint::Context => schema_for!(Reply<office_protocol::ActorContext>),
                Endpoint::Channels => schema_for!(Reply<office_protocol::ChannelPage>),
                Endpoint::Message => schema_for!(Reply<office_protocol::Message>),
                Endpoint::ThreadQuestion => schema_for!(Reply<office_protocol::ThreadQuestion>),
                Endpoint::Thread => schema_for!(Reply<office_protocol::ThreadPage>),
                Endpoint::Search => schema_for!(Reply<office_protocol::SearchResult>),
                Endpoint::Task => schema_for!(Reply<office_protocol::TaskView>),
                Endpoint::TaskExplanation => schema_for!(Reply<office_protocol::TaskExplanation>),
                Endpoint::RunLogs => schema_for!(Reply<office_protocol::RunLog>),
                Endpoint::TaskMutation => schema_for!(Reply<office_protocol::TaskMutation>),
                Endpoint::TaskPage => schema_for!(Reply<office_protocol::TaskPage>),
                Endpoint::TaskResult => schema_for!(Reply<office_protocol::TaskResult>),
                Endpoint::Team => schema_for!(Reply<office_protocol::TeamView>),
                Endpoint::TeamPage => schema_for!(Reply<office_protocol::TeamPage>),
                Endpoint::TeamInvite => schema_for!(Reply<office_protocol::TeamInviteMutation>),
                Endpoint::TeamInvitePage => schema_for!(Reply<office_protocol::TeamInvitePage>),
                Endpoint::Handoff => schema_for!(Reply<office_protocol::HandoffMutation>),
                Endpoint::HandoffPage => schema_for!(Reply<office_protocol::HandoffPage>),
                Endpoint::Delegation => schema_for!(Reply<office_protocol::DelegationGrant>),
                Endpoint::DelegationReceipt => {
                    schema_for!(Reply<office_protocol::DelegationReceipt>)
                }
                Endpoint::RuntimeTemplate => schema_for!(Reply<office_protocol::RuntimeTemplate>),
                Endpoint::Project => schema_for!(Reply<office_protocol::Project>),
                Endpoint::ProjectReceipt => schema_for!(Reply<office_protocol::ProjectReceipt>),
                Endpoint::Checkout => schema_for!(Reply<office_protocol::ProjectCheckout>),
                Endpoint::CheckoutReceipt => schema_for!(Reply<office_protocol::CheckoutReceipt>),
                Endpoint::CheckoutGrant => schema_for!(Reply<office_protocol::CheckoutGrant>),
                Endpoint::CheckoutGrantReceipt => {
                    schema_for!(Reply<office_protocol::CheckoutGrantReceipt>)
                }
                Endpoint::CommandTemplate => {
                    schema_for!(Reply<office_protocol::CommandTemplateReceipt>)
                }
                Endpoint::WorkScope => schema_for!(Reply<office_protocol::WorkScope>),
                Endpoint::WorkScopeReceipt => schema_for!(Reply<office_protocol::WorkScopeReceipt>),
                Endpoint::Area => schema_for!(Reply<office_protocol::Area>),
                Endpoint::AreaReceipt => schema_for!(Reply<office_protocol::AreaReceipt>),
                Endpoint::AccessRequest => schema_for!(Reply<office_protocol::AccessRequest>),
                Endpoint::AccessRequestReceipt => {
                    schema_for!(Reply<office_protocol::AccessRequestReceipt>)
                }
                Endpoint::ScopeRevocationReceipt => {
                    schema_for!(Reply<office_protocol::ScopeRevocationReceipt>)
                }
                Endpoint::PathClaimReceipt => schema_for!(Reply<office_protocol::PathClaimReceipt>),
                Endpoint::PathClaimPage => schema_for!(Reply<office_protocol::PathClaimPage>),
                Endpoint::PathHandoff => schema_for!(Reply<office_protocol::PathHandoff>),
                Endpoint::PathHandoffReceipt => {
                    schema_for!(Reply<office_protocol::PathHandoffReceipt>)
                }
                Endpoint::Proposal => schema_for!(Reply<office_protocol::Proposal>),
                Endpoint::ProposalReceipt => schema_for!(Reply<office_protocol::ProposalReceipt>),
                Endpoint::Decision => schema_for!(Reply<office_protocol::Decision>),
                Endpoint::AreaDecision => schema_for!(Reply<office_protocol::AreaDecision>),
                Endpoint::Artifact => schema_for!(Reply<office_protocol::Artifact>),
                Endpoint::ArtifactUpload => schema_for!(Reply<office_protocol::ArtifactUpload>),
                Endpoint::ArtifactChunkReceipt => {
                    schema_for!(Reply<office_protocol::ArtifactChunkReceipt>)
                }
                Endpoint::ArtifactCleanup => schema_for!(Reply<office_protocol::ArtifactCleanup>),
                Endpoint::ActivationPolicy => schema_for!(Reply<office_protocol::ActivationPolicy>),
                Endpoint::ActivationReview => schema_for!(office_protocol::ActivationReview),
                Endpoint::ManagedInput => schema_for!(Reply<office_protocol::ManagedInput>),
                Endpoint::ManagedInputReceipt => {
                    schema_for!(Reply<office_protocol::ManagedInputReceipt>)
                }
                Endpoint::ManagedWorker => schema_for!(Reply<office_protocol::ManagedWorker>),
                Endpoint::ManagedWorkerStatus => {
                    schema_for!(Reply<office_protocol::ManagedWorkerStatus>)
                }
                Endpoint::ExecutionBudget => {
                    schema_for!(Reply<Option<office_protocol::ExecutionBudget>>)
                }
                Endpoint::WorkspaceBudget => {
                    schema_for!(Reply<Option<office_protocol::WorkspaceBudget>>)
                }
                Endpoint::ExecutionControl => {
                    schema_for!(Reply<office_protocol::ExecutionControl>)
                }
                Endpoint::ExecutionBudgetReceipt => {
                    schema_for!(Reply<office_protocol::ExecutionBudgetReceipt>)
                }
                Endpoint::ExecutionJob => schema_for!(Reply<office_protocol::ExecutionJob>),
                Endpoint::ExecutionReceipt => schema_for!(Reply<office_protocol::ExecutionReceipt>),
                Endpoint::ExecutionLease => {
                    schema_for!(Reply<Option<office_protocol::ExecutionLease>>)
                }
                Endpoint::ExecutionStart => {
                    schema_for!(Reply<office_protocol::ExecutionStartReceipt>)
                }
                Endpoint::ExecutionStartGrant => {
                    schema_for!(Reply<office_protocol::ExecutionStartGrant>)
                }
                Endpoint::ExecutionCompletion => {
                    schema_for!(Reply<office_protocol::ExecutionCompletionReceipt>)
                }
                Endpoint::Run => schema_for!(Reply<office_protocol::Run>),
                Endpoint::RunCredential => {
                    schema_for!(Reply<office_protocol::RunCredentialReceipt>)
                }
                Endpoint::RunContext => schema_for!(office_protocol::RunContextView),
                Endpoint::RunWorkContext => schema_for!(office_protocol::RunWorkContext),
                Endpoint::BackendEvent => schema_for!(office_protocol::BackendEvent),
                Endpoint::BackendCapabilities => schema_for!(office_protocol::BackendCapabilities),
                Endpoint::GatewayRead => schema_for!(office_protocol::GatewayReadReceipt),
                Endpoint::GatewaySearch => schema_for!(office_protocol::GatewaySearchReceipt),
                Endpoint::GatewayStatus => schema_for!(office_protocol::GatewayStatusReceipt),
                Endpoint::CheckoutChange => schema_for!(office_protocol::CheckoutChangeReceipt),
                Endpoint::CheckoutCommand => schema_for!(office_protocol::CheckoutCommand),
                Endpoint::CheckoutCoordinator => {
                    schema_for!(Reply<office_protocol::CheckoutCoordinatorLease>)
                }
                Endpoint::CheckoutHead => schema_for!(Reply<office_protocol::CheckoutContentHead>),
                Endpoint::CheckoutSnapshot => schema_for!(Reply<office_protocol::CheckoutSnapshot>),
                Endpoint::CheckoutReview => schema_for!(Reply<office_protocol::CheckoutReview>),
                Endpoint::CheckoutReviewReceipt => {
                    schema_for!(Reply<office_protocol::CheckoutReviewReceipt>)
                }
                Endpoint::CheckoutReleasePolicy => {
                    schema_for!(Reply<office_protocol::CheckoutReleasePolicy>)
                }
                Endpoint::CheckoutRelease => schema_for!(Reply<office_protocol::CheckoutRelease>),
                Endpoint::RunnerProfile => schema_for!(Reply<office_protocol::RunnerProfile>),
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&schema).expect("schema is serializable")
            );
            ExitCode::SUCCESS
        }
    }
}

fn print_receipt<T: serde::Serialize>(
    result: Result<T, String>,
    json: bool,
    text: impl FnOnce(&T) -> String,
) -> ExitCode {
    match result {
        Ok(result) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&result).expect("receipt is serializable")
                );
            } else {
                println!("{}", text(&result));
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

async fn show_status(url: Url, json: bool) -> Result<bool, &'static str> {
    let response = request(Method::GET, url, "/readyz")?
        .send()
        .await
        .map_err(|_| "Could not reach the CoCodeOp server")?;
    let status = response.status();
    let body: Reply<Readiness> = response
        .json()
        .await
        .map_err(|_| "Invalid readiness response")?;
    let ready = status.is_success() && matches!(&body.outcome, Outcome::Success { .. });
    if json {
        println!(
            "{}",
            serde_json::to_string(&body).map_err(|_| "Cannot encode readiness response")?
        );
    } else {
        match &body.outcome {
            Outcome::Success { result } if ready => println!(
                "Ready · installation {} · API {} · schema {}",
                result.installation_id.0, result.api_version, result.schema_version,
            ),
            Outcome::Failure { error } => {
                eprintln!("{} (request {})", error.message, body.request_id.0)
            }
            _ => return Err("Unexpected HTTP status on the readiness result"),
        }
    }
    Ok(ready)
}

fn request(method: Method, url: Url, path: &str) -> Result<reqwest::RequestBuilder, &'static str> {
    Ok(server_client()?.request(method, server_url(&url, path)?))
}

fn server_url(url: &Url, path: &str) -> Result<Url, &'static str> {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("Use an HTTP(S) server URL without inline credentials");
    }
    url.join(path).map_err(|_| "Invalid server URL")
}

fn server_client() -> Result<reqwest::Client, &'static str> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| "Cannot create an HTTP client")
}

fn print_identity(result: Result<Identity, String>, json: bool) -> ExitCode {
    let identity = match result {
        Ok(identity) => identity,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    if json {
        println!(
            "{}",
            serde_json::to_string(&identity).expect("identity is serializable")
        );
    } else {
        println!("{} ({})", identity.display_name, identity.user_id.0);
        for membership in identity.memberships {
            println!(
                "{} ({}, {:?})",
                membership.name, membership.workspace_id.0, membership.role
            );
        }
    }
    ExitCode::SUCCESS
}
