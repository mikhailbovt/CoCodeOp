use std::{path::PathBuf, process::ExitCode};

use clap::Subcommand;
use office_domain::{ChannelId, MessageId, WorkspaceId};
use office_protocol::{
    AnswerQuestion, AskQuestion, Channel, ChannelPage, CreateChannel, GetQuestion, ListChannels,
    Message, ReadThread, SendMessage, ThreadPage, ThreadQuestion,
};
use reqwest::Url;
use uuid::Uuid;

use crate::{
    identity::{authenticated_post, post_file},
    print_receipt,
};

#[derive(Subcommand)]
pub(crate) enum ChannelCommand {
    /// Create a channel or a direct conversation from JSON.
    Create {
        #[arg(long)]
        file: PathBuf,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// List visible channels.
    List {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        after: Option<Uuid>,
        #[arg(long)]
        limit: Option<u16>,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum MessageCommand {
    /// Post to a channel or reply to a message from a JSON request.
    Send {
        #[arg(long)]
        file: PathBuf,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Ask an exact recipient a question from a JSON request.
    Ask {
        #[arg(long)]
        file: PathBuf,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Answer a question addressed to the current identity.
    Answer {
        #[arg(long)]
        file: PathBuf,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Inspect a question and its inbox and answer references.
    Question {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        question: Uuid,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Read a thread or the root messages of a channel.
    Thread {
        #[arg(long)]
        workspace: Uuid,
        #[arg(long)]
        channel: Uuid,
        #[arg(long)]
        thread: Option<Uuid>,
        #[arg(long)]
        after: Option<Uuid>,
        #[arg(long)]
        limit: Option<u16>,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
}

pub(crate) async fn run_channels(command: ChannelCommand) -> ExitCode {
    match command {
        ChannelCommand::Create { file, url, json } => print_receipt(
            post_file::<CreateChannel, Channel>(&file, url, "/v1/channels").await,
            json,
            |channel| format!("{} · {}", channel.channel_id.0, channel.name),
        ),
        ChannelCommand::List {
            workspace,
            after,
            limit,
            url,
            json,
        } => print_receipt(
            authenticated_post::<ChannelPage>(
                url,
                "/v1/channels/list",
                &ListChannels {
                    workspace_id: WorkspaceId(workspace),
                    after: after.map(ChannelId),
                    limit,
                },
            )
            .await,
            json,
            |page| {
                let mut rows: Vec<_> = page
                    .channels
                    .iter()
                    .map(|channel| format!("{} · {}", channel.channel_id.0, channel.name))
                    .collect();
                if let Some(after) = page.next_after {
                    rows.push(format!("Next page after {}", after.0));
                }
                rows.join("\n")
            },
        ),
    }
}

pub(crate) async fn run_messages(command: MessageCommand) -> ExitCode {
    match command {
        MessageCommand::Ask { file, url, json } => print_receipt(
            post_file::<AskQuestion, ThreadQuestion>(&file, url, "/v1/messages/questions").await,
            json,
            display_question,
        ),
        MessageCommand::Answer { file, url, json } => print_receipt(
            post_file::<AnswerQuestion, ThreadQuestion>(
                &file,
                url,
                "/v1/messages/questions/answer",
            )
            .await,
            json,
            display_question,
        ),
        MessageCommand::Question {
            workspace,
            question,
            url,
            json,
        } => print_receipt(
            authenticated_post::<ThreadQuestion>(
                url,
                "/v1/messages/questions/read",
                &GetQuestion {
                    workspace_id: WorkspaceId(workspace),
                    question_id: MessageId(question),
                },
            )
            .await,
            json,
            display_question,
        ),
        MessageCommand::Send { file, url, json } => print_receipt(
            post_file::<SendMessage, Message>(&file, url, "/v1/messages").await,
            json,
            |message| {
                format!(
                    "Posted {} in thread {}",
                    message.message_id.0, message.thread_id.0
                )
            },
        ),
        MessageCommand::Thread {
            workspace,
            channel,
            thread,
            after,
            limit,
            url,
            json,
        } => print_receipt(
            authenticated_post::<ThreadPage>(
                url,
                "/v1/messages/thread",
                &ReadThread {
                    workspace_id: WorkspaceId(workspace),
                    channel_id: ChannelId(channel),
                    thread_id: thread.map(MessageId),
                    after: after.map(MessageId),
                    limit,
                },
            )
            .await,
            json,
            |page| {
                let mut lines: Vec<_> = page
                    .messages
                    .iter()
                    .map(|message| {
                        format!(
                            "{} · {}\n{}",
                            message.created_at, message.message_id.0, message.body
                        )
                    })
                    .collect();
                if let Some(after) = page.next_after {
                    lines.push(format!("Next page after {}", after.0));
                }
                lines.join("\n")
            },
        ),
    }
}

fn display_question(question: &ThreadQuestion) -> String {
    let mut view = format!(
        "Question {} · Event {} · Expires {}",
        question.question_id.0, question.question_event_id, question.expires_at
    );
    if let Some(answer) = question.answer_id {
        view.push_str(&format!("\nAnswer {}", answer.0));
    }
    view
}
