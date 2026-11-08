//! Permission intent and native launch policy. Config contains only review's
//! vocabulary; restoration is the only boundary that interprets native records.
use super::{Provider, grok};
use crate::config::SandboxLevel;
use crate::writable_roots::GrantedRoot;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Permissions(NativePermissions);

#[derive(Clone, Debug, PartialEq, Eq)]
enum NativePermissions {
    Claude,
    Codex(SandboxLevel),
    Grok(GrokPermissions),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum GrokPermissions {
    ReadOnly,
    FullAccess,
    FreshWorkspace,
    RecordedProfile { name: String, roots: Vec<String> },
    BuiltinWorkspace,
    Custom(String),
    InheritSaved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RootPolicy {
    Derive,
    Recorded,
    Ignore,
}

impl Permissions {
    /// Resolve an omitted profile level here, never in a runner.
    pub fn profile(provider: Provider, level: Option<&SandboxLevel>) -> Self {
        let level = level.cloned().unwrap_or_default();
        Self(match provider {
            Provider::Claude => NativePermissions::Claude,
            Provider::Codex => NativePermissions::Codex(level),
            Provider::Grok => NativePermissions::Grok(match level {
                SandboxLevel::ReadOnly => GrokPermissions::ReadOnly,
                SandboxLevel::WorkspaceWrite => GrokPermissions::FreshWorkspace,
                SandboxLevel::DangerFullAccess => GrokPermissions::FullAccess,
                // A native alias `workspace` historically requests the same
                // built-in as the translated level. Custom generated-looking
                // names, however, never acquire restoration behavior here.
                SandboxLevel::Custom(name) if name == "workspace" => {
                    GrokPermissions::FreshWorkspace
                }
                SandboxLevel::Custom(name) if name == "none" => GrokPermissions::FullAccess,
                SandboxLevel::Custom(name) => GrokPermissions::Custom(name),
            }),
        })
    }

    /// Native takes precedence, including over inconsistent normalized data.
    /// Old Grok rows contain native syntax in `sandbox`; do not translate that
    /// fallback through review's vocabulary. Unknown native names remain opaque.
    pub fn restore(
        provider: Provider,
        level: Option<&str>,
        native: Option<&str>,
        roots: &[String],
    ) -> Self {
        Self(match provider {
            Provider::Claude => NativePermissions::Claude,
            Provider::Codex => {
                NativePermissions::Codex(level.map(SandboxLevel::from).unwrap_or_default())
            }
            Provider::Grok => NativePermissions::Grok(match native.or(level) {
                None => GrokPermissions::InheritSaved,
                Some("read-only") => GrokPermissions::ReadOnly,
                Some("none") => GrokPermissions::FullAccess,
                Some("workspace") => GrokPermissions::BuiltinWorkspace,
                Some(name) if grok::is_generated_profile_name(name) => {
                    GrokPermissions::RecordedProfile {
                        name: name.to_string(),
                        roots: grok::canonical_roots(roots),
                    }
                }
                Some(name) => GrokPermissions::Custom(name.to_string()),
            }),
        })
    }

    pub fn provider(&self) -> Provider {
        match self.0 {
            NativePermissions::Claude => Provider::Claude,
            NativePermissions::Codex(_) => Provider::Codex,
            NativePermissions::Grok(_) => Provider::Grok,
        }
    }

    pub fn selector(&self) -> Option<&str> {
        match &self.0 {
            NativePermissions::Claude => None,
            NativePermissions::Codex(level) => Some(level.as_str()),
            NativePermissions::Grok(selection) => match selection {
                GrokPermissions::ReadOnly => Some("read-only"),
                GrokPermissions::FullAccess => Some("none"),
                GrokPermissions::FreshWorkspace | GrokPermissions::BuiltinWorkspace => {
                    Some("workspace")
                }
                GrokPermissions::RecordedProfile { name, .. } | GrokPermissions::Custom(name) => {
                    Some(name)
                }
                GrokPermissions::InheritSaved => None,
            },
        }
    }

    pub fn native_to_record(&self) -> Option<&str> {
        match self.0 {
            NativePermissions::Grok(_) => self.selector(),
            NativePermissions::Claude | NativePermissions::Codex(_) => None,
        }
    }

    pub(crate) fn accepts_profile_roots(&self) -> bool {
        self.roots() == RootPolicy::Derive
    }

    fn roots(&self) -> RootPolicy {
        match &self.0 {
            NativePermissions::Claude => RootPolicy::Ignore,
            NativePermissions::Codex(SandboxLevel::WorkspaceWrite) => RootPolicy::Derive,
            NativePermissions::Codex(_) => RootPolicy::Ignore,
            NativePermissions::Grok(selection) => match selection {
                GrokPermissions::FreshWorkspace => RootPolicy::Derive,
                GrokPermissions::RecordedProfile { .. } => RootPolicy::Recorded,
                GrokPermissions::ReadOnly
                | GrokPermissions::FullAccess
                | GrokPermissions::BuiltinWorkspace
                | GrokPermissions::Custom(_)
                | GrokPermissions::InheritSaved => RootPolicy::Ignore,
            },
        }
    }

    fn write_setup(&self) -> Option<grok::GrokWrite> {
        match &self.0 {
            NativePermissions::Claude | NativePermissions::Codex(_) => None,
            NativePermissions::Grok(selection) => match selection {
                GrokPermissions::FreshWorkspace => Some(grok::GrokWrite::Fresh),
                GrokPermissions::RecordedProfile { name, .. } => {
                    Some(grok::GrokWrite::Resume(name.clone()))
                }
                GrokPermissions::BuiltinWorkspace => Some(grok::GrokWrite::BuiltinWorkspace),
                GrokPermissions::ReadOnly
                | GrokPermissions::FullAccess
                | GrokPermissions::Custom(_)
                | GrokPermissions::InheritSaved => None,
            },
        }
    }

    /// Query projection for the existing record schema, distinct from native
    /// launch syntax. An omitted saved Grok selector makes no permission claim.
    pub fn recorded_level(&self) -> Option<&str> {
        match &self.0 {
            NativePermissions::Claude => None,
            NativePermissions::Codex(level) => Some(level.as_str()),
            NativePermissions::Grok(selection) => match selection {
                GrokPermissions::ReadOnly => Some("read-only"),
                GrokPermissions::FullAccess => Some("danger-full-access"),
                GrokPermissions::FreshWorkspace
                | GrokPermissions::BuiltinWorkspace
                | GrokPermissions::RecordedProfile { .. } => Some("workspace-write"),
                GrokPermissions::Custom(name) => Some(name),
                GrokPermissions::InheritSaved => None,
            },
        }
    }
}

/// One permission decision feeds setup, arguments, announcements and records.
/// Reasons retain derivation provenance even when raw Codex config replaces
/// the effective set; only effective paths are ever announced or rendered.
pub(super) struct PermissionPlan {
    pub permissions: Permissions,
    pub granted: Vec<GrantedRoot>,
    pub paths: Vec<String>,
    // Provenance of this invocation's roots, retained after preparation
    // replaces a fresh request with its prepared native profile.
    policy: RootPolicy,
}

impl PermissionPlan {
    pub fn new(
        permissions: Permissions,
        root: &Path,
        profile_roots: &[String],
        config: &[String],
    ) -> Self {
        let policy = permissions.roots();
        let granted = match policy {
            RootPolicy::Derive => crate::writable_roots::derive_with(
                root,
                &crate::writable_roots::RealHost,
                profile_roots,
            ),
            RootPolicy::Recorded | RootPolicy::Ignore => Vec::new(),
        };
        let mut paths = match &permissions.0 {
            NativePermissions::Grok(GrokPermissions::RecordedProfile { roots, .. }) => {
                roots.clone()
            }
            _ => granted.iter().map(|g| g.path.clone()).collect(),
        };
        match &permissions.0 {
            NativePermissions::Codex(_) => {
                if let Some(overridden) = super::config_writable_roots_override(config) {
                    paths = overridden;
                }
            }
            NativePermissions::Grok(_) => {
                if policy == RootPolicy::Derive {
                    paths = grok::canonical_roots(&paths);
                }
            }
            NativePermissions::Claude => {}
        }
        if policy == RootPolicy::Ignore && !profile_roots.is_empty() {
            eprintln!(
                "warning: profile writable_roots ignored - they apply only to a codex or grok workspace-write profile (this run: {} {})",
                permissions.provider(),
                permissions.selector().unwrap_or("unspecified")
            );
        }
        Self {
            permissions,
            granted,
            paths,
            policy,
        }
    }

    pub fn derives(&self) -> bool {
        self.policy == RootPolicy::Derive
    }
    pub fn uses_recorded_roots(&self) -> bool {
        self.policy == RootPolicy::Recorded
    }

    pub async fn prepare(
        &mut self,
        root: &Path,
        env: Option<&std::collections::BTreeMap<String, String>>,
    ) -> anyhow::Result<Option<String>> {
        let Some(kind) = self.permissions.write_setup() else {
            return Ok(None);
        };
        let root = root.to_path_buf();
        let roots = self.paths.clone();
        let env = env.cloned();
        let prepared = tokio::task::spawn_blocking(move || {
            grok::prepare_grok_write(&kind, &root, &roots, env.as_ref())
        })
        .await
        .map_err(|e| anyhow::anyhow!("grok setup task failed: {e}"))??;
        if let Some(name) = prepared.profile {
            self.permissions =
                Permissions(NativePermissions::Grok(GrokPermissions::RecordedProfile {
                    name,
                    roots: self.paths.clone(),
                }));
        }
        Ok(prepared.trust_grant)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_resume_selector_preserves_groks_saved_permissions_without_claiming_read_only() {
        let grok = Permissions::restore(Provider::Grok, None, None, &[]);
        assert_eq!(grok.selector(), None);
        assert_eq!(grok.recorded_level(), None);
        assert_eq!(grok.native_to_record(), None);
        assert_eq!(grok.write_setup(), None);
        let codex = Permissions::restore(Provider::Codex, None, None, &[]);
        assert_eq!(codex.selector(), Some("read-only"));
        assert_eq!(codex.recorded_level(), Some("read-only"));
    }

    #[test]
    fn native_record_is_authoritative_and_never_retranslated() {
        let restored = Permissions::restore(
            Provider::Grok,
            Some("workspace-write"),
            Some("workspace-write"),
            &[],
        );
        assert_eq!(restored.selector(), Some("workspace-write"));
        assert_eq!(restored.write_setup(), None);
        assert_eq!(restored.roots(), RootPolicy::Ignore);
        let conflicting =
            Permissions::restore(Provider::Grok, Some("workspace-write"), Some("none"), &[]);
        assert_eq!(conflicting.recorded_level(), Some("danger-full-access"));
        assert_eq!(conflicting.selector(), Some("none"));
    }

    #[test]
    fn generated_looking_custom_names_only_gain_restore_behavior_at_the_record_boundary() {
        let roots = vec!["/b".into(), "/a".into(), "/b".into()];
        let canonical = grok::canonical_roots(&roots);
        let name = "review-ws-0123456789abcdef".to_string();
        let fresh = Permissions::profile(Provider::Grok, Some(&SandboxLevel::Custom(name.clone())));
        assert_eq!(fresh.selector(), Some(name.as_str()));
        assert_eq!(fresh.recorded_level(), Some(name.as_str()));
        assert_eq!(fresh.write_setup(), None);
        assert_eq!(fresh.roots(), RootPolicy::Ignore);
        let restored =
            Permissions::restore(Provider::Grok, Some("workspace-write"), Some(&name), &roots);
        assert_eq!(restored.write_setup(), Some(grok::GrokWrite::Resume(name)));
        assert_eq!(restored.roots(), RootPolicy::Recorded);
        match restored.0 {
            NativePermissions::Grok(GrokPermissions::RecordedProfile { roots, .. }) => {
                assert_eq!(roots, canonical);
            }
            _ => panic!("expected restored profile"),
        }
    }

    #[test]
    fn codex_rederives_but_legacy_grok_workspace_does_not() {
        let codex = Permissions::restore(Provider::Codex, Some("workspace-write"), None, &[]);
        assert_eq!(codex.roots(), RootPolicy::Derive);
        let old = Permissions::restore(Provider::Grok, Some("workspace"), None, &[]);
        assert_eq!(old.write_setup(), Some(grok::GrokWrite::BuiltinWorkspace));
        assert_eq!(old.roots(), RootPolicy::Ignore);
        let fresh = Permissions::profile(Provider::Grok, Some(&SandboxLevel::WorkspaceWrite));
        assert_eq!(fresh.write_setup(), Some(grok::GrokWrite::Fresh));
        assert_eq!(fresh.roots(), RootPolicy::Derive);
    }

    #[test]
    fn claude_never_claims_filesystem_enforcement() {
        for level in SandboxLevel::NAMED {
            let permissions = Permissions::profile(Provider::Claude, Some(&level));
            assert_eq!(permissions.selector(), None);
            assert_eq!(permissions.recorded_level(), None);
            assert_eq!(permissions.roots(), RootPolicy::Ignore);
        }
    }
}
