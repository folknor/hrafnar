//! In-flight run markers.
//!
//! # Why this exists
//!
//! The sidecar log (`sessions.rs`) is written *after* a run returns, so a run
//! that is still going - or one that is wedged and will never return - is
//! completely invisible to `review sessions`. During the 10-hour codex hang that
//! motivated the watchdog, `review sessions` showed the session sitting at one
//! touch the entire time, with the *previous* turn's response, which read
//! exactly like "nothing is happening" while codex was in fact running. The
//! touch count only ever increments on detected completion, so it cannot be used
//! to tell working from wedged.
//!
//! A marker file is written as soon as the session id is known and retained
//! through recording and handoff publication, which gives `review sessions` a third state to report:
//! "turn in flight since <time>".
//!
//! # Why a file rather than a sidecar row
//!
//! The row would have to be written from `harness`, which has the session id
//! but none of the audit metadata (`audit_id`, `private`, archetype) that a
//! sidecar row requires - threading all of it down just to mark liveness is a
//! lot of plumbing for a fact that is worthless once the run ends. A marker file
//! is naturally self-cleaning, and its staleness is independently checkable via
//! the recorded pid.
//!
//! # Staleness
//!
//! Removal is best-effort: `Drop` covers normal returns, errors and panics, but
//! not `SIGKILL` of `review` itself. Each marker therefore records the `review`
//! that owns it (pid and start time, so a reused pid is not mistaken for it),
//! and readers treat a marker whose owner is gone as stale rather than as a
//! live turn - unless the run's keeper is still alive, tearing down what the
//! run started, in which case the marker reads as cleanup pending until it is
//! not. Invocation namespace reservation failures refuse before launching;
//! marker updates after admission are best-effort visibility hints.

// Markers, requests and launch locks deliberately share this store. The lock
// protocol is operator-owned; splitting just markers/requests would divide the
// directory and identity rules without introducing an independent store rule.
// The frozen LaunchLock API intentionally remains a validated string facade.
// Known Lease identities stay typed; raw stream/history evidence is preserved.
// Executable SessionId conversion belongs at action boundaries, not in serde
// for historical markers, whose original strings must remain readable.
pub use crate::process::identity_alive;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Default, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Running,
    HandoffPending,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "MarkerWire")]
pub struct Marker {
    pub session_id: String,
    /// On disk as `provider`, so live markers from older builds still read.
    #[serde(rename = "provider")]
    pub harness: String,
    pub project: String,
    pub started_epoch: u64,
    pub owner: crate::process::ProcessIdentity,
    pub keeper: Option<crate::process::ProcessIdentity>,
    pub signal_ready: bool,
    pub run_id: String,
    pub protocol: u32,
    pub phase: Phase,
    pub predecessor: Option<crate::sessions::handoff::RunId>,
}
// Old active markers remain visible. Their duplicate child_pid is readiness
// evidence only when it names the keeper, never a second signal identity.
#[derive(Deserialize)]
struct MarkerWire {
    session_id: String,
    #[serde(rename = "provider")]
    harness: String,
    project: String,
    started_epoch: u64,
    #[serde(default)]
    owner: Option<crate::process::ProcessIdentity>,
    #[serde(default)]
    keeper: Option<serde_json::Value>,
    #[serde(default)]
    signal_ready: bool,
    #[serde(default)]
    run_id: String,
    #[serde(default)]
    protocol: u32,
    #[serde(default)]
    phase: Phase,
    #[serde(default)]
    predecessor: Option<crate::sessions::handoff::RunId>,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    pid_start: Option<u64>,
    #[serde(default)]
    keeper_start: Option<u64>,
    #[serde(default)]
    child_pid: Option<u32>,
}
impl TryFrom<MarkerWire> for Marker {
    type Error = String;
    fn try_from(w: MarkerWire) -> Result<Self, Self::Error> {
        let owner = w
            .owner
            .or_else(|| {
                w.pid
                    .and_then(|p| crate::process::ProcessIdentity::recorded(p, w.pid_start))
            })
            .ok_or("invalid marker owner")?;
        let keeper = match w.keeper {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::Number(n)) => Some(
                crate::process::ProcessIdentity::recorded(
                    n.as_u64()
                        .and_then(|p| u32::try_from(p).ok())
                        .ok_or("invalid keeper pid")?,
                    w.keeper_start,
                )
                .ok_or("invalid keeper pid")?,
            ),
            Some(v) => Some(serde_json::from_value(v).map_err(|e| e.to_string())?),
        };
        let signal_ready = if w.protocol == 0 {
            keeper.is_some_and(|k| Some(k.pid()) == w.child_pid)
        } else {
            w.signal_ready
        };
        Ok(Self {
            session_id: w.session_id,
            harness: w.harness,
            project: w.project,
            started_epoch: w.started_epoch,
            owner,
            keeper,
            signal_ready,
            run_id: w.run_id,
            protocol: w.protocol,
            phase: w.phase,
            predecessor: w.predecessor,
        })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunState {
    Running,
    CleanupPending,
}
#[derive(Clone)]
pub struct Live {
    pub marker: Marker,
    pub state: RunState,
}
impl std::ops::Deref for Live {
    type Target = Marker;
    fn deref(&self) -> &Marker {
        &self.marker
    }
}
impl Live {
    pub fn cleanup_pending(&self) -> bool {
        self.state == RunState::CleanupPending
    }
}

/// Where markers live. `data_root` overrides the real XDG location and exists so
/// the test harness cannot leave stub markers in the operator's actual
/// `~/.local/share/review/inflight` - redirecting `CODEX_HOME` only redirects
/// the *child*, not the paths `review` itself resolves.
fn dir(data_root: Option<&Path>) -> Option<PathBuf> {
    crate::data::DataDir::from_override(data_root).inflight()
}

/// Is `id` safe to use as a path component for a session?
///
/// The session id reaches here straight from `review resume <id>` on the command
/// line or historical evidence. Launches use checked SessionId values, but
/// these string-facing forensic APIs must also reject unsafe input. Using it
/// unchecked as a path component let `review resume ../foo` escape the marker
/// directory and write (and then, via `Guard::drop`, *delete*) a file elsewhere
/// under the data dir.
///
/// Harness session ids are opaque, so allow ASCII alphanumerics, dashes and
/// underscores while excluding `/`, `.` and anything else that could traverse.
/// An allowlist rather than a "reject `..`" denylist because only the former is
/// safe by construction.
pub(crate) fn is_safe_session_id(id: &str) -> bool {
    crate::harness::SessionId::is_safe(id)
}

/// Reserve the invocation namespace before a harness can launch, including a
/// fresh Codex run whose session is not known until output arrives. Reservation
/// collisions fail launch; there is never a fallback to a shared marker path.
pub struct Reservation {
    path: PathBuf,
    markers: std::sync::Mutex<Vec<PathBuf>>,
}
impl Reservation {
    pub(crate) fn register(&self, guard: &Guard) {
        if let Some((path, _)) = &guard.0 {
            self.markers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(path.clone());
        }
    }
    /// Only paths exclusively created by this invocation are ours to remove.
    /// Called after receipt publication, or by the last reservation owner.
    pub(crate) fn complete(&self) {
        let mut paths = self
            .markers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        paths.retain(|path| match std::fs::remove_file(path) {
            Ok(()) => false,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => {
                eprintln!(
                    "warning: failed to remove owned marker {}: {e}",
                    path.display()
                );
                true
            }
        });
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.complete();
        let _ = std::fs::remove_file(&self.path);
    }
}
pub(crate) fn reserve(
    data: &crate::data::DataDir,
    id: &crate::sessions::handoff::RunId,
) -> std::io::Result<Option<Reservation>> {
    let Some(dir) = data.inflight() else {
        return Ok(None);
    };
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.lease", id.as_str()));
    let _file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let reservation = Reservation {
        path,
        markers: std::sync::Mutex::new(Vec::new()),
    };
    for extension in ["result", "result.tmp"] {
        match std::fs::symlink_metadata(dir.join(format!("{}.{extension}", id.as_str()))) {
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "invocation result namespace already exists",
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    // A crashed owner's reservation persists. Also refuse historical/stale
    // marker residue rather than adopt its namespace if a file was removed.
    let suffix = format!(".{}.json", id.as_str());
    for entry in std::fs::read_dir(&dir)? {
        if entry?.file_name().to_str().is_some_and(|s| {
            s.ends_with(&suffix)
                || s.ends_with(&format!("{suffix}.tmp"))
                || s.ends_with(&format!(".{}.interrupt", id.as_str()))
        }) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "invocation markers already exist",
            ));
        }
    }
    Ok(Some(reservation))
}

/// Exclusively owns a per-invocation path. No other run can rewrite or remove
/// it, so cleanup needs no check-then-act comparison with a shared session file.
pub struct Guard(Option<(PathBuf, Marker)>, bool);
impl Drop for Guard {
    fn drop(&mut self) {
        if let Some((path, _)) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}
impl Guard {
    /// The reservation already registered this exact owned path. Even a failed
    /// phase rewrite transfers deletion authority without an unlink window.
    pub(crate) fn handoff(&mut self) {
        if let Some((path, mut marker)) = self.0.take() {
            marker.phase = Phase::HandoffPending;
            marker.signal_ready = false;
            if let Err(e) = write_marker(&path, &marker) {
                eprintln!("warning: failed to mark pending handoff: {e}");
            }
        }
    }
    pub(crate) fn collision(&self) -> bool {
        self.1
    }
    pub fn record_keeper(&mut self, keeper: u32) {
        if let Some((path, marker)) = self.0.as_mut() {
            marker.keeper = crate::process::ProcessIdentity::observe(keeper);
            if let Err(e) = write_marker(path, marker) {
                eprintln!("warning: failed to update inflight marker: {e}");
            }
        }
    }
    pub fn record_ready(&mut self, ready: bool) {
        if let Some((path, marker)) = self.0.as_mut() {
            marker.signal_ready = ready;
            if let Err(e) = write_marker(path, marker) {
                eprintln!("warning: failed to update inflight marker: {e}");
            }
        }
    }
    pub(crate) fn take_request(&self, data: &crate::data::DataDir) -> bool {
        self.0.as_ref().is_some_and(|(_, marker)| {
            marker
                .request_path(data)
                .is_some_and(|p| std::fs::remove_file(p).is_ok())
        })
    }
}
impl Marker {
    pub(crate) fn request_path(&self, data: &crate::data::DataDir) -> Option<PathBuf> {
        let sid = crate::harness::SessionId::parse(&self.session_id).ok()?;
        let run: crate::sessions::handoff::RunId = self.run_id.clone().try_into().ok()?;
        Some(
            data.inflight()?
                .join(format!("{}.{}.interrupt", sid.as_str(), run.as_str())),
        )
    }
    pub(crate) fn request(&self, data: &crate::data::DataDir) -> std::io::Result<()> {
        std::fs::write(
            self.request_path(data)
                .ok_or_else(|| std::io::Error::other("unsafe run identity"))?,
            "",
        )
    }
}

/// Write a marker via a rename, so a concurrent reader never sees it
/// half-written - a marker that briefly failed to parse would read as "no run
/// in flight" to `review interrupt`. The temporary file's extension keeps it out
/// of `read_live`.
fn write_marker(path: &Path, marker: &Marker) -> std::io::Result<()> {
    let json = serde_json::to_string(marker).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

/// Record that `session_id` is running now. Returns a guard that removes the
/// marker when dropped. Late marker IO failures degrade visibility; invocation
/// namespace reservation is checked independently before harness launch.
pub fn mark(
    session_id: &str,
    harness: crate::harness::Harness,
    project: &str,
    data_root: Option<&Path>,
) -> Guard {
    mark_in(
        session_id,
        harness,
        project,
        &crate::data::DataDir::from_override(data_root),
    )
}

pub(crate) fn mark_in(
    session_id: &str,
    harness: crate::harness::Harness,
    project: &str,
    data: &crate::data::DataDir,
) -> Guard {
    let Ok(id) = crate::harness::SessionId::parse(session_id) else {
        eprintln!("warning: session id is not a safe filename; skipping in-flight marker");
        return Guard(None, false);
    };
    mark_checked(
        &id,
        harness,
        project,
        data,
        &crate::sessions::handoff::RunId::default(),
        None,
    )
}

pub(crate) fn mark_for(
    session_id: &crate::harness::SessionId,
    harness: crate::harness::Harness,
    project: &str,
    data: &crate::data::DataDir,
    run_id: &crate::sessions::handoff::RunId,
    predecessor: Option<&crate::sessions::handoff::RunId>,
) -> Guard {
    mark_checked(session_id, harness, project, data, run_id, predecessor)
}

fn mark_checked(
    session_id: &crate::harness::SessionId,
    harness: crate::harness::Harness,
    project: &str,
    data: &crate::data::DataDir,
    run_id: &crate::sessions::handoff::RunId,
    predecessor: Option<&crate::sessions::handoff::RunId>,
) -> Guard {
    let session_id = session_id.as_str();
    let Some(dir) = data.inflight() else {
        return Guard(None, false);
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("warning: failed to create inflight dir: {e}");
        return Guard(None, false);
    }
    // Validated above as a bare filename component, so this cannot escape `dir`.
    let path = dir.join(format!("{session_id}.{}.json", run_id.as_str()));
    let marker = Marker {
        session_id: session_id.to_owned(),
        harness: harness.as_str().to_owned(),
        project: project.to_owned(),
        started_epoch: crate::harness::now_epoch_secs(),
        owner: crate::process::ProcessIdentity::observe(std::process::id())
            .expect("own positive pid"),
        keeper: None,
        signal_ready: false,
        phase: Phase::Running,
        predecessor: predecessor.cloned(),
        run_id: run_id.as_str().to_owned(),
        protocol: crate::sessions::handoff::PROTOCOL,
    };
    use std::io::Write;
    let tmp = path.with_extension("json.tmp");
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
    {
        Ok(file) => file,
        Err(e) => {
            eprintln!("warning: failed to create inflight marker: {e}");
            return Guard(None, e.kind() == std::io::ErrorKind::AlreadyExists);
        }
    };
    let bytes = serde_json::to_vec(&marker).expect("marker serializes");
    let published = file
        .write_all(&bytes)
        .and_then(|()| std::fs::hard_link(&tmp, &path));
    let _ = std::fs::remove_file(tmp);
    if let Err(e) = published {
        eprintln!("warning: failed to publish inflight marker: {e}");
        return Guard(None, e.kind() == std::io::ErrorKind::AlreadyExists);
    }
    Guard(Some((path, marker)), false)
}

/// Every currently-live in-flight marker. Markers whose owning `review` process
/// and keeper are established gone are stale and cleaned up here. Unknown
/// process observations preserve the marker. Requests are never swept from an
/// observation snapshot: a concurrent marker may have been missed.
///
/// `data_root` is `None` everywhere but tests: `review sessions` and `review
/// interrupt` are operator-facing and read the real location.
pub fn read_live(data_root: Option<&Path>) -> Vec<Live> {
    let Some(dir) = dir(data_root) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        match path.extension().and_then(|e| e.to_str()) {
            Some("json") => {}
            _ => continue,
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(marker) = serde_json::from_str::<Marker>(&content) else {
            continue;
        };
        if !marker.owner.is_gone() {
            out.push(Live {
                marker,
                state: RunState::Running,
            });
            continue;
        }
        // The keeper may have been published after our first read but before
        // owner death. Reread only now, when that owner can no longer update.
        let latest = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Marker>(&s).ok());
        let Some(latest) = latest.filter(|m| m.owner == marker.owner && m.run_id == marker.run_id)
        else {
            out.push(Live {
                marker,
                state: RunState::CleanupPending,
            });
            continue;
        };
        if latest.keeper.is_some_and(|keeper| !keeper.is_gone()) {
            out.push(Live {
                marker: latest,
                state: RunState::CleanupPending,
            });
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    // Absence from a directory scan is not proof of absence: never delete a
    // request here. Exact orphan requests cannot affect another invocation.
    out
}

/// The live marker for `session_id`, if a run of it is in flight on this host.
pub fn live_marker(session_id: &str, data_root: Option<&Path>) -> Option<Live> {
    let id = crate::harness::SessionId::parse(session_id).ok()?;
    let session_id = id.as_str();
    let mut matching = read_live(data_root)
        .into_iter()
        .filter(|m| m.session_id == session_id);
    let first = matching.next()?;
    matching.next().is_none().then_some(first)
}

/// Held by `review message` from before it checks whether the session has a turn
/// in flight until its own run has launched - by which point that run's
/// in-flight marker exists (every runner writes it before spawning when the
/// session id is known). Released by dropping it.
///
/// Without it, two messages to one idle session both saw no turn in flight and
/// both launched a turn on the same session at once. Codex's session lock made
/// the second fail; claude has none, so both ran. And a message waiting behind
/// the global lock was invisible to every other verb: no marker until it
/// launched. With it, a second message waits for the first to launch, then
/// finds its marker and interrupts it, as `review message` promises.
pub struct LaunchLock {
    /// Held only so the lock lives as long as this value: closing it releases
    /// the `flock`.
    _file: std::fs::File,
}

/// How long a launch lock file may sit unused before `lock_session_launch`
/// removes it. "Unused" is judged by modification time, which taking an
/// `flock` does not update, so every open and every acquisition touches the
/// file; and a file whose lock is held right now is never removed, whatever
/// its age. Removing one in use would split the session's messages across two
/// files, each holding a lock on its own.
const LAUNCH_LOCK_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Path of `session_id`'s launch lock, beside its marker.
fn launch_lock_path(session_id: &str, data_root: Option<&Path>) -> Option<PathBuf> {
    if !is_safe_session_id(session_id) {
        return None;
    }
    Some(dir(data_root)?.join(format!("{session_id}.launch")))
}

/// Take `session_id`'s launch lock, waiting while another message to the same
/// session holds it. Blocking - call off the async runtime. `Ok(None)` when no
/// lock can be made (unsafe id, no data dir): the message then proceeds
/// unserialised, as it did before the lock existed, rather than failing.
pub fn lock_session_launch(
    session_id: &str,
    data_root: Option<&Path>,
) -> std::io::Result<Option<LaunchLock>> {
    use std::os::unix::io::AsRawFd;
    let Some(path) = launch_lock_path(session_id, data_root) else {
        return Ok(None);
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        remove_stale_launch_locks(parent, &path);
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    // Touched on open (so a waiter's file is recent) and again on acquiring;
    // see `LAUNCH_LOCK_MAX_AGE`. Best-effort: an untouched file is still a lock.
    let _ = file.set_modified(std::time::SystemTime::now());
    // SAFETY: flock on a descriptor we own; released when the file is closed.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        eprintln!("waiting for another message to session {session_id} to launch...");
        // SAFETY: as above.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let _ = file.set_modified(std::time::SystemTime::now());
    }
    Ok(Some(LaunchLock { _file: file }))
}

/// Whether the lock file at `path` is held by anyone right now.
fn launch_lock_held(path: &Path) -> bool {
    use std::os::unix::io::AsRawFd;
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    // SAFETY: flock on a descriptor we own. A lock taken here is released when
    // `file` drops at the end of this function.
    let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    !taken
}

/// Session ids whose launch lock is held right now: messages waiting to launch,
/// which have no in-flight marker yet. For `review sessions`.
pub fn pending_launches(data_root: Option<&Path>) -> Vec<String> {
    let Some(dir) = dir(data_root) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("launch"))
        .filter(|p| launch_lock_held(p))
        .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(String::from))
        .collect()
}

/// Whether a message to `session_id` holds its launch lock right now: one is
/// waiting to launch, so the session is about to have a turn in flight.
pub fn launch_pending(session_id: &str, data_root: Option<&Path>) -> bool {
    launch_lock_path(session_id, data_root).is_some_and(|path| launch_lock_held(&path))
}

/// Remove launch lock files nobody has used in `LAUNCH_LOCK_MAX_AGE`, so the
/// directory does not collect one per session ever messaged. Best-effort.
fn remove_stale_launch_locks(dir: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep || path.extension().and_then(|e| e.to_str()) != Some("launch") {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > LAUNCH_LOCK_MAX_AGE);
        if old && !launch_lock_held(&path) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Is `session_id`'s interrupt request still waiting for its run to consume it?
pub fn interrupt_pending(session_id: &str, data_root: Option<&Path>) -> bool {
    interrupt_path(session_id, data_root).is_some_and(|p| p.exists())
}

/// Path of `session_id`'s interrupt request, beside its marker.
fn interrupt_path(session_id: &str, data_root: Option<&Path>) -> Option<PathBuf> {
    let id = crate::harness::SessionId::parse(session_id).ok()?;
    interrupt_path_checked(&id, data_root)
}

fn interrupt_path_checked(
    session_id: &crate::harness::SessionId,
    data_root: Option<&Path>,
) -> Option<PathBuf> {
    interrupt_path_in(session_id, &crate::data::DataDir::from_override(data_root))
}

fn interrupt_path_in(
    session_id: &crate::harness::SessionId,
    data: &crate::data::DataDir,
) -> Option<PathBuf> {
    Some(
        data.inflight()?
            .join(format!("{}.interrupt", session_id.as_str())),
    )
}

/// Record that the operator asked for `session_id`'s run to be interrupted.
///
/// Written *before* the signal is sent, so the owning `review` - which checks
/// for it once codex has exited - can tell an interrupt it must honour from a
/// mid-turn death it should auto-resume past. The two look identical from the
/// exit status alone.
pub fn request_interrupt(session_id: &str, data_root: Option<&Path>) -> std::io::Result<()> {
    let path = interrupt_path(session_id, data_root).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session id is not a safe filename",
        )
    })?;
    // Its existence is the whole message.
    std::fs::write(path, "")
}

/// Consume `session_id`'s interrupt request: whether one was pending. Called by
/// the owning run after codex exits, so a request never outlives the run it was
/// aimed at and cannot mark a later resume of the same session as interrupted.
/// Also how a request is withdrawn, with the result ignored.
pub fn take_interrupt_request(session_id: &str, data_root: Option<&Path>) -> bool {
    interrupt_path(session_id, data_root).is_some_and(|p| std::fs::remove_file(p).is_ok())
}

pub(crate) fn take_interrupt_request_in(session_id: &str, data: &crate::data::DataDir) -> bool {
    let Ok(id) = crate::harness::SessionId::parse(session_id) else {
        return false;
    };
    interrupt_path_in(&id, data).is_some_and(|path| std::fs::remove_file(path).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(crate::ids::generate_uuid())
    }

    #[test]
    fn an_interrupt_request_is_consumed_exactly_once() {
        let root = scratch_root();
        std::fs::create_dir_all(root.join("review/inflight")).expect("scratch dir");
        let sid = "019fefb0-227c-7c83-a398-380011b8e66a";

        assert!(
            !take_interrupt_request(sid, Some(&root)),
            "nothing pending yet"
        );
        request_interrupt(sid, Some(&root)).expect("request");
        assert!(
            take_interrupt_request(sid, Some(&root)),
            "the request is seen"
        );
        assert!(
            !take_interrupt_request(sid, Some(&root)),
            "and consumed, so a later run of the session is not marked interrupted"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn per_run_markers_are_exclusive_and_independent() {
        let root = scratch_root();
        let data = crate::data::DataDir::at(root.clone());
        let sid = "same-session";
        let checked = crate::harness::SessionId::parse(sid).expect("id");
        let id = crate::sessions::handoff::RunId::default();
        let first = mark_for(
            &checked,
            crate::harness::Harness::Claude,
            "/p",
            &data,
            &id,
            None,
        );
        let collision = mark_for(
            &checked,
            crate::harness::Harness::Claude,
            "/p",
            &data,
            &id,
            None,
        );
        assert!(collision.collision());
        drop(collision);
        let second = mark(sid, crate::harness::Harness::Claude, "/p", Some(&root));
        assert_eq!(read_live(Some(&root)).len(), 2);
        drop(first);
        assert_eq!(read_live(Some(&root)).len(), 1);
        drop(second);
        assert!(read_live(Some(&root)).is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_held_launch_lock_is_seen_as_a_pending_message() {
        let root = scratch_root();
        let sid = "019fefb0-227c-7c83-a398-380011b8e66c";
        assert!(!launch_pending(sid, Some(&root)), "nothing waiting yet");
        let lock = lock_session_launch(sid, Some(&root))
            .expect("lock")
            .expect("a safe id gets a lock");
        assert!(
            launch_pending(sid, Some(&root)),
            "a held lock means a message is waiting to launch"
        );
        drop(lock);
        assert!(
            !launch_pending(sid, Some(&root)),
            "released once the message has launched"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cleanup_never_removes_a_held_launch_lock_however_old() {
        // flock does not touch mtime, so age alone cannot tell a held lock from
        // an abandoned one. Removing a held one would let a later message to
        // that session lock a fresh file and launch beside the holder.
        let root = scratch_root();
        let held_sid = "019fefb0-227c-7c83-a398-380011b8e66e";
        let held = lock_session_launch(held_sid, Some(&root))
            .expect("lock")
            .expect("lock");
        let held_path = launch_lock_path(held_sid, Some(&root)).expect("path");
        let two_days_ago = std::time::SystemTime::now() - 2 * LAUNCH_LOCK_MAX_AGE;
        std::fs::File::options()
            .write(true)
            .open(&held_path)
            .and_then(|f| f.set_modified(two_days_ago))
            .expect("age the file");
        // Another session's message runs the cleanup.
        drop(lock_session_launch(
            "019fefb0-227c-7c83-a398-380011b8e66f",
            Some(&root),
        ));
        assert!(held_path.exists(), "a held lock survives cleanup");
        // Released and old, it goes.
        drop(held);
        drop(lock_session_launch(
            "019fefb0-227c-7c83-a398-380011b8e670",
            Some(&root),
        ));
        assert!(!held_path.exists(), "an abandoned old lock is removed");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_second_launch_waits_for_the_first() {
        let root = scratch_root();
        let sid = "019fefb0-227c-7c83-a398-380011b8e66d";
        let first = lock_session_launch(sid, Some(&root))
            .expect("lock")
            .expect("lock");
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = {
            let root = root.clone();
            std::thread::spawn(move || {
                let second = lock_session_launch(sid, Some(&root)).expect("lock");
                tx.send(()).expect("send");
                drop(second);
            })
        };
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "the second message must not launch while the first holds the lock"
        );
        drop(first);
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("the second proceeds once the first has launched");
        waiter.join().expect("join");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_marker_whose_owner_is_gone_but_keeper_lives_is_cleanup_pending() {
        let root = scratch_root();
        let me = std::process::id();
        let start = crate::process::stat_ppid_start(me).map(|(_, s)| s);
        let write = |sid: &str, keeper: Option<u32>| {
            let marker = Marker {
                session_id: sid.to_string(),
                harness: "codex".to_string(),
                project: "/p".to_string(),
                started_epoch: 0,
                // Our own pid with a start time that is not ours: an owner
                // whose pid has been reused, so gone.
                owner: crate::process::ProcessIdentity::recorded(me, start.map(|s| s + 1))
                    .expect("owner"),
                signal_ready: false,
                protocol: crate::sessions::handoff::PROTOCOL,
                phase: Phase::Running,
                predecessor: None,
                run_id: sid.to_string(),
                keeper: keeper.and_then(|p| crate::process::ProcessIdentity::recorded(p, start)),
            };
            let path = dir(Some(&root)).expect("dir").join(format!("{sid}.json"));
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            write_marker(&path, &marker).expect("write");
            path
        };
        let pending = "019fefb0-227c-7c83-a398-380011b8e671";
        let stale = "019fefb0-227c-7c83-a398-380011b8e672";
        let pending_path = write(pending, Some(me));
        let stale_path = write(stale, None);
        let live = read_live(Some(&root));
        assert!(
            live.iter()
                .any(|m| m.session_id == pending && m.cleanup_pending()),
            "a live keeper keeps the marker, as cleanup pending"
        );
        assert!(pending_path.exists());
        assert!(
            !live.iter().any(|m| m.session_id == stale),
            "no owner and no keeper: stale"
        );
        assert!(!stale_path.exists(), "and removed");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_interrupt_request_refuses_an_unsafe_id() {
        assert!(request_interrupt("../escape", Some(Path::new("unused"))).is_err());
    }

    #[test]
    fn a_marker_from_before_child_pid_still_parses() {
        let old =
            r#"{"session_id":"s","provider":"codex","project":"/p","started_epoch":1,"pid":2}"#;
        let marker: Marker = serde_json::from_str(old).expect("old marker parses");
        assert!(!marker.signal_ready);
        assert_eq!(marker.protocol, 0);
    }

    #[test]
    fn accepts_harness_session_ids() {
        assert!(is_safe_session_id("019fefb0-227c-7c83-a398-380011b8e66a"));
        assert!(is_safe_session_id("abc_123-DEF"));
    }

    #[test]
    fn rejects_path_traversal() {
        // `review resume ../sessions` would otherwise write and then delete a file
        // outside the marker directory.
        assert!(!is_safe_session_id("../sessions"));
        assert!(!is_safe_session_id("../../etc/passwd"));
        assert!(!is_safe_session_id("a/b"));
        assert!(!is_safe_session_id("."));
        assert!(!is_safe_session_id(".."));
        assert!(!is_safe_session_id("with.dot"));
        assert!(!is_safe_session_id(""));
    }

    #[test]
    fn rejects_absurdly_long_ids() {
        assert!(!is_safe_session_id(&"a".repeat(129)));
    }
}

#[cfg(test)]
mod exact_run_tests {
    use super::*;
    #[test]
    fn requests_cannot_be_taken_by_another_run_or_swept_by_observation() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(crate::ids::generate_uuid());
        let data = crate::data::DataDir::at(root.clone());
        let sid = crate::harness::SessionId::parse("session").expect("id");
        let first = mark_for(
            &sid,
            crate::harness::Harness::Claude,
            "/p",
            &data,
            &crate::sessions::handoff::RunId::default(),
            None,
        );
        let second = mark_for(
            &sid,
            crate::harness::Harness::Claude,
            "/p",
            &data,
            &crate::sessions::handoff::RunId::default(),
            None,
        );
        let marker = &first.0.as_ref().expect("first marker").1;
        marker.request(&data).expect("request");
        assert!(!second.take_request(&data));
        assert!(first.take_request(&data));
        marker.request(&data).expect("late request");
        let orphan = marker.request_path(&data).expect("request path");
        let id: crate::sessions::handoff::RunId = marker.run_id.clone().try_into().expect("run id");
        drop(first);
        assert_eq!(read_live(Some(&root)).len(), 1);
        assert!(
            orphan.exists(),
            "scanning cannot prove an absent marker was not concurrently published"
        );
        assert!(
            reserve(&data, &id).is_err(),
            "orphan requests prevent namespace reuse"
        );
        assert!(!second.take_request(&data));
        drop(second);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn reservation_rejects_temporary_result_residue_and_legacy_readiness_is_not_a_second_pid() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(crate::ids::generate_uuid());
        let data = crate::data::DataDir::at(root.clone());
        let id = crate::sessions::handoff::RunId::default();
        let reservation = reserve(&data, &id).expect("reserve");
        assert!(reserve(&data, &id).is_err());
        drop(reservation);
        let tmp = data
            .inflight()
            .expect("inflight")
            .join(format!("{}.result.tmp", id.as_str()));
        std::fs::write(tmp, "partial").expect("partial publication");
        assert!(reserve(&data, &id).is_err());
        for (child, ready) in [(7, true), (8, false)] {
            let marker: Marker = serde_json::from_value(serde_json::json!({
                "session_id": "s", "provider": "claude", "project": "/p", "started_epoch": 1,
                "pid": 2, "keeper": 7, "keeper_start": 10, "child_pid": child,
            }))
            .expect("old marker");
            assert_eq!(marker.signal_ready, ready);
            assert_eq!(marker.keeper.expect("keeper").pid(), 7);
            assert_eq!(marker.protocol, 0);
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
