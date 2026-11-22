use std::path::{Path, PathBuf};

use super::filter::push_root;
use super::{GrantedRoot, Host, derive};

/// Paths outside `cwd` that a build needs write access to, plus any a profile
/// asked for.
///
/// Profile roots are **added to** the derived ones, not a replacement: the
/// derivation covers what any build on this machine needs, while a profile
/// covers what one project needs beyond that. They go through the same filters,
/// so a profile cannot grant `/`, an ancestor of the workspace, a path
/// containing `..`, or a relative path, and gains nothing by naming a path
/// already inside the workspace.
///
/// Returns an empty vec when nothing is needed - a workspace whose build stays
/// entirely inside itself derives no grants and is left with codex's defaults.
pub fn derive_with(cwd: &Path, host: &impl Host, profile_roots: &[String]) -> Vec<GrantedRoot> {
    let mut out = derive(cwd, host);
    for root in profile_roots {
        let Some(expanded) = expand(root, host) else {
            // `expand` has already explained why the configured path failed.
            continue;
        };
        let path = PathBuf::from(&expanded);
        if !path.is_absolute() {
            eprintln!("warning: ignoring relative writable_roots entry: {root}");
            continue;
        }
        push_root(&mut out, cwd, host, &path, "profile writable_roots");
    }
    out
}

/// Expand a configured root with the same grammar as project paths. Expansion
/// errors are local to this optional grant, so report and drop the entry.
///
/// One `.hrafnar.toml` is read on machines whose layouts differ, so a
/// literal path is often unwritable somewhere. Expansion is the deliberate,
/// visible-in-config kind of environment use - unlike the derivation above, the
/// operator wrote the variable name down.
///
/// An **unset** variable returns `None` rather than expanding to nothing:
/// `$UNSET/data` would otherwise collapse to `/data`, granting a path nobody
/// named, and `$UNSET` alone would collapse to `/` - the one grant this module
/// must never make.
fn expand(raw: &str, host: &impl Host) -> Option<String> {
    match crate::project::expand_path(raw, &|key| host.var(key)) {
        Ok(expanded) => Some(expanded),
        Err(error) => {
            eprintln!("warning: ignoring writable_roots entry {raw}: {error}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writable_roots::test_host::{FakeHost, cwd};

    /// A profile must not be able to undo the derivation - the derived roots are
    /// what makes a build run at all, so replacing rather than extending would
    /// let a profile that names one extra path break the build it was widening.
    #[test]
    fn a_profile_root_does_not_replace_the_derived_ones() {
        let host = FakeHost::new()
            .var("HOME", "/home/dev")
            .var("CARGO_TARGET_DIR", "/media/disk/cargo");
        let grants = derive_with(&cwd(), &host, &["/srv/fixtures".to_string()]);
        let paths: Vec<&str> = grants.iter().map(|g| g.path.as_str()).collect();
        assert!(paths.contains(&"/home/dev/.brokkr"), "{paths:?}");
        assert!(paths.contains(&"/media/disk/cargo"), "{paths:?}");
    }

    /// Profile entries go through the same filters as derived ones, so a
    /// `.hrafnar.toml` cannot hand a run the whole filesystem, and a relative
    /// path (which codex would reject as a config error at launch) is dropped
    /// with a warning rather than emitted.
    #[test]
    fn a_profile_cannot_grant_the_filesystem_root_or_a_relative_path() {
        let host = FakeHost::new();
        let grants = derive_with(
            &cwd(),
            &host,
            &["/".to_string(), "../elsewhere".to_string()],
        );
        assert!(grants.is_empty(), "{grants:?}");
    }

    /// A profile naming a path the host already derived should not double it.
    #[test]
    fn a_profile_root_matching_a_derived_one_is_not_repeated() {
        let host = FakeHost::new().var("CARGO_TARGET_DIR", "/media/disk/cargo");
        let grants = derive_with(&cwd(), &host, &["/media/disk/cargo".to_string()]);
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].why, "cargo target ($CARGO_TARGET_DIR)");
    }

    #[test]
    fn a_profile_root_expands_a_bare_variable() {
        let host = FakeHost::new().var("DATA", "/srv/data");
        let grants = derive_with(&cwd(), &host, &["$DATA/fixtures".to_string()]);
        assert_eq!(grants[0].path, "/srv/data/fixtures");
    }

    #[test]
    fn a_profile_root_expands_a_braced_variable() {
        let host = FakeHost::new().var("DATA", "/srv/data");
        let grants = derive_with(&cwd(), &host, &["${DATA}x/fixtures".to_string()]);
        assert_eq!(grants[0].path, "/srv/datax/fixtures");
    }

    #[test]
    fn a_profile_root_expands_tilde() {
        // Selected by reason, not by index: setting `HOME` also derives the
        // build lock, so position would pin the derivation's shape rather than
        // the expansion under test.
        let host = FakeHost::new().var("HOME", "/home/dev");
        let grants = derive_with(&cwd(), &host, &["~/data".to_string()]);
        let profile: Vec<&str> = grants
            .iter()
            .filter(|g| g.why == "profile writable_roots")
            .map(|g| g.path.as_str())
            .collect();
        assert_eq!(profile, vec!["/home/dev/data"]);
    }

    /// The dangerous case. Expanding an unset variable to nothing would turn
    /// `$UNSET/data` into `/data` - a real path nobody named - and `$UNSET`
    /// alone into `/`, handing the run the whole filesystem.
    #[test]
    fn an_unset_variable_drops_the_entry_rather_than_expanding_to_nothing() {
        let host = FakeHost::new();
        let grants = derive_with(
            &cwd(),
            &host,
            &["$NOPE/data".to_string(), "$NOPE".to_string()],
        );
        assert!(grants.is_empty(), "{grants:?}");
    }

    #[test]
    fn several_variables_in_one_entry_all_expand() {
        let host = FakeHost::new().var("A", "/srv").var("B", "cache");
        let grants = derive_with(&cwd(), &host, &["$A/shared/$B".to_string()]);
        assert_eq!(grants[0].path, "/srv/shared/cache");
    }

    #[test]
    fn an_unclosed_brace_drops_the_entry() {
        let host = FakeHost::new().var("DATA", "/srv/data");
        let grants = derive_with(&cwd(), &host, &["${DATA/fixtures".to_string()]);
        assert!(grants.is_empty(), "{grants:?}");
    }

    /// Expansion must not create a path that skips the filters: a variable
    /// holding `/` still has to be refused.
    #[test]
    fn expansion_cannot_smuggle_the_filesystem_root_past_the_filters() {
        let host = FakeHost::new().var("ROOT", "/");
        let grants = derive_with(&cwd(), &host, &["$ROOT".to_string()]);
        assert!(grants.is_empty(), "{grants:?}");
    }

    /// Likewise for the inside-the-workspace filter.
    #[test]
    fn expansion_into_the_workspace_is_still_dropped() {
        let host = FakeHost::new().var("W", "/home/dev/project");
        let grants = derive_with(&cwd(), &host, &["$W/sub".to_string()]);
        assert!(grants.is_empty(), "{grants:?}");
    }

    #[test]
    fn a_parent_dir_from_an_expanded_variable_is_also_refused() {
        let host = FakeHost::new().var("BASE", "/srv/data");
        let grants = derive_with(&cwd(), &host, &["$BASE/../..".to_string()]);
        assert!(grants.is_empty(), "{grants:?}");
    }
}
