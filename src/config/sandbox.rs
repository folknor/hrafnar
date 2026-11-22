//! `review`'s own sandbox vocabulary as it appears in config: `read-only` (the
//! default), `workspace-write`, `danger-full-access`, or a harness-native
//! name. Native translation and grant semantics belong to
//! `harness::Permissions`, not to the config parser or `review list`.
//!
//! The level names are codex's, because they are the config surface profiles
//! are already written in, and a fresh set would change the meaning of every
//! one of them. Codex takes them verbatim; grok's built-ins are `read-only`,
//! `workspace` and `none`, so `harness::Permissions` rewrites the upper two.
//! That rewrite is load-bearing: grok refuses to start on a profile name it
//! cannot resolve, with an error that reads as a missing config file.
//!
//! Any other value is `Custom` and passes through verbatim rather than being
//! rejected, because grok resolves names against user-defined profiles in
//! `~/.grok/sandbox.toml`. The harness validates its own vocabulary with a
//! better error than we could write, and fails before running a turn, so a
//! typo is cheap. The three level names are therefore reserved: a custom grok
//! profile named `workspace-write` cannot be selected from a profile. That is
//! the trade, because a level means the same thing on every harness only if no
//! harness can reinterpret it.
//!
//! Claude and maki have no filesystem sandbox; their profiles omit the level.
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Review's reserved permission vocabulary. Other names belong to the harness.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SandboxLevel {
    #[default]
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
    Custom(String),
}

impl SandboxLevel {
    pub const NAMED: [Self; 3] = [Self::ReadOnly, Self::WorkspaceWrite, Self::DangerFullAccess];

    pub fn as_str(&self) -> &str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
            Self::Custom(name) => name,
        }
    }
}

impl From<&str> for SandboxLevel {
    fn from(value: &str) -> Self {
        match value {
            "read-only" => Self::ReadOnly,
            "workspace-write" => Self::WorkspaceWrite,
            "danger-full-access" => Self::DangerFullAccess,
            other => Self::Custom(other.to_string()),
        }
    }
}

impl Serialize for SandboxLevel {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SandboxLevel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from(String::deserialize(deserializer)?.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_preserves_named_and_custom_levels() {
        for level in SandboxLevel::NAMED
            .into_iter()
            .chain([SandboxLevel::Custom("native".into())])
        {
            let json = serde_json::to_string(&level).expect("serialize");
            assert_eq!(
                serde_json::from_str::<SandboxLevel>(&json).expect("deserialize"),
                level
            );
        }
    }
}
