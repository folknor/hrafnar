use anyhow::{Context, Result};
use std::path::Path;
use toml_edit::{DocumentMut, Item, Table, value};

/// Load .review.toml as an editable document (or an empty doc if missing).
fn load_doc(config_path: &Path) -> Result<DocumentMut> {
    let raw = match std::fs::read_to_string(config_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", config_path.display()));
        }
    };
    raw.parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", config_path.display()))
}

fn save_doc(config_path: &Path, doc: &DocumentMut) -> Result<()> {
    std::fs::write(config_path, doc.to_string())
        .with_context(|| format!("failed to write {}", config_path.display()))
}

/// Return the configured audit id, creating and persisting one when absent.
/// The config file lock also serializes callers whose global locks differ.
pub fn ensure_audit_id(config_path: &Path) -> Result<String> {
    let doc = load_doc(config_path)?;
    if let Some(id) = configured_audit_id(&doc)? {
        return Ok(id);
    }

    let file_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(config_path)
        .with_context(|| {
            format!(
                "failed to lock {} for audit id update",
                config_path.display()
            )
        })?;
    crate::lock::acquire_blocking(&file_lock)?;

    let mut doc = load_doc(config_path)?;
    if let Some(id) = configured_audit_id(&doc)? {
        return Ok(id);
    }

    let id = crate::config::try_generate_short_id()?;
    set_audit_id(&mut doc, &id)?;
    save_doc(config_path, &doc)?;
    Ok(id)
}

fn configured_audit_id(doc: &DocumentMut) -> Result<Option<String>> {
    let Some(audit) = doc.get("_audit") else {
        return Ok(None);
    };
    let table = audit
        .as_table()
        .ok_or_else(|| anyhow::anyhow!("[_audit] is not a table in .review.toml"))?;
    let Some(id) = table.get("id") else {
        return Ok(None);
    };
    let id = id
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("[_audit].id is not a string in .review.toml"))?;
    if id.is_empty() {
        return Ok(None);
    }
    Ok(Some(id.to_string()))
}

fn set_audit_id(doc: &mut DocumentMut, id: &str) -> Result<()> {
    let audit_table = doc
        .entry("_audit")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[_audit] is not a table in .review.toml"))?;
    audit_table.insert("id", value(id));
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config;

    fn scratch_file(content: &str) -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-scratch");
        std::fs::create_dir_all(&dir).unwrap();
        let name = format!(
            "cfg-{}-{:?}.toml",
            std::process::id(),
            std::thread::current().id()
        );
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn ensure_audit_id_adds_section() {
        let path = scratch_file("[archetypes]\nbugs = \"x\"\n");
        let id = ensure_audit_id(&path).unwrap();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "{id}");
        let raw = std::fs::read_to_string(&path).unwrap();
        let cfg = config::parse_file(&raw, "test", config::Layer::Local).unwrap();
        assert_eq!(cfg.audit.id.as_deref(), Some(id.as_str()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ensure_audit_id_keeps_an_existing_id() {
        let path = scratch_file("[_audit]\nid = \"beef\"\n");
        assert_eq!(ensure_audit_id(&path).unwrap(), "beef");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw, "[_audit]\nid = \"beef\"\n");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ensure_audit_id_replaces_an_empty_id() {
        let path = scratch_file("[_audit]\nid = \"\"\n");
        let id = ensure_audit_id(&path).unwrap();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "{id}");
        let raw = std::fs::read_to_string(&path).unwrap();
        let cfg = config::parse_file(&raw, "test", config::Layer::Local).unwrap();
        assert_eq!(cfg.audit.id.as_deref(), Some(id.as_str()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ensure_audit_id_preserves_existing_content() {
        let initial = "\
# top comment
[archetypes]
bugs = \"find edge cases\"
";
        let path = scratch_file(initial);
        let id = ensure_audit_id(&path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# top comment"));
        assert!(raw.contains("bugs = \"find edge cases\""));
        assert!(raw.contains(&id));
        let _ = std::fs::remove_file(&path);
    }
}
