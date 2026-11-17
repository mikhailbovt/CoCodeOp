use std::{
    collections::HashMap,
    fs::File,
    io::{self, Read, Write},
    os::fd::AsRawFd,
    path::{Component, Path},
};

use office_domain::{SourceManifest, SourcePath};
use office_protocol::{
    MAX_ROOT_ANCESTORS, MAX_SOURCE_BYTES, MAX_SOURCE_FILES, RootProof, SourceBaseline, SourceEntry,
};
use rustix::{
    fd::OwnedFd,
    fs::{self, AtFlags, Dir, FileType, Mode, OFlags, ResolveFlags, Statx, StatxFlags},
};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum InspectionError {
    #[error("expected an absolute directory below the filesystem root, with no parent segments")]
    InvalidRoot,
    #[error("the checkout root is on a mount with unsupported ancestry or a nonlocal filesystem")]
    UnsupportedMount,
    #[error("the host has no valid Linux machine identity")]
    InvalidHostIdentity,
    #[error("the source manifest is invalid")]
    InvalidManifest,
    #[error("selected source contains an unsupported path or file type")]
    UnsupportedEntry,
    #[error("source inspection exceeded its file, byte, depth, or directory entry limit")]
    Limit,
    #[error("a selected source entry changed during inspection; inspect again")]
    SourceChanged,
    #[error("could not read the local checkout: {0}")]
    Io(#[from] io::Error),
    #[error("could not resolve the local checkout with Linux filesystem controls: {0}")]
    Path(#[from] rustix::io::Errno),
    #[error("could not encode the source manifest")]
    Encoding(#[from] serde_json::Error),
}

pub struct CheckoutInspection {
    pub proof: RootProof,
    pub entries: Vec<SourceEntry>,
}

const MAX_WALK_ENTRIES: u32 = 131_072;
const MOUNT_ID: StatxFlags = StatxFlags::MNT_ID;

pub fn inspect_checkout(
    root: &Path,
    manifest: &SourceManifest,
) -> Result<CheckoutInspection, InspectionError> {
    let (directory, directory_id, ancestors) = open_root(root)?;
    let (baseline, entries) = snapshot_source(&directory, manifest)?;
    Ok(CheckoutInspection {
        proof: RootProof {
            host_fingerprint: host_fingerprint()?,
            directory_id,
            ancestors,
            baseline,
        },
        entries,
    })
}

pub(super) fn snapshot_source(
    directory: &OwnedFd,
    manifest: &SourceManifest,
) -> Result<(SourceBaseline, Vec<SourceEntry>), InspectionError> {
    if !manifest.is_valid() {
        return Err(InspectionError::InvalidManifest);
    }
    let mut walker = SourceWalker {
        manifest,
        entries: Vec::new(),
        bytes: 0,
        visited: 0,
    };
    walker.read_directory(directory, "", 0)?;
    walker.entries.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    let baseline = source_baseline(manifest, &walker.entries, walker.bytes)?;
    Ok((baseline, walker.entries))
}

pub(super) fn source_baseline(
    manifest: &SourceManifest,
    entries: &[SourceEntry],
    total_bytes: u64,
) -> Result<SourceBaseline, InspectionError> {
    let mut hash = Sha256::new();
    hash.update(b"cocodeop-source-baseline-v1\0");
    hash.update(serde_json::to_vec(manifest)?);
    hash.update(serde_json::to_vec(entries)?);
    Ok(SourceBaseline {
        manifest_sha256: hex::encode(hash.finalize()),
        file_count: entries.len() as u32,
        total_bytes,
    })
}

pub(super) fn host_fingerprint() -> Result<String, InspectionError> {
    let identity = std::fs::read_to_string("/etc/machine-id")?;
    let identity = identity.trim();
    if identity.len() != 32
        || !identity.bytes().all(|byte| byte.is_ascii_hexdigit())
        || identity.bytes().all(|byte| byte == b'0')
    {
        return Err(InspectionError::InvalidHostIdentity);
    }
    let mut hash = Sha256::new();
    hash.update(b"org.cocodeop.linux-host-v1\0");
    hash.update(identity.as_bytes());
    Ok(hex::encode(hash.finalize()))
}

pub(super) fn open_root(path: &Path) -> Result<(OwnedFd, String, Vec<String>), InspectionError> {
    let mut components = path.components();
    if components.next() != Some(Component::RootDir) || components.as_path().as_os_str().is_empty()
    {
        return Err(InspectionError::InvalidRoot);
    }
    let mut directory = fs::open(
        "/",
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")?;
    let mounts = parse_mounts(&mountinfo);
    let mut status = stat_directory(&directory)?;
    let mut ancestry = Vec::new();
    for component in components {
        let Component::Normal(part) = component else {
            return Err(InspectionError::InvalidRoot);
        };
        if ancestry.len() == MAX_ROOT_ANCESTORS {
            return Err(InspectionError::Limit);
        }
        ancestry.push(directory_key(&status));
        directory = fs::openat2(
            &directory,
            part,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
        )?;
        let next = stat_directory(&directory)?;
        if next.stx_mnt_id != status.stx_mnt_id
            && !mounts
                .get(&next.stx_mnt_id)
                .is_some_and(|mount| mount.root == "/")
        {
            return Err(InspectionError::UnsupportedMount);
        }
        status = next;
    }
    let mount = mounts
        .get(&status.stx_mnt_id)
        .ok_or(InspectionError::UnsupportedMount)?;
    if mount.root != "/" || !matches!(mount.kind, "ext4" | "ext3" | "xfs" | "btrfs" | "overlay") {
        return Err(InspectionError::UnsupportedMount);
    }
    Ok((directory, directory_key(&status), ancestry))
}

fn stat_directory(fd: &OwnedFd) -> Result<Statx, InspectionError> {
    let status = fs::statx(
        fd,
        "",
        AtFlags::EMPTY_PATH,
        StatxFlags::BASIC_STATS | MOUNT_ID,
    )?;
    if status.stx_mask & (MOUNT_ID | StatxFlags::INO).bits() != (MOUNT_ID | StatxFlags::INO).bits()
    {
        return Err(InspectionError::UnsupportedMount);
    }
    Ok(status)
}

fn directory_key(info: &Statx) -> String {
    let mut hash = Sha256::new();
    hash.update(b"cocodeop-linux-directory-v1\0");
    hash.update(info.stx_dev_major.to_be_bytes());
    hash.update(info.stx_dev_minor.to_be_bytes());
    hash.update(info.stx_ino.to_be_bytes());
    hex::encode(hash.finalize())
}

struct Mount<'a> {
    root: &'a str,
    kind: &'a str,
}

fn parse_mounts(info: &str) -> HashMap<u64, Mount<'_>> {
    info.lines()
        .filter_map(|line| {
            let (fields, fs) = line.split_once(" - ")?;
            let mut fields = fields.split_ascii_whitespace();
            let id = fields.next()?.parse().ok()?;
            let root = fields.nth(2)?;
            let kind = fs.split_ascii_whitespace().next()?;
            Some((id, Mount { root, kind }))
        })
        .collect()
}

struct SourceWalker<'a> {
    manifest: &'a SourceManifest,
    entries: Vec<SourceEntry>,
    bytes: u64,
    visited: u32,
}

impl SourceWalker<'_> {
    fn read_directory(
        &mut self,
        fd: &OwnedFd,
        prefix: &str,
        depth: usize,
    ) -> Result<(), InspectionError> {
        if depth >= MAX_ROOT_ANCESTORS {
            return Err(InspectionError::Limit);
        }
        let readable = fs::openat(
            fd,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let directory = Dir::new(readable)?;
        for item in directory {
            let item = item?;
            let name = item.file_name();
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            self.visited += 1;
            if self.visited > MAX_WALK_ENTRIES {
                return Err(InspectionError::Limit);
            }
            let name_utf8 = name
                .to_str()
                .map_err(|_| InspectionError::UnsupportedEntry)?;
            let path = format!("{prefix}{name_utf8}")
                .parse::<SourcePath>()
                .map_err(|_| InspectionError::UnsupportedEntry)?;
            let selected = self.manifest.allows(&path);
            if !selected && !self.manifest.descend(&path) {
                continue;
            }
            let info = fs::statat(fd, name, AtFlags::SYMLINK_NOFOLLOW)?;
            match FileType::from_raw_mode(info.st_mode) {
                FileType::Directory if self.manifest.descend(&path) => {
                    let child = fs::openat2(
                        fd,
                        name,
                        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                        Mode::empty(),
                        descendant_resolution(),
                    )?;
                    self.read_directory(&child, &format!("{}/", path.as_str()), depth + 1)?;
                }
                FileType::RegularFile if selected => {
                    if self.entries.len() >= MAX_SOURCE_FILES as usize || info.st_nlink != 1 {
                        return Err(if info.st_nlink != 1 {
                            InspectionError::UnsupportedEntry
                        } else {
                            InspectionError::Limit
                        });
                    }
                    let entry = hash_file(fd, name, path, MAX_SOURCE_BYTES - self.bytes)?;
                    self.bytes += entry.size_bytes;
                    self.entries.push(entry);
                }
                _ if selected
                    || matches!(FileType::from_raw_mode(info.st_mode), FileType::Symlink) =>
                {
                    return Err(InspectionError::UnsupportedEntry);
                }
                _ => {}
            }
        }
        Ok(())
    }
}

pub(super) fn descendant_resolution() -> ResolveFlags {
    ResolveFlags::BENEATH
        | ResolveFlags::NO_SYMLINKS
        | ResolveFlags::NO_MAGICLINKS
        | ResolveFlags::NO_XDEV
}

fn hash_file(
    dir: &OwnedFd,
    name: &std::ffi::CStr,
    path: SourcePath,
    remaining: u64,
) -> Result<SourceEntry, InspectionError> {
    let mut hash = Sha256::new();
    let (size_bytes, executable) = copy_source_file(dir, name, remaining, &mut hash)?;
    Ok(SourceEntry {
        path,
        sha256: hex::encode(hash.finalize()),
        size_bytes,
        executable,
    })
}

pub(super) fn copy_source_file(
    dir: &OwnedFd,
    name: impl rustix::path::Arg,
    remaining: u64,
    writer: &mut impl Write,
) -> Result<(u64, bool), InspectionError> {
    let (fd, before, size) = open_source_file(dir, name)?;
    if size > remaining {
        return Err(InspectionError::Limit);
    }
    copy_checked_source(&fd, &before, size, writer)?;
    Ok((size, before.st_mode & 0o111 != 0))
}

pub(super) fn copy_checked_source(
    fd: &OwnedFd,
    before: &fs::Stat,
    size: u64,
    writer: &mut impl Write,
) -> Result<(), InspectionError> {
    // Open the checked inode through our own live descriptor, so a replacement name cannot open a device or FIFO.
    let file_path = format!("/proc/self/fd/{}", fd.as_raw_fd());
    let readable = fs::open(
        file_path.as_str(),
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    let mut file = File::from(readable);
    let read = io::copy(&mut (&mut file).take(size + 1), writer)?;
    let after = fs::fstat(&file)?;
    if read != size
        || before.st_dev != after.st_dev
        || before.st_ino != after.st_ino
        || before.st_size != after.st_size
        || before.st_mode != after.st_mode
        || before.st_mtime != after.st_mtime
        || before.st_mtime_nsec != after.st_mtime_nsec
        || before.st_ctime != after.st_ctime
        || before.st_ctime_nsec != after.st_ctime_nsec
        || after.st_nlink != 1
    {
        return Err(InspectionError::SourceChanged);
    }
    Ok(())
}

pub(super) fn open_source_file(
    dir: &OwnedFd,
    name: impl rustix::path::Arg,
) -> Result<(OwnedFd, fs::Stat, u64), InspectionError> {
    let fd = fs::openat2(
        dir,
        name,
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
        descendant_resolution(),
    )?;
    let stat = fs::fstat(&fd)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || stat.st_nlink != 1 {
        return Err(InspectionError::UnsupportedEntry);
    }
    let size = stat
        .st_size
        .try_into()
        .map_err(|_| InspectionError::UnsupportedEntry)?;
    Ok((fd, stat, size))
}
