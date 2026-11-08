//! Review-owned storage. Provider homes and the host-wide launch lock are separate.
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// An immutable XDG data base (before `review/`), including unavailable storage.
/// Resolution preserves historical empty/relative and Unicode-env semantics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataDir(Option<PathBuf>);

static PROCESS: LazyLock<DataDir> = LazyLock::new(|| {
    DataDir::resolve(
        std::env::var("XDG_DATA_HOME").ok(),
        std::env::var("HOME").ok(),
    )
});

impl DataDir {
    fn resolve(xdg: Option<String>, home: Option<String>) -> Self {
        Self(
            xdg.map(PathBuf::from)
                .or_else(|| home.map(|h| PathBuf::from(h).join(".local/share"))),
        )
    }

    pub fn process() -> &'static Self {
        &PROCESS
    }

    /// Explicitly disable review-owned storage without selecting ambient paths.
    pub fn unavailable() -> Self {
        Self(None)
    }

    /// Inject an XDG data base, with exactly the same layout as production.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self(Some(root.into()))
    }

    /// Compatibility boundary for the frozen inflight lock API.
    pub(crate) fn from_override(root: Option<&Path>) -> Self {
        root.map_or_else(|| Self::process().clone(), |p| Self::at(p.to_path_buf()))
    }

    fn child(&self, name: &str) -> Option<PathBuf> {
        Some(self.0.as_ref()?.join("review").join(name))
    }
    pub fn sessions(&self, family: LogFamily) -> Option<PathBuf> {
        self.child(match family {
            LogFamily::Public => "sessions.jsonl",
            LogFamily::Private => "sessions-private.jsonl",
        })
    }
    pub fn orphan_log(&self, family: LogFamily) -> Option<PathBuf> {
        self.child(match family {
            LogFamily::Public => "teardowns.jsonl",
            LogFamily::Private => "teardowns-private.jsonl",
        })
    }
    pub fn inflight(&self) -> Option<PathBuf> {
        self.child("inflight")
    }
    pub fn incidents(&self) -> Option<PathBuf> {
        self.child("incidents")
    }
    pub fn audit(&self, project: &Path, scope: &LogScope) -> Option<PathBuf> {
        let name = project
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_else(|| "unknown".into());
        let family = match scope.family {
            LogFamily::Public => "audit",
            LogFamily::Private => "audit-private",
        };
        Some(
            self.child(family)?
                .join(format!("{}-{name}-{}", crate::config::hostname(), scope.id)),
        )
    }
}

/// Historical ids are opaque nonempty strings, not necessarily generated hex.
/// This is identity, NOT proof of a safe path component. Tightening its syntax
/// would reject existing operator config; audit path confinement is separate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditId(String);

impl AuditId {
    pub fn from_record(raw: &str) -> Option<Self> {
        (!raw.is_empty()).then(|| Self(raw.to_owned()))
    }
    pub fn generate() -> anyhow::Result<Self> {
        Ok(Self(crate::ids::try_generate_short_id()?))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub(crate) fn deserialize_optional<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Self>, D::Error> {
        use serde::Deserialize;
        Ok(Option::<String>::deserialize(d)?
            .as_deref()
            .and_then(Self::from_record))
    }
}
impl std::ops::Deref for AuditId {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}
impl std::fmt::Display for AuditId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogFamily {
    #[default]
    Public,
    Private,
}

/// One resolved project's identity and routing. Writers accept this whole.
#[derive(Clone, Debug)]
pub struct LogScope {
    id: AuditId,
    family: LogFamily,
}
impl LogScope {
    pub fn new(id: AuditId, private: bool) -> Self {
        Self {
            id,
            family: if private {
                LogFamily::Private
            } else {
                LogFamily::Public
            },
        }
    }
    pub fn id(&self) -> &AuditId {
        &self.id
    }
    pub fn family(&self) -> LogFamily {
        self.family
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_resolution_preserves_existing_env_policy() {
        for (xdg, home, expected) in [
            (Some("/xdg"), Some("/home"), Some("/xdg")),
            (None, Some("/home"), Some("/home/.local/share")),
            (Some(""), Some("/home"), Some("")),
            (Some("relative"), None, Some("relative")),
            (None, None, None),
        ] {
            let data = DataDir::resolve(xdg.map(str::to_owned), home.map(str::to_owned));
            assert_eq!(data.0.as_deref(), expected.map(Path::new));
        }
        assert_eq!(DataDir::from_override(None), *DataDir::process());
        assert_eq!(
            DataDir::from_override(Some(Path::new("relative"))),
            DataDir::at("relative")
        );
    }

    #[test]
    fn unavailable_data_stays_unavailable_in_every_store() {
        let data = DataDir::resolve(None, None);
        let scope = LogScope::new(AuditId::from_record("old").expect("test fixture"), true);
        assert!(data.sessions(scope.family()).is_none());
        assert!(data.orphan_log(scope.family()).is_none());
        assert!(data.audit(Path::new("/project"), &scope).is_none());
        assert!(data.inflight().is_none());
        assert!(data.incidents().is_none());
        assert!(crate::sessions::read_all_in(&data).is_empty());
        assert_eq!(crate::incident::list_recent_in(&data, 1).failures.len(), 1);
        let _guard = crate::inflight::mark_in(
            "unavailable-data-test",
            crate::provider::Provider::Codex,
            "/p",
            &data,
        );
        assert!(!crate::inflight::take_interrupt_request_in(
            "unavailable-data-test",
            &data
        ));
    }

    #[test]
    fn audit_identity_preserves_opaque_config_values_and_normalizes_absence() {
        for raw in ["", "beef", "a", " ", "legacy/../id"] {
            let input = format!(
                "[_audit]\nid = {}\n",
                serde_json::to_string(raw).expect("test fixture")
            );
            let cfg = crate::config::parse_file(&input, "test", crate::config::Layer::Local)
                .expect("test fixture");
            assert_eq!(cfg.audit.id, AuditId::from_record(raw));
            assert_eq!(cfg.audit.id.as_deref(), (!raw.is_empty()).then_some(raw));
        }
        let cfg = crate::config::parse_file("[_audit]", "test", crate::config::Layer::Local)
            .expect("test fixture");
        assert!(cfg.audit.id.is_none());
        assert!(
            crate::config::parse_file("[_audit]\nid = 4", "test", crate::config::Layer::Local)
                .is_err()
        );
    }
}
