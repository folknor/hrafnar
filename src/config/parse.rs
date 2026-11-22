//! Parsing one config file. The sections (`[archetypes]`, `[profiles]`,
//! `[_defaults]`, `[_audit]`, `[_projects]`) are peeled off a `toml::Table`,
//! and anything left over is an error: a retired harness-scoped
//! `[<harness>.<profile>]` table gets a message saying where it moved, and any
//! other table (a retired legacy host table, a typo) is an unknown section.
//! Each `[profiles.<name>]` names its harness with a required `harness` key,
//! which is taken off before the rest is read as a `Profile`, so a misspelled
//! field is still an error. `[_audit]` in the global file is an error. The
//! format itself is documented in `docs/config.md`.

use super::{
    AuditConfig, BARE, CONFIG_FILENAME, ConfigFile, DefaultsConfig, Harness, Layer, Profile,
    reserved_names, toml_key,
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
             archetype, so launch one `review -a <name>` per member instead"
        );
    }
    if let Some(toml::Value::Table(d)) = table.get("_defaults") {
        for retired in ["providers", "provider"] {
            if d.contains_key(retired) {
                bail!(
                    "[_defaults].{retired} in {origin}: the provider is part of the profile \
                     now - write `profile = \"<name>\"` naming a [profiles.<name>] table"
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
            "[_defaults].claude_token_command in {origin}: a command review runs to fetch a \
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
        Some(toml::Value::Table(t)) => parse_profiles(t, origin)?,
        Some(_) => bail!("[profiles] in {origin}: expected a table of [profiles.<name>] tables"),
        None => BTreeMap::new(),
    };

    // Nothing else is a section. Both retired profile forms are named, since
    // an existing file is the likeliest source of one.
    if let Some(key) = table.keys().next() {
        if let Ok(harness) = key.parse::<Harness>() {
            bail!(
                "[{key}.*] in {origin}: provider-scoped profile tables were removed - \
                 move each [{key}.<name>] to [profiles.<name>] with provider = \"{harness}\""
            );
        }
        bail!(
            "unknown section '{key}' in {origin}\n  \
             Sections are [archetypes], [profiles.<name>], [_defaults], [_audit] and \
             [_projects]. Legacy host tables ([<host>.<provider>.<profile>]) were \
             removed: move a host's profiles to [profiles.<name>] with a provider key."
        );
    }

    for (name, prime) in &archetypes {
        if reserved_names().contains(name) {
            bail!("'{name}' is a reserved name and cannot be used as an archetype in {origin}");
        }
        if name == BARE && !prime.trim().is_empty() {
            bail!(
                "archetype 'bare' in {origin} has a prompt, but `bare` means no archetype - \
                 `review bare` and `-a bare` ignore it\n  \
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

/// `[profiles.<name>]` tables: each takes its required `harness` off, reads
/// the rest strictly as a `Profile`, and is validated against that harness.
fn parse_profiles(t: toml::Table, origin: &str) -> Result<BTreeMap<String, (Harness, Profile)>> {
    let mut out = BTreeMap::new();
    for (name, v) in t {
        let at = format!("[profiles.{}] in {origin}", toml_key(&name));
        let toml::Value::Table(mut fields) = v else {
            bail!("{at}: expected a table");
        };
        let harness: Harness = match fields.remove("provider") {
            Some(p) => p.try_into().map_err(|e| anyhow::anyhow!("{at}: {e}"))?,
            None => bail!(
                "{at}: missing `provider` - every profile names the provider it runs on \
                 (supported: {})",
                Harness::supported()
            ),
        };
        let profile: Profile = toml::Value::Table(fields)
            .try_into()
            .map_err(|e| anyhow::anyhow!("{at}: {e}"))?;
        profile
            .validate(harness)
            .map_err(|e| anyhow::anyhow!("{at}: {e}"))?;
        out.insert(name, (harness, profile));
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::{ReviewConfig, resolve};
    use super::*;

    /// Parse a single local file and resolve it alone, as a project with no
    /// global config would be.
    fn local_only(raw: &str) -> Result<ReviewConfig> {
        Ok(resolve(
            parse_file(raw, "local", Layer::Local)?,
            None,
            "h",
            crate::config::ConfigFiles::default(),
        )?)
    }

    /// A project file naming a command review would run is code execution
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
    fn parses_profiles_with_their_harness() {
        let raw = "\
[profiles.opus]
provider = \"claude\"
model = \"Opus 4.8\"
effort = \"medium\"
env = { ANTHROPIC_BASE_URL = \"http://localhost:8787\" }

[profiles.implement]
provider = \"codex\"
model = \"gpt-5.6-terra\"
sandbox = \"workspace-write\"

[profiles.plain]
provider = \"grok\"
";
        let cfg = local_only(raw).unwrap();
        let opus = cfg.resolve_profile("opus").unwrap();
        assert_eq!(opus.harness, Harness::Claude);
        assert_eq!(opus.table, "[profiles.opus]");
        assert_eq!(opus.profile.model.as_deref(), Some("Opus 4.8"));
        assert_eq!(
            opus.profile.env.as_ref().unwrap()["ANTHROPIC_BASE_URL"],
            "http://localhost:8787"
        );
        let implement = cfg.resolve_profile("implement").unwrap();
        assert_eq!(implement.harness, Harness::Codex);
        assert_eq!(
            implement
                .profile
                .sandbox
                .as_ref()
                .map(crate::config::SandboxLevel::as_str),
            Some("workspace-write")
        );
        // A harness alone is a profile: that harness on its own defaults.
        let plain = cfg.resolve_profile("plain").unwrap();
        assert_eq!(plain.harness, Harness::Grok);
        assert!(plain.profile.model.is_none());
        assert!(cfg.resolve_profile("nope").is_none());
    }

    #[test]
    fn a_profile_must_name_a_known_harness() {
        let err = local_only("[profiles.x]\nmodel = \"m\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing `provider`"), "{err}");
        let err = local_only("[profiles.x]\nprovider = \"gpt\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown harness 'gpt'"), "{err}");
        assert!(err.contains("[profiles.x]"), "{err}");
    }

    #[test]
    fn a_misspelled_profile_key_is_still_an_error() {
        let err = local_only("[profiles.x]\nprovider = \"codex\"\nmodle = \"m\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("modle"), "{err}");
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
            local_only("[_defaults]\nprofile = \"deep\"\n[profiles.deep]\nprovider = \"codex\"\n")
                .unwrap();
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
            err.contains("[profiles.<name>] with provider = \"codex\""),
            "{err}"
        );
        let err = local_only("[myhost.codex.deep]\nmodel = \"m\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown section 'myhost'"), "{err}");
        assert!(err.contains("Legacy host tables"), "{err}");
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
        // can: `review sessions` runs the subcommand, silently, instead of the
        // archetype.
        for name in ["sessions", "help", "init", "incidents", "config", "resume"] {
            let as_archetype = format!("[archetypes]\n{name} = \"x\"\n");
            let err = local_only(&as_archetype).unwrap_err().to_string();
            assert!(err.contains("reserved"), "{name} as archetype: {err}");
        }
    }

    #[test]
    fn bare_may_only_be_an_empty_archetype() {
        // `bare = ""` is in most existing configs and means what `review bare`
        // means. A prompt under that name would be silently ignored by it.
        assert!(local_only("[archetypes]\nbare = \"\"\n").is_ok());
        let err = local_only("[archetypes]\nbare = \"be terse\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("bare"), "{err}");
    }
}
