use super::resolve::check_projects;
use super::{
    CONFIG_FILENAME, ConfigFile, ConfigFiles, Layer, ReviewConfig, hostname, parse_file, resolve,
    try_generate_short_id,
};
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

/// Where the operator's global config lives: `$XDG_CONFIG_HOME/review/config.toml`,
/// else `$HOME/.config/review/config.toml`.
pub fn global_config_path() -> Option<PathBuf> {
    global_config_path_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

/// A relative `XDG_CONFIG_HOME` or `HOME` is ignored, as the XDG spec requires
/// for the former - either would otherwise resolve against whatever directory
/// `review` was launched from, making the global config a per-directory one.
fn global_config_path_from(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = match xdg.map(PathBuf::from).filter(|p| p.is_absolute()) {
        Some(p) => p,
        None => home
            .map(PathBuf::from)
            .filter(|h| h.is_absolute())?
            .join(".config"),
    };
    Some(base.join("review").join("config.toml"))
}

/// Where to look for the project's `.review.toml`.
///
/// The two cases are kept apart because they permit different things, and a
/// bare directory would lose which one applies. `Discover` is the ambient
/// rule - walk up to the git root - for the invoking directory and for an
/// explicit `-C ./path`. `Exact` is a root that is already known to be the
/// project, a registered name or a session's recorded root: its config must sit
/// directly in it. Walking up from one would let a registered or recorded root
/// whose config had gone adopt an ancestor's - another project's audit id and
/// privacy setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    Discover(PathBuf),
    Exact(PathBuf),
}

impl Selection {
    /// The invoking directory, walked up as `review` always has.
    pub fn cwd() -> Result<Self> {
        let dir = std::env::current_dir()
            .map_err(|e| anyhow::anyhow!("failed to get current directory: {e}"))?;
        Ok(Selection::Discover(dir))
    }

    /// The directory the selection starts from.
    pub fn dir(&self) -> &std::path::Path {
        match self {
            Selection::Discover(d) | Selection::Exact(d) => d,
        }
    }
}

/// Load the project's `.review.toml` (required: it carries the audit id) and
/// the global config (optional), and resolve them for this host.
pub fn load(sel: &Selection) -> Result<(ReviewConfig, PathBuf)> {
    let local_path = match locate_config(sel)? {
        Located::Found(path) => path,
        Located::Missing(why) => bail!("{why}"),
    };
    let project_root = project_root_of(&local_path)?;
    Ok((load_layers(Some(local_path))?, project_root))
}

/// Like `load`, but a missing `.review.toml` is not an error: the config is the
/// global layer alone and the project root is `None`. A file that exists but
/// does not parse still is - callers use this to decide things like whether a
/// run's logs are private, and silently treating a broken config as an absent
/// one would downgrade a private project to the public log.
pub fn load_optional(sel: &Selection) -> Result<(ReviewConfig, Option<PathBuf>)> {
    match locate_config(sel)? {
        Located::Found(path) => {
            let root = project_root_of(&path)?;
            Ok((load_layers(Some(path))?, Some(root)))
        }
        // An exact root was chosen as the project; showing the global layer
        // alone would describe a project that is not there.
        Located::Missing(why) if matches!(sel, Selection::Exact(_)) => bail!("{why}"),
        Located::Missing(_) => Ok((load_layers(None)?, None)),
    }
}

/// The project root - the directory holding `.review.toml` - without parsing
/// anything, for callers that only need to know which project they are in.
pub fn project_root(sel: &Selection) -> Result<Option<PathBuf>> {
    match locate_config(sel)? {
        Located::Found(path) => Ok(Some(project_root_of(&path)?)),
        Located::Missing(_) => Ok(None),
    }
}

/// The root is canonicalized, so one checkout has one identity: it is what the
/// sidecar and in-flight markers record and what `-C` and `review sessions`
/// compare against, and a symlinked spelling must not split a project in two.
fn project_root_of(local_path: &std::path::Path) -> Result<PathBuf> {
    let parent = local_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", local_path.display()))?;
    Ok(std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf()))
}

fn load_layers(local_path: Option<PathBuf>) -> Result<ReviewConfig> {
    let local = match local_path {
        Some(ref p) => {
            let raw = std::fs::read_to_string(p)
                .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", p.display()))?;
            parse_file(&raw, &p.display().to_string(), Layer::Local)?
        }
        None => ConfigFile::default(),
    };

    let global_path = global_config_path();
    let global = match global_path {
        Some(ref p) => match std::fs::read_to_string(p) {
            Ok(raw) => Some(parse_file(&raw, &p.display().to_string(), Layer::Global)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => bail!("failed to read {}: {e}", p.display()),
        },
        None => None,
    };
    let global_loaded = global.is_some();

    let mut cfg = resolve(local, global, &hostname())?;
    cfg.files = ConfigFiles {
        local: local_path,
        global: global_path,
        global_loaded,
    };
    Ok(cfg)
}

enum Located {
    Found(PathBuf),
    /// Not found; carries the message saying where the search stopped.
    Missing(String),
}

fn locate_config(sel: &Selection) -> Result<Located> {
    let mut dir = match sel {
        Selection::Exact(root) => {
            let candidate = root.join(CONFIG_FILENAME);
            return Ok(if candidate.exists() {
                Located::Found(candidate)
            } else {
                Located::Missing(format!(
                    "no {CONFIG_FILENAME} in {}\n\n\
                     Run `review -C <project> init` to create one.",
                    root.display()
                ))
            });
        }
        Selection::Discover(start) => start.clone(),
    };

    loop {
        let candidate = dir.join(CONFIG_FILENAME);
        if candidate.exists() {
            return Ok(Located::Found(candidate));
        }
        if dir.join(".git").exists() {
            return Ok(Located::Missing(format!(
                "no {CONFIG_FILENAME} found (searched up to git root: {})\n\n\
                 Run `review init` to create one.",
                dir.display()
            )));
        }
        if !dir.pop() {
            return Ok(Located::Missing(format!(
                "no {CONFIG_FILENAME} found in current or parent directories\n\n\
                 Run `review init` to create one."
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
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((BTreeMap::new(), Some(path)));
        }
        Err(e) => bail!("failed to read {}: {e}", path.display()),
    };
    let mut file = parse_file(&raw, &path.display().to_string(), Layer::Global)?;
    let host = hostname();
    let mine = file.projects.remove(&host).unwrap_or_default();
    check_projects(&mine, &host, "global")?;
    Ok((mine, Some(path)))
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
# Providers to fan out to when --provider is omitted:
# [_defaults]
# providers = [\"codex\"]
#
# Groups fan out to multiple archetypes:
# [_groups]
# sweep = [\"security\", \"bugs\"]
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

    if path.exists() {
        bail!("{CONFIG_FILENAME} already exists in {}", dir.display());
    }

    if let Located::Found(existing) = locate_config(&Selection::Discover(dir.to_path_buf()))? {
        bail!(
            "{CONFIG_FILENAME} already exists at {}\n  \
             Creating another here would shadow it.",
            existing.display()
        );
    }

    let audit_id = try_generate_short_id()?;
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

    #[test]
    fn global_config_path_prefers_absolute_xdg() {
        let p = global_config_path_from(Some("/x/cfg".into()), Some("/home/u".into()));
        assert_eq!(p, Some(PathBuf::from("/x/cfg/review/config.toml")));
        // Relative XDG_CONFIG_HOME is ignored per the XDG spec.
        let p = global_config_path_from(Some("rel".into()), Some("/home/u".into()));
        assert_eq!(p, Some(PathBuf::from("/home/u/.config/review/config.toml")));
        assert_eq!(global_config_path_from(None, None), None);
        assert_eq!(global_config_path_from(None, Some("".into())), None);
        // A relative HOME would make the global config depend on the launch
        // directory, the same reason a relative XDG_CONFIG_HOME is ignored.
        assert_eq!(global_config_path_from(None, Some("rel".into())), None);
    }
}
