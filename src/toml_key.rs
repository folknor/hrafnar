/// Format a hostname as a TOML key, quoting only if it contains dots.
pub fn toml_key(key: &str) -> String {
    if key.contains('.') {
        format!("\"{key}\"")
    } else {
        key.to_string()
    }
}
