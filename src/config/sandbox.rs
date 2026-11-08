use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Review's reserved permission vocabulary. Other names belong to the provider.
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
