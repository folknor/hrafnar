//! File-format vocabulary and resolved configuration. Shared data types stay at
//! this boundary; parsing, resolution, discovery and writing each have one owner.
//! Native permissions belong to harnesses, registry rules to project::registry,
//! and random identifiers to the dependency-free ids module.
mod error;
pub(crate) mod load;
pub use error::{ConfigError, Result, SearchBoundary};
mod parse;
mod profile;
mod resolve;
mod sandbox;

pub use crate::ids::{generate_uuid, try_generate_short_id};
pub use load::{global_config_path, global_projects, load, load_optional, project_root};
pub use parse::parse_file;
pub use resolve::resolve;
pub use sandbox::SandboxLevel;

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub use crate::harness::Harness;

pub(crate) const CONFIG_FILENAME: &str = ".review.toml";

/// Reserved CLI vocabulary is derived from Clap (including hidden commands
/// and aliases). Keep the dependency one way: CLI construction must not consult
/// this list, because constructing it renders the CLI help.
pub fn reserved_names() -> &'static std::collections::BTreeSet<String> {
    static NAMES: std::sync::LazyLock<std::collections::BTreeSet<String>> =
        std::sync::LazyLock::new(|| {
            use clap::CommandFactory;
            let mut command = crate::cli::Cli::command();
            command.build();
            let mut names = std::collections::BTreeSet::from(["help".to_string()]);
            for sub in command.get_subcommands() {
                names.insert(sub.get_name().to_string());
                names.extend(sub.get_all_aliases().map(str::to_string));
            }
            names
        });
    &NAMES
}

/// `bare` is the explicit no-prime selector, and the legacy positional
/// `review bare` means the same. Existing `bare = ""` definitions stay valid;
/// a non-empty prompt would be ignored by both forms.
const BARE: &str = "bare";

/// Which file a resolved value came from.
///
/// Resolution is POSIX-tool style: the command line beats the project's
/// `.review.toml`, which beats the operator's global config. Nothing is built
/// in. The split exists because the two files answer different questions: a
/// project owns its domain archetypes, while *which model serves a tier* is the
/// operator's current opinion, which changes with every model release and used
/// to be restated per project, per host, per tier until nobody could keep up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Local,
    Global,
}

impl Layer {
    pub fn as_str(self) -> &'static str {
        match self {
            Layer::Local => "local",
            Layer::Global => "global",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuditConfig {
    #[serde(default)]
    pub private: bool,
    #[serde(
        default,
        deserialize_with = "crate::data::AuditId::deserialize_optional"
    )]
    pub id: Option<crate::data::AuditId>,
}

/// Defaults under `[_defaults]`. Each field is `Option` so an absent key falls
/// through to the next layer while a present one - even an empty list - wins.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DefaultsConfig {
    /// The profile used when `-p` is omitted.
    pub profile: Option<String>,
    /// Seconds of rollout silence, with no final answer written, after which a
    /// codex run is treated as stalled: killed, bundled, and reported as a
    /// failure. `0` disables the check; omitted uses the built-in default
    /// (see `watchdog::Timings`).
    ///
    /// This is a genuine timeout resting on an *empirical* property of codex -
    /// that it wakes itself every few minutes and cannot stay silent - so it is
    /// deliberately tunable, and switchable off, in case a future codex changes
    /// cadence. codex-only; claude has no rollout to watch.
    pub stall_timeout_secs: Option<StallTimeout>,
    /// A shell command whose stdout is a Claude OAuth token, for a claude run
    /// whose own login failed. Global config only (refused in a project file).
    /// Run lazily: only after a fresh claude run fails to authenticate, or up
    /// front for a session that was created with it. The token is passed to
    /// that claude process as `CLAUDE_CODE_OAUTH_TOKEN` and never recorded.
    pub claude_token_command: Option<String>,
}

/// `[_defaults].stall_timeout_secs`, decoded where it is parsed: `0` means
/// off, any other value a timeout of that many seconds. Absence is not `Off`
/// (it leaves the codex adapter's own default in place), so the field holding
/// this stays an `Option`, and a local `0` can still override a global value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(from = "u64")]
pub enum StallTimeout {
    Off,
    After(std::num::NonZeroU64),
}

impl From<u64> for StallTimeout {
    fn from(secs: u64) -> Self {
        std::num::NonZeroU64::new(secs).map_or(Self::Off, Self::After)
    }
}

impl StallTimeout {
    /// The watchdog's form: `None` disables its stall branch.
    pub fn grace(self) -> Option<std::time::Duration> {
        match self {
            Self::Off => None,
            Self::After(secs) => Some(std::time::Duration::from_secs(secs.get())),
        }
    }
}

/// Shown as the number the operator wrote, `0` for off.
impl std::fmt::Display for StallTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => f.write_str("0"),
            Self::After(secs) => write!(f, "{secs}"),
        }
    }
}

/// A named settings profile: optional model, effort, sandbox, and env overrides
/// applied to a harness invocation when selected via `--profile`. The
/// harness it runs on is the table's required `harness` key, peeled off
/// before this is deserialized and kept beside it (`ProfileDef::harness`).
///
/// Unknown keys are an error. A profile wins whole, so a project profile with a
/// misspelled key (`modle = ...`) would otherwise parse as an empty profile and
/// silently discard the global definition it replaces - model, sandbox and all.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Free text for whoever picks a profile - what it is for, when to reach
    /// for it. Never sent to a harness; `review list` shows it. A field
    /// rather than a TOML comment because the parser discards comments, and
    /// `review list` renders only what the resolver produced.
    pub comment: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Review's sandbox vocabulary, resolved into native permissions before
    /// launch. Custom names reach the harness unchanged. Defaults to read-only
    /// when unset. Claude profiles must omit this field: it has no filesystem
    /// sandbox on this axis.
    pub sandbox: Option<SandboxLevel>,
    pub env: Option<BTreeMap<String, String>>,
    /// Extra codex `-c key=value` overrides, each passed verbatim as its own
    /// `-c`. Codex-only. Lets a profile force config the CLI doesn't expose - in
    /// particular a custom HTTP-transport harness to dodge the websocket death
    /// path (`model_provider` + `model_providers.<name>`).
    #[serde(default)]
    pub config: Vec<String>,
    /// Extra writable roots for a `workspace-write` run, **added to** the ones
    /// derived from the host (`src/writable_roots/`) rather than replacing
    /// them: the derived set covers what any build needs on this machine, and
    /// this covers what a particular project needs beyond that - a data
    /// directory, a sibling checkout, a generated-asset cache.
    ///
    /// Applied to codex workspace-write and fresh grok workspace-write runs.
    /// Rejected in config for other levels and harnesses, because
    /// widening a `read-only` profile would contradict the only thing that
    /// profile promises. `~`, `$VAR` and `${VAR}` expand, which is what lets one
    /// profile serve hosts with different layouts.
    #[serde(default)]
    pub writable_roots: Vec<String>,
}

/// One config file, parsed and checked on its own but not yet resolved against
/// the other layer.
#[derive(Debug, Default)]
pub struct ConfigFile {
    pub archetypes: BTreeMap<String, String>,
    pub audit: AuditConfig,
    pub defaults: DefaultsConfig,
    /// `[profiles.<name>]`: name -> (harness, profile).
    pub profiles: BTreeMap<String, (Harness, Profile)>,
    /// `[_projects.<host>]`: host -> project name -> path as written. Global
    /// file only, for `-C <name>` (see `project.rs`).
    pub projects: BTreeMap<String, BTreeMap<String, String>>,
}

/// A resolved value plus the layer and table it came from.
#[derive(Debug, Clone)]
pub struct Sourced<T> {
    pub value: T,
    pub layer: Layer,
    /// The table that supplied it, as written in the file (`[_defaults]`,
    /// `[profiles.deep]`).
    pub table: String,
}

/// One definition of a profile.
#[derive(Debug, Clone)]
pub struct ProfileDef {
    pub harness: Harness,
    pub profile: Profile,
    pub layer: Layer,
    pub table: String,
}

/// Every definition of one profile name, in precedence order. All of them
/// name the same harness: resolution refuses a project profile that would
/// silently switch which harness `-p <name>` launches. The first wins **whole** - a winning profile replaces the
/// others outright rather than merging field by field, so no field of a run can
/// come from a table that did not mention it. The rest are kept so `review
/// config` can show what a stale override is hiding.
#[derive(Debug, Clone)]
pub struct ProfileEntry {
    pub effective: ProfileDef,
    pub shadowed: Vec<ProfileDef>,
}

/// The files that were read to build a `ReviewConfig`.
#[derive(Debug, Clone, Default)]
pub struct ConfigFiles {
    /// The project's `.review.toml`. `None` only from `load_optional` outside a
    /// project, which `review list` uses to show the global layer anyway.
    pub local: Option<PathBuf>,
    /// Where the global config is looked for. `None` when neither
    /// `XDG_CONFIG_HOME` nor `HOME` names a usable directory.
    pub global: Option<PathBuf>,
    /// Whether that global file existed and was read.
    pub global_loaded: bool,
}

/// The effective configuration: local and global layers resolved, for this
/// host.
#[derive(Debug)]
pub struct ReviewConfig {
    pub archetypes: BTreeMap<String, Sourced<String>>,
    /// Always the project's: an audit id identifies a project.
    pub audit: AuditConfig,
    pub profile: Option<Sourced<String>>,
    pub stall_timeout_secs: Option<Sourced<StallTimeout>>,
    /// Global layer only, so never a project's choice.
    pub claude_token_command: Option<String>,
    /// profile name -> entry.
    pub profiles: BTreeMap<String, ProfileEntry>,
    pub hostname: String,
    // Both are constructed together by resolve; callers cannot replace one
    // independently and create a config paired with a different root.
    files: ConfigFiles,
    project_root: Option<crate::project::ProjectRoot>,
    /// This host's `[_projects.<host>]` registry, name -> path as written.
    pub projects: BTreeMap<String, String>,
}

impl ReviewConfig {
    pub fn files(&self) -> &ConfigFiles {
        &self.files
    }
    pub fn project_root(&self) -> Option<&crate::project::ProjectRoot> {
        self.project_root.as_ref()
    }
    pub fn require_project_root(&self) -> Result<&crate::project::ProjectRoot> {
        self.project_root().ok_or(ConfigError::ProjectRequired)
    }

    /// The archetype a run is primed with, as `(name, prime)`. No selector and
    /// `bare` both mean no prime: `bare` is a selector, not a configured
    /// prompt, so it works without any definition. One run is one session, so
    /// there is exactly one archetype; several are several `review` calls.
    pub(crate) fn select_archetype(
        &self,
        selector: Option<&str>,
    ) -> anyhow::Result<(String, String)> {
        let name = match selector {
            None => return Ok((BARE.to_string(), String::new())),
            Some(name) if name == BARE => return Ok((BARE.to_string(), String::new())),
            Some(name) => name,
        };
        if name.contains(',') {
            anyhow::bail!(
                "-a takes one archetype, not a list ('{name}')\n  \
                 Each run is one session: launch one `review` per archetype."
            );
        }
        if let Some(prime) = self.archetype(name) {
            return Ok((name.to_string(), prime.to_string()));
        }
        let available: Vec<&str> = self.archetypes.keys().map(String::as_str).collect();
        anyhow::bail!(
            "'{name}' not found in {}\n  configured: {}",
            self.searched(),
            if available.is_empty() {
                "(none)".to_string()
            } else {
                available.join(", ")
            }
        )
    }

    /// The effective definition of `profile`, if any layer defines it.
    pub fn resolve_profile(&self, profile: &str) -> Option<&ProfileDef> {
        self.profiles.get(profile).map(|e| &e.effective)
    }

    /// The effective definition, or an error naming the files searched and
    /// the profiles that do exist. Keep the lookup and its explanation together
    /// so the app does not need to restate the resolver's search rules.
    pub(crate) fn require_profile(&self, profile: &str) -> Result<&ProfileDef> {
        self.resolve_profile(profile).ok_or_else(|| {
            let table = format!("[profiles.{}]", toml_key(profile));
            let mut searched = Vec::new();
            if let Some(local) = &self.files.local {
                searched.push(format!("{table} in {}", local.display()));
            }
            if let Some(global) = &self.files.global {
                let absent = if self.files.global_loaded {
                    ""
                } else {
                    " (absent)"
                };
                searched.push(format!("{table} in {}{absent}", global.display()));
            }
            let searched = if searched.is_empty() {
                "no config file paths were available".to_string()
            } else {
                searched.join(", ")
            };
            ConfigError::ProfileUndefined {
                profile: profile.to_string(),
                searched,
                defined: self.profiles.keys().cloned().collect(),
            }
        })
    }

    pub fn archetype(&self, name: &str) -> Option<&str> {
        self.archetypes.get(name).map(|s| s.value.as_str())
    }

    pub fn default_profile(&self) -> Option<&str> {
        self.profile.as_ref().map(|s| s.value.as_str())
    }

    pub fn stall_timeout(&self) -> Option<StallTimeout> {
        self.stall_timeout_secs.as_ref().map(|s| s.value)
    }

    /// Every file that was consulted, for error messages about something no
    /// layer defines.
    pub fn searched(&self) -> String {
        let mut parts = vec![match self.files.local {
            Some(ref l) => l.display().to_string(),
            None => format!("(no {CONFIG_FILENAME})"),
        }];
        if let Some(ref g) = self.files.global {
            let absent = if self.files.global_loaded {
                ""
            } else {
                " (absent)"
            };
            parts.push(format!("{}{absent}", g.display()));
        }
        parts.join(", ")
    }
}

pub fn hostname() -> String {
    gethostname::gethostname().to_string_lossy().to_string()
}

pub use crate::toml_key::toml_key;

pub use crate::config_write::init;
pub use crate::project::Selection;

#[cfg(test)]
mod selection_tests {
    use super::*;
    #[test]
    fn root_and_provenance_are_derived_together_and_profile_errors_stay_typed() {
        let cfg = resolve(
            ConfigFile::default(),
            None,
            "host",
            ConfigFiles {
                local: Some("/missing/project/.review.toml".into()),
                ..Default::default()
            },
        )
        .expect("resolve");
        assert_eq!(
            cfg.require_project_root().expect("root").as_path(),
            std::path::Path::new("/missing/project")
        );
        let error = cfg.require_profile("missing").expect_err("profile");
        assert!(
            matches!(&error, ConfigError::ProfileUndefined { profile, .. } if profile == "missing")
        );
        let error = anyhow::Error::new(error).context("launch settings");
        assert!(matches!(
            error.downcast_ref::<ConfigError>(),
            Some(ConfigError::ProfileUndefined { .. })
        ));
        let global =
            resolve(ConfigFile::default(), None, "host", ConfigFiles::default()).expect("global");
        assert!(global.project_root().is_none());
        assert!(matches!(
            global.require_project_root(),
            Err(ConfigError::ProjectRequired)
        ));
    }

    #[test]
    fn bare_is_synthetic_with_or_without_a_definition() {
        for definition in ["", "bare = ''\n", "bare = '  '\n"] {
            let cfg = resolve(
                parse_file(
                    &format!("[archetypes]\n{definition}a='prime'"),
                    "fixture",
                    Layer::Local,
                )
                .expect("parse"),
                None,
                "host",
                ConfigFiles::default(),
            )
            .expect("resolve");
            assert_eq!(
                cfg.select_archetype(Some("bare")).expect("bare"),
                cfg.select_archetype(None).expect("implicit bare")
            );
            assert_eq!(
                crate::prompt::assemble(
                    &cfg.select_archetype(Some("bare")).expect("bare").1,
                    "  raw\n"
                ),
                "  raw\n"
            );
        }
    }

    #[test]
    fn selection_takes_exactly_one_archetype() {
        let raw = "[archetypes]\na = 'prime'\nb = 'second'\n";
        let cfg = resolve(
            parse_file(raw, "fixture", Layer::Local).expect("parse"),
            None,
            "host",
            ConfigFiles::default(),
        )
        .expect("resolve");
        assert_eq!(
            cfg.select_archetype(Some("a")).expect("select"),
            ("a".into(), "prime".into())
        );
        assert_eq!(
            cfg.select_archetype(None).expect("bare"),
            ("bare".into(), String::new())
        );
        assert!(cfg.select_archetype(Some("unknown")).is_err());
        let err = cfg
            .select_archetype(Some("a,b"))
            .expect_err("a list")
            .to_string();
        assert!(err.contains("one archetype"), "{err}");
    }
}
