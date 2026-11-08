use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::Grok;

impl Grok {
    /// Grok's `is_unsafe_trust_root`: relative, the filesystem root, or the
    /// user's home (compared canonically, as grok's `is_home_dir` does).
    fn unsafe_root(&self, key: &Path) -> bool {
        !key.is_absolute()
            || key.parent().is_none()
            || self.user_home.as_deref().is_some_and(|h| h == key)
    }

    /// Grok's trust key for a directory, modelled on `trust.rs` `workspace_key`:
    /// the canonical git root containing it; for a linked worktree, the main
    /// checkout it belongs to; and the canonical directory itself when there is
    /// no repo or the repo root is an unsafe root (a dotfiles repo at `$HOME`).
    ///
    /// Used to read the store and to name a grant; grok performs the actual
    /// write itself. Where it cannot model grok - a grok-managed
    /// `-w` worktree, resolved through grok's sqlite registry - `check_trust`
    /// refuses rather than guess, because a wrong key here could let `--trust`
    /// overwrite a declined folder.
    pub(in crate::provider::grok) fn workspace_key(&self, dir: &Path) -> PathBuf {
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let Some(root) = dir.ancestors().find(|a| a.join(".git").exists()) else {
            return dir;
        };
        let key = main_checkout_of_worktree(root).unwrap_or_else(|| root.to_path_buf());
        let key = std::fs::canonicalize(&key).unwrap_or(key);
        if self.unsafe_root(&key) { dir } else { key }
    }
}

/// For a linked worktree (`.git` points into `.git/worktrees/<name>`), the main
/// checkout's directory. Other `.git` files, including submodule pointers into
/// `.git/modules`, stay keyed on that repository's own root.
fn main_checkout_of_worktree(root: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(root.join(".git")).ok()?;
    let gitdir = PathBuf::from(text.strip_prefix("gitdir:")?.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        root.join(gitdir)
    };
    let worktrees = gitdir.parent()?;
    (worktrees.file_name()? == "worktrees")
        .then(|| worktrees.parent()?.parent().map(Path::to_path_buf))
        .flatten()
}

/// What a `trusted_folders.toml` body says about one trust key.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::provider::grok) enum Trust {
    Trusted,
    /// No decision recorded: grok would prompt, and `dontAsk` cancels.
    Undecided,
    /// An explicit `trusted = false` - the operator said no.
    Declined,
}

/// Mirror of grok's `FolderTrust` (`trust.rs`).
#[derive(serde::Deserialize)]
#[allow(dead_code)] // fields exist to be type-checked, as grok checks them
struct FolderTrust {
    trusted: bool,
    #[serde(default)]
    decided_at: Option<i64>,
}

/// Mirror of grok's `TrustDocument` (`trust.rs`).
#[derive(serde::Deserialize)]
struct TrustDocument {
    #[serde(default)]
    folders: BTreeMap<String, FolderTrust>,
}

/// Parse a trust store the way grok does, whole. `Err` is a store grok would
/// treat as unreadable - and grok then trusts nothing at all.
fn parse_trust(text: &str) -> anyhow::Result<TrustDocument> {
    if text.trim().is_empty() {
        return Ok(TrustDocument {
            folders: BTreeMap::new(),
        });
    }
    toml::from_str(text).map_err(|e| anyhow::anyhow!("grok cannot parse trusted_folders.toml: {e}"))
}

/// One fresh observation of whether Grok can load its whole trust store.
/// This rejects observed symlinks, including ancestors. Inspection and read
/// are separate operations, not an atomic confinement guarantee.
enum StoreState {
    Missing,
    Unloadable(anyhow::Error),
    Loaded(TrustDocument),
}

impl Grok {
    fn load_store(&self) -> StoreState {
        let path = self.home.join("trusted_folders.toml");
        for part in path.ancestors() {
            match std::fs::symlink_metadata(part) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return StoreState::Unloadable(anyhow::anyhow!(
                        "{} is reached through a symlink - grok refuses to load it",
                        path.display()
                    ));
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return StoreState::Unloadable(anyhow::anyhow!(
                        "cannot inspect {}: {e}",
                        part.display()
                    ));
                }
            }
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return StoreState::Missing,
            Err(e) => {
                return StoreState::Unloadable(anyhow::anyhow!(
                    "cannot read {}: {e}",
                    path.display()
                ));
            }
        };
        match parse_trust(&text) {
            Ok(doc) => StoreState::Loaded(doc),
            Err(e) => StoreState::Unloadable(anyhow::anyhow!(
                "{e} ({}) - grok would trust nothing; fix the file",
                path.display()
            )),
        }
    }
}

impl Grok {
    /// The decision grok would reach for a run in `dir`. Grok's gate queries
    /// the store with the *workspace key*, not the directory
    /// (`folder_trust::decide_inputs` → `is_trusted_this_process(key)`), and
    /// `trust.rs` `is_trusted` only counts an entry the key starts with *and*
    /// that shares the key's workspace id. An ancestor of the key belongs to a
    /// different workspace, and an entry below it does not prefix it, so the
    /// only entry that can decide is the one for the key itself. (A
    /// `trusted = false` on a subdirectory therefore does not block a run keyed
    /// on the repo root - grok never consults it.)
    fn decision(&self, doc: &TrustDocument, key: &Path) -> Trust {
        if self.unsafe_root(key) {
            return Trust::Undecided;
        }
        match key.to_str().and_then(|k| doc.folders.get(k)) {
            Some(f) if f.trusted => Trust::Trusted,
            Some(_) => Trust::Declined,
            None => Trust::Undecided,
        }
    }

    /// Observe a modelled trust key and its decision, including Declined.
    /// `Err` where the key cannot be modelled or the store cannot load:
    /// - an unsafe key (filesystem root, user home, relative path), which grok
    ///   never trusts even with `--trust`;
    /// - a store grok cannot parse (it would trust nothing and cancel at the
    ///   first edit), or a symlinked one (grok refuses to load it);
    /// - a grok-managed `-w` worktree, whose key grok resolves through its own
    ///   registry, which `review` cannot read - guessing wrong could let the
    ///   grant land on a declined source repo.
    ///
    /// A missing store is `Undecided`: `--trust` creates it.
    pub(in crate::provider::grok) fn observe_trust(
        &self,
        dir: &Path,
    ) -> anyhow::Result<(PathBuf, Trust)> {
        let key = self.workspace_key(dir);
        // Grok refuses these keys even when --trust is explicit. An absent
        // store must not turn a permanent refusal into a grant announcement.
        if self.unsafe_root(&key) {
            anyhow::bail!(
                "grok never trusts {} (filesystem root, user home, or relative trust key)",
                key.display()
            );
        }
        let canonical = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        // `dir` is canonical, but GROK_HOME may contain `..`. Compare against
        // the same canonical spelling so a managed worktree cannot evade this
        // refusal. Keep `self.home` raw below: the symlink check must still see
        // aliases grok itself refuses to load.
        let canonical_home = match std::fs::canonicalize(&self.home) {
            Ok(home) => Some(home),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => anyhow::bail!("cannot resolve grok home {}: {e}", self.home.display()),
        };
        if canonical_home
            .as_deref()
            .is_some_and(|home| canonical.starts_with(home.join("worktrees")))
        {
            anyhow::bail!(
                "{} is a grok-managed worktree; review cannot model its trust key - \
                 trust it from grok",
                dir.display()
            );
        }
        let decision = match self.load_store() {
            StoreState::Missing => Trust::Undecided,
            StoreState::Unloadable(e) => return Err(e),
            StoreState::Loaded(doc) => self.decision(&doc, &key),
        };
        Ok((key, decision))
    }

    /// Launch policy refuses an explicit no. Observation itself retains that
    /// decision so postchecks and hints can name it without parsing errors.
    pub(in crate::provider::grok) fn check_trust(
        &self,
        dir: &Path,
    ) -> anyhow::Result<(PathBuf, Trust)> {
        let (key, decision) = self.observe_trust(dir)?;
        let path = self.home.join("trusted_folders.toml");
        if decision == Trust::Declined {
            anyhow::bail!(
                "{} is explicitly untrusted in {} - review will not override that; \
                 change it there if you mean to",
                key.display(),
                path.display()
            );
        }
        Ok((key, decision))
    }

    /// Observation errors explain loadability, never pretend to prove an
    /// untrusted decision. Missing stores are genuinely undecided.
    pub(in crate::provider::grok) fn untrusted_hint(&self, dir: &Path) -> Option<String> {
        let path = self.home.join("trusted_folders.toml");
        match self.observe_trust(dir) {
            Ok((_, Trust::Trusted)) => None,
            Ok((key, Trust::Declined)) => Some(format!(
                "{} is explicitly untrusted in {}",
                key.display(),
                path.display()
            )),
            Ok((key, Trust::Undecided)) => Some(format!(
                "{} is not trusted in {}",
                key.display(),
                path.display()
            )),
            Err(e) => Some(format!("could not check grok folder trust: {e:#}")),
        }
    }
}

/// Lines of grok's stderr that report a failed `--trust` grant. Grok prints
/// only failures (`folder_trust::report_cli_trust_grant` is silent on success),
/// and every one of them - `Couldn't save folder trust...`, a refusal reason -
/// names "folder trust". A failed grant is otherwise invisible whenever the
/// turn happens not to edit, and a process-local one means the *next* run is
/// untrusted again.
pub(in crate::provider::grok) fn trust_failures(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter(|l| l.contains("folder trust"))
        .map(|l| l.trim().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{grok_at, scratch};
    use super::*;

    /// A parsed trust store holding exactly these entries.
    fn store(entries: &[(&Path, bool)]) -> TrustDocument {
        TrustDocument {
            folders: entries
                .iter()
                .map(|(p, t)| {
                    (
                        p.to_string_lossy().into_owned(),
                        FolderTrust {
                            trusted: *t,
                            decided_at: None,
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn decision_is_grok_lookup_on_the_workspace_key() {
        let grok = grok_at(Path::new("/unused"));
        let root = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).expect("manifest dir");
        let src = root.join("src");
        let parent = root.parent().expect("parent").to_path_buf();

        assert_eq!(
            grok.decision(&store(&[(&root, true)]), &grok.workspace_key(&src)),
            Trust::Trusted
        );
        assert_eq!(grok.decision(&store(&[]), &root), Trust::Undecided);
        // An ancestor above the repo is a different workspace: it does not count.
        assert_eq!(
            grok.decision(&store(&[(&parent, true)]), &root),
            Trust::Undecided
        );
        // Grok queries with the workspace key, so an entry below it is never
        // consulted - not even a `false` on the very directory the run is in.
        assert_eq!(
            grok.decision(
                &store(&[(&root, true), (&src, false)]),
                &grok.workspace_key(&src)
            ),
            Trust::Trusted
        );
        assert_eq!(
            grok.decision(&store(&[(&root, false)]), &grok.workspace_key(&src)),
            Trust::Declined
        );
    }

    #[test]
    fn workspace_key_follows_grok() {
        let grok = grok_at(Path::new("/unused"));
        // A subdirectory of a repo keys on the repo root.
        let root = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).expect("manifest dir");
        assert_eq!(grok.workspace_key(&root.join("src")), root);

        let base = std::fs::canonicalize(scratch()).expect("scratch");
        // A linked worktree keys on its main checkout.
        let main = base.join("main");
        std::fs::create_dir_all(main.join(".git/worktrees/wt")).expect("main repo");
        let wt = base.join("wt");
        std::fs::create_dir_all(wt.join("src")).expect("worktree");
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", main.join(".git/worktrees/wt").display()),
        )
        .expect("gitdir file");
        assert_eq!(grok.workspace_key(&wt.join("src")), main);

        // A submodule .git file points into .git/modules, not .git/worktrees.
        // It stays keyed on the submodule root just as grok's git2 key does.
        let superproject = base.join("superproject");
        let submodule = superproject.join("submodule");
        std::fs::create_dir_all(superproject.join(".git/modules/x")).expect("module gitdir");
        std::fs::create_dir_all(submodule.join("src")).expect("submodule");
        std::fs::write(submodule.join(".git"), "gitdir: ../.git/modules/x\n")
            .expect("submodule gitdir file");
        assert_eq!(main_checkout_of_worktree(&submodule), None);
        assert_eq!(
            grok.workspace_key(&submodule),
            std::fs::canonicalize(&submodule).expect("canonical submodule")
        );

        // A repo root that is the user's home (dotfiles) is never the key; grok
        // falls back to the directory itself.
        let dotfiles_home = base.join("home");
        std::fs::create_dir_all(dotfiles_home.join(".git")).expect("home repo");
        std::fs::create_dir_all(dotfiles_home.join("project")).expect("project");
        let at_home = Grok {
            home: PathBuf::from("/unused"),
            user_home: Some(dotfiles_home.clone()),
        };
        assert_eq!(
            at_home.workspace_key(&dotfiles_home.join("project")),
            dotfiles_home.join("project")
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn check_trust_refuses_a_no_an_unreadable_store_and_a_grok_worktree() {
        let home = scratch();
        let grok = grok_at(&home);
        let dir = std::fs::canonicalize(scratch()).expect("project dir");
        assert_eq!(
            grok.check_trust(&dir).expect("no store yet").1,
            Trust::Undecided,
            "--trust creates the store"
        );
        // The scratch dir lives inside this repo, so grok's key for it is the
        // repo root - the entries below have to be keyed the same way.
        let key = grok.workspace_key(&dir);

        let declined = format!("[folders.\"{}\"]\ntrusted = false\n", key.display());
        std::fs::write(home.join("trusted_folders.toml"), declined).expect("seed");
        let err = grok
            .check_trust(&dir)
            .expect_err("an explicit no is not overridden");
        assert!(err.to_string().contains("explicitly untrusted"), "{err}");
        assert!(grok.untrusted_hint(&dir).is_some());

        std::fs::write(
            home.join("trusted_folders.toml"),
            "[folders.\"/x\"]\ntrusted = 1\n",
        )
        .expect("seed");
        let err = grok
            .check_trust(&dir)
            .expect_err("grok would trust nothing");
        assert!(err.to_string().contains("trust nothing"), "{err}");

        let trusted = format!("[folders.\"{}\"]\ntrusted = true\n", key.display());
        std::fs::write(home.join("trusted_folders.toml"), trusted).expect("seed");
        assert_eq!(grok.check_trust(&dir).expect("trusted").1, Trust::Trusted);
        assert_eq!(grok.untrusted_hint(&dir), None);

        // A grok-managed `-w` worktree: its key lives in grok's own registry.
        let managed = home.join("worktrees/abc");
        std::fs::create_dir_all(&managed).expect("managed worktree");
        let err = grok.check_trust(&managed).expect_err("cannot be modelled");
        assert!(err.to_string().contains("grok-managed worktree"), "{err}");

        // The home spelling may include `..`, while the project path above is
        // canonical. The worktree refusal must compare equivalent paths.
        let nested_home = home.join("nested-grok");
        let detour = home.join("detour");
        let aliased_worktree = nested_home.join("worktrees/abc");
        std::fs::create_dir_all(&detour).expect("detour");
        std::fs::create_dir_all(&aliased_worktree).expect("aliased worktree");
        let alias = home.join("detour/../nested-grok");
        let grok = grok_at(&alias);
        let err = grok
            .check_trust(&aliased_worktree)
            .expect_err("noncanonical home must not bypass refusal");
        assert!(err.to_string().contains("grok-managed worktree"), "{err}");
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsafe_keys_are_refused_before_a_missing_store_can_request_a_grant() {
        let home = scratch();
        let project = std::fs::canonicalize(scratch()).expect("project");
        let grok = Grok {
            home: home.clone(),
            user_home: Some(project.clone()),
        };
        // Use the containing repo root as a dotfiles home, so the key itself
        // is unsafe even though the directory exists and the store is missing.
        let key = grok.workspace_key(&project);
        let grok = Grok {
            user_home: Some(key.clone()),
            ..grok
        };
        assert!(
            grok.check_trust(&key)
                .expect_err("home is never trusted")
                .to_string()
                .contains("never trusts")
        );
        assert!(
            grok.check_trust(Path::new("/"))
                .expect_err("root is never trusted")
                .to_string()
                .contains("never trusts")
        );
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn all_trust_consumers_share_store_loadability_and_keep_declines_observable() {
        let home = scratch();
        let project = scratch();
        let grok = grok_at(&home);
        let key = grok.workspace_key(&project);
        let path = home.join("trusted_folders.toml");
        assert!(matches!(grok.load_store(), StoreState::Missing));
        assert!(
            grok.untrusted_hint(&project)
                .expect("missing is undecided")
                .contains("not trusted")
        );
        std::fs::write(
            &path,
            format!("[folders.\"{}\"]\ntrusted = false\n", key.display()),
        )
        .expect("declined");
        assert_eq!(
            grok.observe_trust(&project).expect("observable decline").1,
            Trust::Declined
        );
        assert!(grok.check_trust(&project).is_err());
        assert!(
            grok.untrusted_hint(&project)
                .expect("declined hint")
                .contains("explicitly untrusted")
        );
        std::fs::write(&path, "[broken").expect("malformed");
        assert!(matches!(grok.load_store(), StoreState::Unloadable(_)));
        assert!(
            grok.untrusted_hint(&project)
                .expect("load hint")
                .contains("could not check")
        );
        assert!(grok.observe_trust(&project).is_err());
        std::fs::remove_file(&path).expect("remove malformed");
        let target = home.join("actual.toml");
        std::fs::write(
            &target,
            format!("[folders.\"{}\"]\ntrusted = true\n", key.display()),
        )
        .expect("trusted target");
        std::os::unix::fs::symlink(&target, &path).expect("symlink store");
        assert!(matches!(grok.load_store(), StoreState::Unloadable(_)));
        assert!(grok.observe_trust(&project).is_err());
        assert!(
            grok.untrusted_hint(&project)
                .expect("symlink hint")
                .contains("symlink")
        );
        std::fs::remove_file(&path).expect("remove link");
        std::fs::create_dir(&path).expect("directory instead of store");
        assert!(matches!(grok.load_store(), StoreState::Unloadable(_)));
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn only_trust_failures_are_surfaced_from_stderr() {
        let stderr = "some log line\nCouldn't save folder trust: the folder path changed. Start Grok again.\n";
        assert_eq!(trust_failures(stderr).len(), 1);
        assert!(trust_failures("all good\n").is_empty());
    }
}
