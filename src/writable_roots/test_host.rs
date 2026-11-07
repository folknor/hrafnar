use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::Host;

pub(super) struct FakeHost {
    vars: HashMap<String, String>,
    links: HashMap<PathBuf, PathBuf>,
    canon: HashMap<PathBuf, PathBuf>,
    files: HashMap<PathBuf, String>,
    dirs: Vec<PathBuf>,
}

impl FakeHost {
    pub(super) fn new() -> Self {
        Self {
            vars: HashMap::new(),
            links: HashMap::new(),
            canon: HashMap::new(),
            files: HashMap::new(),
            dirs: Vec::new(),
        }
    }
    pub(super) fn file(mut self, at: &str, contents: &str) -> Self {
        self.files.insert(PathBuf::from(at), contents.to_string());
        self
    }
    pub(super) fn dir(mut self, at: &str) -> Self {
        self.dirs.push(PathBuf::from(at));
        self
    }
    pub(super) fn var(mut self, key: &str, value: &str) -> Self {
        self.vars.insert(key.to_string(), value.to_string());
        self
    }
    pub(super) fn link(mut self, at: &str, to: &str) -> Self {
        self.links.insert(PathBuf::from(at), PathBuf::from(to));
        self
    }
    /// Model a path that *resolves* somewhere other than it is spelled -
    /// the case a purely lexical filter cannot see.
    pub(super) fn resolves(mut self, path: &str, to: &str) -> Self {
        self.canon.insert(PathBuf::from(path), PathBuf::from(to));
        self
    }
}

impl Host for FakeHost {
    fn var(&self, key: &str) -> Option<String> {
        self.vars.get(key).cloned()
    }
    fn read_link(&self, path: &Path) -> Option<PathBuf> {
        self.links.get(path).cloned()
    }
    /// Unmapped paths resolve to themselves, matching the common case of a
    /// directory with no symlink components.
    fn canonicalize(&self, path: &Path) -> Option<PathBuf> {
        Some(self.canon.get(path).cloned().unwrap_or(path.to_path_buf()))
    }
    fn read_to_string(&self, path: &Path) -> Option<String> {
        self.files.get(path).cloned()
    }
    fn is_dir(&self, path: &Path) -> bool {
        self.dirs.iter().any(|d| d == path)
    }
}

pub(super) fn cwd() -> PathBuf {
    PathBuf::from("/home/dev/project")
}
