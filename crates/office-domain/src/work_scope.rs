use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{PathRule, SourceManifest, SourcePath};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PathAccess {
    pub allow: Vec<PathRule>,
    #[serde(default)]
    pub deny: Vec<PathRule>,
}

impl PathAccess {
    pub fn allows(&self, path: &SourcePath) -> bool {
        !path.is_reserved()
            && self.allow.iter().any(|rule| rule.matches(path))
            && !self.deny.iter().any(|rule| rule.matches(path))
    }

    pub fn descend(&self, path: &SourcePath) -> bool {
        let directory = PathRule::Directory { path: path.clone() };
        !path.is_reserved()
            && self.allow.iter().any(|rule| rule.intersects(&directory))
            && !self.deny.iter().any(|rule| rule.covers(&directory))
    }

    pub fn covers_uninterrupted(&self, requested: &PathRule) -> bool {
        !requested.path().is_some_and(SourcePath::is_reserved)
            && self.allow.iter().any(|rule| rule.covers(requested))
            && !self.deny.iter().any(|rule| rule.intersects(requested))
    }

    fn covers_selected(&self, manifest: &SourceManifest, paths: &PathRule) -> bool {
        match paths {
            PathRule::File { path } => manifest.allows(path) && self.allows(path),
            _ => {
                self.covers_uninterrupted(paths)
                    && manifest.include.iter().any(|rule| rule.covers(paths))
                    && !manifest.exclude.iter().any(|rule| rule.intersects(paths))
            }
        }
    }

    fn covers_command_source(&self, manifest: &SourceManifest, paths: &PathRule) -> bool {
        if let PathRule::File { path } = paths {
            return manifest.allows(path) && self.allows(path);
        }
        if paths.path().is_some_and(SourcePath::is_reserved)
            || !self.allow.iter().any(|rule| rule.covers(paths))
            || !manifest.include.iter().any(|rule| rule.covers(paths))
            || manifest.exclude.iter().any(|rule| rule.covers(paths))
        {
            return false;
        }
        // The command's actual source selection also applies the checkout exclusions.
        self.deny
            .iter()
            .filter(|rule| rule.intersects(paths))
            .all(|denied| {
                let overlap = if paths.covers(denied) { denied } else { paths };
                manifest
                    .exclude
                    .iter()
                    .any(|excluded| excluded.covers(overlap))
            })
    }

    pub fn reaches(&self, resource: &PathRule) -> bool {
        self.allow.iter().any(|allowed| {
            let overlap = if allowed.covers(resource) {
                resource
            } else if resource.covers(allowed) {
                allowed
            } else {
                return false;
            };
            !self.deny.iter().any(|denied| denied.covers(overlap))
        })
    }

    fn is_valid(&self) -> bool {
        self.allow.len() <= 128
            && self.deny.len() <= 128
            && !self
                .allow
                .iter()
                .any(|rule| rule.path().is_some_and(SourcePath::is_reserved))
    }

    fn within_manifest(&self, manifest: &SourceManifest) -> bool {
        self.allow.iter().all(|requested| match requested {
            PathRule::File { path } => manifest.allows(path),
            _ => {
                manifest.include.iter().any(|rule| rule.covers(requested))
                    && !manifest.exclude.iter().any(|deny| deny.covers(requested))
            }
        })
    }

    fn narrow(&self, requested: &Self) -> Option<Self> {
        if !requested.allow.iter().all(|child| {
            self.allow.iter().any(|parent| parent.covers(child))
                && !self.deny.iter().any(|deny| deny.covers(child))
        }) {
            return None;
        }
        let mut effective = requested.clone();
        for deny in &self.deny {
            if effective.allow.iter().any(|rule| rule.intersects(deny))
                && !effective.deny.contains(deny)
            {
                effective.deny.push(deny.clone());
            }
        }
        Some(effective)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WriteOperation {
    Create,
    Replace,
    Delete,
    Rename,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkPermissions {
    pub read: PathAccess,
    pub write: PathAccess,
    pub write_operations: Vec<WriteOperation>,
    #[serde(default)]
    pub create_directories: bool,
    #[serde(default)]
    pub change_executable: bool,
    #[serde(default)]
    pub command_template_ids: Vec<Uuid>,
}

impl WorkPermissions {
    pub fn is_valid(&self) -> bool {
        self.read.is_valid()
            && self.write.is_valid()
            && distinct(&self.write_operations, 4)
            && distinct(&self.command_template_ids, 16)
            && (!self.create_directories || self.write_operations.contains(&WriteOperation::Create))
    }

    pub fn within_manifest(&self, manifest: &SourceManifest) -> bool {
        self.read.within_manifest(manifest) && self.write.within_manifest(manifest)
    }

    pub fn can_write_in(&self, resource: &PathRule) -> bool {
        (!self.write_operations.is_empty() || self.change_executable)
            && self.write.reaches(resource)
    }

    pub fn narrow(&self, requested: &Self) -> Option<Self> {
        if !requested.is_valid()
            || !requested
                .write_operations
                .iter()
                .all(|operation| self.write_operations.contains(operation))
            || (requested.create_directories && !self.create_directories)
            || (requested.change_executable && !self.change_executable)
            || !requested
                .command_template_ids
                .iter()
                .all(|id| self.command_template_ids.contains(id))
        {
            return None;
        }
        Some(Self {
            read: self.read.narrow(&requested.read)?,
            write: self.write.narrow(&requested.write)?,
            ..requested.clone()
        })
    }

    pub fn permits(&self, manifest: &SourceManifest, action: FileAction<'_>) -> bool {
        let readable = |path| manifest.allows(path) && self.read.allows(path);
        let writable = |path| manifest.allows(path) && self.write.allows(path);
        let can = |operation| self.write_operations.contains(&operation);
        match action {
            FileAction::Read(path) => readable(path),
            FileAction::Create(path) => writable(path) && can(WriteOperation::Create),
            FileAction::Replace(path) => {
                writable(path) && readable(path) && can(WriteOperation::Replace)
            }
            FileAction::Delete(path) => {
                writable(path) && readable(path) && can(WriteOperation::Delete)
            }
            FileAction::Rename { source, target } => {
                source != target
                    && readable(source)
                    && writable(source)
                    && writable(target)
                    && can(WriteOperation::Rename)
            }
            FileAction::CreateDirectory(path) => {
                writable(path) && self.create_directories && can(WriteOperation::Create)
            }
            FileAction::ChangeExecutable(path) => {
                writable(path) && readable(path) && self.change_executable
            }
        }
    }

    pub fn permits_command(
        &self,
        id: Uuid,
        manifest: &SourceManifest,
        inputs: &[PathRule],
    ) -> bool {
        self.command_template_ids.contains(&id)
            && inputs
                .iter()
                .all(|input| self.read.covers_command_source(manifest, input))
    }

    pub fn permits_claim(&self, manifest: &SourceManifest, paths: &PathRule) -> bool {
        !matches!(paths, PathRule::Tree)
            && self.write.covers_selected(manifest, paths)
            && (self.write_operations.contains(&WriteOperation::Create)
                || self.write_operations.contains(&WriteOperation::Rename)
                || ((!self.write_operations.is_empty() || self.change_executable)
                    && self.read.covers_selected(manifest, paths)))
    }
}

fn distinct<T: Eq>(items: &[T], limit: usize) -> bool {
    items.len() <= limit
        && items
            .iter()
            .enumerate()
            .all(|(index, value)| !items[..index].contains(value))
}

#[derive(Clone, Copy, Debug)]
pub enum FileAction<'a> {
    Read(&'a SourcePath),
    Create(&'a SourcePath),
    Replace(&'a SourcePath),
    Delete(&'a SourcePath),
    Rename {
        source: &'a SourcePath,
        target: &'a SourcePath,
    },
    CreateDirectory(&'a SourcePath),
    ChangeExecutable(&'a SourcePath),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(value: &str) -> SourcePath {
        value.parse().unwrap()
    }
    fn directory(value: &str) -> PathRule {
        PathRule::Directory { path: path(value) }
    }
    fn file(value: &str) -> PathRule {
        PathRule::File { path: path(value) }
    }

    #[test]
    fn bounded_intersection_cannot_add_parent_permissions() {
        let parent = WorkPermissions {
            read: PathAccess {
                allow: vec![directory("src")],
                deny: vec![directory("src/internal")],
            },
            write: PathAccess {
                allow: vec![directory("src/client")],
                deny: vec![file("src/client/locked.rs")],
            },
            write_operations: vec![WriteOperation::Create, WriteOperation::Replace],
            ..WorkPermissions::default()
        };
        let manifest = SourceManifest {
            include: vec![PathRule::Tree],
            exclude: vec![directory("src/generated")],
        };
        let requested = WorkPermissions {
            read: PathAccess {
                allow: vec![directory("src")],
                deny: vec![directory("src/other")],
            },
            write: PathAccess {
                allow: vec![directory("src/client")],
                deny: vec![],
            },
            write_operations: vec![WriteOperation::Replace],
            ..WorkPermissions::default()
        };
        let narrowed = parent.narrow(&requested).unwrap();
        for dir in [
            "src/client",
            "src/internal",
            "src/generated",
            "src/other",
            "src/client-dev",
            "docs",
        ] {
            for name in ["ready.rs", "locked.rs", ".git"] {
                let path = path(&format!("{dir}/{name}"));
                for action in [
                    FileAction::Read(&path),
                    FileAction::Create(&path),
                    FileAction::Replace(&path),
                    FileAction::Delete(&path),
                ] {
                    assert!(
                        !narrowed.permits(&manifest, action) || parent.permits(&manifest, action)
                    );
                }
            }
        }
        assert!(!narrowed.permits(&manifest, FileAction::Read(&path("src/internal/main.rs"))));
        assert!(!narrowed.permits(&manifest, FileAction::Read(&path("src/other/main.rs"))));
        assert!(narrowed.permits(&manifest, FileAction::Replace(&path("src/client/main.rs"))));
        assert!(!narrowed.permits(
            &manifest,
            FileAction::Replace(&path("src/client/locked.rs"))
        ));
        let expansion = WorkPermissions {
            write_operations: vec![WriteOperation::Delete],
            ..requested
        };
        assert!(parent.narrow(&expansion).is_none());
    }

    #[test]
    fn edits_and_commands_require_each_independent_capability() {
        let command_id = Uuid::from_u128(47);
        let manifest = SourceManifest {
            include: vec![PathRule::Tree],
            exclude: vec![directory("secrets")],
        };
        let access = WorkPermissions {
            read: PathAccess {
                allow: vec![directory("src")],
                deny: vec![directory("src/private")],
            },
            write: PathAccess {
                allow: vec![directory("src"), directory("out")],
                deny: vec![file("out/final.json")],
            },
            write_operations: vec![
                WriteOperation::Create,
                WriteOperation::Replace,
                WriteOperation::Rename,
            ],
            command_template_ids: vec![command_id],
            ..WorkPermissions::default()
        };
        let source = path("src/new.json");
        let write_only = path("out/new.json");
        let denied = path("out/final.json");
        assert!(access.permits(&manifest, FileAction::Create(&write_only)));
        assert!(!access.permits(&manifest, FileAction::Replace(&write_only)));
        assert!(access.permits(
            &manifest,
            FileAction::Rename {
                source: &source,
                target: &write_only
            }
        ));
        assert!(!access.permits(
            &manifest,
            FileAction::Rename {
                source: &source,
                target: &denied
            }
        ));
        assert!(!access.permits(&manifest, FileAction::CreateDirectory(&path("out/missing"))));
        assert!(!access.permits(&manifest, FileAction::ChangeExecutable(&source)));
        assert!(access.permits_command(command_id, &manifest, &[file("src/new.json")]));
        assert!(!access.permits_command(command_id, &manifest, &[directory("src")]));
        assert!(!access.permits_command(command_id, &manifest, &[file("secrets/token")]))
    }

    #[test]
    fn a_full_command_source_may_exclude_only_paths_already_omitted_by_the_manifest() {
        let command_id = Uuid::from_u128(53);
        let manifest = SourceManifest {
            include: vec![PathRule::Tree],
            exclude: vec![directory("local")],
        };
        let mut access = WorkPermissions {
            read: PathAccess {
                allow: vec![PathRule::Tree],
                deny: vec![directory("local")],
            },
            command_template_ids: vec![command_id],
            ..WorkPermissions::default()
        };
        assert!(access.permits_command(command_id, &manifest, &manifest.include));
        assert!(!access.permits_command(command_id, &manifest, &[file("local/cache.db")]));
        access.read.deny.push(directory("src/private"));
        assert!(!access.permits_command(command_id, &manifest, &manifest.include));
        assert!(access.permits_command(command_id, &manifest, &[directory("src/public")]));
    }
}
