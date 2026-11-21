//! Running one provider invocation. `invoke` takes a `LaunchSpec`, prepares
//! the adapter (`prepare::Prepared`: permissions, writable roots, model
//! arguments, a caller-minted session id where the provider accepts one),
//! dispatches to that adapter's runner, and returns a `ProviderResult`.
//! Each runner (`claude_compat` for claude and maki, `codex`, `grok`) drives
//! the shared `lifecycle` and classifies what it observed into an `Outcome`.
//! Everything that depends on the provider is an exhaustive match on
//! `Provider` in this module tree.

mod artifacts;
mod claude_compat;
mod claude_stream;
pub(crate) mod codex;
mod grok;
mod kind;
mod launch;
mod lifecycle;
mod model;
mod outcome;
mod output;
mod permissions;
mod prepare;
mod result;
mod runtime;
mod session;
mod stream;

pub(crate) use artifacts::session_artifacts;
use claude_compat::{Dialect, run_claude_compat};
pub use kind::{Provider, UnknownProvider};
pub use launch::{InheritedSettings, LaunchSpec, ProfileOrigin, RunSettings};
use lifecycle::{Lease, LeaseUse, Run};
pub use lifecycle::{ProcessSpec, Stdin};
pub use model::ModelSpec;
pub use outcome::{Answer, EndedBy, Outcome, StatusFacts, Termination, Turn, WatchdogTermination};
pub use output::{print_result, resume_hint};
pub use permissions::Permissions;
pub use result::{
    CodexForensics, Digest, DigestSummary, ProviderForensics, ProviderResult, Served, Usage,
    now_epoch_secs,
};
pub use runtime::{ProviderCommand, ProviderCommands, ProviderRuntime};
pub use session::{SessionId, SessionStart};

use codex::run_codex;
use result::{RunOutput, answered_status_digest, result_served, status_digest};

#[cfg(test)]
mod tests;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;

/// Check whether a provider binary is available on PATH.
pub fn is_available(provider: Provider) -> bool {
    which::which(provider.binary()).is_ok()
}

/// The continuation to send when a fresh run of `r`'s provider ended in a way
/// that provider recovers from by resuming once, or `None`.
///
/// Whether a no-answer outcome is a recoverable death is the adapter's reading
/// of its own failure evidence, so it is asked here rather than decided by
/// orchestration from the provider's name. It must stay a pure reading of the
/// result. The vetoes every provider shares stay in `app::launch::after_run`:
/// an interrupted run and a stated failure are settled before this is asked;
/// an unclean teardown and a missing session id are checked after, because
/// they only withhold a resume this function asked for, and reporting a
/// withheld resume for a provider that never resumes would be false.
pub fn recovery(r: &ProviderResult) -> Option<&'static str> {
    match r.provider {
        Provider::Codex => codex::recovery(r),
        // A claude, maki or grok turn that ends without an answer says why, or
        // was killed; none has codex's mid-turn wedge for a resume to finish.
        Provider::Claude | Provider::Grok | Provider::Maki => None,
    }
}

/// Create a fresh, exclusively-owned temp file for codex's `-o` output and
/// return its path. A random unguessable name (UUID) plus `O_EXCL` creation
/// (`create_new`) makes collisions between concurrent runs impossible and
/// defeats symlink pre-planting on what used to be a predictable pid+archetype
/// path. The file is created empty; codex overwrites it with the final message.
fn new_output_file() -> Result<String> {
    new_temp_file("codex")
}

/// The same exclusive-creation dance for any provider temp file. `tag` only
/// distinguishes the files to a human reading the temp dir; the UUID is what
/// makes the name unique and unguessable.
fn new_temp_file(tag: &str) -> Result<String> {
    let dir = std::env::temp_dir();
    for _ in 0..8 {
        let path = dir.join(format!("review-{tag}-{}.txt", crate::ids::generate_uuid()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Ok(path.to_string_lossy().into_owned()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(anyhow::anyhow!("failed to create {tag} temp file: {e}"));
            }
        }
    }
    anyhow::bail!("could not create a unique {tag} temp file after 8 tries")
}

/// Caps on how much provider output we buffer in memory. Generous - real reviews
/// are far under these; the cap only stops a runaway stream from OOMing review.
const STDOUT_CAPTURE_CAP: usize = 64 << 20; // 64 MiB
const STDERR_CAPTURE_CAP: usize = 8 << 20; // 8 MiB

/// Root sets already announced this process, so an identical set is printed once
/// while a *different* one always prints.
///
/// A fan-out today runs one profile in one directory, so every invocation
/// derives the same set and a plain "have we announced?" flag would do. Keying
/// on the set instead costs nothing and removes the trap: the moment roots can
/// differ between invocations - per-archetype profiles being the obvious next
/// config change - a flag would leave the *wider* run unannounced while the
/// operator had seen only the narrower block, which is precisely the invisible
/// permission this announcement exists to prevent.
static ROOTS_ANNOUNCED: std::sync::Mutex<Option<std::collections::HashSet<Vec<String>>>> =
    std::sync::Mutex::new(None);

/// Whether this exact root set still needs announcing.
fn claim_roots_announcement(paths: &[String]) -> bool {
    match ROOTS_ANNOUNCED.lock() {
        Ok(mut guard) => guard
            .get_or_insert_with(Default::default)
            .insert(paths.to_vec()),
        // A poisoned lock means another thread panicked mid-announcement.
        // Announcing twice is noise; staying silent about a widening is not.
        Err(_) => true,
    }
}

/// Buffer shared between a reader task and the runner.
///
/// The reader appends as bytes arrive rather than returning at EOF, so the
/// runner can take whatever has been captured *so far* even if the reader never
/// finishes. That matters because the drain grace can expire with real output
/// already in hand - the NDJSON stream, and with it the log lines and the
/// forensic `stdout.jsonl` - and throwing it away would blind exactly the
/// failure this code exists to diagnose.
type SharedBuf = std::sync::Arc<std::sync::Mutex<Vec<u8>>>;

fn new_shared_buf() -> SharedBuf {
    std::sync::Arc::new(std::sync::Mutex::new(Vec::new()))
}

/// Append `chunk` to a shared buffer, respecting `cap`. The lock is taken and
/// released inside this call and never held across an await.
fn push_capped(buf: &SharedBuf, chunk: &[u8], cap: usize) {
    if let Ok(mut guard) = buf.lock()
        && guard.len() < cap
    {
        let take = (cap - guard.len()).min(chunk.len());
        guard.extend_from_slice(&chunk[..take]);
    }
}

/// Read `r` to EOF into `out`, buffering at most `cap` bytes but continuing to
/// drain the rest (so the child never blocks on a full pipe).
async fn read_capped<R>(r: R, cap: usize, out: SharedBuf)
where
    R: tokio::io::AsyncRead + Unpin,
{
    stream::read_stream(r, stream::Capture::head(out, cap), stream::Ignore).await;
}

/// Take a reader's output once the child is reaped, without waiting on it
/// forever.
///
/// Pipe EOF needs *every* holder of the write end to close it, so a reader can
/// outlive the child. We give it `grace` to finish, then abort it - an aborted
/// task stops reading, whereas simply dropping the `JoinHandle` would detach it
/// and leave it running - and take whatever it has buffered either way.
async fn collect_reader(
    mut handle: tokio::task::JoinHandle<()>,
    buf: &SharedBuf,
    grace: std::time::Duration,
    what: &str,
) -> Vec<u8> {
    if tokio::time::timeout(grace, &mut handle).await.is_err() {
        handle.abort();
        // Abort requests cancellation; joining also waits for any synchronous
        // chunk processing and its fact publication to finish before snapshot.
        let _ = handle.await;
        eprintln!(
            "warning: {what} was still open {}s after the process was reaped \
             (something inherited the pipe); using the {} bytes captured so far",
            grace.as_secs(),
            buf.lock().map(|b| b.len()).unwrap_or(0)
        );
    }
    buf.lock().map(|b| b.clone()).unwrap_or_default()
}

/// Removes a path on drop, so the codex `-o` temp file is cleaned up on every
/// exit path of the runner - including error returns before the normal read.
struct RemoveOnDrop(String);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Signals the caller that the provider process has actually been spawned.
///
/// The global lock exists to space out provider *launches*, so it has to be held
/// until the spawn has happened - releasing it beforehand leaves a critical
/// section that protects nothing, and lets every queued invocation through to
/// launch simultaneously. Sending on this (or dropping it, on a failed spawn)
/// is what tells the caller it is safe to release.
pub type LaunchSignal = tokio::sync::oneshot::Sender<()>;

pub async fn invoke(
    mut launch: LaunchSpec,
    launched: Option<LaunchSignal>,
    runtime: &ProviderRuntime,
    predecessor: Option<crate::sessions::handoff::RunId>,
) -> ProviderResult {
    let mut invocation_runtime = runtime.clone();
    invocation_runtime.predecessor = predecessor;
    invocation_runtime.run_id = crate::sessions::handoff::RunId::default();
    let reservation_error =
        match crate::inflight::reserve(&invocation_runtime.data, &invocation_runtime.run_id) {
            Ok(reservation) => {
                invocation_runtime.reservation = reservation.map(std::sync::Arc::new);
                None
            }
            Err(e) => {
                invocation_runtime.reservation = None;
                Some(anyhow::anyhow!("cannot reserve invocation markers: {e}"))
            }
        };
    let runtime = &invocation_runtime;
    let provider = launch.provider;
    let selection = &launch.settings.selection;
    let permissions = launch.settings.permissions.clone();
    let project_root = launch.root.as_path();
    let profile_roots = &launch.settings.writable_roots;
    let selection_check = selection.validate(provider);
    if permissions.provider() != provider || selection_check.is_err() || reservation_error.is_some()
    {
        return ProviderResult {
            run_id: reservation_error.is_none().then(|| runtime.run_id.clone()),
            _reservation: runtime.reservation.clone(),
            provider,
            output: Err(reservation_error
                .or_else(|| selection_check.err())
                .unwrap_or_else(|| {
                    anyhow::anyhow!(
                        "permission plan belongs to {}, not {provider}",
                        permissions.provider()
                    )
                })),
            session_id: None,
            codex_home: None,
            digest: None,
            completed_epoch: now_epoch_secs(),
            permissions: None,
            writable_roots: Vec::new(),
            model: None,
            effort: None,
            served: Served::default(),
            grok_trust: None,
            leftovers: crate::contained::Leftovers::default(),
        };
    }
    let mut plan = permissions::PermissionPlan::new(permissions, project_root, profile_roots);
    let setup = match launch.start.prepare(provider) {
        Err(e) => Err(e),
        Ok(()) => match provider {
            Provider::Claude => {
                claude_compat::prepare(&launch);
                Ok(prepare::Prepared::ClaudeCompat(Dialect::Claude))
            }
            Provider::Maki => {
                claude_compat::prepare(&launch);
                Ok(prepare::Prepared::ClaudeCompat(Dialect::Maki))
            }
            Provider::Codex => {
                codex::prepare(&launch, &mut plan);
                Ok(prepare::Prepared::Codex)
            }
            Provider::Grok => grok::prepare(&launch, &mut plan)
                .await
                .map(prepare::Prepared::Grok),
        },
    };
    let derives = plan.derives();
    let resumes_profile = plan.uses_recorded_roots();
    let root_paths = &plan.paths;
    let granted_roots = &plan.granted;
    // Announced once per distinct root set, not once per invocation: a fan-out
    // launches the same profile against several archetypes and would otherwise
    // repeat an identical block per run.
    //
    // What is announced is the *effective* set, not the derived one. Keying
    // either the guard or the loop on `granted_roots` broke the "every grant is
    // printed" fence three ways: a profile `config` override that replaced the
    // roots printed the derived paths instead of the ones the run actually got,
    // one that *added* roots to an empty derivation printed nothing at all, and
    // an override to `[]` printed grants the run did not receive. The whole
    // justification for deriving from ambient environment is that the result is
    // visible rather than inferred, so the line must follow the run.
    if !root_paths.is_empty() && claim_roots_announcement(root_paths) {
        let other = if resumes_profile {
            "recorded with the session"
        } else {
            "profile config override"
        };
        for line in announcement_lines(provider, other, granted_roots, root_paths) {
            eprintln!("{line}");
        }
    }
    // A workspace-write run that ends up with nothing outside the workspace is
    // usually a stripped environment rather than a workspace that needs nothing
    // (see `writable_roots::derive`, which has a legitimate zero case). It is
    // therefore a policy warning, not a correctness diagnostic - but silence
    // here is what let 40 consecutive runs record no roots unnoticed while every
    // build inside them died at a lock outside the sandbox.
    if derives && plan.lacks_build_roots() {
        eprintln!(
            "warning: workspace-write run with no writable roots outside the \
             workspace - a build needing one (the build lock, the cargo home, \
             or a shared target directory) will fail read-only. Declare it with \
             `writable_roots` on this profile if so."
        );
    }

    let mut prepared = None;
    let result = match setup {
        Err(e) => Err(e),
        Ok(mut state) => {
            // Taken here rather than earlier so preparation's own writes are
            // not attributed to the run. See `crate::git_watch`.
            // Filesystem reads and a `git config` child: kept off the runtime's
            // worker threads.
            let root = project_root.to_path_buf();
            let git = tokio::task::spawn_blocking(move || crate::git_watch::Snapshot::take(&root))
                .await
                .ok();
            let result = state.run(&launch, &plan, launched, runtime).await;
            match git {
                Some(git) => {
                    let changes = tokio::task::spawn_blocking(move || git.changes())
                        .await
                        .unwrap_or_else(|_| vec!["git metadata check failed".into()]);
                    report_git_changes(provider, &changes);
                }
                None => report_git_changes(provider, &["git metadata check failed".into()]),
            }
            prepared = Some(state);
            result
        }
    };
    let completed_epoch = now_epoch_secs();
    let mut result = match result {
        Ok(run) => ProviderResult {
            run_id: Some(runtime.run_id.clone()),
            _reservation: runtime.reservation.clone(),
            provider,
            output: Ok(Turn {
                text: run.text,
                outcome: run.outcome,
            }),
            session_id: run.session_id,
            codex_home: run.codex_home,
            digest: run.digest,
            completed_epoch,
            permissions: Some(plan.permissions.clone()),
            writable_roots: root_paths.clone(),
            model: selection.model().map(str::to_string),
            effort: selection.effort().map(str::to_string),
            served: run.served,
            grok_trust: None,
            leftovers: run.leftovers,
        },
        Err(e) => ProviderResult {
            run_id: Some(runtime.run_id.clone()),
            _reservation: runtime.reservation.clone(),
            provider,
            output: Err(e),
            // A requested resume ID identifies the audit attempt, not proof
            // that a turn ran. Recording keeps Err resumes out of the sidecar.
            // Caller-minted fresh IDs likewise do not prove provider storage.
            session_id: match &launch.start {
                SessionStart::Resume(id) => Some(id.as_str().to_owned()),
                SessionStart::Fresh { .. } => None,
            },
            codex_home: None,
            digest: None,
            completed_epoch,
            // A launch that failed still launched with these permissions
            // requested, and a row that omitted them would read as a run with
            // none.
            permissions: Some(plan.permissions.clone()),
            writable_roots: root_paths.clone(),
            model: selection.model().map(str::to_string),
            effort: selection.effort().map(str::to_string),
            served: Served::default(),
            grok_trust: None,
            // A runner that failed after its launch left its verdict in the
            // ledger (`contained::Contained`'s drop), which the exit status
            // reads; this row has no outcome to carry.
            leftovers: crate::contained::Leftovers::default(),
        },
    };
    if let Some(prepared) = prepared {
        prepared.record(&mut result);
    }
    result
}

/// Warn about git metadata a run changed. Advisory: names paths and the kind
/// of change, never contents, and a run with no warning is not thereby safe.
fn report_git_changes(provider: Provider, changes: &[String]) {
    if changes.is_empty() {
        return;
    }
    eprintln!(
        "warning: {provider}: git metadata that git may execute from changed while \
         this run was live, by it or by anything else, or could not be checked - \
         inspect before running git in this repository:"
    );
    for line in changes {
        eprintln!("  {line}");
    }
}

/// The `writable root` lines for a launch: one per **effective** root, carrying
/// the derivation's reason where the path came from the derivation and saying so
/// plainly where it did not.
///
/// Split out from `invoke` because the defect this replaced was invisible in
/// review: the announcement read from the derived set while the run used the
/// effective one, so the two could disagree with nothing failing. A pure
/// function over both sets is the only shape that can be tested.
fn announcement_lines(
    provider: Provider,
    other: &str,
    granted_roots: &[crate::writable_roots::GrantedRoot],
    root_paths: &[String],
) -> Vec<String> {
    root_paths
        .iter()
        .map(|path| {
            // A derived root can state why a build needs it; one that arrived
            // another way - a codex `config` override, or a grok profile root
            // another project added - cannot, and must not borrow a reason from
            // a derivation it did not come from.
            match granted_roots.iter().find(|g| &g.path == path) {
                Some(grant) => format!("{provider}: writable root {} ({})", grant.path, grant.why),
                None => format!("{provider}: writable root {path} ({other})"),
            }
        })
        .collect()
}

/// Write the prompt to stdin and close it. Run on the task `lifecycle::Run`
/// owns, so aborting that task cancels the write itself.
async fn write_stdin(
    mut stdin: tokio::process::ChildStdin,
    prompt_bytes: Vec<u8>,
) -> Result<(), anyhow::Error> {
    let result = stdin.write_all(&prompt_bytes).await;
    drop(stdin);
    result.map_err(|e| anyhow::anyhow!("failed to write prompt to stdin: {e}"))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("...");
    out
}

pub(crate) use output::print_digest_summary;
