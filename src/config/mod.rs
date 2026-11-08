//! File-format vocabulary and resolved configuration. Shared data types stay at
//! this boundary; parsing, resolution, discovery and writing each have one owner.
//! Native permissions belong to providers, registry rules to project::registry,
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

pub use crate::provider::Provider;

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
            let mut names =
                std::collections::BTreeSet::from(["all".to_string(), "help".to_string()]);
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
    /// The provider list used when `--provider` is omitted.
    pub providers: Option<Vec<Provider>>,
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
/// applied to a provider invocation when selected via `--profile`.
///
/// Unknown keys are an error. A profile wins whole, so a project profile with a
/// misspelled key (`modle = ...`) would otherwise parse as an empty profile and
/// silently discard the global definition it replaces - model, sandbox and all.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Free text for whoever picks a profile - what it is for, when to reach
    /// for it. Never sent to a provider; `review config` shows it. A field
    /// rather than a TOML comment because the parser discards comments, and
    /// `review config` renders only what the resolver produced.
    pub comment: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Review's sandbox vocabulary, resolved into native permissions before
    /// launch. Custom names reach the provider unchanged. Defaults to read-only
    /// when unset. Claude profiles must omit this field: it has no filesystem
    /// sandbox on this axis.
    pub sandbox: Option<SandboxLevel>,
    pub env: Option<BTreeMap<String, String>>,
    /// Extra codex `-c key=value` overrides, each passed verbatim as its own
    /// `-c`. Codex-only. Lets a profile force config the CLI doesn't expose - in
    /// particular a custom HTTP-transport provider to dodge the websocket death
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
    /// Rejected in config for other levels and providers, because
    /// widening a `read-only` profile would contradict the only thing that
    /// profile promises. `~`, `$VAR` and `${VAR}` expand, which is what lets one
    /// hostless profile serve hosts with different layouts.
    #[serde(default)]
    pub writable_roots: Vec<String>,
}

/// provider -> profile name -> profile.
type ProfileTables = BTreeMap<Provider, BTreeMap<String, Profile>>;

/// One config file, parsed and checked on its own but not yet resolved against
/// the other layer.
#[derive(Debug, Default)]
pub struct ConfigFile {
    pub archetypes: BTreeMap<String, String>,
    pub groups: BTreeMap<String, Vec<String>>,
    pub audit: AuditConfig,
    pub defaults: DefaultsConfig,
    /// `[<provider>.<profile>]`.
    pub profiles: ProfileTables,
    /// Legacy `[<host>.<provider>.<profile>]`, keyed by host. Still parsed so
    /// existing files need no edit; for the host it names, it beats a hostless
    /// profile of the same name in the same file.
    pub hosts: BTreeMap<String, ProfileTables>,
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
    /// `[codex.deep]`, `[plantasjen.codex.deep]`).
    pub table: String,
}

/// One definition of a profile. A legacy host-scoped one carries its host.
#[derive(Debug, Clone)]
pub struct ProfileDef {
    pub profile: Profile,
    pub layer: Layer,
    pub table: String,
    pub host: Option<String>,
}

/// Every definition of one `(provider, profile)` visible from this host, in
/// precedence order. The first wins **whole** - a winning profile replaces the
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
    /// project, which `review config` uses to show the global layer anyway.
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
    pub groups: BTreeMap<String, Sourced<Vec<String>>>,
    /// Always the project's: an audit id identifies a project.
    pub audit: AuditConfig,
    pub providers: Option<Sourced<Vec<Provider>>>,
    pub stall_timeout_secs: Option<Sourced<StallTimeout>>,
    /// provider -> profile name -> entry.
    pub profiles: BTreeMap<Provider, BTreeMap<String, ProfileEntry>>,
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

    /// Expand names, groups and the synthetic bare selector in selection order.
    /// `bare` is a selector, not a configured prime: all excludes its legacy
    /// empty definition, and explicit bare works without any such definition.
    pub(crate) fn select_archetypes(
        &self,
        selector: Option<&str>,
    ) -> anyhow::Result<Vec<(String, String)>> {
        use anyhow::bail;
        let mut archetypes_to_run: Vec<(String, String)> = Vec::new();
        match selector {
            None => archetypes_to_run.push((BARE.to_string(), String::new())),
            Some(arg) => {
                for name in arg.split(',') {
                    if name == BARE {
                        // The legacy positional form has always meant this without
                        // consulting config; keep the explicit flag equivalent.
                        archetypes_to_run.push((BARE.to_string(), String::new()));
                    } else if name == "all" {
                        archetypes_to_run.extend(
                            self.archetypes
                                .iter()
                                .filter(|(n, _)| n.as_str() != BARE)
                                .map(|(n, p)| (n.clone(), p.value.clone())),
                        );
                    } else if let Some(group) = self.groups.get(name) {
                        // Config resolution removes cross-layer losers and rejects
                        // same-layer clashes, so group-first lookup is not precedence.
                        for member in &group.value {
                            // Resolution checked every member exists.
                            let prime = self.archetype(member).ok_or_else(|| {
                                anyhow::anyhow!("group member {member} has no archetype")
                            })?;
                            archetypes_to_run.push((member.clone(), prime.to_string()));
                        }
                    } else if let Some(prime) = self.archetype(name) {
                        archetypes_to_run.push((name.to_string(), prime.to_string()));
                    } else {
                        let mut available: Vec<&str> =
                            self.archetypes.keys().map(String::as_str).collect();
                        available.extend(self.groups.keys().map(String::as_str));
                        bail!(
                            "'{name}' not found in {}\n  \
                         configured: {}",
                            self.searched(),
                            if available.is_empty() {
                                "(none)".to_string()
                            } else {
                                available.join(", ")
                            }
                        );
                    }
                }
            }
        }

        // Deduplicate (e.g. "all" + a specific archetype, or overlapping groups)
        let mut seen = std::collections::HashSet::new();
        archetypes_to_run.retain(|(name, _)| seen.insert(name.clone()));

        if archetypes_to_run.is_empty() {
            bail!(
                "no priming archetypes configured in {}\n  \
             Use -a bare or omit -a to send stdin unchanged, or add one under [archetypes].",
                self.searched()
            );
        }

        Ok(archetypes_to_run)
    }

    /// The effective profile for `provider`, if any layer defines it.
    pub fn resolve_profile(&self, provider: Provider, profile: &str) -> Option<&Profile> {
        self.profiles
            .get(&provider)?
            .get(profile)
            .map(|e| &e.effective.profile)
    }

    /// The effective profile, or an error describing the tables searched in
    /// precedence order. Keep the lookup and its explanation together so the
    /// app does not need to restate the config resolver's search rules.
    pub(crate) fn require_profile(&self, provider: Provider, profile: &str) -> Result<&Profile> {
        self.resolve_profile(provider, profile).ok_or_else(|| {
            let host_table = format!("[{}.{provider}.{profile}]", toml_key(&self.hostname));
            let common_table = format!("[{provider}.{profile}]");
            let mut searched = Vec::new();

            if let Some(local) = &self.files.local {
                searched.push(format!("{host_table} in {}", local.display()));
                searched.push(format!("{common_table} in {}", local.display()));
            }
            if let Some(global) = &self.files.global {
                let absent = if self.files.global_loaded {
                    ""
                } else {
                    " (absent)"
                };
                searched.push(format!("{host_table} in {}{absent}", global.display()));
                searched.push(format!("{common_table} in {}{absent}", global.display()));
            }

            let searched = if searched.is_empty() {
                "no config file paths were available".to_string()
            } else {
                searched.join(", ")
            };
            ConfigError::ProfileUndefined {
                provider,
                profile: profile.to_string(),
                searched,
            }
        })
    }

    pub fn archetype(&self, name: &str) -> Option<&str> {
        self.archetypes.get(name).map(|s| s.value.as_str())
    }

    pub fn default_providers(&self) -> Option<&[Provider]> {
        self.providers.as_ref().map(|s| s.value.as_slice())
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
        let error = cfg
            .require_profile(Provider::Codex, "missing")
            .expect_err("profile");
        assert!(
            matches!(&error, ConfigError::ProfileUndefined { provider: Provider::Codex, profile, .. } if profile == "missing")
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
        let local =
            parse_file("[_groups]\ng=['missing']", "fixture", Layer::Local).expect("parse local");
        assert!(matches!(
            resolve(local, None, "host", ConfigFiles::default()),
            Err(ConfigError::Resolve { .. })
        ));
    }

    #[test]
    fn bare_is_synthetic_and_all_excludes_it_with_or_without_a_definition() {
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
                cfg.select_archetypes(Some("bare")).expect("bare"),
                cfg.select_archetypes(None).expect("implicit bare")
            );
            assert_eq!(
                cfg.select_archetypes(Some("all")).expect("all"),
                vec![("a".into(), "prime".into())]
            );
            assert_eq!(
                crate::prompt::assemble(
                    &cfg.select_archetypes(Some("bare")).expect("bare")[0].1,
                    "  raw\n"
                ),
                "  raw\n"
            );
        }
    }

    #[test]
    fn selection_expands_groups_deduplicates_and_keeps_bare_unprimed() {
        let raw = "[archetypes]\na = 'prime'\nb = 'second'\nbare = ''\n[_groups]\ng = ['b', 'a']\n";
        let mut cfg = resolve(
            parse_file(raw, "fixture", Layer::Local).expect("parse"),
            None,
            "host",
            ConfigFiles::default(),
        )
        .expect("resolve");
        assert_eq!(
            cfg.select_archetypes(Some("g,a,bare")).expect("select"),
            vec![
                ("b".into(), "second".into()),
                ("a".into(), "prime".into()),
                ("bare".into(), String::new())
            ]
        );
        assert_eq!(
            cfg.select_archetypes(None).expect("bare"),
            vec![("bare".into(), String::new())]
        );
        assert!(cfg.select_archetypes(Some("unknown")).is_err());
        cfg.archetypes.remove("b");
        assert!(cfg.select_archetypes(Some("g")).is_err());
    }
}
