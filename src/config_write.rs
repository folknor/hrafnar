//! The only code that writes config. `init` creates a new `.review.toml`;
//! `ensure_audit_id` is the only writer of an existing one, generating and
//! persisting the audit id under a lock on the file when it is absent, so two
//! first runs of an id-less project agree on one id. Archetypes and profiles
//! are hand-edited and never written here.

use crate::config::load::{Located, entry_exists, locate_config};
use crate::config::{CONFIG_FILENAME, Selection};
use anyhow::{Context, Result, bail};
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
pub fn ensure_audit_id(config_path: &Path) -> Result<crate::data::AuditId> {
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

    let id = crate::data::AuditId::generate()?;
    set_audit_id(&mut doc, &id)?;
    save_doc(config_path, &doc)?;
    Ok(id)
}

fn configured_audit_id(doc: &DocumentMut) -> Result<Option<crate::data::AuditId>> {
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
    Ok(crate::data::AuditId::from_record(id))
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

const INIT_TEMPLATE_PREFIX: &str = "\
# Project config for `review`. Everything here is optional except [_audit].
# Settings resolve command line, then this file, then the operator's global
# config (~/.config/review/config.toml, same format). Run `review config` to see
# the effective result and where each value came from.
#
# Archetypes are priming prompts: a name mapped to text prepended to stdin.
# A run without an archetype sends stdin unchanged.
#
# [archetypes]
# security = \"You are a security expert for this project. Read the codebase.\"
#
# The provider a run uses when --provider is omitted:
# [_defaults]
# provider = \"codex\"
#
# Named profiles, selected with --profile, as [<provider>.<profile>]. A profile
# here replaces a global profile of the same name entirely.
# [codex.deep]
# model = \"gpt-6-luna\"
# effort = \"high\"
# sandbox = \"read-only\"
";

/// Create a starter config in the selected directory itself - never in an
/// ancestor, whichever kind of selection it is.
pub fn init(sel: &Selection) -> Result<()> {
    let dir = sel.dir();
    let path = dir.join(CONFIG_FILENAME);

    if entry_exists(&path)? {
        bail!("{CONFIG_FILENAME} already exists in {}", dir.display());
    }

    if let Located::Found(existing) = locate_config(&Selection::Discover(dir.to_path_buf()))? {
        bail!(
            "{CONFIG_FILENAME} already exists at {}\n  \
             Creating another here would shadow it.",
            existing.display()
        );
    }

    let audit_id = crate::data::AuditId::generate()?;
    let mut content = INIT_TEMPLATE_PREFIX.to_string();
    content.push_str(&format!("\n[_audit]\nid = \"{audit_id}\"\n"));
    std::fs::write(&path, content)
        .map_err(|e| anyhow::anyhow!("failed to write {CONFIG_FILENAME}: {e}"))?;

    println!("Created {}", path.display());
    println!();
    println!("Next steps:");
    println!("  1. Run `review config` to see what the global config already provides");
    println!("  2. Run: echo \"check for issues\" | review --provider codex");
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
        assert_eq!(ensure_audit_id(&path).unwrap().as_str(), "beef");
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
        assert!(raw.contains(id.as_str()));
        let _ = std::fs::remove_file(&path);
    }
    #[test]
    fn audit_id_read_failures_preserve_existing_entries() {
        // Invalid UTF-8 fails read_to_string even under root; chmod-based
        // unreadability tests can silently succeed for privileged runners.
        let path = scratch_file("placeholder");
        let bytes = b"[archetypes]\n# \xff existing bytes\n";
        std::fs::write(&path, bytes).expect("write unreadable text");
        let error = ensure_audit_id(&path).expect_err("read must fail");
        assert!(error.to_string().contains("failed to read"), "{error:#}");
        assert!(error.downcast_ref::<std::io::Error>().is_some());
        assert_eq!(std::fs::read(&path).expect("bytes remain"), bytes);
        std::fs::remove_file(&path).expect("remove fixture file");
        std::fs::create_dir(&path).expect("directory at config path");
        let sentinel = path.join("keep");
        std::fs::write(&sentinel, "untouched").expect("sentinel");
        assert!(
            ensure_audit_id(&path)
                .expect_err("directory is not config")
                .to_string()
                .contains("failed to read")
        );
        assert_eq!(
            std::fs::read_to_string(&sentinel).expect("preserved"),
            "untouched"
        );
        std::fs::remove_dir_all(path).expect("cleanup");
    }
}
