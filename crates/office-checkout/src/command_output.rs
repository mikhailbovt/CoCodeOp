use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use office_domain::SourceManifest;
use office_protocol::{
    CheckoutChange, CheckoutContent, CommandOutputFile, CommandTemplateSpec, FileEncoding,
    GatewayFile, MAX_COMMAND_OUTPUT_BYTES, MAX_COMMAND_OUTPUT_FILES, SourceEntry,
};
use rustix::fd::OwnedFd;
use sha2::{Digest, Sha256};

use crate::{
    SnapshotError,
    linux::{copy_source_file, open_root, snapshot_source},
};

#[derive(Default)]
pub struct CommandFiles {
    pub outputs: Vec<CommandOutputFile>,
    pub staged_changes: Vec<CheckoutChange>,
    pub truncated: bool,
}

pub fn command_files(spec: &CommandTemplateSpec, sealed: &Path, scratch: &Path) -> CommandFiles {
    if spec.scratch_outputs.is_empty() {
        return CommandFiles::default();
    }
    let selected = SourceManifest {
        include: spec.scratch_outputs.clone(),
        exclude: Vec::new(),
    };
    let result = if spec.writable_staging {
        compare_staging(&selected, sealed, &scratch.join("source"))
    } else {
        collect_outputs(&selected, scratch)
    };
    result.unwrap_or_else(|_| CommandFiles {
        truncated: true,
        ..CommandFiles::default()
    })
}

fn collect_outputs(
    selected: &SourceManifest,
    scratch: &Path,
) -> Result<CommandFiles, SnapshotError> {
    let (root, _, _) = open_root(scratch)?;
    let (_, entries) = snapshot_source(&root, selected)?;
    let mut result = CommandFiles::default();
    let mut remaining = MAX_COMMAND_OUTPUT_BYTES;
    for entry in entries {
        if result.outputs.len() == MAX_COMMAND_OUTPUT_FILES || entry.size_bytes > remaining {
            result.truncated = true;
            break;
        }
        let data = read_payload(&root, &entry)?;
        remaining -= entry.size_bytes;
        result.outputs.push(CommandOutputFile {
            path: entry.path,
            file: GatewayFile {
                sha256: entry.sha256,
                size_bytes: entry.size_bytes,
                encoding: data.encoding,
                content: data.content,
            },
        });
    }
    Ok(result)
}

fn compare_staging(
    selected: &SourceManifest,
    sealed: &Path,
    scratch: &Path,
) -> Result<CommandFiles, SnapshotError> {
    let (old_root, _, _) = open_root(sealed)?;
    let (new_root, _, _) = open_root(scratch)?;
    let entries = |root| -> Result<_, SnapshotError> {
        Ok(snapshot_source(root, selected)?
            .1
            .into_iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect::<BTreeMap<_, _>>())
    };
    let before = entries(&old_root)?;
    let after = entries(&new_root)?;
    let paths: BTreeSet<_> = before.keys().chain(after.keys()).collect();
    let mut result = CommandFiles::default();
    let mut remaining = MAX_COMMAND_OUTPUT_BYTES;
    for path in paths {
        let old = before.get(path);
        let new = after.get(path);
        if let (Some(old), Some(new)) = (old, new)
            && old.sha256 == new.sha256
            && old.executable == new.executable
        {
            continue;
        }
        if result.staged_changes.len() == MAX_COMMAND_OUTPUT_FILES
            || new.is_some_and(|file| file.size_bytes > remaining)
        {
            result.truncated = true;
            break;
        }
        let change = match (old, new) {
            (Some(old), Some(new)) => CheckoutChange::Replace {
                path: path.clone(),
                before_sha256: old.sha256.clone(),
                file: read_payload(&new_root, new)?,
                executable: (old.executable != new.executable).then_some(new.executable),
            },
            (None, Some(new)) => CheckoutChange::Create {
                path: path.clone(),
                file: read_payload(&new_root, new)?,
                executable: new.executable,
            },
            (Some(old), None) => CheckoutChange::Delete {
                path: path.clone(),
                before_sha256: old.sha256.clone(),
            },
            _ => return Err(SnapshotError::SourceUnavailable),
        };
        remaining -= new.map_or(0, |file| file.size_bytes);
        result.staged_changes.push(change);
    }
    Ok(result)
}

fn read_payload(root: &OwnedFd, entry: &SourceEntry) -> Result<CheckoutContent, SnapshotError> {
    let mut bytes = Vec::new();
    let (size, executable) =
        copy_source_file(root, entry.path.as_str(), entry.size_bytes, &mut bytes)?;
    if entry.size_bytes != size
        || entry.executable != executable
        || hex::encode(Sha256::digest(&bytes)) != entry.sha256
    {
        return Err(SnapshotError::SourceUnavailable);
    }
    let (encoding, content) = match std::str::from_utf8(&bytes) {
        Ok(text) if !text.contains('\0') => (FileEncoding::Utf8, text.to_owned()),
        _ => (FileEncoding::Base64, STANDARD.encode(bytes)),
    };
    Ok(CheckoutContent { encoding, content })
}
