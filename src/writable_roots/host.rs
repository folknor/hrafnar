use std::path::{Path, PathBuf};

/// Environment and filesystem lookups, injected so the derivation is a pure
/// function under test - the real host's `CARGO_TARGET_DIR` and `target`
/// symlink must not decide whether the suite passes.
pub trait Host {
    fn var(&self, key: &str) -> Option<String>;
    /// The target of `path` if it is a symlink, resolved to an absolute path.
    fn read_link(&self, path: &Path) -> Option<PathBuf>;
    /// `path` with every symlink component resolved, or `None` if it does not
    /// exist. Used to check what a grant *reaches*, not merely what it spells.
    fn canonicalize(&self, path: &Path) -> Option<PathBuf>;
    /// The contents of a file, or `None` if it cannot be read.
    fn read_to_string(&self, path: &Path) -> Option<String>;
    fn is_dir(&self, path: &Path) -> bool;
}

/// The real host.
pub struct RealHost;

impl Host for RealHost {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.is_empty())
    }

    fn read_link(&self, path: &Path) -> Option<PathBuf> {
        // `read_link` errors on a non-symlink, which is the common case and not
        // worth distinguishing: either way there is nothing to grant.
        let target = std::fs::read_link(path).ok()?;
        if target.is_absolute() {
            Some(target)
        } else {
            // A relative link resolves against the link's own directory.
            Some(path.parent()?.join(target))
        }
    }

    fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
        std::fs::canonicalize(path).ok()
    }

    fn read_to_string(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `RealHost` is the one implementation with logic of its own, and
    /// `FakeHost` cannot exercise it: an empty environment variable must read as
    /// unset, or an exported-but-blank `CARGO_TARGET_DIR` would reach the
    /// absolute-path guard as `""`.
    #[test]
    fn the_real_host_treats_an_empty_variable_as_unset() {
        // SAFETY: single-threaded test, and the variable is removed after.
        unsafe { std::env::set_var("HRAFNAR_EMPTY_PROBE", "") };
        assert_eq!(RealHost.var("HRAFNAR_EMPTY_PROBE"), None);
        unsafe { std::env::set_var("HRAFNAR_EMPTY_PROBE", "x") };
        assert_eq!(RealHost.var("HRAFNAR_EMPTY_PROBE").as_deref(), Some("x"));
        unsafe { std::env::remove_var("HRAFNAR_EMPTY_PROBE") };
    }
}
