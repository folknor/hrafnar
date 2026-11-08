use anyhow::{Result, bail};
use std::collections::BTreeMap;

/// Validate this host's registry. Only this host's: another machine's table is
/// that machine's business, and a clash in it must not stop this one loading.
pub(crate) fn check_projects(
    names: &BTreeMap<String, String>,
    host: &str,
    layer: &str,
) -> Result<()> {
    let mut folded = std::collections::HashMap::new();
    for name in names.keys() {
        if let Err(why) = check_name(name) {
            bail!(
                "[_projects.{}] ({layer} config): {why}",
                crate::toml_key::toml_key(host)
            );
        }
        if let Some(prev) = folded.insert(name.to_ascii_lowercase(), name) {
            bail!(
                "[_projects.{}] ({layer} config): '{prev}' and '{name}' differ only in case, \
                 and names match ignoring case",
                crate::toml_key::toml_key(host)
            );
        }
    }
    Ok(())
}

/// A registry key must be a name: anything spelled as a path could never be
/// selected by it, so it is refused where it is written.
pub fn check_name(name: &str) -> std::result::Result<(), String> {
    if name.trim().is_empty() {
        return Err("a project name cannot be empty".to_string());
    }
    if super::is_path(name) {
        return Err(format!(
            "'{name}' is spelled like a path, so `-C {name}` would never look it up"
        ));
    }
    if name.ends_with('/') || name.chars().any(char::is_whitespace) {
        return Err(format!(
            "'{name}' has a trailing slash or whitespace, which no selector can match"
        ));
    }
    Ok(())
}
