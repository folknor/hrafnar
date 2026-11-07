use std::path::Path;

use super::{GrantedRoot, Host};

/// Add one candidate root, applying every filter a grant must pass.
///
/// Filters run against the **resolved** path as well as the spelled one. A
/// lexical check answers "what does this say", and the sandbox binds "what does
/// this reach": `/srv/cache` may be a symlink to `/`, in which case granting it
/// grants the filesystem while passing every string test. The resolved form is
/// what gets emitted, so the log names the directory that actually becomes
/// writable. A path that does not exist yet cannot be resolved and is taken at
/// its word - codex will fail on it if it is wrong, and refusing it outright
/// would break granting a cache directory the build itself creates.
pub(super) fn push_root(
    out: &mut Vec<GrantedRoot>,
    cwd: &Path,
    host: &impl Host,
    path: &Path,
    why: &'static str,
) {
    let resolved = host.canonicalize(path);
    let path: &Path = resolved.as_deref().unwrap_or(path);
    // `..` defeats every lexical check below - `/..` is absolute, is not a
    // prefix of cwd and is not empty, so it would sail through and codex would
    // resolve it to `/`. Refuse rather than normalise: `/a/../b` and `/b` are
    // the same path only when `/a` is not a symlink, and this module does not
    // get to assume that.
    if path
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        eprintln!(
            "warning: ignoring writable root containing '..': {}",
            path.display()
        );
        return;
    }
    // An *ancestor* of the workspace is the mirror image of the check below and
    // just as wrong: `~` or a `$HOME` whose intended subdirectory was dropped
    // resolves to the home directory, and granting it hands over every repo,
    // `~/.ssh`, and `review`'s own logs. A grant that contains the workspace is
    // never the narrow thing this module promises.
    if cwd.starts_with(path) {
        eprintln!(
            "warning: ignoring writable root that contains the workspace: {}",
            path.display()
        );
        return;
    }
    // Only paths outside the workspace are worth granting: cwd is already
    // writable, so granting a subpath of it would be noise that reads like a
    // widening.
    if path.starts_with(cwd) {
        return;
    }
    let Some(text) = path.to_str() else {
        return;
    };
    // A control character cannot be carried by a TOML basic string, so such a
    // path would render a `-c` value codex refuses to parse - an opaque launch
    // failure. A path really containing a newline is far more likely a mangled
    // environment variable than a directory anyone meant to grant.
    if text.contains(|c: char| c.is_control()) {
        eprintln!("warning: ignoring writable root containing a control character");
        return;
    }
    // A symlink target routinely carries a trailing slash while the env var
    // naming the same directory does not, so compare and emit the normalised
    // form or the two arrive as separate grants - which is harmless to codex but
    // reads as two widenings where there is one.
    let text = text.trim_end_matches('/');
    if text.is_empty() {
        // `/` is not a target dir; it is the whole filesystem.
        return;
    }
    if out.iter().any(|g| g.path == text) {
        return;
    }
    out.push(GrantedRoot {
        path: text.to_string(),
        why,
    });
}

#[cfg(test)]
mod tests {
    use crate::writable_roots::test_host::{FakeHost, cwd};
    use crate::writable_roots::{derive, derive_with};

    /// A path inside the workspace is writable already. Granting it would widen
    /// nothing while making the run look as though it had been widened.
    #[test]
    fn paths_inside_the_workspace_are_not_granted() {
        let host = FakeHost::new()
            .var("CARGO_TARGET_DIR", "/home/dev/project/target")
            .link("/home/dev/project/target", "/home/dev/project/.cache");
        assert!(derive(&cwd(), &host).is_empty());
    }

    /// The env var and the symlink routinely name the same directory; granting
    /// it twice would be harmless but reads as two separate widenings.
    #[test]
    fn the_same_path_from_two_sources_is_granted_once() {
        let host = FakeHost::new()
            .var("CARGO_TARGET_DIR", "/media/disk/cargo")
            .link("/home/dev/project/target", "/media/disk/cargo");
        let grants = derive(&cwd(), &host);
        assert_eq!(grants.len(), 1);
    }

    /// Observed in the field: `read_link` returned `/media/disk/cargo/` for a
    /// symlink created with a trailing slash, while `CARGO_TARGET_DIR` named the
    /// same directory without one, and both were granted.
    #[test]
    fn a_trailing_slash_does_not_make_a_second_grant() {
        let host = FakeHost::new()
            .var("CARGO_TARGET_DIR", "/media/disk/cargo")
            .link("/home/dev/project/target", "/media/disk/cargo/");
        let grants = derive(&cwd(), &host);
        assert_eq!(grants.len(), 1, "{grants:?}");
        assert_eq!(grants[0].path, "/media/disk/cargo");
    }

    #[test]
    fn a_profile_root_is_added_to_the_derived_ones() {
        let host = FakeHost::new().var("HOME", "/home/dev");
        let grants = derive_with(&cwd(), &host, &["/srv/fixtures".to_string()]);
        let Some(last) = grants.last() else {
            panic!("a profile root must be granted");
        };
        assert_eq!(last.path, "/srv/fixtures");
        assert_eq!(last.why, "profile writable_roots");
        assert!(
            grants.len() > 1,
            "the derived roots must survive: {grants:?}"
        );
    }

    /// The filters are lexical, but the sandbox binds what a path *reaches*.
    /// `/srv/build-cache` spelled innocently but symlinked to `/` passes every
    /// string test and grants the filesystem.
    #[test]
    fn a_root_that_resolves_to_the_filesystem_root_is_refused() {
        let host = FakeHost::new().resolves("/srv/build-cache", "/");
        let grants = derive_with(&cwd(), &host, &["/srv/build-cache".to_string()]);
        assert!(grants.is_empty(), "{grants:?}");
    }

    #[test]
    fn a_root_that_resolves_to_an_ancestor_of_the_workspace_is_refused() {
        let host = FakeHost::new().resolves("/srv/cache", "/home/dev");
        let grants = derive_with(&cwd(), &host, &["/srv/cache".to_string()]);
        assert!(grants.is_empty(), "{grants:?}");
    }

    /// The resolved form is what gets granted, so the announcement and the
    /// sidecar name the directory that actually becomes writable rather than
    /// the alias the operator happened to type.
    #[test]
    fn a_resolved_root_is_recorded_by_its_real_path() {
        let host = FakeHost::new().resolves("/srv/alias", "/media/disk/cargo");
        let grants = derive_with(&cwd(), &host, &["/srv/alias".to_string()]);
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].path, "/media/disk/cargo");
    }

    /// A derived root is symlink-checked too, not just profile entries: an
    /// operator's `./target` link or `CARGO_TARGET_DIR` can point anywhere.
    #[test]
    fn a_derived_root_that_resolves_wide_is_refused() {
        let host = FakeHost::new()
            .var("CARGO_TARGET_DIR", "/srv/cargo")
            .resolves("/srv/cargo", "/");
        assert!(derive(&cwd(), &host).is_empty());
    }

    /// `..` defeats every lexical filter: `/..` is absolute, is not a prefix of
    /// cwd and is not empty, so before this was refused it was emitted verbatim
    /// and codex resolved it to `/`.
    #[test]
    fn a_parent_dir_component_cannot_smuggle_a_wider_grant() {
        let host = FakeHost::new();
        for entry in ["/..", "/srv/..", "/srv/../../etc", "/home/dev/project/.."] {
            let grants = derive_with(&cwd(), &host, &[entry.to_string()]);
            assert!(grants.is_empty(), "{entry} produced {grants:?}");
        }
    }

    /// The mirror of the inside-the-workspace filter, and the likelier operator
    /// mistake: `~` (or a `$HOME` whose intended subdirectory was dropped)
    /// resolves to the home directory, which *contains* the workspace. Granting
    /// it hands over every repo, `~/.ssh`, and review's own logs.
    #[test]
    fn a_root_containing_the_workspace_is_refused() {
        let host = FakeHost::new().var("HOME", "/home/dev");
        for entry in ["~", "$HOME", "/home/dev", "/home", "/"] {
            // Setting `HOME` also derives the build lock, which is a legitimate
            // grant and not what this test is about - only the profile entry
            // must be refused.
            let grants: Vec<_> = derive_with(&cwd(), &host, &[entry.to_string()])
                .into_iter()
                .filter(|g| g.why == "profile writable_roots")
                .collect();
            assert!(grants.is_empty(), "{entry} produced {grants:?}");
        }
    }

    /// A sibling of the workspace under the same parent is a legitimate grant -
    /// the ancestor check must not be a string prefix test.
    #[test]
    fn a_sibling_of_the_workspace_is_still_granted() {
        let host = FakeHost::new();
        let grants = derive_with(&cwd(), &host, &["/home/dev/project-other".to_string()]);
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].path, "/home/dev/project-other");
    }

    /// A control character cannot live in a TOML basic string, so emitting one
    /// would render a `-c` value codex refuses to parse - an opaque launch
    /// failure rather than a grant.
    #[test]
    fn a_control_character_in_a_path_is_refused() {
        let host = FakeHost::new().var("CARGO_HOME", "/home/dev/.cargo\nx");
        let grants = derive(&cwd(), &host);
        assert!(grants.is_empty(), "{grants:?}");
    }

    /// Granting `/` would hand the run the entire filesystem, which is the one
    /// outcome this module must never produce.
    #[test]
    fn the_filesystem_root_is_never_granted() {
        let host = FakeHost::new().var("CARGO_TARGET_DIR", "/");
        assert!(derive(&cwd(), &host).is_empty());
    }
}
