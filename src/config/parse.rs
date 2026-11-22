//! Parsing one config file. The sections (`[archetypes]`, `[profiles]`,
//! `[_defaults]`, `[_audit]`, `[_projects]`) are peeled off a `toml::Table`,
//! and anything left over is an error: a retired harness-scoped
//! `[<harness>.<profile>]` table gets a message saying where it moved, and any
//! other table (a retired legacy host table, a typo) is an unknown section.
//! Each `[profiles.<name>]` holds an optional `comment` and one entry per
//! harness, each read strictly as a `Profile`, so a misspelled field is still
//! an error. `[_audit]` in the global file is an error. The format itself is
//! documented in `docs/config.md` and `docs/profiles.md`.

use super::{
    AuditConfig, BARE, CONFIG_FILENAME, ConfigFile, DefaultsConfig, Harness, Layer, Profile,
    ProfileDef, reserved_names, toml_key,
};
use anyhow::{Result, bail};
use std::collections::BTreeMap;

/// Parse one config file. `origin` names it in errors. Checks everything that
/// can be checked within the file alone. Host-scoped `[_projects]` names are checked
/// later, once the current host is known, by `resolve` or `global_projects`.
pub fn parse_file(raw: &str, origin: &str, layer: Layer) -> super::Result<ConfigFile> {
    parse_file_inner(raw, origin, layer).map_err(|source| super::ConfigError::InvalidFile {
        origin: origin.to_string(),
        source,
    })
}

fn parse_file_inner(raw: &str, origin: &str, layer: Layer) -> Result<ConfigFile> {
    // Parse to a table first, then peel off the sections by name. This avoids
    // serde `flatten`, which does not coexist with a sibling named field
    // (`archetypes`).
    let mut table: toml::Table =
        toml::from_str(raw).map_err(|e| anyhow::anyhow!("failed to parse {origin}: {e}"))?;

    // Retired keys are refused rather than ignored, so a config written for
    // the old behaviour says why it no longer does what it says.
    if table.contains_key("_groups") {
        bail!(
            "[_groups] in {origin}: archetype groups were removed - a run takes one \
             archetype, so launch one `hrafnar -a <name>` per member instead"
        );
    }
    if let Some(toml::Value::Table(d)) = table.get("_defaults") {
        for retired in ["providers", "provider"] {
            if d.contains_key(retired) {
                bail!(
                    "[_defaults].{retired} in {origin}: the harness is chosen from the \
                     profile now - write `profile = \"<name>\"` naming a [profiles.<name>] \
                     table, and `prefer = [\"<harness>\", ...]` to order the harnesses a \
                     profile may run on"
                );
            }
        }
    }
    let audit: AuditConfig = match table.remove("_audit") {
        Some(_) if layer == Layer::Global => bail!(
            "[_audit] in {origin}: the audit block identifies a project, so it belongs \
             in that project's {CONFIG_FILENAME}, not in the global config"
        ),
        Some(v) => v
            .try_into()
            .map_err(|e| anyhow::anyhow!("[_audit] in {origin}: {e}"))?,
        None => AuditConfig::default(),
    };
    let defaults: DefaultsConfig = match table.remove("_defaults") {
        Some(v) => v
            .try_into()
            .map_err(|e| anyhow::anyhow!("[_defaults] in {origin}: {e}"))?,
        None => DefaultsConfig::default(),
    };
    if layer == Layer::Local && defaults.claude_token_command.is_some() {
        bail!(
            "[_defaults].claude_token_command in {origin}: a command hrafnar runs to fetch a \
             credential belongs in the global config; one named by a project's \
             {CONFIG_FILENAME} would run code from any checkout"
        );
    }
    let projects: BTreeMap<String, BTreeMap<String, String>> = match table.remove("_projects") {
        Some(_) if layer == Layer::Local => bail!(
            "[_projects] in {origin}: the project registry maps names to this machine's \
             checkouts, so it belongs in the global config, not in a project's {CONFIG_FILENAME}"
        ),
        Some(v) => v
            .try_into()
            .map_err(|e| anyhow::anyhow!("[_projects] in {origin}: expected [_projects.<host>] tables of name = \"path\": {e}"))?,
        None => BTreeMap::new(),
    };
    let archetypes: BTreeMap<String, String> = match table.remove("archetypes") {
        Some(v) => v
            .try_into()
            .map_err(|e| anyhow::anyhow!("[archetypes] in {origin}: {e}"))?,
        None => BTreeMap::new(),
    };
    let profiles = match table.remove("profiles") {
        Some(toml::Value::Table(t)) => parse_profiles(t, origin, layer)?,
        Some(_) => bail!("[profiles] in {origin}: expected a table of [profiles.<name>] tables"),
        None => BTreeMap::new(),
    };

    // Nothing else is a section. Both retired profile forms are named, since
    // an existing file is the likeliest source of one.
    if let Some(key) = table.keys().next() {
        if let Ok(harness) = key.parse::<Harness>() {
            bail!(
                "[{key}.*] in {origin}: harness-scoped profile tables were removed - \
                 move each [{key}.<name>] to [profiles.<name>.{harness}]"
            );
        }
        bail!(
            "unknown section '{key}' in {origin}\n  \
             Sections are [archetypes], [profiles.<name>], [_defaults], [_audit] and \
             [_projects]. Legacy host tables ([<host>.<harness>.<profile>]) were \
             removed: move a host's profiles to [profiles.<name>.<harness>]."
        );
    }

    for (name, prime) in &archetypes {
        if reserved_names().contains(name) {
            bail!("'{name}' is a reserved name and cannot be used as an archetype in {origin}");
        }
        if name == BARE && !prime.trim().is_empty() {
            bail!(
                "archetype 'bare' in {origin} has a prompt, but `bare` means no archetype - \
                 `hrafnar bare` and `-a bare` ignore it\n  \
                 Give it an empty prompt (bare = \"\") or rename it."
            );
        }
    }

    Ok(ConfigFile {
        archetypes,
        audit,
        defaults,
        profiles,
        projects,
    })
}

/// Keys of a harness entry, so a profile that writes one at the top level
/// (the format before profiles held one entry per harness) is told where it
/// goes rather than only that the key is unknown.
const ENTRY_KEYS: &[&str] = &[
    "model",
    "effort",
    "sandbox",
    "env",
    "config",
    "writable_roots",
];

/// `[profiles.<name>]` tables: an optional `comment` and one entry per
/// harness, `[profiles.<name>.<harness>]`, each read strictly as a `Profile`
/// and validated against its harness. Any other key is an error, and so is a
/// profile with no harness entry.
fn parse_profiles(
    t: toml::Table,
    origin: &str,
    layer: Layer,
) -> Result<BTreeMap<String, ProfileDef>> {
    let mut out = BTreeMap::new();
    for (name, v) in t {
        let table = format!("[profiles.{}]", toml_key(&name));
        let at = format!("{table} in {origin}");
        let entry_table = |harness: &str| format!("[profiles.{}.{harness}]", toml_key(&name));
        let toml::Value::Table(fields) = v else {
            bail!("{at}: expected a table");
        };
        // Checked before the other keys, so an old-format profile gets this
        // advice rather than an unknown-key error for its `model`.
        if fields.contains_key("provider") {
            bail!(
                "{at}: `provider` was removed - a profile now holds one entry per \
                 harness. Move the settings under {}, naming the harness that \
                 `provider` named.",
                entry_table("<harness>")
            );
        }
        let mut comment = None;
        let mut entries = BTreeMap::new();
        for (key, value) in fields {
            if key == "comment" {
                let toml::Value::String(c) = value else {
                    bail!("{at}: `comment` must be a string");
                };
                comment = Some(c);
                continue;
            }
            let Ok(harness) = key.parse::<Harness>() else {
                let hint = if ENTRY_KEYS.contains(&key.as_str()) {
                    format!(
                        "\n  `{key}` belongs to one harness: move it under {}.",
                        entry_table("<harness>")
                    )
                } else {
                    String::new()
                };
                bail!(
                    "{at}: unknown key '{key}'; a profile takes `comment` and one entry \
                     per harness ({}){hint}",
                    Harness::supported()
                );
            };
            let at = format!("{} in {origin}", entry_table(harness.as_str()));
            let profile: Profile = match value {
                toml::Value::Table(_) => {
                    value.try_into().map_err(|e| anyhow::anyhow!("{at}: {e}"))?
                }
                _ => bail!(
                    "{at}: expected a table of settings (`{harness} = {{}}` for its defaults)"
                ),
            };
            profile
                .validate(harness)
                .map_err(|e| anyhow::anyhow!("{at}: {e}"))?;
            entries.insert(harness, profile);
        }
        if entries.is_empty() {
            bail!(
                "{at}: no harness entry - a profile names at least one harness it runs on \
                 ({}), as {} (`{{}}` for that harness's defaults)",
                Harness::supported(),
                entry_table("<harness>")
            );
        }
        out.insert(
            name,
            ProfileDef {
                comment,
                entries,
                layer,
                table,
            },
        );
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::{HrafnarConfig, resolve};
    use super::*;

    /// Parse a single local file and resolve it alone, as a project with no
    /// global config would be.
    fn local_only(raw: &str) -> Result<HrafnarConfig> {
        Ok(resolve(
            parse_file(raw, "local", Layer::Local)?,
            None,
            "h",
            crate::config::ConfigFiles::default(),
        )?)
    }

    /// A project file naming a command hrafnar would run is code execution
    /// from any checkout; only the operator's global file may.
    #[test]
    fn claude_token_command_is_global_only() {
        let raw = "[_defaults]\nclaude_token_command = \"echo t\"\n";
        let err = local_only(raw).unwrap_err().to_string();
        assert!(err.contains("global config"), "{err}");
        let cfg = resolve(
            parse_file("", "local", Layer::Local).unwrap(),
            Some(parse_file(raw, "global", Layer::Global).unwrap()),
            "h",
            crate::config::ConfigFiles::default(),
        )
        .unwrap();
        assert_eq!(cfg.claude_token_command.as_deref(), Some("echo t"));
    }

    #[test]
    fn parses_archetypes() {
        let raw = "\
[archetypes]
security = \"be a security expert\"
bugs = \"find edge cases\"
";
        let cfg = local_only(raw).unwrap();
        assert_eq!(cfg.archetypes.len(), 2);
        assert_eq!(cfg.archetype("security"), Some("be a security expert"));
        assert_eq!(cfg.archetype("bugs"), Some("find edge cases"));
    }

    #[test]
    fn parses_profiles_with_one_entry_per_harness() {
        let raw = "\
[profiles.review]
comment = \"Standard reviewer.\"
codex = { model = \"gpt-5.6-terra\", effort = \"low\", sandbox = \"workspace-write\" }
claude = { model = \"Opus 4.8\", env = { ANTHROPIC_BASE_URL = \"http://localhost:8787\" } }

[profiles.plain]
grok = {}
";
        let cfg = local_only(raw).unwrap();
        let review = cfg.resolve_profile("review").unwrap();
        assert_eq!(review.table, "[profiles.review]");
        assert_eq!(review.layer, Layer::Local);
        assert_eq!(review.comment.as_deref(), Some("Standard reviewer."));
        assert_eq!(
            review.harnesses().collect::<Vec<_>>(),
            [Harness::Claude, Harness::Codex]
        );
        let claude = review.entry(Harness::Claude).unwrap();
        assert_eq!(claude.table, "[profiles.review.claude]");
        assert_eq!(claude.profile.model.as_deref(), Some("Opus 4.8"));
        assert_eq!(
            claude.profile.env.as_ref().unwrap()["ANTHROPIC_BASE_URL"],
            "http://localhost:8787"
        );
        let codex = review.entry(Harness::Codex).unwrap();
        assert_eq!(
            codex
                .profile
                .sandbox
                .as_ref()
                .map(crate::config::SandboxLevel::as_str),
            Some("workspace-write")
        );
        assert!(review.entry(Harness::Grok).is_none());
        // An empty entry is that harness on its own defaults.
        let plain = cfg.resolve_profile("plain").unwrap();
        assert!(plain.comment.is_none());
        assert!(plain.entries[&Harness::Grok].model.is_none());
        assert!(cfg.resolve_profile("nope").is_none());
    }

    #[test]
    fn a_dotted_profile_name_is_quoted_in_entry_tables() {
        let cfg = local_only("[profiles.\"a.b\".codex]\n").unwrap();
        let def = cfg.resolve_profile("a.b").unwrap();
        assert_eq!(def.table, "[profiles.\"a.b\"]");
        assert_eq!(
            def.entry(Harness::Codex).unwrap().table,
            "[profiles.\"a.b\".codex]"
        );
    }

    #[test]
    fn a_profile_needs_a_harness_entry() {
        for raw in ["[profiles.x]\n", "[profiles.x]\ncomment = \"c\"\n"] {
            let err = local_only(raw).unwrap_err().to_string();
            assert!(err.contains("no harness entry"), "{err}");
            assert!(err.contains("[profiles.x]"), "{err}");
        }
    }

    #[test]
    fn the_retired_provider_key_says_where_settings_moved() {
        let err = local_only("[profiles.x]\nprovider = \"codex\"\nmodel = \"m\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("`provider` was removed"), "{err}");
        assert!(err.contains("[profiles.x.<harness>]"), "{err}");
    }

    #[test]
    fn unknown_profile_keys_name_the_allowed_ones() {
        let err = local_only("[profiles.x]\ngpt = {}\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown key 'gpt'"), "{err}");
        assert!(err.contains("`comment`"), "{err}");
        assert!(err.contains("claude, codex, grok, maki"), "{err}");
        // An entry setting written at the profile level is pointed at its home.
        let err = local_only("[profiles.x]\nmodel = \"m\"\ncodex = {}\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown key 'model'"), "{err}");
        assert!(
            err.contains("move it under [profiles.x.<harness>]"),
            "{err}"
        );
    }

    #[test]
    fn entries_and_comments_must_have_the_right_shape() {
        let err = local_only("[profiles.x]\ncodex = \"gpt\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("[profiles.x.codex]"), "{err}");
        assert!(err.contains("expected a table"), "{err}");
        let err = local_only("[profiles.x]\ncomment = 1\ncodex = {}\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("`comment` must be a string"), "{err}");
    }

    #[test]
    fn a_misspelled_entry_key_is_still_an_error() {
        let err = local_only("[profiles.x.codex]\nmodle = \"m\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("modle"), "{err}");
        assert!(err.contains("[profiles.x.codex]"), "{err}");
    }

    #[test]
    fn each_entry_is_validated_against_its_own_harness() {
        // sandbox is fine for codex, refused for claude, in the same profile.
        let err = local_only(
            "[profiles.x]\ncodex = { sandbox = \"read-only\" }\nclaude = { sandbox = \"read-only\" }\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("[profiles.x.claude]"), "{err}");
        assert!(err.contains("claude does not honour 'sandbox'"), "{err}");
    }

    #[test]
    fn parses_defaults_prefer() {
        let cfg = local_only("[_defaults]\nprefer = [\"codex\", \"claude\"]\n").unwrap();
        assert_eq!(cfg.prefer(), [Harness::Codex, Harness::Claude]);
        let err = local_only("[_defaults]\nprefer = [\"gpt\"]\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("gpt"), "{err}");
    }

    #[test]
    fn empty_config_parses() {
        let cfg = local_only("").unwrap();
        assert!(cfg.archetypes.is_empty());
        assert!(cfg.profiles.is_empty());
        assert!(cfg.profile.is_none());
    }

    #[test]
    fn parses_defaults_profile() {
        let cfg =
            local_only("[_defaults]\nprofile = \"deep\"\n[profiles.deep]\ncodex = {}\n").unwrap();
        assert_eq!(cfg.default_profile(), Some("deep"));
    }

    #[test]
    fn retired_keys_are_refused_by_name() {
        for raw in [
            "[_defaults]\nproviders = [\"claude\"]\n",
            "[_defaults]\nprovider = \"claude\"\n",
        ] {
            let err = local_only(raw).unwrap_err().to_string();
            assert!(err.contains("profile = "), "{err}");
            assert!(err.contains("prefer = "), "{err}");
        }
        let err = local_only("[archetypes]\na = \"a\"\n[_groups]\ng = [\"a\"]\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("groups were removed"), "{err}");
    }

    #[test]
    fn retired_profile_tables_say_where_they_moved() {
        let err = local_only("[codex.deep]\nmodel = \"m\"\n")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("move each [codex.<name>] to [profiles.<name>.codex]"),
            "{err}"
        );
        let err = local_only("[myhost.codex.deep]\nmodel = \"m\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown section 'myhost'"), "{err}");
        assert!(err.contains("Legacy host tables"), "{err}");
        assert!(err.contains("[profiles.<name>.<harness>]"), "{err}");
        let err = local_only("stray = 1\n").unwrap_err().to_string();
        assert!(err.contains("unknown section 'stray'"), "{err}");
    }

    #[test]
    fn reserved_archetype_name_errors() {
        let err = local_only("[archetypes]\nhelp = \"a\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("reserved"), "{err}");
    }

    #[test]
    fn subcommand_names_are_reserved_as_archetype_names() {
        // `-a` cannot collide with a subcommand, but the legacy positional form
        // can: `hrafnar sessions` runs the subcommand, silently, instead of the
        // archetype.
        for name in ["sessions", "help", "init", "incidents", "config", "resume"] {
            let as_archetype = format!("[archetypes]\n{name} = \"x\"\n");
            let err = local_only(&as_archetype).unwrap_err().to_string();
            assert!(err.contains("reserved"), "{name} as archetype: {err}");
        }
    }

    #[test]
    fn bare_may_only_be_an_empty_archetype() {
        // `bare = ""` is in most existing configs and means what `hrafnar bare`
        // means. A prompt under that name would be silently ignored by it.
        assert!(local_only("[archetypes]\nbare = \"\"\n").is_ok());
        let err = local_only("[archetypes]\nbare = \"be terse\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("bare"), "{err}");
    }
}
