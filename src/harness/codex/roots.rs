/// Render the resolved effective set, which may come from a raw profile override.
pub(super) fn config_override_paths(paths: &[String]) -> Option<String> {
    if paths.is_empty() {
        return None;
    }
    let list = paths
        .iter()
        .map(|g| format!("\"{}\"", escape_toml(g)))
        .collect::<Vec<_>>()
        .join(",");
    Some(format!("sandbox_workspace_write.writable_roots=[{list}]"))
}

fn escape_toml(value: &str) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => {
                write!(&mut out, "\\u{:04x}", u32::from(c)).expect("writing to String");
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_override_is_a_toml_array_of_the_granted_paths() {
        let paths = [
            "/home/dev/.brokkr",
            "/home/dev/.cargo",
            "/home/dev/.local/share/brokkr",
            "/media/disk/cargo",
        ]
        .map(str::to_string);
        assert_eq!(config_override_paths(&[]), None);
        assert_eq!(
            config_override_paths(&paths).as_deref(),
            Some(
                r#"sandbox_workspace_write.writable_roots=["/home/dev/.brokkr","/home/dev/.cargo","/home/dev/.local/share/brokkr","/media/disk/cargo"]"#
            )
        );
    }

    #[test]
    fn raw_config_roots_with_controls_roundtrip_through_the_generated_override() {
        // Raw Codex config is intentionally not subject to derivation filters.
        // Each -c is parsed, even though a later raw entry wins its value.
        let mut path = String::from("/cache/\\\"");
        path.extend((0u8..=31).map(char::from));
        path.extend((127u8..=159).map(char::from));
        path.push('雪');
        let raw = format!(
            "sandbox_workspace_write.writable_roots={}",
            toml::Value::Array(vec![toml::Value::String(path.clone())])
        );
        let paths = super::super::config_writable_roots_override(&[raw]).expect("raw override");
        assert_eq!(paths, [path.clone()]);
        let generated = config_override_paths(&paths).expect("generated override");
        let doc: toml::Table = generated.parse().expect("all controls escaped for TOML");
        assert_eq!(
            doc["sandbox_workspace_write"]["writable_roots"][0].as_str(),
            Some(path.as_str())
        );
    }

    /// A path is attacker-influenced only in the sense that it comes from the
    /// environment, but an unescaped quote would end the TOML string and let the
    /// rest of the value set an unrelated config key.
    #[test]
    fn a_quote_in_a_path_cannot_inject_another_config_key() {
        let hostile = "/run/\"],approval_policy=\"on-request";
        let paths = [format!("{hostile}/.brokkr"), format!("{hostile}/.cargo")];
        let Some(rendered) = config_override_paths(&paths) else {
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
