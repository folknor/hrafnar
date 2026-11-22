//! Locating a project's git metadata, and noticing when a run changed what git
//! will execute.
//!
//! A codex `workspace-write` run is granted the project's own `.git` so that it
//! can commit (codex otherwise mounts it read-only). Claude and maki have no
//! filesystem sandbox and could always write it. Either way, a run can leave
//! behind something git runs later, outside any sandbox, the next time the
//! operator uses the repository: a hook, or a config entry naming a program
//! (`core.fsmonitor`, `core.hooksPath`, a filter or diff driver, a pager, a
//! credential helper, an include pulling in another file).
//!
//! Preventing that inside the sandbox is not on offer: git rewrites `config`
//! by renaming `config.lock` over it, so a read-only `config` also breaks
//! ordinary branch tracking, and the unsandboxed harnesses would need the
//! check anyway. So hrafnar snapshots those files at launch and, at the end of
//! the run, warns about each one that changed. It reports paths and the kind of
//! change only - config can carry credentials, and hook bodies are untrusted
//! output. What it could not check is reported too, rather than passing as
//! unchanged.
//!
//! The snapshot is taken for every harness and sandbox level, after launch
//! preparation, so preparation's own writes are not attributed to the run. It
//! covers `config`, `config.worktree` and `hooks/` in both the gitdir and the
//! common dir, and follows `include.path`, `includeIf.*.path` and
//! `core.hooksPath`. Each config is parsed by `git config --file -
//! --no-includes` from bytes already read, under a 1 MiB limit, so git opens no
//! file the snapshot did not capture. Nothing is ever restored: a warning is
//! the whole response.
//!
//! This is advisory and best effort, and a clean result means nothing about
//! security: it is a comparison of two points in time, and a change it reports
//! may have been made by anything, not only the run. Not watched: the `config`
//! and hooks of nested git directories (`modules/`, `worktrees/`), git's
//! global and system config, and anything that ran git unsandboxed while the
//! run was still live. A hrafnar process killed outright never reaches the
//! comparison.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Larger than any real config, hook or locator file; past it a file is
/// reported unchecked.
const READ_LIMIT: u64 = 1 << 20;
/// Include chains deeper than this are not followed, and say so.
const INCLUDE_DEPTH: usize = 8;

/// Where a project's git metadata lives, and how it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitDirs {
    /// The directory holding `.git`.
    pub toplevel: PathBuf,
    /// The per-checkout git directory: index, `HEAD`. For an unreadable
    /// pointer, the pointer file itself.
    pub gitdir: PathBuf,
    /// Shared objects, refs and config. Equal to `gitdir` outside a linked
    /// worktree.
    pub commondir: PathBuf,
    pub layout: Layout,
    /// A `commondir` file exists but could not be used (not a regular file,
    /// unreadable, empty), so `commondir` above is only a fallback.
    pub commondir_unusable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// `.git` is a real directory in the project root itself.
    Direct,
    /// `.git` in the project root is a pointer file (linked worktree,
    /// submodule, separate gitdir).
    Pointer,
    /// A `.git` pointer file that could not be read or parsed.
    BadPointer,
    /// The project root is below the repository's top level.
    Ancestor,
}

/// Find the git metadata for `root`: its own `.git`, else the nearest
/// ancestor's. `None` outside a repository.
pub fn locate(root: &Path) -> Option<GitDirs> {
    for dir in root.ancestors() {
        let dot_git = dir.join(".git");
        let Ok(meta) = std::fs::symlink_metadata(&dot_git) else {
            continue;
        };
        let ancestor = dir != root;
        let found = |gitdir: PathBuf, layout: Layout| {
            let (commondir, commondir_unusable) = match read_commondir(&gitdir) {
                Ok(Some(common)) => (common, false),
                Ok(None) => (gitdir.clone(), false),
                Err(()) => (gitdir.clone(), true),
            };
            Some(GitDirs {
                toplevel: dir.to_path_buf(),
                gitdir,
                commondir,
                layout: if ancestor { Layout::Ancestor } else { layout },
                commondir_unusable,
            })
        };
        if meta.is_dir() {
            return found(dot_git, Layout::Direct);
        }
        return match read_pointer(&dot_git) {
            Some(gitdir) => found(gitdir, Layout::Pointer),
            None => Some(GitDirs {
                toplevel: dir.to_path_buf(),
                gitdir: dot_git.clone(),
                commondir: dot_git,
                layout: if ancestor {
                    Layout::Ancestor
                } else {
                    Layout::BadPointer
                },
                commondir_unusable: false,
            }),
        };
    }
    None
}

/// Strip what git strips from a one-line locator file: trailing CR and LF,
/// nothing else, since a path may legitimately end in a space.
fn line(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 0 && matches!(bytes[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    &bytes[..end]
}

fn relative_to(base: &Path, bytes: &[u8]) -> Option<PathBuf> {
    if bytes.is_empty() {
        return None;
    }
    Some(base.join(OsStr::from_bytes(bytes)))
}

/// `gitdir: <path>`, relative to the pointer file's directory.
fn read_pointer(dot_git: &Path) -> Option<PathBuf> {
    let (_, bytes) = bounded_read(dot_git).ok()?;
    let target = line(&bytes).strip_prefix(b"gitdir: ")?;
    relative_to(dot_git.parent()?, target)
}

/// `<gitdir>/commondir`, relative to the gitdir. `Ok(None)` only when absent;
/// one that exists and cannot be used is an error, never a silent fallback.
fn read_commondir(gitdir: &Path) -> Result<Option<PathBuf>, ()> {
    match bounded_read(&gitdir.join("commondir")) {
        Ok((_, bytes)) => relative_to(gitdir, line(&bytes)).map(Some).ok_or(()),
        Err(ReadError::Missing) => Ok(None),
        Err(_) => Err(()),
    }
}

#[derive(Debug)]
enum ReadError {
    Missing,
    /// Not a regular file at the moment it was opened.
    NotRegular,
    Other(String),
}

/// Open without following a symlink and without blocking on a FIFO, then
/// check the type and read at most the limit from that same open file, so a
/// replacement between a check and the read cannot redirect or stall it.
fn bounded_read(path: &Path) -> Result<(u32, Vec<u8>), ReadError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path);
    let mut file = match file {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ReadError::Missing),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Err(ReadError::NotRegular),
        Err(e) => return Err(ReadError::Other(e.to_string())),
    };
    let meta = file
        .metadata()
        .map_err(|e| ReadError::Other(e.to_string()))?;
    if !meta.is_file() {
        return Err(ReadError::NotRegular);
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(READ_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| ReadError::Other(e.to_string()))?;
    if bytes.len() as u64 > READ_LIMIT {
        return Err(ReadError::Other("larger than 1 MiB".into()));
    }
    Ok((meta.permissions().mode() & 0o7777, bytes))
}

/// One watched path's state.
#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Missing,
    File { mode: u32, bytes: Vec<u8> },
    Symlink(PathBuf),
    Directory,
    Other,
    Unreadable(String),
}

fn read_state(path: &Path) -> State {
    match bounded_read(path) {
        Ok((mode, bytes)) => State::File { mode, bytes },
        Err(ReadError::Missing) => State::Missing,
        Err(ReadError::Other(e)) => State::Unreadable(e),
        Err(ReadError::NotRegular) => match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => match std::fs::read_link(path) {
                Ok(target) => State::Symlink(target),
                Err(e) => State::Unreadable(e.to_string()),
            },
            Ok(meta) if meta.is_dir() => State::Directory,
            Ok(meta) if meta.is_file() => State::Unreadable("changed while being read".into()),
            Ok(_) => State::Other,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::Missing,
            Err(e) => State::Unreadable(e.to_string()),
        },
    }
}

/// The watched files and hook directories, captured at one moment.
#[derive(Debug, Default)]
pub struct Snapshot {
    root: PathBuf,
    layout: Option<GitDirs>,
    files: BTreeMap<PathBuf, State>,
    /// Hook directories and the entries in each.
    dirs: BTreeMap<PathBuf, Result<BTreeSet<PathBuf>, String>>,
    /// What the baseline could not cover, reported with the changes.
    gaps: Vec<String>,
}

impl Snapshot {
    /// Capture the metadata git may execute from, for the repository holding
    /// `root`. Empty outside a repository.
    pub fn take(root: &Path) -> Self {
        let layout = locate(root);
        let mut snap = Self {
            root: root.to_path_buf(),
            layout: layout.clone(),
            ..Self::default()
        };
        let Some(dirs) = layout else {
            return snap;
        };
        // The files that decide where git looks: rewriting either redirects
        // git to different config and hooks while every watched file there
        // stays the same.
        let dot_git = dirs.toplevel.join(".git");
        if dot_git != dirs.gitdir || !dot_git.is_dir() {
            snap.files.insert(dot_git.clone(), read_state(&dot_git));
        }
        // An unreadable pointer, or one naming no directory, at any level.
        if !dirs.gitdir.is_dir() {
            snap.gaps.push(format!(
                "{}: not checked (no git directory found through it)",
                shown(&dot_git)
            ));
            return snap;
        }
        let commondir_file = dirs.gitdir.join("commondir");
        snap.files
            .insert(commondir_file.clone(), read_state(&commondir_file));
        if dirs.commondir_unusable {
            snap.gaps.push(format!(
                "{}: not checked (exists but could not be used; the shared config and hooks it names were not watched)",
                shown(&commondir_file)
            ));
        }

        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut bases = vec![dirs.gitdir.clone()];
        if dirs.commondir != dirs.gitdir {
            bases.push(dirs.commondir.clone());
        }
        let mut configs = Vec::new();
        for base in &bases {
            configs.push((base.join("config"), 0));
            configs.push((base.join("config.worktree"), 0));
            snap.watch_dir(&base.join("hooks"));
        }
        // Parsed configs are tracked apart from captured files: a hooks
        // directory scan may already have captured a file an include names,
        // and that must not stop its own includes being followed.
        let mut parsed = BTreeSet::new();
        while let Some((path, depth)) = configs.pop() {
            if !parsed.insert(path.clone()) {
                continue;
            }
            let state = snap
                .files
                .entry(path.clone())
                .or_insert_with(|| read_state(&path))
                .clone();
            let bytes = match state {
                State::File { bytes, .. } => bytes,
                // Git follows it; the comparison only sees the link itself.
                State::Symlink(_) => {
                    snap.gaps.push(format!(
                        "{}: is a symlink, the file it points to not checked",
                        shown(&path)
                    ));
                    continue;
                }
                State::Missing => continue,
                // Unreadable is reported by the comparison; a FIFO or a
                // directory where git expects a config file would otherwise
                // compare equal and pass as clean.
                State::Unreadable(_) => continue,
                State::Directory | State::Other => {
                    snap.gaps.push(format!(
                        "{}: not checked (not a regular file)",
                        shown(&path)
                    ));
                    continue;
                }
            };
            let entries = match path_entries(&bytes) {
                Ok(entries) => entries,
                Err(e) => {
                    snap.gaps.push(format!(
                        "{}: includes and hooksPath not checked ({e})",
                        shown(&path)
                    ));
                    continue;
                }
            };
            let dir = path.parent().unwrap_or(Path::new("/"));
            for entry in entries {
                let (PathEntry::Include(value) | PathEntry::HooksPath(value)) = &entry;
                let base = match entry {
                    PathEntry::Include(_) => dir,
                    PathEntry::HooksPath(_) => dirs.toplevel.as_path(),
                };
                let Some(target) = resolve(value, base, home.as_deref()) else {
                    snap.gaps.push(format!(
                        "{}: a path it names was not checked ({})",
                        shown(&path),
                        String::from_utf8_lossy(value).escape_debug()
                    ));
                    continue;
                };
                match entry {
                    PathEntry::Include(_) => {
                        if depth < INCLUDE_DEPTH {
                            configs.push((target, depth + 1));
                        } else {
                            snap.gaps.push(format!(
                                "{}: not checked (include chain deeper than {INCLUDE_DEPTH})",
                                shown(&target)
                            ));
                        }
                    }
                    PathEntry::HooksPath(_) => snap.watch_dir(&target),
                }
            }
        }
        snap
    }

    fn watch_dir(&mut self, dir: &Path) {
        if self.dirs.contains_key(dir) {
            return;
        }
        let listing = list_dir(dir);
        if let Ok(entries) = &listing {
            for entry in entries {
                self.files
                    .entry(entry.clone())
                    .or_insert_with(|| read_state(entry));
            }
        }
        self.dirs.insert(dir.to_path_buf(), listing);
    }

    /// Re-read every path this snapshot watched and describe what differs,
    /// along with whatever could not be checked. Empty when nothing changed
    /// and coverage was complete.
    pub fn changes(&self) -> Vec<String> {
        let mut lines = self.gaps.clone();
        // Compared even when there was no repository at launch: one created
        // during the run (an umbrella directory is a supported root) is new
        // metadata git will execute from.
        let now = locate(&self.root);
        if now != self.layout {
            let to = now
                .as_ref()
                .map(|d| shown(&d.commondir))
                .unwrap_or_else(|| "no repository".into());
            lines.push(format!(
                "{}: repository layout changed (git metadata now resolves to {to})",
                shown(&self.root)
            ));
        }
        let mut now = BTreeMap::new();
        for path in self.files.keys() {
            now.insert(path.clone(), read_state(path));
        }
        for (dir, before) in &self.dirs {
            match (before, list_dir(dir)) {
                (Ok(before), Ok(after)) => {
                    for added in after.difference(before) {
                        now.insert(added.clone(), read_state(added));
                    }
                }
                (Err(e), _) => {
                    lines.push(format!("{}: not checked ({e})", shown(dir)));
                }
                (_, Err(e)) => {
                    lines.push(format!("{}: not checked ({e})", shown(dir)));
                }
            }
        }
        for (path, after) in &now {
            let before = self.files.get(path).unwrap_or(&State::Missing);
            if let Some(change) = describe(before, after) {
                lines.push(format!("{}: {change}", shown(path)));
            }
        }
        lines
    }
}

fn list_dir(dir: &Path) -> Result<BTreeSet<PathBuf>, String> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|e| e.map(|e| e.path()).map_err(|e| e.to_string()))
            .collect(),
        // A hook directory created during the run shows its hooks as added.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(e) => Err(e.to_string()),
    }
}

fn describe(before: &State, after: &State) -> Option<String> {
    use State::*;
    let text = match (before, after) {
        (Unreadable(e), _) | (_, Unreadable(e)) => format!("not checked ({e})"),
        (a, b) if a == b => return None,
        (Missing, _) => "added".into(),
        (_, Missing) => "removed".into(),
        (
            File {
                mode: m1,
                bytes: b1,
            },
            File {
                mode: m2,
                bytes: b2,
            },
        ) => match (b1 != b2, m1 != m2) {
            (true, true) => "content and mode changed".into(),
            (true, false) => "content changed".into(),
            _ => format!("mode changed ({m1:o} to {m2:o})"),
        },
        (Symlink(_), Symlink(_)) => "symlink target changed".into(),
        (_, Symlink(_)) => "now a symlink".into(),
        (Symlink(_), _) => "no longer a symlink".into(),
        _ => "file type changed".into(),
    };
    Some(text)
}

/// A path as printed: control characters and other oddities escaped, since a
/// file name the run created is untrusted output.
fn shown(path: &Path) -> String {
    path.to_string_lossy().escape_debug().to_string()
}

/// A path-valued config entry, as pathname bytes: git does not require them
/// to be UTF-8, and a lossy conversion would watch the wrong file.
#[derive(Debug, PartialEq, Eq)]
enum PathEntry {
    Include(Vec<u8>),
    HooksPath(Vec<u8>),
}

/// How long `git config` may take on one bounded input before it is killed
/// and the file reported unchecked. It parses at most 1 MiB, so any real run
/// finishes far inside this; the bound only stops a wedged git from holding
/// the launch.
const GIT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// The include and hook-path entries in one config file's bytes, parsed by
/// git itself from stdin with includes off, so git opens no file the snapshot
/// did not read through its own size limit. The input is written from its own
/// thread while the output is drained here: `git config --list` prints while
/// it parses, so writing everything first deadlocks once both pipes fill.
fn path_entries(bytes: &[u8]) -> Result<Vec<PathEntry>, String> {
    let mut child = Command::new("git")
        .args([
            "--no-pager",
            "config",
            "--file",
            "-",
            "--no-includes",
            "--null",
            "--list",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run git: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("git stdin unavailable")?;
    let input = bytes.to_vec();
    // Neither pipe thread is ever joined: a descendant of git holding a pipe
    // open past git's own exit must not hold the launch, so the output
    // arrives over a channel with the deadline's remainder as its bound, and
    // a stuck thread is simply abandoned.
    std::thread::spawn(move || {
        // A config git rejects can close stdin early; the exit status says so.
        let _ = stdin.write_all(&input);
    });
    let mut stdout = child.stdout.take().ok_or("git stdout unavailable")?;
    let (sent, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = sent.send(stdout.read_to_end(&mut out).map(|_| out));
    });
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < GIT_DEADLINE => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Ok(None) => {
                let _ = child.kill();
                // Reaped off this thread: a child stuck in uninterruptible
                // sleep would otherwise hold the launch past the deadline.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Err(format!(
                    "git config did not finish in {}s",
                    GIT_DEADLINE.as_secs()
                ));
            }
            Err(e) => return Err(format!("git config failed: {e}")),
        }
    };
    if !status.success() {
        return Err("git could not parse it".into());
    }
    let remaining = GIT_DEADLINE.saturating_sub(started.elapsed());
    match received.recv_timeout(remaining) {
        Ok(Ok(out)) => Ok(parse_list(&out)),
        Ok(Err(e)) => Err(format!("reading git config output failed: {e}")),
        Err(_) => Err(format!(
            "git config output did not end in {}s",
            GIT_DEADLINE.as_secs()
        )),
    }
}

/// `git config --null --list` output: `key\nvalue\0` per entry. Section and
/// key names come back lowercased; subsections keep their case.
fn parse_list(out: &[u8]) -> Vec<PathEntry> {
    let mut entries = Vec::new();
    for record in out.split(|&b| b == 0) {
        let Some(split) = record.iter().position(|&b| b == b'\n') else {
            continue;
        };
        let (key, value) = (&record[..split], record[split + 1..].to_vec());
        let key = String::from_utf8_lossy(key);
        if key == "include.path" || (key.starts_with("includeif.") && key.ends_with(".path")) {
            entries.push(PathEntry::Include(value));
        } else if key == "core.hookspath" {
            entries.push(PathEntry::HooksPath(value));
        }
    }
    entries
}

/// A config path value as git resolves it: `~/` from `$HOME`, relative paths
/// against `base`. `None` for what this does not resolve (empty, `~user/`,
/// `~/` with no `$HOME`), which the caller reports as unchecked.
fn resolve(value: &[u8], base: &Path, home: Option<&Path>) -> Option<PathBuf> {
    if value.is_empty() {
        return None;
    }
    if let Some(rest) = value.strip_prefix(b"~/") {
        // `~//x` is `$HOME//x` to git; joining an absolute `/x` would drop
        // `$HOME` altogether.
        let start = rest.iter().position(|&b| b != b'/').unwrap_or(rest.len());
        return home.map(|h| h.join(OsStr::from_bytes(&rest[start..])));
    }
    if value.starts_with(b"~") {
        return None;
    }
    let path = Path::new(OsStr::from_bytes(value));
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(crate::ids::generate_uuid());
        std::fs::create_dir_all(&root).expect("scratch dir");
        root
    }

    #[test]
    fn a_direct_repo_is_located_as_such() {
        let root = scratch();
        std::fs::create_dir_all(root.join(".git")).expect("git dir");
        let dirs = locate(&root).expect("located");
        assert_eq!(dirs.layout, Layout::Direct);
        assert_eq!(dirs.gitdir, root.join(".git"));
        assert_eq!(dirs.commondir, dirs.gitdir);
    }

    #[test]
    fn a_subdirectory_project_is_located_through_its_ancestor() {
        let root = scratch();
        std::fs::create_dir_all(root.join(".git")).expect("git dir");
        std::fs::create_dir_all(root.join("sub")).expect("sub");
        let dirs = locate(&root.join("sub")).expect("located");
        assert_eq!(dirs.layout, Layout::Ancestor);
        assert_eq!(dirs.toplevel, root);
    }

    /// A linked worktree: `.git` points at a per-worktree gitdir whose
    /// `commondir` names the shared store, where config and hooks live.
    #[test]
    fn a_pointer_file_resolves_gitdir_and_commondir() {
        let root = scratch();
        let main = root.join("main/.git");
        let wt = main.join("worktrees/wt");
        std::fs::create_dir_all(&wt).expect("worktree gitdir");
        std::fs::write(wt.join("commondir"), "../..\n").expect("commondir");
        let checkout = root.join("wt");
        std::fs::create_dir_all(&checkout).expect("checkout");
        std::fs::write(checkout.join(".git"), format!("gitdir: {}\n", wt.display()))
            .expect("pointer");
        let dirs = locate(&checkout).expect("located");
        assert_eq!(dirs.layout, Layout::Pointer);
        assert_eq!(dirs.gitdir, wt);
        assert_eq!(dirs.commondir, wt.join("../.."));
    }

    /// Git strips only the line ending from a gitfile, so a directory name
    /// ending in a space is a valid target.
    #[test]
    fn a_pointer_keeps_trailing_spaces_and_drops_only_the_line_ending() {
        let root = scratch();
        std::fs::write(root.join(".git"), "gitdir: store \r\n").expect("pointer");
        let dirs = locate(&root).expect("located");
        assert_eq!(dirs.gitdir, root.join("store "));
    }

    #[test]
    fn an_unparseable_pointer_is_located_as_bad_and_reported_unchecked() {
        let root = scratch();
        std::fs::write(root.join(".git"), "not a pointer\n").expect("pointer");
        assert_eq!(locate(&root).expect("located").layout, Layout::BadPointer);
        let changes = Snapshot::take(&root).changes();
        assert!(
            changes.iter().any(|l| l.contains("not checked")),
            "{changes:?}"
        );
    }

    /// A FIFO where a locator file belongs must not block the launch, and
    /// must not pass as a repository with no shared store.
    #[test]
    fn a_fifo_commondir_does_not_block_and_is_reported() {
        let root = scratch();
        std::fs::create_dir_all(root.join(".git")).expect("git dir");
        let fifo = std::ffi::CString::new(root.join(".git/commondir").as_os_str().as_bytes())
            .expect("path");
        // SAFETY: valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let dirs = locate(&root).expect("located");
        assert!(dirs.commondir_unusable);
        let changes = Snapshot::take(&root).changes();
        assert!(
            changes.iter().any(|l| l.contains("commondir: not checked")),
            "{changes:?}"
        );
    }

    /// A repository created during a run in a directory that had none is new
    /// metadata git will execute from.
    #[test]
    fn a_repository_created_during_the_run_is_reported() {
        let root = scratch();
        let snap = Snapshot::take(&root);
        std::fs::create_dir_all(root.join(".git")).expect("git dir");
        let changes = snap.changes();
        assert!(
            changes
                .iter()
                .any(|l| l.contains("repository layout changed")),
            "{changes:?}"
        );
    }

    #[test]
    fn an_unresolvable_include_is_reported_unchecked() {
        let root = scratch();
        std::fs::create_dir_all(root.join(".git")).expect("git dir");
        std::fs::write(
            root.join(".git/config"),
            "[include]\n\tpath = ~someone/x.cfg\n",
        )
        .expect("config");
        let changes = Snapshot::take(&root).changes();
        assert!(
            changes
                .iter()
                .any(|l| l.contains("a path it names was not checked")),
            "{changes:?}"
        );
    }

    #[test]
    fn a_doubled_slash_after_the_tilde_stays_under_home() {
        assert_eq!(
            resolve(b"~//hooks", Path::new("/r"), Some(Path::new("/home/dev"))),
            Some(PathBuf::from("/home/dev/hooks"))
        );
    }

    #[test]
    fn a_fifo_at_an_include_path_is_reported_unchecked() {
        let root = scratch();
        std::fs::create_dir_all(root.join(".git")).expect("git dir");
        std::fs::write(root.join(".git/config"), "[include]\n\tpath = pipe.cfg\n").expect("config");
        let fifo = std::ffi::CString::new(root.join(".git/pipe.cfg").as_os_str().as_bytes())
            .expect("path");
        // SAFETY: valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let changes = Snapshot::take(&root).changes();
        assert!(
            changes
                .iter()
                .any(|l| l.contains("pipe.cfg: not checked (not a regular file)")),
            "{changes:?}"
        );
    }

    /// Include paths are pathname bytes: a non-UTF-8 name must be watched as
    /// itself, not as its lossy replacement.
    #[test]
    fn a_non_utf8_include_path_is_kept_as_bytes() {
        let out = b"include.path\nx\xffy.cfg\0";
        assert_eq!(
            parse_list(out),
            vec![PathEntry::Include(b"x\xffy.cfg".to_vec())]
        );
    }

    #[test]
    fn an_unchanged_repo_reports_nothing() {
        let root = scratch();
        std::fs::create_dir_all(root.join(".git/hooks")).expect("hooks");
        std::fs::write(root.join(".git/config"), "[core]\n\tbare = false\n").expect("config");
        let snap = Snapshot::take(&root);
        assert!(snap.changes().is_empty(), "{:?}", snap.changes());
    }

    #[test]
    fn a_planted_hook_and_a_changed_config_are_reported() {
        let root = scratch();
        std::fs::create_dir_all(root.join(".git/hooks")).expect("hooks");
        std::fs::write(root.join(".git/config"), "[core]\n\tbare = false\n").expect("config");
        let snap = Snapshot::take(&root);
        std::fs::write(root.join(".git/hooks/pre-commit"), "#!/bin/sh\n").expect("hook");
        std::fs::write(root.join(".git/config"), "[core]\n\tfsmonitor = x\n").expect("config");
        let changes = snap.changes();
        assert!(
            changes
                .iter()
                .any(|l| l.ends_with("hooks/pre-commit: added")),
            "{changes:?}"
        );
        assert!(
            changes
                .iter()
                .any(|l| l.ends_with(".git/config: content changed")),
            "{changes:?}"
        );
    }

    /// Adding `commondir` redirects git to another store's config and hooks
    /// while every file watched at launch stays the same.
    #[test]
    fn a_commondir_added_during_the_run_is_reported() {
        let root = scratch();
        std::fs::create_dir_all(root.join(".git")).expect("git dir");
        std::fs::create_dir_all(root.join("elsewhere")).expect("other store");
        let snap = Snapshot::take(&root);
        std::fs::write(root.join(".git/commondir"), "../elsewhere\n").expect("commondir");
        let changes = snap.changes();
        assert!(
            changes
                .iter()
                .any(|l| l.contains("repository layout changed")),
            "{changes:?}"
        );
        assert!(
            changes.iter().any(|l| l.ends_with(".git/commondir: added")),
            "{changes:?}"
        );
    }

    /// Making an existing sample hook executable is enough for git to run it,
    /// and changes no byte of it.
    #[test]
    fn an_executable_bit_change_is_reported() {
        let root = scratch();
        let hook = root.join(".git/hooks/pre-commit");
        std::fs::create_dir_all(root.join(".git/hooks")).expect("hooks");
        std::fs::write(&hook, "#!/bin/sh\n").expect("hook");
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o644)).expect("mode");
        let snap = Snapshot::take(&root);
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("mode");
        let changes = snap.changes();
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert!(
            changes[0].contains("mode changed (644 to 755)"),
            "{changes:?}"
        );
    }

    /// An existing `include.path` pulling in a file in the working tree, which
    /// any run could already write: a change there changes git's behaviour
    /// while leaving `.git/config` untouched.
    #[test]
    fn an_include_target_is_followed_relative_to_its_including_file() {
        let root = scratch();
        std::fs::create_dir_all(root.join(".git")).expect("git dir");
        std::fs::write(
            root.join(".git/config"),
            "[include]\n\tpath = ../extra.cfg\n",
        )
        .expect("config");
        std::fs::write(root.join("extra.cfg"), "[core]\n").expect("include");
        let snap = Snapshot::take(&root);
        std::fs::write(root.join("extra.cfg"), "[core]\n\tpager = x\n").expect("include");
        let changes = snap.changes();
        assert!(
            changes
                .iter()
                .any(|l| l.ends_with("extra.cfg: content changed")),
            "{changes:?}"
        );
    }

    /// A hooks directory scan that captures a file first must not stop that
    /// file's own includes being followed when an include later names it.
    #[test]
    fn a_config_captured_by_a_hooks_scan_is_still_parsed() {
        let root = scratch();
        std::fs::create_dir_all(root.join(".git")).expect("git dir");
        std::fs::write(
            root.join(".git/config"),
            "[core]\n\thooksPath = .git\n[include]\n\tpath = extra.cfg\n",
        )
        .expect("config");
        std::fs::write(
            root.join(".git/extra.cfg"),
            "[include]\n\tpath = ../deeper.cfg\n",
        )
        .expect("include");
        std::fs::write(root.join("deeper.cfg"), "[core]\n").expect("deeper");
        let snap = Snapshot::take(&root);
        std::fs::write(root.join("deeper.cfg"), "[core]\n\tpager = x\n").expect("deeper");
        let changes = snap.changes();
        assert!(
            changes
                .iter()
                .any(|l| l.ends_with("deeper.cfg: content changed")),
            "{changes:?}"
        );
    }

    #[test]
    fn a_hooks_path_directory_is_watched_relative_to_the_toplevel() {
        let root = scratch();
        std::fs::create_dir_all(root.join(".git")).expect("git dir");
        std::fs::create_dir_all(root.join("githooks")).expect("hooks dir");
        std::fs::write(root.join(".git/config"), "[core]\n\thooksPath = githooks\n")
            .expect("config");
        let snap = Snapshot::take(&root);
        std::fs::write(root.join("githooks/post-checkout"), "#!/bin/sh\n").expect("hook");
        let changes = snap.changes();
        assert!(
            changes
                .iter()
                .any(|l| l.ends_with("githooks/post-checkout: added")),
            "{changes:?}"
        );
    }

    /// A config large enough to fill both pipes must not deadlock the parse.
    #[test]
    fn a_large_config_is_parsed_without_deadlock() {
        let mut config = String::from("[include]\n\tpath = x.cfg\n[big]\n");
        while config.len() < 900 * 1024 {
            config.push_str("\tkey = some value that fills the pipe buffers\n");
        }
        let entries = path_entries(config.as_bytes()).expect("parsed");
        assert_eq!(entries, vec![PathEntry::Include(b"x.cfg".to_vec())]);
    }

    #[test]
    fn config_list_output_is_parsed_for_path_entries() {
        let out = b"core.bare\nfalse\0include.path\n~/a.cfg\0includeif.gitdir:/x/.path\nb.cfg\0core.hookspath\nhooks\0";
        assert_eq!(
            parse_list(out),
            vec![
                PathEntry::Include(b"~/a.cfg".to_vec()),
                PathEntry::Include(b"b.cfg".to_vec()),
                PathEntry::HooksPath(b"hooks".to_vec()),
            ]
        );
    }

    #[test]
    fn a_control_character_in_a_reported_path_is_escaped() {
        assert_eq!(
            shown(Path::new("/r/.git/hooks/a\nb")),
            "/r/.git/hooks/a\\nb"
        );
    }
}
