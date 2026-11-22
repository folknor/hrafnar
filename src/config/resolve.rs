//! Combining the local and global layers into one `ReviewConfig`. Every
//! resolved value is `Sourced` (layer and table), and each profile keeps the
//! definitions it shadows, so `review config` can show both. The precedence
//! rules and their reasons are in `reference/design.md`; the tests are in
//! `resolve/tests.rs`.

use super::{
    ConfigFile, ConfigFiles, Layer, ProfileDef, ProfileEntry, Provider, ReviewConfig, Sourced,
    toml_key,
};
use anyhow::Result;
use std::collections::BTreeMap;

/// Resolve the local and (optional) global layers for `hostname`.
///
/// Everything is first-definition-wins over the precedence order local, then
/// global; within one file a legacy host table for this host beats a hostless
/// table. Profiles and lists win whole - nothing is merged field by field or
/// element by element.
pub fn resolve(
    local: ConfigFile,
    global: Option<ConfigFile>,
    hostname: &str,
    files: ConfigFiles,
) -> super::Result<ReviewConfig> {
    let project_root = files
        .local
        .as_deref()
        .map(crate::project::ProjectRoot::from_config_path)
        .transpose()?;
    resolve_inner(local, global, hostname, files, project_root)
        .map_err(|source| super::ConfigError::Resolve { source })
}

fn resolve_inner(
    local: ConfigFile,
    global: Option<ConfigFile>,
    hostname: &str,
    files: ConfigFiles,
    project_root: Option<crate::project::ProjectRoot>,
) -> Result<ReviewConfig> {
    let audit = local.audit.clone();
    let mut layers: Vec<(Layer, ConfigFile)> = vec![(Layer::Local, local)];
    if let Some(g) = global {
        layers.push((Layer::Global, g));
    }

    let mut archetypes: BTreeMap<String, Sourced<String>> = BTreeMap::new();
    let mut provider: Option<Sourced<Provider>> = None;
    let mut stall_timeout_secs: Option<Sourced<super::StallTimeout>> = None;
    let mut claude_token_command: Option<String> = None;
    let mut defs: BTreeMap<Provider, BTreeMap<String, Vec<ProfileDef>>> = BTreeMap::new();
    let mut projects = BTreeMap::new();

    for (layer, mut file) in layers {
        // Only the global layer can carry one (`parse_file` refuses it locally).
        // A named `-C` selection checks this same registry in `global_projects`
        // before the full project config is loaded; both passes use this helper.
        if let Some(mine) = file.projects.remove(hostname) {
            crate::project::registry::check_projects(&mine, hostname, layer.as_str())?;
            projects = mine;
        }
        for (name, prime) in file.archetypes {
            archetypes.entry(name).or_insert_with(|| Sourced {
                value: prime,
                layer,
                table: "[archetypes]".into(),
            });
        }
        if provider.is_none()
            && let Some(p) = file.defaults.provider
        {
            provider = Some(Sourced {
                value: p,
                layer,
                table: "[_defaults]".into(),
            });
        }
        // `parse_file` refuses it in the local layer.
        if layer == Layer::Global {
            claude_token_command = file.defaults.claude_token_command.take();
        }
        if stall_timeout_secs.is_none()
            && let Some(s) = file.defaults.stall_timeout_secs
        {
            stall_timeout_secs = Some(Sourced {
                value: s,
                layer,
                table: "[_defaults]".into(),
            });
        }

        // Host tables first: pushing in precedence order makes the first
        // definition of each profile the effective one.
        let mut hosts = file.hosts;
        if let Some(host_tables) = hosts.remove(hostname) {
            for (prov, profiles) in host_tables {
                for (name, profile) in profiles {
                    let table = format!("[{}.{prov}.{name}]", toml_key(hostname));
                    defs.entry(prov)
                        .or_default()
                        .entry(name)
                        .or_default()
                        .push(ProfileDef {
                            profile,
                            layer,
                            table,
                            host: Some(hostname.to_string()),
                        });
                }
            }
        }
        for (prov, profiles) in file.profiles {
            for (name, profile) in profiles {
                let table = format!("[{prov}.{name}]");
                defs.entry(prov)
                    .or_default()
                    .entry(name)
                    .or_default()
                    .push(ProfileDef {
                        profile,
                        layer,
                        table,
                        host: None,
                    });
            }
        }
    }

    let profiles = defs
        .into_iter()
        .map(|(prov, by_name)| {
            let entries = by_name
                .into_iter()
                .filter_map(|(name, list)| {
                    let mut defs = list.into_iter();
                    let effective = defs.next()?;
                    Some((
                        name,
                        ProfileEntry {
                            effective,
                            shadowed: defs.collect(),
                        },
                    ))
                })
                .collect();
            (prov, entries)
        })
        .collect();

    Ok(ReviewConfig {
        archetypes,
        audit,
        provider,
        stall_timeout_secs,
        claude_token_command,
        profiles,
        hostname: hostname.to_string(),
        files,
        project_root,
        projects,
    })
}

#[cfg(test)]
mod tests;
