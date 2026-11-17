use std::{path::PathBuf, process::ExitCode};

use clap::Subcommand;
use office_domain::{NoteId, WorkspaceId};
use office_protocol::{GetNote, NoteRevision, UpsertNote};
use reqwest::Url;

use crate::{
    identity::{authenticated_post, post_file},
    print_receipt,
};

#[derive(Subcommand)]
pub(crate) enum NoteCommand {
    /// Create or revise a note from a JSON request file.
    Upsert {
        #[arg(long)]
        file: PathBuf,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
    /// Read the latest note or a particular immutable revision.
    Get {
        #[arg(long)]
        workspace: uuid::Uuid,
        #[arg(long)]
        id: uuid::Uuid,
        #[arg(long)]
        revision: Option<i64>,
        #[arg(long, default_value = "http://127.0.0.1:8400")]
        url: Url,
        #[arg(long)]
        json: bool,
    },
}

pub(crate) async fn run(command: NoteCommand) -> ExitCode {
    let (result, json) = match command {
        NoteCommand::Upsert { url, file, json } => (
            post_file::<UpsertNote, NoteRevision>(&file, url, "/v1/notes").await,
            json,
        ),
        NoteCommand::Get {
            workspace,
            id,
            revision,
            url,
            json,
        } => (
            authenticated_post(
                url,
                "/v1/notes/read",
                &GetNote {
                    workspace_id: WorkspaceId(workspace),
                    note_id: NoteId(id),
                    revision,
                },
            )
            .await,
            json,
        ),
    };
    print_receipt(result, json, |note| {
        format!(
            "{} · {} · revision {}\n{}",
            note.title, note.note_id.0, note.revision, note.body,
        )
    })
}
