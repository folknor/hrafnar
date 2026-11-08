use super::{
    AuditConfig, BARE, CONFIG_FILENAME, ConfigFile, DefaultsConfig, Layer, Profile, ProfileTables,
    Provider, reserved_names, toml_key,
};
use anyhow::{Result, bail};
use std::collections::BTreeMap;

fn unknown_provider_hint() -> String {
    format!("supported: {}", Provider::supported())
}

/// Parse one config file. `origin` names it in errors. Checks everything that
/// can be checked within the file alone; cross-file checks (group members,
/// group/archetype name clashes) happen in `resolve`, because a project's group
/// may name a global archetype. Host-scoped `[_projects]` names are checked
/// later, once the current host is known, by `resolve` or `global_projects`.
pub fn parse_file(raw: &str, origin: &str, layer: Layer) -> super::Result<ConfigFile> {
    parse_file_inner(raw, origin, layer).map_err(|source| super::ConfigError::InvalidFile {
        origin: origin.to_string(),
        source,
    })
}

fn parse_file_inner(raw: &str, origin: &str, layer: Layer) -> Result<ConfigFile> {
    // Parse to a table first, then peel off the reserved sections by name.
    // Everything left over is a provider table or a legacy host table. This
    // avoids serde `flatten`, which does not coexist with a sibling named field
    // (`archetypes`).
    let mut table: toml::Table =
        toml::from_str(raw).map_err(|e| anyhow::anyhow!("failed to parse {origin}: {e}"))?;

    let groups: BTreeMap<String, Vec<String>> = match table.remove("_groups") {
        Some(v) => v
            .try_into()
            .map_err(|e| anyhow::anyhow!("[_groups] in {origin}: {e}"))?,
        None => BTreeMap::new(),
    };
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

    // Remaining top-level tables: a known provider name is a hostless
    // `[<provider>.<profile>]` table; anything else is a legacy host.
    let mut profiles: ProfileTables = BTreeMap::new();
    let mut hosts: BTreeMap<String, ProfileTables> = BTreeMap::new();
    for (key, val) in table {
        let toml::Value::Table(inner) = val else {
            bail!("unexpected top-level key '{key}' in {origin}: expected a table");
        };
        let Ok(provider) = key.parse::<Provider>() else {
            hosts.insert(key.clone(), parse_host(inner, &key, origin)?);
            continue;
        };
        // Provider names and legacy host names share the top-level namespace,
        // so a host named after a provider - one that exists now, or one added
        // later - would silently change meaning. Both readings are therefore
        // checked against the full schema: a table that is also a valid legacy
        // host table is refused rather than guessed at. (A non-empty one only:
        // an empty `[<provider>]` table cannot be meant as a host.)
        if !inner.is_empty() && parse_host(inner.clone(), &key, origin).is_ok() {
            bail!(
                "[{key}.*] in {origin} reads both as {key} profiles ([{key}.<profile>]) and as \
                 a legacy host table for a host named '{key}' ([{key}.<provider>.<profile>])\n  \
                 Rename the profile if you meant {key} profiles, or move the host's profiles \
                 to hostless [<provider>.<profile>] tables if you meant the host."
            );
        }
        profiles.insert(provider, parse_profiles(inner, provider, &key, origin)?);
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
    for (name, members) in &groups {
        if reserved_names().contains(name) || name == BARE {
            bail!("'{name}' is a reserved name and cannot be used as a group in {origin}");
        }
        if members.is_empty() {
            bail!("group '{name}' is empty in {origin}");
        }
        let mut seen = std::collections::HashSet::new();
        for member in members {
            if !seen.insert(member) {
                bail!("group '{name}' contains duplicate archetype '{member}' in {origin}");
            }
            // A global group may only name global archetypes. The global file
            // is read in every project, so a group leaning on one project's
            // archetype would be an error everywhere else - and a global group
            // shadowed by a project group is checked here or nowhere.
            if layer == Layer::Global && !archetypes.contains_key(member) {
                bail!(
                    "group '{name}' references archetype '{member}', which {origin} does not \
                     define\n  A global group may only name global archetypes."
                );
            }
        }
    }
    // `[_defaults].providers` was validated as it was deserialized: each entry
    // is a `Provider`, whose parse error names the supported list.

    Ok(ConfigFile {
        archetypes,
        groups,
        audit,
        defaults,
        profiles,
        hosts,
        projects,
    })
}

/// A legacy `[<host>.<provider>.<profile>]` table's contents, strictly: every
/// child must be a known provider's table of valid profiles.
fn parse_host(inner: toml::Table, key: &str, origin: &str) -> Result<ProfileTables> {
    let mut host_profiles: ProfileTables = BTreeMap::new();
    for (prov, pv) in inner {
        let Ok(provider) = prov.parse::<Provider>() else {
            bail!(
                "unknown provider '{prov}' in [{key}.{prov}.*] in {origin}\n  \
                 {}\n  \
                 A top-level table is either a provider ([<provider>.<profile>]) \
                 or a legacy host ([<host>.<provider>.<profile>]).",
                unknown_provider_hint()
            );
        };
        let toml::Value::Table(pt) = pv else {
            bail!("[{key}.{prov}] in {origin}: expected a table of profiles");
        };
        let prefix = format!("{}.{prov}", toml_key(key));
        host_profiles.insert(provider, parse_profiles(pt, provider, &prefix, origin)?);
    }
    Ok(host_profiles)
}

fn parse_profiles(
    t: toml::Table,
    provider: Provider,
    prefix: &str,
    origin: &str,
) -> Result<BTreeMap<String, Profile>> {
    let mut out = BTreeMap::new();
    for (name, v) in t {
        let profile: Profile = v
            .try_into()
            .map_err(|e| anyhow::anyhow!("[{prefix}.{name}] in {origin}: {e}"))?;
        profile
            .validate(provider)
            .map_err(|e| anyhow::anyhow!("[{prefix}.{name}] in {origin}: {e}"))?;
        out.insert(name, profile);
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
    fn local_only(raw: &str, host: &str) -> Result<ReviewConfig> {
        Ok(resolve(
            parse_file(raw, "local", Layer::Local)?,
            None,
            host,
            crate::config::ConfigFiles::default(),
        )?)
    }

    #[test]
    fn parses_archetypes() {
        let raw = "\
[archetypes]
security = \"be a security expert\"
bugs = \"find edge cases\"
";
        let cfg = local_only(raw, "h").unwrap();
        assert_eq!(cfg.archetypes.len(), 2);
        assert_eq!(cfg.archetype("security"), Some("be a security expert"));
        assert_eq!(cfg.archetype("bugs"), Some("find edge cases"));
    }

    #[test]
    fn parses_legacy_host_profiles() {
        let raw = "\
[archetypes]
bugs = \"find edge cases\"

[myhost.claude.opus]
model = \"Opus 4.8\"
effort = \"medium\"
env = { ANTHROPIC_BASE_URL = \"http://localhost:8787\" }

[myhost.codex.implement]
model = \"gpt-5.6-terra\"
effort = \"high\"
sandbox = \"workspace-write\"
";
        let cfg = local_only(raw, "myhost").unwrap();

        let opus = cfg.resolve_profile(Provider::Claude, "opus").unwrap();
        assert_eq!(opus.model.as_deref(), Some("Opus 4.8"));
        assert_eq!(opus.effort.as_deref(), Some("medium"));
        assert_eq!(opus.sandbox, None);
        assert_eq!(
            opus.env.as_ref().unwrap()["ANTHROPIC_BASE_URL"],
            "http://localhost:8787"
        );

        let implement = cfg.resolve_profile(Provider::Codex, "implement").unwrap();
        assert_eq!(implement.model.as_deref(), Some("gpt-5.6-terra"));
        assert_eq!(implement.effort.as_deref(), Some("high"));
        assert_eq!(
            implement
                .sandbox
                .as_ref()
                .map(crate::config::SandboxLevel::as_str),
            Some("workspace-write")
        );

        assert!(cfg.resolve_profile(Provider::Claude, "nope").is_none());

        // Another host's tables do not apply here.
        let other = local_only(raw, "otherhost").unwrap();
        assert!(other.resolve_profile(Provider::Claude, "opus").is_none());
    }

    #[test]
    fn parses_hostless_profiles() {
        let raw = "\
[codex.deep]
model = \"gpt-6-luna\"
sandbox = \"read-only\"
";
        for host in ["a", "b"] {
            let cfg = local_only(raw, host).unwrap();
            let deep = cfg.resolve_profile(Provider::Codex, "deep").unwrap();
            assert_eq!(deep.model.as_deref(), Some("gpt-6-luna"));
        }
    }

    #[test]
    fn empty_config_parses() {
        let cfg = local_only("", "h").unwrap();
        assert!(cfg.archetypes.is_empty());
        assert!(cfg.profiles.is_empty());
        assert!(cfg.providers.is_none());
    }

    #[test]
    fn unknown_provider_in_profile_errors() {
        let raw = "\
[archetypes]
bugs = \"x\"

[myhost.gpt.fast]
model = \"whatever\"
";
        let err = local_only(raw, "myhost").unwrap_err().to_string();
        assert!(err.contains("unknown provider 'gpt'"), "{err}");
    }

    #[test]
    fn parses_defaults_providers() {
        let raw = "\
[_defaults]
providers = [\"claude\", \"codex\"]
";
        let cfg = local_only(raw, "h").unwrap();
        assert_eq!(
            cfg.default_providers().unwrap(),
            &[Provider::Claude, Provider::Codex]
        );
    }

    #[test]
    fn a_provider_table_that_is_also_a_valid_host_table_is_refused() {
        // A legacy host named after a provider: `[claude.codex.deep]` is codex's
        // `deep` profile on host `claude`, and also claude's `codex` profile
        // with a `deep` key. Neither reading may win silently.
        let raw = "[claude.codex.deep]\nmodel = \"m\"\n";
        let err = local_only(raw, "h").unwrap_err().to_string();
        assert!(err.contains("reads both as claude profiles"), "{err}");
        // An empty profile named after a provider satisfies both readings too.
        assert!(local_only("[claude.codex]\n", "h").is_err());
    }

    #[test]
    fn a_provider_table_that_is_not_a_host_table_is_a_provider_table() {
        // `env` is a table, so this is host-shaped, but as a host table its
        // `env` "profile" holds an unknown key: only the provider reading is
        // valid.
        let raw = "[claude.codex]\nenv = { TOKEN = \"v\" }\n";
        let cfg = local_only(raw, "h").unwrap();
        assert!(cfg.resolve_profile(Provider::Claude, "codex").is_some());
        // An empty provider table is never a host.
        assert!(local_only("[claude]\n", "h").is_ok());
    }

    #[test]
    fn parses_groups() {
        let raw = "\
[archetypes]
security = \"a\"
bugs = \"b\"

[_groups]
sweep = [\"security\", \"bugs\"]
";
        let cfg = local_only(raw, "h").unwrap();
        assert_eq!(cfg.groups.len(), 1);
        assert_eq!(cfg.groups["sweep"].value, vec!["security", "bugs"]);
    }

    #[test]
    fn group_with_unknown_member_errors() {
        let raw = "\
[archetypes]
security = \"a\"

[_groups]
sweep = [\"security\", \"nonexistent\"]
";
        let err = local_only(raw, "h").unwrap_err().to_string();
        assert!(err.contains("nonexistent"), "{err}");
    }

    #[test]
    fn group_name_conflicts_with_archetype() {
        let raw = "\
[archetypes]
security = \"a\"

[_groups]
security = [\"security\"]
";
        let err = local_only(raw, "h").unwrap_err().to_string();
        assert!(err.contains("conflicts"), "{err}");
    }

    #[test]
    fn reserved_archetype_name_errors() {
        let err = local_only("[archetypes]\nall = \"a\"\n", "h")
            .unwrap_err()
            .to_string();
        assert!(err.contains("reserved"), "{err}");
    }

    #[test]
    fn subcommand_names_are_reserved_as_archetype_and_group_names() {
        // `-a` cannot collide with a subcommand, but the legacy positional form
        // can: `review sessions` runs the subcommand, silently, instead of the
        // archetype.
        for name in ["sessions", "help", "init", "incidents", "config", "resume"] {
            let as_archetype = format!("[archetypes]\n{name} = \"x\"\n");
            let err = local_only(&as_archetype, "h").unwrap_err().to_string();
            assert!(err.contains("reserved"), "{name} as archetype: {err}");
            let as_group = format!("[archetypes]\nx = \"x\"\n[_groups]\n{name} = [\"x\"]\n");
            let err = local_only(&as_group, "h").unwrap_err().to_string();
            assert!(err.contains("reserved"), "{name} as group: {err}");
        }
    }

    #[test]
    fn bare_may_only_be_an_empty_archetype() {
        // `bare = ""` is in most existing configs and means what `review bare`
        // means. A prompt under that name would be silently ignored by it.
        assert!(local_only("[archetypes]\nbare = \"\"\n", "h").is_ok());
        let err = local_only("[archetypes]\nbare = \"be terse\"\n", "h")
            .unwrap_err()
            .to_string();
        assert!(err.contains("bare"), "{err}");
        let err = local_only("[archetypes]\nx = \"x\"\n[_groups]\nbare = [\"x\"]\n", "h")
            .unwrap_err()
            .to_string();
        assert!(err.contains("reserved"), "{err}");
    }
}
