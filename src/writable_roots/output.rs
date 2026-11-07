use super::GrantedRoot;

/// The codex `-c` override expressing a set of grants, or `None` when empty.
///
/// TOML strings are emitted with escaping so a path containing a quote or a
/// backslash cannot break out of the array and inject an unrelated key.
pub fn config_override(grants: &[GrantedRoot]) -> Option<String> {
    if grants.is_empty() {
        return None;
    }
    let list = grants
        .iter()
        .map(|g| format!("\"{}\"", escape_toml(&g.path)))
        .collect::<Vec<_>>()
        .join(",");
    Some(format!("sandbox_workspace_write.writable_roots=[{list}]"))
}

fn escape_toml(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writable_roots::derive;
    use crate::writable_roots::test_host::{FakeHost, cwd};

    #[test]
    fn the_override_is_a_toml_array_of_the_granted_paths() {
        let host = FakeHost::new()
            .var("HOME", "/home/dev")
            .var("CARGO_TARGET_DIR", "/media/disk/cargo");
        let grants = derive(&cwd(), &host);
        assert_eq!(
            config_override(&grants).as_deref(),
            Some(
                r#"sandbox_workspace_write.writable_roots=["/home/dev/.brokkr","/home/dev/.cargo","/home/dev/.local/share/brokkr","/media/disk/cargo"]"#
            )
        );
    }

    /// A path is attacker-influenced only in the sense that it comes from the
    /// environment, but an unescaped quote would end the TOML string and let the
    /// rest of the value set an unrelated config key.
    #[test]
    fn a_quote_in_a_path_cannot_inject_another_config_key() {
        let hostile = "/run/\"],approval_policy=\"on-request";
        let host = FakeHost::new().var("HOME", hostile);
        let grants = derive(&cwd(), &host);
        let Some(rendered) = config_override(&grants) else {
            panic!("a granted root must render an override");
        };

        // Substring checks would be fooled here: the injected text is present
        // verbatim, correctly quarantined *inside* a TOML string. The property
        // that matters is what a parser sees, so parse it.
        let parsed: toml::Table = rendered
            .parse()
            .unwrap_or_else(|e| panic!("override must be valid TOML: {e}: {rendered}"));
        assert_eq!(
            parsed.len(),
            1,
            "injection created a second key: {parsed:?}"
        );
        let roots = parsed["sandbox_workspace_write"]["writable_roots"]
            .as_array()
            .unwrap_or_else(|| panic!("writable_roots must be an array: {parsed:?}"));
        // The whole hostile string survives inside each array element, escaped -
        // the grants are subdirectories of it, so the quote never escapes the
        // TOML string it is quarantined in.
        assert_eq!(
            roots[0].as_str(),
            Some(format!("{hostile}/.brokkr").as_str())
        );
        assert!(
            roots
                .iter()
                .all(|r| r.as_str().is_some_and(|s| s.starts_with(hostile))),
            "every element must be a subdirectory of the hostile HOME: {roots:?}"
        );
    }
}
