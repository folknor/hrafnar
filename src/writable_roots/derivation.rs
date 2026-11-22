use std::path::{Path, PathBuf};

use super::dependencies::path_dependency_repos;
use super::filter::push_root;
use super::{GrantedRoot, Host};

/// Resolve an environment-derived root through its final `..` component
/// before `push_root` applies its spelling check. The suffix after that
/// component may name a directory the build has not created yet.
fn push_environment_root(
    out: &mut Vec<GrantedRoot>,
    cwd: &Path,
    host: &impl Host,
    path: &Path,
    why: &'static str,
) {
    let components: Vec<_> = path.components().collect();
    let Some(last_parent) = components
        .iter()
        .rposition(|component| *component == std::path::Component::ParentDir)
    else {
        push_root(out, cwd, host, path, why);
        return;
    };

    let mut prefix = PathBuf::new();
    for component in &components[..=last_parent] {
        prefix.push(component.as_os_str());
    }
    let Some(mut resolved) = host.canonicalize(&prefix) else {
        // Leave unresolved parent components for push_root to refuse.
        push_root(out, cwd, host, path, why);
        return;
    };
    for component in &components[last_parent + 1..] {
        resolved.push(component.as_os_str());
    }
    push_root(out, cwd, host, &resolved, why);
}

/// Paths outside `cwd` that a build needs write access to.
///
/// Returns an empty vec when nothing is needed - a workspace whose build stays
/// entirely inside itself derives no grants and is left with codex's defaults.
pub fn derive(cwd: &Path, host: &impl Host) -> Vec<GrantedRoot> {
    let mut out: Vec<GrantedRoot> = Vec::new();

    // The build lock. Fatal when missing, and never inside the workspace.
    //
    // The grant is the whole directory, which also covers brokkr's `--commit`
    // worktrees under `worktrees/`. A sibling of the project could not be
    // granted: creating one writes into the parent of every repo, the sha is
    // chosen inside the run, and codex writable roots take no globs. Brokkr
    // makes a per-worktree container here, with the checkout and one symlink
    // per out-of-repo path dependency side by side, so `../dep` still resolves
    // and the sandbox, which binds the resolved path, keeps the real sibling
    // checkout read-only. Their build output goes under the project's target,
    // which the cargo-target grants below already cover.
    //
    // `$HOME/.brokkr`, not `$XDG_RUNTIME_DIR`, because `$HOME` is set by every
    // login path, including the ones that skip pam_systemd and so drop
    // `XDG_RUNTIME_DIR`. It resolves identically from a terminal, an ssh
    // session and an agent harness, which is what makes the grant derivable
    // at all rather than something each project has to declare. A directory,
    // never a lock file at the home root, which would need `$HOME` writable.
    if let Some(home) = host.var("HOME") {
        let path = PathBuf::from(home).join(".brokkr");
        if path.is_absolute() {
            push_environment_root(
                &mut out,
                cwd,
                host,
                &path,
                "build lock and --commit worktrees ($HOME/.brokkr)",
            );
        }
    }

    // The cargo home: registry index, downloaded `.crate` archives, git
    // checkouts, and the package-cache lock.
    //
    // This one fails *conditionally*. A build whose dependencies are already
    // unpacked needs nothing here that matters: cargo's `.package-cache` flock
    // degrades silently on a read-only filesystem rather than erroring, and
    // the global-cache tracker tolerates the same. So a sandboxed run without
    // this grant compiles, lints and runs full test suites. It turns fatal the
    // moment a crate has to be *fetched* - a new dependency, a bump,
    // a lockfile the workspace has not populated yet - at which point the run
    // dies before compilation with `failed to open ... registry/cache/...:
    // Read-only file system (os error 30)`, an error whose text points at the
    // cache rather than at the sandbox.
    //
    // `$CARGO_HOME`, else `$HOME/.cargo`, which is cargo's own documented
    // default and therefore as deterministic as the lock directory above.
    let cargo_home = host
        .var("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| host.var("HOME").map(|h| PathBuf::from(h).join(".cargo")));
    if let Some(path) = cargo_home.filter(|p| p.is_absolute()) {
        push_environment_root(&mut out, cwd, host, &path, "cargo home ($CARGO_HOME)");
    }

    // Brokkr's command history (`history.db`, SQLite, so its journal files
    // need the directory, not just the file).
    //
    // Failing this write is silent: the command itself still succeeds but
    // leaves no row in `brokkr history`, which is the one record of what an
    // agent's brokkr was doing (for instance, which run held the build lock).
    //
    // `$XDG_DATA_HOME/brokkr`, else `$HOME/.local/share/brokkr`, which is
    // where brokkr itself resolves it. A relative `XDG_DATA_HOME` is invalid
    // per the XDG spec and falls back the same way.
    let data_home = host
        .var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            host.var("HOME")
                .map(|h| PathBuf::from(h).join(".local").join("share"))
        });
    if let Some(path) = data_home.filter(|p| p.is_absolute()) {
        push_environment_root(
            &mut out,
            cwd,
            host,
            &path.join("brokkr"),
            "brokkr command history ($XDG_DATA_HOME/brokkr)",
        );
    }

    // An explicit shared cargo cache, exported globally on some hosts.
    if let Some(dir) = host.var("CARGO_TARGET_DIR") {
        let path = PathBuf::from(dir);
        if path.is_absolute() {
            push_environment_root(
                &mut out,
                cwd,
                host,
                &path,
                "cargo target ($CARGO_TARGET_DIR)",
            );
        }
    }

    // A `target` symlink pointing at a shared cache. This is checked even when
    // `CARGO_TARGET_DIR` is set, because the two can disagree and the symlink
    // is what an unset-env build would use.
    //
    // The link target is resolved here, before the filter. A relative link
    // (`target -> ../shared-target`) comes back from `read_link` joined to the
    // link's directory, so it carries `..`, which `push_root` refuses on the
    // spelling. Canonicalizing it resolves the `..` against the real directory
    // the link sits in, which is exactly what the kernel does when cargo
    // follows the link. A target that does not exist cannot be resolved and is
    // passed on as spelled, so a dangling link with `..` stays refused.
    if let Some(link) = host.read_link(&cwd.join("target")) {
        let target = host.canonicalize(&link).unwrap_or(link);
        push_root(
            &mut out,
            cwd,
            host,
            &target,
            "cargo target (./target symlink)",
        );
    }

    // The Git LFS store of every sibling checkout a path dependency lives in.
    //
    // A build stamp runs `git status` over those checkouts, and the sandbox
    // leaves them read-only. Whenever a tracked LFS file's stat data no longer
    // matches the index (a re-checkout, a touch), git re-hashes it through the
    // LFS clean filter, which writes into `.git/lfs/tmp` and then
    // `.git/lfs/objects` - and dies with `read-only file system`. A normal
    // status would save the refreshed stat data so it happens once, but the
    // sandbox cannot write the index either, so every sandboxed status in that
    // checkout fails until someone runs one outside. The failed git call made
    // the build stamp unidentifiable, which surfaced as test failures far from
    // the cause. The checkouts come from `dependencies::path_dependency_repos`.
    //
    // Only `.git/lfs` is granted, never `.git`: refs, config and hooks stay
    // read-only. Only checkouts that already have a `.git/lfs` directory, so a
    // repo without LFS is granted nothing.
    for repo in path_dependency_repos(cwd, host) {
        push_root(
            &mut out,
            cwd,
            host,
            &repo.join(".git").join("lfs"),
            "git lfs store of a path dependency's checkout",
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writable_roots::test_host::{FakeHost, cwd};

    #[test]
    fn a_workspace_that_needs_nothing_grants_nothing() {
        let host = FakeHost::new();
        assert!(derive(&cwd(), &host).is_empty());
    }

    #[test]
    fn the_build_lock_directory_is_granted() {
        let host = FakeHost::new().var("HOME", "/home/dev");
        let grants = derive(&cwd(), &host);
        assert_eq!(grants[0].path, "/home/dev/.brokkr");
    }

    /// The conditional failure: a warm cargo home needs no write (the
    /// package-cache flock degrades silently on a read-only filesystem), so a
    /// missing grant here passes every build until one has to *fetch* a crate,
    /// which then dies before compilation.
    #[test]
    fn the_cargo_home_is_granted() {
        let host = FakeHost::new().var("HOME", "/home/dev");
        let grants = derive(&cwd(), &host);
        let paths: Vec<&str> = grants.iter().map(|g| g.path.as_str()).collect();
        assert!(paths.contains(&"/home/dev/.cargo"), "{paths:?}");
    }

    /// `$CARGO_HOME` wins over the `$HOME/.cargo` default - a host that
    /// relocates its registry would otherwise be granted a directory cargo
    /// never touches while the real one stayed read-only.
    #[test]
    fn an_explicit_cargo_home_overrides_the_default_location() {
        let host = FakeHost::new()
            .var("HOME", "/home/dev")
            .var("CARGO_HOME", "/media/disk/cargo-home");
        let paths: Vec<String> = derive(&cwd(), &host).into_iter().map(|g| g.path).collect();
        assert!(
            paths.iter().any(|p| p == "/media/disk/cargo-home"),
            "{paths:?}"
        );
        assert!(!paths.iter().any(|p| p == "/home/dev/.cargo"), "{paths:?}");
    }

    /// A sandboxed brokkr that cannot write its history drops the row silently,
    /// so whole codex sessions went missing from `brokkr history`.
    #[test]
    fn the_brokkr_history_directory_is_granted() {
        let host = FakeHost::new().var("HOME", "/home/dev");
        let paths: Vec<String> = derive(&cwd(), &host).into_iter().map(|g| g.path).collect();
        assert!(
            paths.iter().any(|p| p == "/home/dev/.local/share/brokkr"),
            "{paths:?}"
        );

        let host = host.var("XDG_DATA_HOME", "/srv/xdg");
        let paths: Vec<String> = derive(&cwd(), &host).into_iter().map(|g| g.path).collect();
        assert!(paths.iter().any(|p| p == "/srv/xdg/brokkr"), "{paths:?}");
        assert!(
            !paths.iter().any(|p| p == "/home/dev/.local/share/brokkr"),
            "{paths:?}"
        );
    }

    /// The lock directory must not depend on `XDG_RUNTIME_DIR`, which is
    /// absent from any login that skips pam_systemd (Tailscale SSH among
    /// them), so a derivation consulting it would go silent in that case.
    #[test]
    fn the_build_lock_ignores_xdg_runtime_dir() {
        let host = FakeHost::new()
            .var("HOME", "/home/dev")
            .var("XDG_RUNTIME_DIR", "/run/user/1000");
        let paths: Vec<String> = derive(&cwd(), &host).into_iter().map(|g| g.path).collect();
        assert!(paths.iter().any(|p| p == "/home/dev/.brokkr"), "{paths:?}");
        assert!(
            !paths.iter().any(|p| p.starts_with("/run/user")),
            "{paths:?}"
        );
    }

    /// A login with no `HOME` cannot have its lock location derived. Inventing
    /// one would grant `/.brokkr`, a path nobody named.
    #[test]
    fn no_home_derives_no_build_lock() {
        assert!(derive(&cwd(), &FakeHost::new()).is_empty());
    }

    /// `$HOME` itself is never the grant - only the dedicated subdirectory. A
    /// bare lock file at the home root would need the home directory writable,
    /// handing over every repo, `~/.ssh` and hrafnar's own logs.
    #[test]
    fn the_build_lock_grant_is_never_the_home_directory() {
        let host = FakeHost::new().var("HOME", "/home/dev");
        let grants = derive(&cwd(), &host);
        assert!(
            grants.iter().all(|g| g.path != "/home/dev"),
            "home itself must never be granted: {grants:?}"
        );
    }

    #[test]
    fn a_global_cargo_target_dir_is_granted() {
        let host = FakeHost::new().var("CARGO_TARGET_DIR", "/media/disk/cargo");
        let grants = derive(&cwd(), &host);
        assert_eq!(grants.len(), 1, "{grants:?}");
        assert_eq!(grants[0].path, "/media/disk/cargo");
    }

    #[test]
    fn a_target_symlink_pointing_outside_is_granted() {
        let host = FakeHost::new().link("/home/dev/project/target", "/media/disk/cargo");
        let grants = derive(&cwd(), &host);
        assert_eq!(grants.len(), 1, "{grants:?}");
        assert_eq!(grants[0].path, "/media/disk/cargo");
    }

    /// The shared-cache layout several repos carry: a relative link to a
    /// sibling directory. `read_link` hands back the target joined to the
    /// link's directory, `..` and all; it must still be granted, by its
    /// resolved path, rather than refused by the spelled-`..` filter.
    #[test]
    fn a_relative_target_symlink_through_a_parent_dir_is_granted() {
        let host = FakeHost::new()
            .link(
                "/home/dev/project/target",
                "/home/dev/project/../shared-target",
            )
            .resolves(
                "/home/dev/project/../shared-target",
                "/home/dev/shared-target",
            );
        let grants = derive(&cwd(), &host);
        assert_eq!(grants.len(), 1, "{grants:?}");
        assert_eq!(grants[0].path, "/home/dev/shared-target");
        assert_eq!(grants[0].why, "cargo target (./target symlink)");
    }

    /// Resolving the link does not bypass the filters: a relative link that
    /// climbs to an ancestor of the workspace is still refused.
    #[test]
    fn a_relative_target_symlink_to_an_ancestor_is_refused() {
        let host = FakeHost::new()
            .link("/home/dev/project/target", "/home/dev/project/..")
            .resolves("/home/dev/project/..", "/home/dev");
        assert!(derive(&cwd(), &host).is_empty());
    }

    /// The two can disagree - an exported var overriding a repo's own symlink.
    /// A build may consult either depending on how it is invoked, so both are
    /// granted rather than guessing which one wins.
    #[test]
    fn a_disagreeing_env_var_and_symlink_are_both_granted() {
        let host = FakeHost::new()
            .var("CARGO_TARGET_DIR", "/media/disk/cargo")
            .link("/home/dev/project/target", "/media/other/cargo");
        let grants = derive(&cwd(), &host);
        assert_eq!(grants.len(), 2);
    }

    /// A relative `CARGO_TARGET_DIR` resolves against the build's own cwd, so it
    /// is already inside the workspace and there is nothing to grant. Passing it
    /// through verbatim would emit a relative path into an absolute-path config.
    #[test]
    fn a_relative_cargo_target_dir_is_ignored() {
        let host = FakeHost::new().var("CARGO_TARGET_DIR", "../shared-target");
        assert!(derive(&cwd(), &host).is_empty());
    }

    #[test]
    fn an_empty_env_var_is_ignored() {
        let host = FakeHost::new().var("CARGO_TARGET_DIR", "");
        // The real host filters empties; the fake stores them, so assert the
        // absolute-path guard rejects it too.
        assert!(derive(&cwd(), &host).is_empty());
    }
}
