mod ids;
mod load;
mod parse;
mod resolve;
mod sandbox;

pub use ids::{generate_uuid, try_generate_short_id};
pub use load::{
    Selection, global_config_path, global_projects, init, load, load_optional, project_root,
};
pub use parse::parse_file;
pub use resolve::resolve;
pub use sandbox::sandbox_for;

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

const CONFIG_FILENAME: &str = ".review.toml";
pub const KNOWN_PROVIDERS: &[&str] = &["claude", "codex", "grok"];

/// Names that can't be archetypes or groups: `all`, the `-a` keyword for every
/// configured archetype except `bare`, and the subcommand names. `-a` itself
/// cannot collide with a subcommand, but the legacy positional form can: `review
/// sessions` runs the subcommand, silently, rather than the archetype. So they
/// stay reserved while that alias exists. Keep in step with `cli::Command` and the
/// "Reserved words" table in `docs/config.md`.
pub const RESERVED_NAMES: &[&str] = &[
    "all",
    "message",
    "resume",
    "interrupt",
    "config",
    "sessions",
    "incidents",
    "init",
    "man",
    "help",
];

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
    pub id: Option<String>,
}

/// Defaults under `[_defaults]`. Each field is `Option` so an absent key falls
/// through to the next layer while a present one - even an empty list - wins.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DefaultsConfig {
    /// The provider list used when `--provider` is omitted.
    pub providers: Option<Vec<String>>,
    /// Seconds of rollout silence, with no final answer written, after which a
    /// codex run is treated as stalled: killed, bundled, and reported as a
    /// failure. `0` disables the check; omitted uses the built-in default
    /// (see `watchdog::Timings`).
    ///
    /// This is a genuine timeout resting on an *empirical* property of codex -
    /// that it wakes itself every few minutes and cannot stay silent - so it is
    /// deliberately tunable, and switchable off, in case a future codex changes
    /// cadence. codex-only; claude has no rollout to watch.
    pub stall_timeout_secs: Option<u64>,
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
    /// Sandbox / write-access level, passed as `--sandbox` by both codex (e.g.
    /// `read-only`, `workspace-write`) and grok (which resolves the name against
    /// its own built-in and `sandbox.toml` profiles). Defaults to `read-only`
    /// when unset. Ignored by claude, which has no sandbox on this axis.
    pub sandbox: Option<String>,
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
    /// Ignored unless the profile's `sandbox` is `workspace-write`, because
    /// widening a `read-only` profile would contradict the only thing that
    /// profile promises. `~`, `$VAR` and `${VAR}` expand, which is what lets one
    /// hostless profile serve hosts with different layouts.
    #[serde(default)]
    pub writable_roots: Vec<String>,
}

/// provider -> profile name -> profile.
type ProfileTables = BTreeMap<String, BTreeMap<String, Profile>>;

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
    pub providers: Option<Sourced<Vec<String>>>,
    pub stall_timeout_secs: Option<Sourced<u64>>,
    /// provider -> profile name -> entry.
    pub profiles: BTreeMap<String, BTreeMap<String, ProfileEntry>>,
    pub hostname: String,
    pub files: ConfigFiles,
    /// This host's `[_projects.<host>]` registry, name -> path as written.
    pub projects: BTreeMap<String, String>,
}

impl ReviewConfig {
    /// The effective profile for `provider`, if any layer defines it.
    pub fn resolve_profile(&self, provider: &str, profile: &str) -> Option<&Profile> {
        self.profiles
            .get(provider)?
            .get(profile)
            .map(|e| &e.effective.profile)
    }

    /// The effective profile, or an error describing the tables searched in
    /// precedence order. Keep the lookup and its explanation together so the
    /// app does not need to restate the config resolver's search rules.
    pub(crate) fn require_profile(
        &self,
        provider: &str,
        profile: &str,
    ) -> anyhow::Result<&Profile> {
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
            anyhow::anyhow!(
                "profile '{profile}' not defined for provider '{provider}'\n  looked for {searched}"
            )
        })
    }

    pub fn archetype(&self, name: &str) -> Option<&str> {
        self.archetypes.get(name).map(|s| s.value.as_str())
    }

    pub fn default_providers(&self) -> Option<&[String]> {
        self.providers.as_ref().map(|s| s.value.as_slice())
    }

    pub fn stall_timeout_secs(&self) -> Option<u64> {
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

/// Format a hostname as a TOML key, quoting only if it contains dots.
pub fn toml_key(key: &str) -> String {
    if key.contains('.') {
        format!("\"{key}\"")
    } else {
        key.to_string()
    }
}
