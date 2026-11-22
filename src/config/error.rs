//! Errors classify failed config operations, retaining rich schema diagnostics as
//! sources. Individual TOML keys are not actionable error variants; absence,
//! unreadable storage, invalid files, resolution and undefined profiles are.
use std::{fmt, path::PathBuf};

pub type Result<T> = std::result::Result<T, ConfigError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchBoundary {
    Exact(PathBuf),
    GitRoot(PathBuf),
    FilesystemRoot(PathBuf),
}

impl fmt::Display for SearchBoundary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(root) => write!(f, "no .review.toml in {}\n\nRun `review -C <project> init` to create one.", root.display()),
            Self::GitRoot(root) => write!(f, "no .review.toml found (searched up to git root: {})\n\nRun `review init` to create one.", root.display()),
            Self::FilesystemRoot(_) => f.write_str("no .review.toml found in current or parent directories\n\nRun `review init` to create one."),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    NotFound {
        boundary: SearchBoundary,
    },
    Inspect {
        path: PathBuf,
        source: std::io::Error,
    },
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Syntax and file-local schema/policy errors, not just TOML syntax.
    InvalidFile {
        origin: String,
        source: anyhow::Error,
    },
    Resolve {
        source: anyhow::Error,
    },
    ProfileUndefined {
        profile: String,
        searched: String,
        /// Every profile some layer does define, for the error to list.
        defined: Vec<String>,
    },
    InvalidPath {
        path: PathBuf,
    },
    ProjectRequired,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { boundary } => boundary.fmt(f),
            Self::Inspect { path, source } => {
                write!(f, "failed to inspect {}: {source}", path.display())
            }
            Self::Read { path, source } => write!(f, "failed to read {}: {source}", path.display()),
            Self::InvalidFile { source, .. } | Self::Resolve { source } => source.fmt(f),
            Self::ProfileUndefined {
                profile,
                searched,
                defined,
            } => write!(
                f,
                "profile '{profile}' is not defined\n  looked for {searched}\n  defined: {}",
                if defined.is_empty() {
                    "(none)".to_string()
                } else {
                    defined.join(", ")
                }
            ),
            Self::InvalidPath { path } => write!(f, "{} has no parent directory", path.display()),
            Self::ProjectRequired => {
                f.write_str("project configuration required; only the global layer was loaded")
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Inspect { source, .. } | Self::Read { source, .. } => Some(source),
            Self::InvalidFile { source, .. } | Self::Resolve { source } => Some(source.as_ref()),
            _ => None,
        }
    }
}
