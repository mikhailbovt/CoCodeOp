use std::time::{Duration, Instant};

use office_domain::{PathAccess, SourceManifest, SourcePath};
use office_protocol::{
    GatewayReadOutcome, GatewaySearchMatch, GatewayStatusEntry, MAX_GATEWAY_READ_BYTES,
    MAX_GATEWAY_SNIPPET_BYTES, MAX_GATEWAY_STATUS_HASH_BYTES, MAX_ROOT_ANCESTORS, MAX_SOURCE_FILES,
};
use rustix::{
    fd::OwnedFd,
    fs::{self, AtFlags, Dir, FileType, Mode, OFlags},
    io::Errno,
};
use sha2::{Digest, Sha256};

use super::{
    CheckoutCoordinator, CheckoutReadError,
    linux::{InspectionError, copy_source_file, descendant_resolution, open_source_file},
};

const MAX_DIRECTORY_ENTRIES: usize = 131_072;
const MAX_SEARCH_FILES: usize = 256;

impl CheckoutCoordinator {
    pub fn search_code(
        &self,
        manifest: &SourceManifest,
        access: &PathAccess,
        under: Option<&SourcePath>,
        needle: &str,
        limit: usize,
    ) -> Result<GatewayReadOutcome, CheckoutReadError> {
        let files = paths(&self.root, manifest, access, under)?;
        let mut matches = Vec::new();
        let mut truncated = files.len() > MAX_SEARCH_FILES;
        for path in files.into_iter().take(MAX_SEARCH_FILES) {
            let bytes = match self.read_file(manifest, access, &path, MAX_GATEWAY_READ_BYTES) {
                Ok(bytes) => bytes,
                Err(CheckoutReadError::Limit | CheckoutReadError::Changed) => {
                    truncated = true;
                    continue;
                }
                Err(CheckoutReadError::Forbidden | CheckoutReadError::Missing) => continue,
                Err(error) => return Err(error),
            };
            let Ok(text) = std::str::from_utf8(&bytes) else {
                continue;
            };
            if text.contains('\0') {
                continue;
            }
            let mut sha = None;
            for (index, line) in text.lines().enumerate() {
                let Some(offset) = line.find(needle) else {
                    continue;
                };
                if matches.len() == limit {
                    return Ok(GatewayReadOutcome::Search {
                        matches,
                        truncated: true,
                    });
                }
                let left = line.floor_char_boundary(offset.saturating_sub(24));
                let right =
                    line.floor_char_boundary((left + MAX_GATEWAY_SNIPPET_BYTES).min(line.len()));
                matches.push(GatewaySearchMatch {
                    path: path.clone(),
                    line_number: (index + 1) as u32,
                    file_sha256: sha
                        .get_or_insert_with(|| hex::encode(Sha256::digest(&bytes)))
                        .clone(),
                    snippet: line[left..right].to_owned(),
                });
            }
        }
        Ok(GatewayReadOutcome::Search { matches, truncated })
    }

    pub fn checkout_status(
        &self,
        manifest: &SourceManifest,
        access: &PathAccess,
        under: Option<&SourcePath>,
        after: Option<&SourcePath>,
        limit: usize,
    ) -> Result<GatewayReadOutcome, CheckoutReadError> {
        let candidates = paths(&self.root, manifest, access, under)?;
        let mut candidates = candidates
            .into_iter()
            .filter(|path| after.is_none_or(|before| path > before))
            .peekable();
        let mut files = Vec::new();
        let mut remaining = MAX_GATEWAY_STATUS_HASH_BYTES;
        while files.len() < limit
            && let Some(path) = candidates.next()
        {
            let mut sha = Sha256::new();
            match copy_source_file(&self.root, path.as_str(), remaining, &mut sha) {
                Ok((size, executable)) => {
                    remaining -= size;
                    files.push(GatewayStatusEntry {
                        path,
                        size_bytes: size,
                        executable,
                        sha256: Some(hex::encode(sha.finalize())),
                    });
                }
                Err(InspectionError::Limit) => match open_source_file(&self.root, path.as_str()) {
                    Ok((_, stat, size_bytes)) if size_bytes > MAX_GATEWAY_STATUS_HASH_BYTES => {
                        files.push(GatewayStatusEntry {
                            path,
                            size_bytes,
                            executable: stat.st_mode & 0o111 != 0,
                            sha256: None,
                        });
                    }
                    Ok((_, _, size_bytes)) if size_bytes > remaining && !files.is_empty() => {
                        let next_after = files.last().map(|entry| entry.path.clone());
                        return Ok(GatewayReadOutcome::Status { files, next_after });
                    }
                    Ok(_) => return Err(CheckoutReadError::Changed),
                    Err(
                        InspectionError::Path(
                            Errno::NOENT | Errno::LOOP | Errno::XDEV | Errno::NOTDIR,
                        )
                        | InspectionError::UnsupportedEntry,
                    ) => (),
                    Err(_) => return Err(CheckoutReadError::Unavailable),
                },
                Err(
                    InspectionError::Path(Errno::NOENT | Errno::LOOP | Errno::XDEV | Errno::NOTDIR)
                    | InspectionError::UnsupportedEntry,
                ) => (),
                Err(InspectionError::SourceChanged) => return Err(CheckoutReadError::Changed),
                Err(_) => return Err(CheckoutReadError::Unavailable),
            }
        }
        let next_after = candidates
            .peek()
            .and_then(|_| files.last())
            .map(|entry| entry.path.clone());
        Ok(GatewayReadOutcome::Status { files, next_after })
    }
}

pub(super) fn paths(
    root: &OwnedFd,
    manifest: &SourceManifest,
    access: &PathAccess,
    under: Option<&SourcePath>,
) -> Result<Vec<SourcePath>, CheckoutReadError> {
    let mut walk = ScopeWalk {
        manifest,
        access,
        files: Vec::new(),
        visited: 0,
        until: Instant::now() + Duration::from_secs(2),
    };
    if let Some(prefix) = under {
        if !walk.descend(prefix) {
            return Err(CheckoutReadError::Forbidden);
        }
        let directory = match open_directory(root, prefix.as_str()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(Vec::new()),
            Err(Errno::LOOP | Errno::XDEV | Errno::NOTDIR) => {
                return Err(CheckoutReadError::Forbidden);
            }
            Err(_) => return Err(CheckoutReadError::Unavailable),
        };
        walk.visit(
            &directory,
            &format!("{}/", prefix.as_str()),
            prefix.as_str().split('/').count(),
        )?;
    } else {
        walk.visit(root, "", 0)?;
    }
    walk.files.sort_unstable();
    Ok(walk.files)
}

struct ScopeWalk<'a> {
    manifest: &'a SourceManifest,
    access: &'a PathAccess,
    files: Vec<SourcePath>,
    visited: usize,
    until: Instant,
}

impl ScopeWalk<'_> {
    fn descend(&self, path: &SourcePath) -> bool {
        self.manifest.descend(path) && self.access.descend(path)
    }

    fn visit(
        &mut self,
        directory: &OwnedFd,
        prefix: &str,
        depth: usize,
    ) -> Result<(), CheckoutReadError> {
        if depth >= MAX_ROOT_ANCESTORS {
            return Err(CheckoutReadError::Limit);
        }
        let fd = open_directory(directory, ".").map_err(|_| CheckoutReadError::Unavailable)?;
        let stream = Dir::new(fd).map_err(|_| CheckoutReadError::Unavailable)?;
        for item in stream {
            let item = item.map_err(|_| CheckoutReadError::Unavailable)?;
            self.visited += 1;
            if self.visited > MAX_DIRECTORY_ENTRIES || Instant::now() >= self.until {
                return Err(CheckoutReadError::Unavailable);
            }
            let name = item.file_name();
            let Ok(value) = name.to_str() else {
                continue;
            };
            let Ok(path) = format!("{prefix}{value}").parse::<SourcePath>() else {
                continue;
            };
            let leaf = self.manifest.allows(&path) && self.access.allows(&path);
            let branch = self.descend(&path);
            if !leaf && !branch {
                continue;
            }
            let info = match fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(value) => value,
                Err(Errno::NOENT) => continue,
                Err(_) => return Err(CheckoutReadError::Unavailable),
            };
            match FileType::from_raw_mode(info.st_mode) {
                FileType::Directory if branch => match open_directory(directory, name) {
                    Ok(child) => self.visit(&child, &format!("{}/", path.as_str()), depth + 1)?,
                    Err(Errno::NOENT | Errno::LOOP | Errno::XDEV | Errno::NOTDIR) => (),
                    Err(_) => return Err(CheckoutReadError::Unavailable),
                },
                FileType::RegularFile if leaf && info.st_nlink == 1 => {
                    if self.files.len() >= MAX_SOURCE_FILES as usize {
                        return Err(CheckoutReadError::Limit);
                    }
                    self.files.push(path);
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn open_directory(root: &OwnedFd, path: impl rustix::path::Arg) -> Result<OwnedFd, Errno> {
    fs::openat2(
        root,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        descendant_resolution(),
    )
}
