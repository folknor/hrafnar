//! Finding and reading the config files. The project's `.hrafnar.toml` is found
//! by walking up from the selected directory (or taken exactly from a `-C`
//! root) and is required for a run, because it carries the audit id; the
//! global file is optional. `load_optional` serves `hrafnar list`, which
//! outside a project shows the global layer alone; `project_root` locates a
//! project without parsing it, for `hrafnar sessions`. Loading is strict: a
//! file that exists but does not parse is an error, never treated as absent,
//! because `hrafnar message` reads `[_audit].private` from it, and a silent
//! fallback would file a private project's rows in the public log.

use super::{
    CONFIG_FILENAME, ConfigFile, ConfigFiles, HrafnarConfig, LEGACY_CONFIG_FILENAME, Layer,
    Selection, hostname, parse_file, resolve,
};
use super::{ConfigError, Result, SearchBoundary};
use crate::project::ProjectRoot;
use crate::project::registry::check_projects;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

/// Where the operator's global config lives: `$XDG_CONFIG_HOME/hrafnar/config.toml`,
/// else `$HOME/.config/hrafnar/config.toml`.
pub fn global_config_path() -> Option<PathBuf> {
    global_config_path_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

/// A relative `XDG_CONFIG_HOME` or `HOME` is ignored, as the XDG spec requires
/// for the former - either would otherwise resolve against whatever directory
/// `hrafnar` was launched from, making the global config a per-directory one.
fn global_config_path_from(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = match xdg.map(PathBuf::from).filter(|p| p.is_absolute()) {
        Some(p) => p,
        None => home
            .map(PathBuf::from)
            .filter(|h| h.is_absolute())?
            .join(".config"),
    };
    Some(base.join("hrafnar").join("config.toml"))
}

/// Load the project's `.hrafnar.toml` (required: it carries the audit id) and
/// the global config (optional), and resolve them for this host.
pub fn load(sel: &Selection) -> Result<HrafnarConfig> {
    let local_path = match locate_config(sel)? {
        Located::Found(path) => path,
        Located::Missing(boundary) => return Err(ConfigError::NotFound { boundary }),
    };
    load_layers(Some(local_path))
}

/// Like `load`, but a missing `.hrafnar.toml` is not an error: the config is the
/// global layer alone and the project root is `None`. A file that exists but
/// does not parse still is - callers use this to decide things like whether a
/// run's logs are private, and silently treating a broken config as an absent
/// one would downgrade a private project to the public log.
pub fn load_optional(sel: &Selection) -> Result<HrafnarConfig> {
    match locate_config(sel)? {
        Located::Found(path) => load_layers(Some(path)),
        // An exact root was chosen as the project; showing the global layer
        // alone would describe a project that is not there.
        Located::Missing(boundary) if matches!(sel, Selection::Exact(_)) => {
            Err(ConfigError::NotFound { boundary })
        }
        Located::Missing(_) => load_layers(None),
    }
}

/// The project root - the directory holding `.hrafnar.toml` - without parsing
/// anything, for callers that only need to know which project they are in.
pub fn project_root(sel: &Selection) -> Result<Option<ProjectRoot>> {
    match locate_config(sel)? {
        Located::Found(path) => Ok(Some(ProjectRoot::from_config_path(&path)?)),
        Located::Missing(_) => Ok(None),
    }
}

fn load_layers(local_path: Option<PathBuf>) -> Result<HrafnarConfig> {
    let local = match local_path {
        Some(ref p) => {
            let raw = std::fs::read_to_string(p).map_err(|source| ConfigError::Read {
                path: p.clone(),
                source,
            })?;
            parse_file(&raw, &p.display().to_string(), Layer::Local)?
        }
        None => ConfigFile::default(),
    };

    let global_path = global_config_path();
    let global = match global_path {
        Some(ref p) => read_optional_file(p)?
            .map(|raw| parse_file(&raw, &p.display().to_string(), Layer::Global))
            .transpose()?,
        None => None,
    };
    let global_loaded = global.is_some();

    let files = ConfigFiles {
        local: local_path,
        global: global_path,
        global_loaded,
    };
    resolve(local, global, &hostname(), files)
}

/// Presence of a reachable directory entry, including dangling symlinks.
/// Metadata errors are not absence: falling back could change audit/privacy.
/// This is admission, not an atomic check-and-open or an init creation lock.
pub(crate) fn entry_exists(path: &std::path::Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(ConfigError::Inspect {
            path: path.to_path_buf(),
            source,
        }),
    }
}

// Both full loading and pre-selection registry lookup use the same optional
// global-file rule. An existing unreadable/dangling entry is never absence.
fn read_optional_file(path: &std::path::Path) -> Result<Option<String>> {
    if !entry_exists(path)? {
        return Ok(None);
    }
    std::fs::read_to_string(path)
        .map(Some)
        .map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })
}

pub(crate) enum Located {
    Found(PathBuf),
    /// Genuine absence, distinct from failed inspection or an invalid file.
    Missing(SearchBoundary),
}

/// The project file directly in `dir`: `.hrafnar.toml`, else the legacy
/// `.review.toml`. Both present is an error (see `LEGACY_CONFIG_FILENAME`).
fn config_in(dir: &std::path::Path) -> Result<Option<PathBuf>> {
    let current = dir.join(CONFIG_FILENAME);
    let legacy = dir.join(LEGACY_CONFIG_FILENAME);
    match (entry_exists(&current)?, entry_exists(&legacy)?) {
        (true, true) => Err(ConfigError::BothConfigs {
            dir: dir.to_path_buf(),
        }),
        (true, false) => Ok(Some(current)),
        (false, true) => Ok(Some(legacy)),
        (false, false) => Ok(None),
    }
}

pub(crate) fn locate_config(sel: &Selection) -> Result<Located> {
    let mut dir = match sel {
        Selection::Exact(root) => {
            return Ok(match config_in(root)? {
                Some(path) => Located::Found(path),
                None => Located::Missing(SearchBoundary::Exact(root.clone())),
            });
        }
        Selection::Discover(start) => start.clone(),
    };

    loop {
        if let Some(path) = config_in(&dir)? {
            return Ok(Located::Found(path));
        }
        if entry_exists(&dir.join(".git"))? {
            return Ok(Located::Missing(SearchBoundary::GitRoot(dir)));
        }
        let searched_to = dir.clone();
        if !dir.pop() {
            return Ok(Located::Missing(SearchBoundary::FilesystemRoot(
                searched_to,
            )));
        }
    }
}

/// This host's `[_projects.<host>]` registry from the global file alone,
/// without a project config: `-C <name>` is resolved before anything knows
/// which project to read. A global file that does not parse is an error here
/// too, rather than "no such project".
pub fn global_projects() -> Result<(BTreeMap<String, String>, Option<PathBuf>)> {
    let Some(path) = global_config_path() else {
        return Ok((BTreeMap::new(), None));
    };
    let Some(raw) = read_optional_file(&path)? else {
        return Ok((BTreeMap::new(), Some(path)));
    };
    let mut file = parse_file(&raw, &path.display().to_string(), Layer::Global)?;
    let host = hostname();
    let mine = file.projects.remove(&host).unwrap_or_default();
    // This lookup must work before a registered project's config can be loaded.
    // Resolving the whole global file here would validate unrelated settings,
    // then the selected project's full load would parse it again anyway.
    check_projects(&mine, &host, "global").map_err(|source| ConfigError::Resolve { source })?;
    Ok((mine, Some(path)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn discovery_errors_and_dangling_entries_never_fall_back() {
        use std::os::unix::fs::symlink;
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(format!("config-discovery-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let child = root.join("child");
        std::fs::create_dir_all(&child).expect("fixture");
        std::fs::write(root.join(CONFIG_FILENAME), "[_audit]\nid = 'ancestor'\n")
            .expect("ancestor");
        symlink("missing", child.join(CONFIG_FILENAME)).expect("dangling config");
        assert!(read_optional_file(&child.join(CONFIG_FILENAME)).is_err());
        assert!(
            read_optional_file(&child.join("absent-global"))
                .expect("optional absent")
                .is_none()
        );
        for sel in [
            Selection::Exact(child.clone()),
            Selection::Discover(child.clone()),
        ] {
            assert!(
                load_optional(&sel).is_err(),
                "dangling config must be selected, then fail reading"
            );
            assert!(crate::config_write::init(&sel).is_err());
        }
        std::fs::remove_file(child.join(CONFIG_FILENAME)).expect("remove dangling config");
        symlink("loop", root.join("loop")).expect("parent loop");
        let sel = Selection::Discover(root.join("loop/child"));
        assert!(locate_config(&sel).is_err());
        assert!(load_optional(&sel).is_err());
        assert!(!entry_exists(&root.join("absent")).expect("missing is absence"));
        symlink("missing", child.join(".git")).expect("dangling git boundary");
        assert!(matches!(
            locate_config(&Selection::Discover(child)).expect("boundary"),
            Located::Missing(_)
        ));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn a_legacy_review_toml_is_the_project_file_unless_both_exist() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(format!("config-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let child = root.join("child");
        std::fs::create_dir_all(&child).expect("fixture");
        let legacy = root.join(LEGACY_CONFIG_FILENAME);
        std::fs::write(&legacy, "[_audit]\nid = 'legacy'\n").expect("legacy config");

        for sel in [
            Selection::Exact(root.clone()),
            Selection::Discover(child.clone()),
        ] {
            assert!(matches!(locate_config(&sel).unwrap(), Located::Found(p) if p == legacy));
        }
        let cfg = load(&Selection::Exact(root.clone())).unwrap();
        assert_eq!(cfg.files().local.as_deref(), Some(legacy.as_path()));
        assert_eq!(
            cfg.audit.id.as_ref().map(crate::data::AuditId::as_str),
            Some("legacy")
        );
        // init must not add a .hrafnar.toml beside or below a legacy file.
        assert!(crate::config_write::init(&Selection::Exact(root.clone())).is_err());
        assert!(crate::config_write::init(&Selection::Exact(child.clone())).is_err());

        std::fs::write(root.join(CONFIG_FILENAME), "[_audit]\nid = 'new'\n").expect("config");
        let error = match locate_config(&Selection::Discover(child)) {
            Err(error) => error,
            Ok(_) => panic!("both files must be refused"),
        };
        assert!(matches!(&error, ConfigError::BothConfigs { dir } if *dir == root));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn typed_errors_distinguish_absence_storage_and_invalid_configuration() {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(format!("config-errors-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("fixture");
        std::fs::write(base.join(".git"), "").expect("boundary");
        let exact = Selection::Exact(base.clone());
        assert!(
            matches!(load(&exact), Err(ConfigError::NotFound { boundary: SearchBoundary::Exact(root) }) if root == base)
        );
        assert!(matches!(
            load_optional(&exact),
            Err(ConfigError::NotFound { .. })
        ));
        assert!(
            matches!(locate_config(&Selection::Discover(base.clone())).expect("locate"), Located::Missing(SearchBoundary::GitRoot(root)) if root == base)
        );
        assert!(
            read_optional_file(&base.join(CONFIG_FILENAME))
                .expect("missing")
                .is_none()
        );
        std::fs::create_dir(base.join(CONFIG_FILENAME)).expect("unreadable directory");
        assert!(matches!(
            load_optional(&exact),
            Err(ConfigError::Read { .. })
        ));
        std::fs::remove_dir(base.join(CONFIG_FILENAME)).expect("remove");
        std::fs::write(base.join(CONFIG_FILENAME), "[broken").expect("malformed");
        let error = load_optional(&exact).expect_err("invalid");
        assert!(
            matches!(&error, ConfigError::InvalidFile { origin, .. } if origin.ends_with(CONFIG_FILENAME))
        );
        let wrapped = anyhow::Error::new(error).context("message target");
        assert!(matches!(
            wrapped.downcast_ref::<ConfigError>(),
            Some(ConfigError::InvalidFile { .. })
        ));
        use std::os::unix::fs::symlink;
        symlink("loop", base.join("loop")).expect("loop");
        assert!(matches!(
            entry_exists(&base.join("loop/child")),
            Err(ConfigError::Inspect { .. })
        ));
        std::fs::remove_dir_all(base).expect("cleanup");
    }

    #[test]
    fn global_config_path_prefers_absolute_xdg() {
        let p = global_config_path_from(Some("/x/cfg".into()), Some("/home/u".into()));
        assert_eq!(p, Some(PathBuf::from("/x/cfg/hrafnar/config.toml")));
        // Relative XDG_CONFIG_HOME is ignored per the XDG spec.
        let p = global_config_path_from(Some("rel".into()), Some("/home/u".into()));
        assert_eq!(
            p,
            Some(PathBuf::from("/home/u/.config/hrafnar/config.toml"))
        );
        assert_eq!(global_config_path_from(None, None), None);
        assert_eq!(global_config_path_from(None, Some("".into())), None);
        // A relative HOME would make the global config depend on the launch
        // directory, the same reason a relative XDG_CONFIG_HOME is ignored.
        assert_eq!(global_config_path_from(None, Some("rel".into())), None);
    }
}
