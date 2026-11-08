//! Complete launch requests. Preparation still belongs to `invoke`: a fresh
//! identity and effective permission grants need not exist when this is built.
use super::{ModelSpec, Permissions, Provider, SessionStart};
use crate::{config, sessions};
use std::{collections::BTreeMap, path::PathBuf};

/// The winning definition selected for this invocation, not session lineage or
/// proof that the requested launch succeeded. No environment values are saved.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProfileOrigin {
    pub layer: String,
    pub table: String,
}

#[derive(Debug, Clone)]
pub struct RunSettings {
    pub(super) selection: ModelSpec,
    pub(super) permissions: Permissions,
    /// Requested roots; effective grants live in PermissionPlan and the result.
    pub(super) writable_roots: Vec<String>,
    pub(super) env: Option<BTreeMap<String, String>>,
    pub(super) config: Vec<String>,
    origin: Option<ProfileOrigin>,
}

impl RunSettings {
    pub fn from_profile(provider: Provider, definition: Option<&config::ProfileDef>) -> Self {
        let default = config::Profile::default();
        let profile = definition.map(|d| &d.profile).unwrap_or(&default);
        Self {
            selection: ModelSpec::new(
                provider,
                profile.model.as_deref(),
                profile.effort.as_deref(),
                &profile.config,
            ),
            permissions: Permissions::profile(provider, profile.sandbox.as_ref()),
            writable_roots: profile.writable_roots.clone(),
            env: profile.env.clone(),
            config: profile.config.clone(),
            origin: definition.map(|d| ProfileOrigin {
                layer: d.layer.as_str().to_owned(),
                table: d.table.clone(),
            }),
        }
    }

    /// Restores only recorded launch policy. Profile env values and arbitrary
    /// raw config were never saved and must not be reconstructed from today's
    /// profile. CODEX_HOME is a saved storage location, not profile env replay.
    pub fn from_inherited(provider: Provider, inherited: InheritedSettings) -> Self {
        Self {
            selection: ModelSpec::new(
                provider,
                inherited.model.as_deref(),
                inherited.effort.as_deref(),
                &[],
            ),
            permissions: inherited
                .sandbox
                .unwrap_or_else(|| Permissions::restore(provider, None, None, &[])),
            writable_roots: inherited.writable_roots,
            env: inherited
                .codex_home
                .map(|home| BTreeMap::from([("CODEX_HOME".to_owned(), home)])),
            config: Vec::new(),
            origin: None,
        }
    }

    pub fn origin(&self) -> Option<&ProfileOrigin> {
        self.origin.as_ref()
    }
    pub fn env_keys(&self) -> Vec<String> {
        self.env
            .as_ref()
            .map(|env| env.keys().cloned().collect())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub provider: Provider,
    pub start: SessionStart,
    pub settings: RunSettings,
    pub prompt: String,
    pub root: PathBuf,
}

#[derive(Default)]
pub struct InheritedSettings {
    pub sandbox: Option<Permissions>,
    pub writable_roots: Vec<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub codex_home: Option<String>,
}

impl InheritedSettings {
    /// What a `review resume` launches with, taken from the session's own last
    /// recorded run: sandbox level, writable roots, model and reasoning effort.
    ///
    /// A resume carries no profile; before restoration was explicit, a
    /// resume used to silently drop the permissions the session was created with: a
    /// `workspace-write` run that died mid-turn came back as a read-only resume that
    /// could not touch the files it had been editing, and the failure surfaced as
    /// the model reporting a read-only filesystem rather than as anything about
    /// `review`. The permissions are a property of the *session*, not of the
    /// invocation that happens to be driving it.
    ///
    /// The sidecar already records both, and records the **effective** values (what
    /// the run received, not what its profile asked for), which is exactly what has
    /// to carry forward - the recurring lesson of the codex sandbox work is that
    /// those two differ. Roots are passed as profile roots rather than as a verbatim
    /// list so they go back through the same filters and are re-derived against
    /// *this* host: a host fact like the build lock belongs to the machine the
    /// resume runs on, not to the one that recorded the row.
    ///
    /// Known levels are recorded in review's vocabulary. Grok also saves its
    /// native selector in `provider_sandbox`, since a session cannot change profiles.
    /// Old rows have only `sandbox`, already in Grok's native form, so that field
    /// remains the fallback. A row predating both fields inherits nothing.
    ///
    /// Model and effort carry forward for the same reason and were originally left
    /// out on the grounds that only `model` was recorded, so a partial restoration
    /// would be less predictable than none. Measurement killed that argument: every
    /// resume ran on codex's **built-in default model** at its default
    /// effort, not the profile's, and since `--ignore-user-config` that default is
    /// not even the operator's configured one. A session's first turn on
    /// `gpt-5.6-sol`/`low` followed by five resumed turns on a different model is
    /// not a neutral fallback - it silently changes the model mid-session, at a
    /// different price, with nothing in the output saying so. The fix is to record
    /// `effort` too rather than to keep inheriting neither.
    ///
    /// Env and profile `config` are still *not* inherited: env values are
    /// deliberately never recorded (they can carry secrets) and `config` is not
    /// recorded at all, so there is nothing to restore from. That is an absence of
    /// data, not a judgement about predictability.
    pub fn from_record(record: Option<&sessions::SessionRecord>, provider: Provider) -> Self {
        let Some(record) = record.filter(|r| r.provider == provider.as_str()) else {
            return Self::default();
        };
        Self {
            sandbox: Some(Permissions::restore(
                provider,
                record.sandbox.as_deref(),
                record.provider_sandbox.as_deref(),
                &record.writable_roots,
            )),
            writable_roots: record.writable_roots.clone(),
            model: record.model.clone(),
            effort: record.effort.clone(),
            codex_home: match provider {
                Provider::Codex => record.codex_home.clone(),
                Provider::Claude | Provider::Grok | Provider::Maki => None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> sessions::SessionRecord {
        serde_json::from_value(serde_json::json!({
            "timestamp": "2026-01-01T00:00:00Z", "project": "/project",
            "hostname": "host", "audit_id": "audit", "provider": "codex",
            "archetype": "bare", "session_id": "session", "model": "selected",
            "effort": "high", "sandbox": "workspace-write",
            "writable_roots": ["/cache"], "env_keys": ["SECRET"],
            "codex_home": "/saved/codex", "operator_prompt": "p",
            "assembled_prompt": "p", "review_version": "0.0.0",
            "profile_origin": {"layer": "global", "table": "[codex.old]"}
        }))
        .expect("historical row")
    }

    #[test]
    fn profile_settings_keep_winning_origin_and_selector_precedence() {
        let definition = config::ProfileDef {
            profile: config::Profile {
                model: Some("dedicated".into()),
                effort: Some("low".into()),
                config: vec![
                    "model=\"raw\"".into(),
                    "model_reasoning_effort=\"high\"".into(),
                ],
                env: Some(BTreeMap::from([("SECRET".into(), "secret value".into())])),
                writable_roots: vec!["/requested".into()],
                sandbox: Some(config::SandboxLevel::WorkspaceWrite),
                ..Default::default()
            },
            layer: config::Layer::Global,
            table: "[host.codex.deep]".into(),
            host: Some("host".into()),
        };
        let settings = RunSettings::from_profile(Provider::Codex, Some(&definition));
        assert_eq!(settings.selection.model(), Some("dedicated"));
        assert_eq!(settings.selection.effort(), Some("high"));
        assert_eq!(settings.env_keys(), ["SECRET"]);
        assert_eq!(settings.writable_roots, ["/requested"]);
        assert_eq!(settings.permissions.selector(), Some("workspace-write"));
        assert_eq!(settings.origin().expect("origin").layer, "global");
        assert_eq!(
            settings.origin().expect("origin").table,
            "[host.codex.deep]"
        );
        let encoded = serde_json::to_string(&settings.origin()).expect("origin json");
        assert!(!encoded.contains("secret value"));
    }

    #[test]
    fn inherited_settings_preserve_recorded_policy_without_replaying_a_profile() {
        let record = row();
        let settings = RunSettings::from_inherited(
            Provider::Codex,
            InheritedSettings::from_record(Some(&record), Provider::Codex),
        );
        assert_eq!(settings.selection.model(), Some("selected"));
        assert_eq!(settings.selection.effort(), Some("high"));
        assert_eq!(settings.permissions.selector(), Some("workspace-write"));
        assert_eq!(settings.writable_roots, ["/cache"]);
        assert_eq!(settings.env_keys(), ["CODEX_HOME"]);
        assert_eq!(
            settings.env.as_ref().expect("home")["CODEX_HOME"],
            "/saved/codex"
        );
        assert!(settings.config.is_empty());
        assert!(settings.origin().is_none());
        let mismatched = RunSettings::from_inherited(
            Provider::Grok,
            InheritedSettings::from_record(Some(&record), Provider::Grok),
        );
        assert!(mismatched.env.is_none());
        assert!(mismatched.writable_roots.is_empty());
        assert!(mismatched.selection.model().is_none());
        assert!(mismatched.permissions.selector().is_none());
    }

    #[test]
    fn unprofiled_settings_have_defaults_without_inventing_provenance() {
        for &provider in Provider::all() {
            let settings = RunSettings::from_profile(provider, None);
            assert!(settings.origin().is_none());
            assert!(settings.env_keys().is_empty());
            assert!(settings.config.is_empty());
            assert_eq!(settings.permissions, Permissions::profile(provider, None));
        }
    }
}
