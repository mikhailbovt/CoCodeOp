use std::{process::ExitCode, time::Duration};

use clap::{Args, Subcommand};
use office_domain::{TaskId, WorkspaceId};
use office_protocol::{
    AckInbox, ActorContext, GetContext, GetInboxDelivery, InboxDelivery, InboxPage, InboxReceipt,
    ReadInbox,
};
use reqwest::Url;
use uuid::Uuid;

use crate::{identity::authenticated_post, print_receipt};

#[derive(Args)]
pub(crate) struct PageArgs {
    #[arg(long)]
    workspace: Uuid,
    #[arg(long)]
    cursor: Option<Uuid>,
    #[arg(long)]
    limit: Option<u16>,
    #[arg(long, default_value = "http://127.0.0.1:8400")]
    url: Url,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub(crate) struct ContextArgs {
    #[arg(long)]
    workspace: Uuid,
    #[arg(long)]
    inbox_cursor: Option<Uuid>,
    #[arg(long)]
    tasks_after: Option<Uuid>,
    #[arg(long)]
    limit: Option<u16>,
    #[arg(long, default_value = "http://127.0.0.1:8400")]
    url: Url,
    #[arg(long)]
    json: bool,
}

pub(crate) async fn run_context(args: ContextArgs) -> ExitCode {
    let request = GetContext {
        workspace_id: WorkspaceId(args.workspace),
        inbox_cursor: args.inbox_cursor,
        tasks_after: args.tasks_after.map(TaskId),
        limit: args.limit,
    };
    print_receipt(
        authenticated_post::<ActorContext>(args.url, "/v1/context", &request).await,
        args.json,
        |view| {
            let mut lines = view.workflow.steps.clone();
            lines.extend(
                view.diagnostics
                    .iter()
                    .map(|issue| issue.next_action.clone()),
            );
            if let Some(page) = &view.inbox {
                lines.push(display_page(page));
            }
            lines.extend(view.tasks.items.iter().map(|task| {
                format!(
                    "{} · {} · {}",
                    task.task_id.0,
                    task.status.as_str(),
                    task.objective
                )
            }));
            if let Some(after) = view.tasks.next_after {
                lines.push(format!("Next task page --tasks-after {}", after.0));
            }
            lines.join("\n")
        },
    )
}

#[derive(Subcommand)]
pub(crate) enum InboxCommand {
    /// Read one page of visible events and return a resume cursor.
    Read(PageArgs),
    /// Inspect whether the exact recipient has read or acknowledged an addressed event.
    Delivery {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        event: Uuid,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Acknowledge processing of one event for your identity.
    Ack {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        event: Uuid,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum EventsCommand {
    Tail {
        #[command(flatten)]
        page: PageArgs,
        #[arg(long)]
        follow: bool,
    },
}

pub(crate) async fn run(command: InboxCommand) -> ExitCode {
    match command {
        InboxCommand::Read(page) => read(page, false).await,
        InboxCommand::Delivery {
            workspace,
            event,
            url,
            json,
        } => {
            let request = GetInboxDelivery {
                workspace_id: WorkspaceId(workspace),
                event_id: event,
            };
            print_receipt(
                authenticated_post::<InboxDelivery>(url, "/v1/inbox/delivery", &request).await,
                json,
                |delivery| format!("{} · {}", delivery.event_id, delivery.state.as_str()),
            )
        }
        InboxCommand::Ack {
            workspace,
            event,
            url,
            json,
        } => {
            let request = AckInbox {
                workspace_id: WorkspaceId(workspace),
                event_id: event,
            };
            print_receipt(
                authenticated_post::<InboxReceipt>(url, "/v1/inbox/ack", &request).await,
                json,
                |receipt| {
                    format!(
                        "Acknowledged {} at {}",
                        receipt.event_id, receipt.acknowledged_at
                    )
                },
            )
        }
    }
}

pub(crate) async fn run_events(command: EventsCommand) -> ExitCode {
    let EventsCommand::Tail { page, follow } = command;
    if follow {
        tokio::select! {
            result = read(page, true) => result,
            _ = tokio::signal::ctrl_c() => ExitCode::SUCCESS,
        }
    } else {
        read(page, false).await
    }
}

async fn read(page: PageArgs, follow: bool) -> ExitCode {
    let mut request = ReadInbox {
        workspace_id: WorkspaceId(page.workspace),
        cursor: page.cursor,
        limit: page.limit,
    };
    let mut first_page = true;
    loop {
        let response =
            authenticated_post::<InboxPage>(page.url.clone(), "/v1/inbox/read", &request).await;
        let Ok(response) = response else {
            return print_receipt(response, page.json, display_page);
        };
        request.cursor = Some(response.next_cursor);
        let has_more = response.has_more;
        if first_page || !response.events.is_empty() {
            let status = print_receipt(Ok(response), page.json, display_page);
            if status != ExitCode::SUCCESS {
                return status;
            }
        }
        if !follow {
            return ExitCode::SUCCESS;
        }
        first_page = false;
        if !has_more {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

fn display_page(page: &InboxPage) -> String {
    let mut lines: Vec<String> = page
        .events
        .iter()
        .map(|event| {
            format!(
                "{} · {} · {} {}",
                event.created_at, event.event_id, event.event_type, event.payload
            )
        })
        .collect();
    lines.push(format!("Next cursor {}", page.next_cursor));
    lines.join("\n")
}
