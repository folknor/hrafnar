//! Combining the local and global layers into one `ReviewConfig`. Every
//! resolved value is `Sourced` (layer and table), and each profile records
//! its layer and table, so `review list --all` can show where it came from.
//! The tests are in `resolve/tests.rs`.
//!
//! The rules, and why:
//!
//! - Archetypes are a union, the project winning per name, because a project
//!   owns its domain archetypes.
//! - Each `[_defaults]` key comes whole from the first layer that sets it.
//! - Profiles are add-only: a project may add a profile but never redefine a
//!   global one. Otherwise `-p deep` would run something different in one
//!   project than everywhere else, and a field of a run could come from a
//!   table that never mentioned it.
//! - `[_audit]` is always the project's, because the audit id identifies the
//!   project. A `.review.toml` holding only `[_audit]` is a complete config.

use super::{ConfigFile, ConfigFiles, Layer, ProfileDef, ReviewConfig, Sourced};
use anyhow::{Result, bail};
use std::collections::BTreeMap;

/// The file a layer was read from, for errors that name both files.
fn layer_file(files: &ConfigFiles, layer: Layer) -> String {
    let path = match layer {
        Layer::Local => files.local.as_deref(),
        Layer::Global => files.global.as_deref(),
    };
    match path {
        Some(p) => p.display().to_string(),
        None => format!("the {} config", layer.as_str()),
    }
}

/// Resolve the local and (optional) global layers for `hostname`, which
/// selects this host's `[_projects.<host>]` registry.
///
/// Archetypes and each `[_defaults]` key are first-definition-wins over the
/// precedence order local, then global. Profiles are add-only: a name both
/// layers define is an error.
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
    let mut profile: Option<Sourced<String>> = None;
    let mut stall_timeout_secs: Option<Sourced<super::StallTimeout>> = None;
    let mut claude_token_command: Option<String> = None;
    let mut prefer: Option<Sourced<Vec<super::Harness>>> = None;
    let mut profiles: BTreeMap<String, ProfileDef> = BTreeMap::new();
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
        if profile.is_none()
            && let Some(p) = file.defaults.profile
        {
            profile = Some(Sourced {
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
        if prefer.is_none()
            && let Some(p) = file.defaults.prefer
        {
            prefer = Some(Sourced {
                value: p,
                layer,
                table: "[_defaults]".into(),
            });
        }
        // Profiles are add-only across layers. A project may define new ones,
        // but redefining a global profile is refused: a checked-out repo's
        // config must not be able to change what `-p <name>` means everywhere
        // else - swap its model, widen its sandbox, add env - for the same
        // reason `claude_token_command` is global-only.
        for (name, mut def) in file.profiles {
            // The layer is the slot the file was resolved in.
            def.layer = layer;
            if let Some(earlier) = profiles.get(&name) {
                bail!(
                    "profile '{name}' is defined in both {} and {}\n  \
                     A project may add profiles but not redefine a global one: pick \
                     another name for the project's profile.",
                    layer_file(&files, earlier.layer),
                    layer_file(&files, def.layer)
                );
            }
            profiles.insert(name, def);
        }
    }

    Ok(ReviewConfig {
        archetypes,
        audit,
        profile,
        stall_timeout_secs,
        claude_token_command,
        prefer,
        profiles,
        hostname: hostname.to_string(),
        files,
        project_root,
        projects,
    })
}

#[cfg(test)]
mod tests;
