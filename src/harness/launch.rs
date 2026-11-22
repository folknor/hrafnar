//! Complete launch requests. Preparation still belongs to `invoke`: a fresh
//! identity and effective permission grants need not exist when this is built.
use super::{Harness, ModelSpec, Permissions, SessionStart};
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
    /// Claude only: a token from `claude_token_command`, passed to the child
    /// as `CLAUDE_CODE_OAUTH_TOKEN`. Its presence is recorded; never its value.
    pub(super) claude_token: Option<super::claude_token::ClaudeToken>,
}

impl RunSettings {
    /// `definition` is the chosen harness's entry of the run's profile; its
    /// table (`[profiles.<name>.<harness>]`) is what the session row records.
    pub fn from_profile(harness: Harness, definition: Option<&config::HarnessEntry>) -> Self {
        let default = config::Profile::default();
        let profile = definition.map(|d| &d.profile).unwrap_or(&default);
        Self {
            selection: ModelSpec::new(
                harness,
                profile.model.as_deref(),
                profile.effort.as_deref(),
                &profile.config,
            ),
            permissions: Permissions::profile(harness, profile.sandbox.as_ref()),
            writable_roots: profile.writable_roots.clone(),
            env: profile.env.clone(),
            config: profile.config.clone(),
            origin: definition.map(|d| ProfileOrigin {
                layer: d.layer.as_str().to_owned(),
                table: d.table.clone(),
            }),
            claude_token: None,
        }
    }

    /// Restores only recorded launch policy. Profile env values and arbitrary
    /// raw config were never saved and must not be reconstructed from today's
    /// profile. CODEX_HOME is a saved storage location, not profile env replay.
    pub fn from_inherited(harness: Harness, inherited: InheritedSettings) -> Self {
        Self {
            selection: ModelSpec::new(
                harness,
                inherited.model.as_deref(),
                inherited.effort.as_deref(),
                &[],
            ),
            permissions: inherited
                .sandbox
                .unwrap_or_else(|| Permissions::restore(harness, None, None, &[])),
            writable_roots: inherited.writable_roots,
            env: inherited
                .codex_home
                .map(|home| BTreeMap::from([("CODEX_HOME".to_owned(), home)])),
            config: Vec::new(),
            origin: None,
            claude_token: None,
        }
    }

    pub fn origin(&self) -> Option<&ProfileOrigin> {
        self.origin.as_ref()
    }

    /// Launch claude with this token in place of its own login.
    pub fn with_claude_token(mut self, token: super::claude_token::ClaudeToken) -> Self {
        self.claude_token = Some(token);
        self
    }

    /// The child already has a token without the command's help.
    pub fn has_own_claude_token(&self) -> bool {
        super::claude_token::env_has_token(self.env.as_ref())
    }

    pub fn uses_claude_token(&self) -> bool {
        self.claude_token.is_some()
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
    pub harness: Harness,
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
    /// What a `hrafnar resume` launches with comes from the session's last
    /// recorded run: permissions, writable roots, model and effort, because they
    /// are properties of the session, not of the invocation driving it. A resume
    /// carries no profile, so without this a `workspace-write` session would
    /// come back read-only.
    ///
    /// The row records effective values (what the run received, not what its
    /// profile asked for). Roots go back as profile roots, not a verbatim list,
    /// so they pass the same filters and are re-derived on this host.
    ///
    /// Model and effort are inherited because codex resolves the model again on
    /// every invocation and would otherwise switch to its built-in default mid
    /// session. Env values and raw `config` are not recorded, so they are not
    /// inherited. A row for another harness inherits nothing.
    /// [`Permissions::restore`] decides what a row without a selector means
    /// (the native one is stored under the `provider_sandbox` key).
    pub fn from_record(record: Option<&sessions::SessionRecord>, harness: Harness) -> Self {
        let Some(record) = record.filter(|r| r.harness == harness.as_str()) else {
            return Self::default();
        };
        Self {
            sandbox: Some(Permissions::restore(
                harness,
                record.sandbox.as_deref(),
                record.harness_sandbox.as_deref(),
                &record.writable_roots,
            )),
            writable_roots: record.writable_roots.clone(),
            model: record.model.clone(),
            effort: record.effort.clone(),
            codex_home: match harness {
                Harness::Codex => record.codex_home.clone(),
                Harness::Claude | Harness::Grok | Harness::Maki => None,
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
            "assembled_prompt": "p", "hrafnar_version": "0.0.0",
            "profile_origin": {"layer": "global", "table": "[codex.old]"}
        }))
        .expect("historical row")
    }

    #[test]
    fn profile_settings_keep_winning_origin_and_selector_precedence() {
        let definition = config::HarnessEntry {
            harness: Harness::Codex,
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
            },
            layer: config::Layer::Global,
            table: "[profiles.deep.codex]".into(),
        };
        let settings = RunSettings::from_profile(Harness::Codex, Some(&definition));
        assert_eq!(settings.selection.model(), Some("dedicated"));
        assert_eq!(settings.selection.effort(), Some("high"));
        assert_eq!(settings.env_keys(), ["SECRET"]);
        assert_eq!(settings.writable_roots, ["/requested"]);
        assert_eq!(settings.permissions.selector(), Some("workspace-write"));
        assert_eq!(settings.origin().expect("origin").layer, "global");
        assert_eq!(
            settings.origin().expect("origin").table,
            "[profiles.deep.codex]"
        );
        let encoded = serde_json::to_string(&settings.origin()).expect("origin json");
        assert!(!encoded.contains("secret value"));
    }

    #[test]
    fn inherited_settings_preserve_recorded_policy_without_replaying_a_profile() {
        let record = row();
        let settings = RunSettings::from_inherited(
            Harness::Codex,
            InheritedSettings::from_record(Some(&record), Harness::Codex),
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
            Harness::Grok,
            InheritedSettings::from_record(Some(&record), Harness::Grok),
        );
        assert!(mismatched.env.is_none());
        assert!(mismatched.writable_roots.is_empty());
        assert!(mismatched.selection.model().is_none());
        assert!(mismatched.permissions.selector().is_none());
    }

    #[test]
    fn unprofiled_settings_have_defaults_without_inventing_provenance() {
        for &harness in Harness::all() {
            let settings = RunSettings::from_profile(harness, None);
            assert!(settings.origin().is_none());
            assert!(settings.env_keys().is_empty());
            assert!(settings.config.is_empty());
            assert_eq!(settings.permissions, Permissions::profile(harness, None));
        }
    }
}
