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
    /// claude and maki run on their own permission configuration; `review`
    /// grants nothing and claims no filesystem enforcement for them.
    Claude,
    Maki,
    Codex(SandboxLevel),
    Grok(GrokPermissions),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum GrokPermissions {
    ReadOnly,
    FullAccess,
    FreshWorkspace,
    RecordedProfile {
        name: grok::ProfileName,
        roots: grok::RootSet,
    },
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
            Provider::Maki => NativePermissions::Maki,
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
            Provider::Maki => NativePermissions::Maki,
            Provider::Codex => {
                NativePermissions::Codex(level.map(SandboxLevel::from).unwrap_or_default())
            }
            Provider::Grok => NativePermissions::Grok(match native.or(level) {
                None => GrokPermissions::InheritSaved,
                Some("read-only") => GrokPermissions::ReadOnly,
                Some("none") => GrokPermissions::FullAccess,
                Some("workspace") => GrokPermissions::BuiltinWorkspace,
                Some(name) => match grok::ProfileName::from_record(name) {
                    Some(name) => GrokPermissions::RecordedProfile {
                        name,
                        roots: grok::RootSet::new(roots),
                    },
                    None => GrokPermissions::Custom(name.to_string()),
                },
            }),
        })
    }

    pub fn provider(&self) -> Provider {
        match self.0 {
            NativePermissions::Claude => Provider::Claude,
            NativePermissions::Maki => Provider::Maki,
            NativePermissions::Codex(_) => Provider::Codex,
            NativePermissions::Grok(_) => Provider::Grok,
        }
    }

    pub fn selector(&self) -> Option<&str> {
        match &self.0 {
            NativePermissions::Claude | NativePermissions::Maki => None,
            NativePermissions::Codex(level) => Some(level.as_str()),
            NativePermissions::Grok(selection) => match selection {
                GrokPermissions::ReadOnly => Some("read-only"),
                GrokPermissions::FullAccess => Some("none"),
                GrokPermissions::FreshWorkspace | GrokPermissions::BuiltinWorkspace => {
                    Some("workspace")
                }
                GrokPermissions::RecordedProfile { name, .. } => Some(name.as_str()),
                GrokPermissions::Custom(name) => Some(name),
                GrokPermissions::InheritSaved => None,
            },
        }
    }

    pub fn native_to_record(&self) -> Option<&str> {
        match self.0 {
            NativePermissions::Grok(_) => self.selector(),
            NativePermissions::Claude | NativePermissions::Maki | NativePermissions::Codex(_) => {
                None
            }
        }
    }

    pub(crate) fn accepts_profile_roots(&self) -> bool {
        self.roots() == RootPolicy::Derive
    }

    fn roots(&self) -> RootPolicy {
        match &self.0 {
            NativePermissions::Claude | NativePermissions::Maki => RootPolicy::Ignore,
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

    pub(super) fn write_setup(&self) -> Option<grok::GrokWrite> {
        match &self.0 {
            NativePermissions::Claude | NativePermissions::Maki | NativePermissions::Codex(_) => {
                None
            }
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
            NativePermissions::Claude | NativePermissions::Maki => None,
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

/// Effective grant facts shared by adapter setup, arguments, announcements and records.
/// Reasons retain derivation provenance even when raw Codex config replaces
/// the effective set; only effective paths are ever announced or rendered.
pub(super) struct PermissionPlan {
    pub permissions: Permissions,
    pub granted: Vec<GrantedRoot>,
    pub paths: Vec<String>,
    // Provenance of this invocation's roots, retained after preparation
    // replaces a fresh request with its prepared native profile.
    policy: RootPolicy,
    /// The project's `.git`, when this plan granted it.
    git_grant: Option<String>,
    root: std::path::PathBuf,
}

/// Codex mounts `<workspace>/.git` read-only in `workspace-write` unless an
/// explicit rule already covers that path, which leaves an agent able to edit
/// and build but not commit (`.git/index.lock`: read-only file system). Naming
/// it as a writable root is that explicit rule. Only for a real `.git`
/// directory in the project root: a pointer file (linked worktree, submodule,
/// separate gitdir) leads to state other checkouts share, and a project below
/// the repository's top level has its `.git` outside the workspace, so both
/// get a note instead of a grant. What a run changes in git's executable
/// metadata is reported at exit (`crate::git_watch`).
fn codex_git_grant(root: &Path) -> Option<GrantedRoot> {
    use crate::git_watch::Layout;
    let dirs = crate::git_watch::locate(root)?;
    if dirs.layout != Layout::Direct {
        let why = match dirs.layout {
            Layout::Pointer => {
                "its .git is a pointer file (linked worktree, submodule or separate gitdir)"
            }
            Layout::BadPointer => "its .git pointer file could not be read",
            _ => "the repository's .git is above the project root",
        };
        // A profile `config` override can still grant it, so this states the
        // automatic decision rather than predicting the commit.
        eprintln!(
            "note: codex: the project's git metadata was not granted writable, {why}; \
             git commit needs it"
        );
        return None;
    }
    let path = std::fs::canonicalize(&dirs.gitdir).unwrap_or(dirs.gitdir);
    let path = path.to_str()?.to_string();
    Some(GrantedRoot {
        path,
        why: "git commit (the project's .git)",
    })
}

impl PermissionPlan {
    pub fn new(permissions: Permissions, root: &Path, profile_roots: &[String]) -> Self {
        let policy = permissions.roots();
        // A resume passes its recorded roots back as profile roots, and a
        // recorded project `.git` would reach the generic filter, which
        // resolves symlinks before its inside-the-workspace check: a `.git`
        // replaced by a symlink to a sibling would be granted there. Drop it
        // by spelling first, so only `codex_git_grant` decides on `.git`.
        let project_git = [
            Some(root.join(".git")),
            std::fs::canonicalize(root).ok().map(|r| r.join(".git")),
        ];
        // Codex only: grok has no automatic `.git` grant, so an explicit or
        // recorded one there keeps its previous meaning.
        let is_codex = matches!(permissions.0, NativePermissions::Codex(_));
        let profile_roots: Vec<String> = profile_roots
            .iter()
            .filter(|r| {
                !is_codex
                    || !project_git
                        .iter()
                        .flatten()
                        .any(|g| Path::new(r.as_str()) == g)
            })
            .cloned()
            .collect();
        let profile_roots = profile_roots.as_slice();
        let mut granted = match policy {
            RootPolicy::Derive => crate::writable_roots::derive_with(
                root,
                &crate::writable_roots::RealHost,
                profile_roots,
            ),
            RootPolicy::Recorded | RootPolicy::Ignore => Vec::new(),
        };
        let mut paths = match &permissions.0 {
            NativePermissions::Grok(GrokPermissions::RecordedProfile { roots, .. }) => {
                roots.paths().to_vec()
            }
            _ => granted.iter().map(|g| g.path.clone()).collect(),
        };
        let mut git_grant = None;
        match &permissions.0 {
            NativePermissions::Codex(_) => {
                if policy == RootPolicy::Derive
                    && let Some(grant) = codex_git_grant(root)
                    && !paths.contains(&grant.path)
                {
                    paths.push(grant.path.clone());
                    git_grant = Some(grant.path.clone());
                    granted.push(grant);
                }
            }
            NativePermissions::Grok(_) => {
                if policy == RootPolicy::Derive {
                    paths = grok::RootSet::new(&paths).paths().to_vec();
                }
            }
            NativePermissions::Claude | NativePermissions::Maki => {}
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
            git_grant,
            root: root.to_path_buf(),
        }
    }

    pub fn derives(&self) -> bool {
        self.policy == RootPolicy::Derive
    }

    /// No effective root outside the workspace, not counting the project's
    /// own `.git`, which says nothing about whether a build's roots were
    /// found. Checked against the effective paths, which a codex `config`
    /// override may have replaced with entries inside the workspace.
    pub fn lacks_build_roots(&self) -> bool {
        self.paths
            .iter()
            .all(|p| self.git_grant.as_ref() == Some(p) || Path::new(p).starts_with(&self.root))
    }
    pub fn uses_recorded_roots(&self) -> bool {
        self.policy == RootPolicy::Recorded
    }

    /// Commit a successfully created native profile without changing provenance.
    pub fn grok_profile(&mut self, roots: grok::RootSet) {
        self.paths = roots.paths().to_vec();
        self.permissions = Permissions(NativePermissions::Grok(GrokPermissions::RecordedProfile {
            name: roots.profile_name(),
            roots,
        }));
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
        let canonical = grok::RootSet::new(&roots);
        let name = "review-ws-0123456789abcdef".to_string();
        let fresh = Permissions::profile(Provider::Grok, Some(&SandboxLevel::Custom(name.clone())));
        assert_eq!(fresh.selector(), Some(name.as_str()));
        assert_eq!(fresh.recorded_level(), Some(name.as_str()));
        assert_eq!(fresh.write_setup(), None);
        assert_eq!(fresh.roots(), RootPolicy::Ignore);
        let restored =
            Permissions::restore(Provider::Grok, Some("workspace-write"), Some(&name), &roots);
        assert_eq!(
            restored.write_setup(),
            Some(grok::GrokWrite::Resume(
                grok::ProfileName::from_record(&name).expect("generated name")
            ))
        );
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
    fn claude_and_maki_never_claim_filesystem_enforcement() {
        for provider in [Provider::Claude, Provider::Maki] {
            for level in SandboxLevel::NAMED {
                let permissions = Permissions::profile(provider, Some(&level));
                assert_eq!(permissions.provider(), provider);
                assert_eq!(permissions.selector(), None);
                assert_eq!(permissions.recorded_level(), None);
                assert_eq!(permissions.native_to_record(), None);
                assert_eq!(permissions.roots(), RootPolicy::Ignore);
            }
            let restored = Permissions::restore(provider, Some("workspace-write"), None, &[]);
            assert_eq!(restored.recorded_level(), None);
        }
    }
}
