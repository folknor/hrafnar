use std::path::{Path, PathBuf};

use super::Host;

/// The git checkouts, other than the workspace's own, that hold the path
/// dependencies named in the workspace's root `Cargo.toml` and use Git LFS.
pub(super) fn path_dependency_repos(cwd: &Path, host: &impl Host) -> Vec<PathBuf> {
    let Some(text) = host.read_to_string(&cwd.join("Cargo.toml")) else {
        return Vec::new();
    };
    let Ok(manifest) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    collect_dependency_paths(&manifest, false, &mut paths);

    let own = repo_of(cwd, host);
    let mut repos: Vec<PathBuf> = Vec::new();
    for dep in paths {
        let dep = cwd.join(dep);
        let Some(resolved) = host.canonicalize(&dep) else {
            continue;
        };
        let Some(repo) = repo_of(&resolved, host) else {
            continue;
        };
        if Some(&repo) == own.as_ref() || repos.contains(&repo) {
            continue;
        }
        if host.is_dir(&repo.join(".git").join("lfs")) {
            repos.push(repo);
        }
    }
    repos
}

/// Every `path` of a dependency entry, found in any table whose key ends in
/// `dependencies` - which covers `[dependencies]`, the dev and build kinds,
/// `[workspace.dependencies]` and `[target.<cfg>.dependencies]`.
fn collect_dependency_paths(table: &toml::Table, in_deps: bool, out: &mut Vec<String>) {
    for (key, value) in table {
        let toml::Value::Table(inner) = value else {
            continue;
        };
        if in_deps {
            if let Some(toml::Value::String(path)) = inner.get("path") {
                out.push(path.clone());
            }
        } else {
            collect_dependency_paths(inner, key.ends_with("dependencies"), out);
        }
    }
}

/// The nearest directory at or above `path` that holds a `.git` directory.
fn repo_of(path: &Path, host: &impl Host) -> Option<PathBuf> {
    path.ancestors()
        .find(|dir| host.is_dir(&dir.join(".git")))
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use crate::writable_roots::derive;
    use crate::writable_roots::test_host::{FakeHost, cwd};

    /// The workspace and a sibling checkout holding its path dependencies.
    fn lfs_sibling(sibling_has_lfs: bool) -> FakeHost {
        let manifest = r#"
[package]
name = "project"

[dependencies]
piners-data = { path = "../piners/crates/piners-data" }
serde = "1"

[target.'cfg(unix)'.dev-dependencies.piners-facts]
path = "../piners/crates/piners-facts"
"#;
        let host = FakeHost::new()
            .file("/home/dev/project/Cargo.toml", manifest)
            .dir("/home/dev/project/.git")
            .dir("/home/dev/piners/.git")
            .resolves(
                "/home/dev/project/../piners/crates/piners-data",
                "/home/dev/piners/crates/piners-data",
            )
            .resolves(
                "/home/dev/project/../piners/crates/piners-facts",
                "/home/dev/piners/crates/piners-facts",
            );
        if sibling_has_lfs {
            host.dir("/home/dev/piners/.git/lfs")
        } else {
            host
        }
    }

    /// A sandboxed `git status` in a sibling checkout re-hashes a stale LFS
    /// file through the clean filter, which writes under `.git/lfs` and failed
    /// with `read-only file system`, making the build stamp unidentifiable.
    #[test]
    fn a_path_dependencys_lfs_store_is_granted_once() {
        let grants = derive(&cwd(), &lfs_sibling(true));
        let paths: Vec<&str> = grants.iter().map(|g| g.path.as_str()).collect();
        assert_eq!(paths, vec!["/home/dev/piners/.git/lfs"]);
    }

    #[test]
    fn a_path_dependency_checkout_without_lfs_is_granted_nothing() {
        assert!(derive(&cwd(), &lfs_sibling(false)).is_empty());
    }

    /// A path dependency inside the workspace's own repo needs nothing: its
    /// `.git` is the workspace's, which codex keeps read-only on purpose.
    #[test]
    fn a_path_dependency_in_the_workspaces_own_repo_is_granted_nothing() {
        let host = FakeHost::new()
            .file(
                "/home/dev/project/Cargo.toml",
                "[dependencies]\ninner = { path = \"crates/inner\" }\n",
            )
            .dir("/home/dev/project/.git")
            .dir("/home/dev/project/.git/lfs");
        assert!(derive(&cwd(), &host).is_empty());
    }
}
