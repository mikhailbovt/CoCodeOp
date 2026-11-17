use std::{error::Error, fmt, str::FromStr};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A UTF-8, relative path in the managed source tree.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub struct SourcePath(String);

#[derive(Debug)]
pub struct InvalidSourcePath;

impl fmt::Display for InvalidSourcePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("expected a normalized, nonempty project-relative path")
    }
}

impl Error for InvalidSourcePath {}

impl SourcePath {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Component-aware ancestry. A directory called `src/a` does not cover `src/ab`.
    pub fn contains(&self, path: &Self) -> bool {
        path == self
            || path
                .0
                .strip_prefix(&self.0)
                .is_some_and(|suffix| suffix.starts_with('/'))
    }

    pub fn is_reserved(&self) -> bool {
        let mut components = self.0.split('/').peekable();
        while let Some(part) = components.next() {
            let part = part.to_ascii_lowercase();
            if matches!(
                part.as_str(),
                ".git" | ".cocodeop" | ".ssh" | ".aws" | ".env"
            ) || (part.starts_with(".env.")
                && (part != ".env.example" || components.peek().is_some()))
            {
                return true;
            }
        }
        false
    }

    fn needs_explicit_file_rule(&self) -> bool {
        self.0
            .rsplit('/')
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case(".env.example"))
    }
}

impl FromStr for SourcePath {
    type Err = InvalidSourcePath;

    fn from_str(path: &str) -> Result<Self, Self::Err> {
        Self::try_from(path.to_owned())
    }
}

impl TryFrom<String> for SourcePath {
    type Error = InvalidSourcePath;

    fn try_from(path: String) -> Result<Self, Self::Error> {
        if path.is_empty()
            || path.len() > 1024
            || path.contains(['\\', ':'])
            || path.chars().any(char::is_control)
            || path
                .split('/')
                .any(|component| matches!(component, "" | "." | "..") || component.len() > 255)
        {
            return Err(InvalidSourcePath);
        }
        Ok(Self(path))
    }
}

impl From<SourcePath> for String {
    fn from(value: SourcePath) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PathRule {
    File { path: SourcePath },
    Directory { path: SourcePath },
    Tree,
}

impl PathRule {
    pub fn matches(&self, candidate: &SourcePath) -> bool {
        match self {
            Self::File { path } => path == candidate,
            Self::Directory { path } => path.contains(candidate),
            Self::Tree => true,
        }
    }

    pub fn covers(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Tree, _) => true,
            (_, Self::File { path }) => self.matches(path),
            (Self::Directory { path }, Self::Directory { path: child }) => path.contains(child),
            _ => false,
        }
    }

    pub fn intersects(&self, other: &Self) -> bool {
        self.covers(other) || other.covers(self)
    }

    pub fn path(&self) -> Option<&SourcePath> {
        match self {
            Self::File { path } | Self::Directory { path } => Some(path),
            Self::Tree => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceManifest {
    pub include: Vec<PathRule>,
    #[serde(default)]
    pub exclude: Vec<PathRule>,
}

impl SourceManifest {
    pub fn is_valid(&self) -> bool {
        (1..=128).contains(&self.include.len())
            && self.exclude.len() <= 128
            && !self
                .include
                .iter()
                .any(|rule| rule.path().is_some_and(SourcePath::is_reserved))
    }

    pub fn allows(&self, path: &SourcePath) -> bool {
        !path.is_reserved()
            && self.include.iter().any(|rule| {
                rule.matches(path)
                    && (!path.needs_explicit_file_rule() || matches!(rule, PathRule::File { .. }))
            })
            && !self.exclude.iter().any(|rule| rule.matches(path))
    }

    /// A directory may contain selected files even when the directory itself is outside the selection.
    pub fn descend(&self, directory: &SourcePath) -> bool {
        let rule = PathRule::Directory {
            path: directory.clone(),
        };
        !directory.is_reserved()
            && !directory.needs_explicit_file_rule()
            && self.include.iter().any(|include| include.intersects(&rule))
            && !self.exclude.iter().any(|exclude| exclude.covers(&rule))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> SourcePath {
        text.parse().unwrap()
    }

    #[test]
    fn paths_are_normalized_and_component_aware() {
        for rejected in [
            "", ".", "a//b", "a/../b", "a/./b", "a/b/", "/a", "C:/a", "a\\b", "a\0b",
        ] {
            assert!(rejected.parse::<SourcePath>().is_err(), "{rejected}");
        }
        let parent = PathRule::Directory {
            path: path("src/a"),
        };
        let inside = PathRule::File {
            path: path("src/a/data.json"),
        };
        let adjacent = PathRule::File {
            path: path("src/ab/data.json"),
        };
        assert!(parent.covers(&inside) && inside.intersects(&parent));
        assert!(!inside.covers(&parent) && !parent.intersects(&adjacent));
    }

    #[test]
    fn source_selection_respects_default_deny_exclusions_and_exact_includes() {
        let manifest = SourceManifest {
            include: vec![
                PathRule::Directory { path: path("src") },
                PathRule::File {
                    path: path("etc/.env.example"),
                },
            ],
            exclude: vec![PathRule::Directory {
                path: path("src/local"),
            }],
        };
        assert!(manifest.is_valid());
        assert!(manifest.descend(&path("etc")));
        assert!(!manifest.descend(&path("src/local")));
        assert!(!manifest.allows(&path("etc/other")));
        assert!(manifest.allows(&path("etc/.env.example")));
        assert!(manifest.allows(&path("src/code.rs")));
        assert!(!manifest.allows(&path("src/.git/config")));
        assert!(!manifest.allows(&path("src/.env.local")));
        assert!(!manifest.allows(&path("src/.env.example")));
        assert!(!manifest.allows(&path("src/.env.example/hidden")));
    }
}
