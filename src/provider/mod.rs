mod claude;
mod codex;
mod grok;
mod output;
mod result;
mod runtime;

pub use claude::run_claude;
pub use codex::run_codex_json;
pub use output::{print_result, resume_hint};
pub use result::{Digest, DigestSummary, ProviderResult, RunOutput, Served, Usage, now_epoch_secs};
pub use runtime::{
    ProviderRuntime, Unrecorded, install_signal_supervisor, orphan_log_path, stop_requested,
};

use codex::run_codex;
use grok::run_grok;
use result::{
    answered_status_digest, no_answer_digest, result_served, review_no_answer_digest, signal_name,
};

#[cfg(test)]
mod tests;

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Check whether a provider binary is available on PATH.
pub fn is_available(provider: &str) -> bool {
    which::which(provider).is_ok()
}

/// Create a fresh, exclusively-owned temp file for codex's `-o` output and
/// return its path. A random unguessable name (UUID) plus `O_EXCL` creation
/// (`create_new`) makes collisions between concurrent runs impossible and
/// defeats symlink pre-planting on what used to be a predictable pid+archetype
/// path. The file is created empty; codex overwrites it with the final message.
pub fn new_output_file() -> Result<String> {
    new_temp_file("codex")
}

/// The same exclusive-creation dance for any provider temp file. `tag` only
/// distinguishes the files to a human reading the temp dir; the UUID is what
/// makes the name unique and unguessable.
fn new_temp_file(tag: &str) -> Result<String> {
    let dir = std::env::temp_dir();
    for _ in 0..8 {
        let path = dir.join(format!(
            "review-{tag}-{}.txt",
            crate::config::generate_uuid()
        ));
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
async fn read_capped<R>(mut r: R, cap: usize, out: SharedBuf)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut chunk = [0u8; 16384];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => push_capped(&out, &chunk[..n], cap),
        }
    }
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

/// The session associated with an invocation is the echoed/generated ID for a
/// fresh run, and the requested ID for a resume. Keeping it on resumed results
/// lets the shared printer name the session and print any interruption hint
/// without changing resume recording, which uses the caller's ID directly.
fn invocation_session_id(
    oneshot: bool,
    echoed: Option<String>,
    oneshot_id: Option<String>,
    requested_id: &str,
) -> Option<String> {
    if oneshot {
        echoed.or(oneshot_id)
    } else {
        Some(requested_id.to_string())
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

#[allow(clippy::too_many_arguments)]
pub async fn invoke(
    provider: &str,
    session_id: &str,
    model: Option<&str>,
    effort: Option<&str>,
    sandbox: Option<&str>,
    profile_roots: &[String],
    env: Option<&std::collections::BTreeMap<String, String>>,
    config: &[String],
    prompt: &str,
    project_root: &Path,
    oneshot: bool,
    launched: Option<LaunchSignal>,
    runtime: &ProviderRuntime,
) -> ProviderResult {
    // Profiles carry `review`'s own sandbox vocabulary; each provider spells the
    // levels differently and grok hard-errors on a name it cannot resolve, so
    // the translation happens once, here, and the runners below only ever see
    // their own native value.
    let sandbox = sandbox.map(|s| crate::config::sandbox_for(provider, s));
    let sandbox = sandbox.as_deref();
    let grok_write = if provider == "grok" {
        grok::grok_write_launch(oneshot, sandbox)
    } else {
        None
    };
    // Roots are *derived* for a codex write run and a fresh grok write run. A
    // resumed grok session under a `review-ws-*` profile instead takes the roots
    // recorded with it, verbatim: the profile name is fixed for the session's
    // life (grok refuses to switch it) and names exactly one root set, so
    // re-deriving on another host would describe a different profile.
    let derives = grok_write == Some(grok::GrokWrite::Fresh)
        || (provider == "codex" && sandbox == Some("workspace-write"));
    let resumes_profile = matches!(grok_write, Some(grok::GrokWrite::Resume(_)));

    // Derived here rather than inside the runners so the same values that
    // widen the run are the ones recorded on the result: a grant that is applied
    // but not logged is exactly the invisible permission this is meant to avoid.
    let granted_roots: Vec<crate::writable_roots::GrantedRoot> = if resumes_profile {
        Vec::new()
    } else if derives {
        // Against `project_root`, not the process cwd: every runner spawns
        // the provider with `.current_dir(project_root)`, and `.review.toml`
        // discovery deliberately allows launching from a descendant of it.
        // Deriving from the process cwd inspected `<subdir>/target` instead
        // of the workspace's, so a run started one directory down silently
        // failed to grant the root its build needed.
        crate::writable_roots::derive_with(
            project_root,
            &crate::writable_roots::RealHost,
            profile_roots,
        )
    } else {
        // Silence here would be indistinguishable from the paths having been
        // granted: an operator who put `writable_roots` on the wrong profile
        // gets a build that fails for reasons the config appears to rule out.
        if !profile_roots.is_empty() {
            eprintln!(
                "warning: profile writable_roots ignored - they apply only to \
                     a codex or grok workspace-write profile (this run: {} {})",
                provider,
                sandbox.unwrap_or("read-only")
            );
        }
        Vec::new()
    };
    let mut root_paths: Vec<String> = if resumes_profile {
        profile_roots.to_vec()
    } else {
        granted_roots.iter().map(|g| g.path.clone()).collect()
    };
    // Grok's profile name is a hash of the canonical set, and the recorded
    // roots must hash back to it on resume - so record them canonically too.
    if grok_write.is_some() {
        root_paths = grok::canonical_roots(&root_paths);
    }
    // A profile `config` entry restating the key wins, deliberately - but the
    // record must follow the run, not the derivation, or the sidecar makes a
    // confident false claim: a profile overriding to `["/secret"]` would be
    // logged with the derived roots, and one overriding to `[]` would be logged
    // as widened when it was not.
    // `config` is codex `-c` only; grok never sees it.
    if provider == "codex"
        && let Some(overridden) = config_writable_roots_override(config)
    {
        root_paths = overridden;
    }

    // A grok write run needs grok's own config ready before it launches, or it
    // fails in ways that cost a whole run to discover: trust (else the first
    // edit cancels the turn) and its roots profile (else every build dies at
    // the brokkr lock). A setup failure is a launch failure - running anyway
    // would reproduce those failures more expensively. Blocking file I/O and a
    // `flock` another `review` may hold, so off the async runtime.
    let mut trust_grant: Option<String> = None;
    let mut grok_profile: Option<String> = None;
    let setup: Result<()> = match grok_write.clone() {
        None => Ok(()),
        Some(kind) => {
            let root = project_root.to_path_buf();
            let roots = root_paths.clone();
            let env = env.cloned();
            let prepared = tokio::task::spawn_blocking(move || {
                grok::prepare_grok_write(&kind, &root, &roots, env.as_ref())
            })
            .await
            .map_err(|e| anyhow::anyhow!("grok setup task failed: {e}"))
            .and_then(|r| r);
            prepared.map(|p| {
                grok_profile = p.profile;
                trust_grant = p.trust_grant;
            })
        }
    };
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
    if !root_paths.is_empty() && claim_roots_announcement(&root_paths) {
        let other = if resumes_profile {
            "recorded with the session"
        } else {
            "profile config override"
        };
        for line in announcement_lines(provider, other, &granted_roots, &root_paths) {
            eprintln!("{line}");
        }
    }
    // A workspace-write run that ends up with nothing outside the workspace is
    // usually a stripped environment rather than a workspace that needs nothing
    // (see `writable_roots::derive`, which has a legitimate zero case). It is
    // therefore a policy warning, not a correctness diagnostic - but silence
    // here is what let 40 consecutive runs record no roots unnoticed while every
    // build inside them died at a lock outside the sandbox.
    if derives && root_paths.is_empty() {
        eprintln!(
            "warning: workspace-write run with no writable roots outside the \
             workspace - a build needing one (the build lock, the cargo home, \
             or a shared target directory) will fail read-only. Declare it with \
             `writable_roots` on this profile if so."
        );
    }

    let sandbox = grok_profile.as_deref().or(sandbox);

    let result = match (provider, setup) {
        (_, Err(e)) => Err(e),
        (provider, Ok(())) => match provider {
            // `sandbox` is an OS filesystem sandbox, which codex and grok both have
            // and claude does not: claude's `--permission-mode` is a tool-approval
            // policy on a different axis with no honest mapping, so claude ignores
            // it. `config` (codex `-c`) stays codex-only.
            "claude" => {
                run_claude(
                    session_id,
                    model,
                    effort,
                    env,
                    prompt,
                    project_root,
                    oneshot,
                    launched,
                    runtime,
                )
                .await
            }
            "grok" => {
                run_grok(
                    session_id,
                    model,
                    effort,
                    sandbox,
                    trust_grant.is_some(),
                    env,
                    prompt,
                    project_root,
                    oneshot,
                    launched,
                    runtime,
                )
                .await
            }
            "codex" => {
                run_codex(
                    session_id,
                    model,
                    effort,
                    sandbox,
                    &granted_roots,
                    env,
                    config,
                    prompt,
                    project_root,
                    oneshot,
                    launched,
                    runtime,
                )
                .await
            }
            other => Err(anyhow::anyhow!("unknown provider: {other}")),
        },
    };
    // Record a trust grant only if it actually landed. Grok reports a refused or
    // process-local grant only on stderr (surfaced by `run_grok`), and a
    // process-local one leaves the next run untrusted - so "we passed --trust"
    // is not "the folder is now permanently trusted", and the sidecar field
    // claims the latter. Re-read after the run; small file, no lock.
    if trust_grant.is_some() {
        let landed = grok::trust_landed(env, project_root);
        if !landed {
            eprintln!(
                "warning: grok --trust did not durably trust {} - the next write run \
                 there will try again",
                trust_grant.as_deref().unwrap_or_default()
            );
            trust_grant = None;
        }
    }
    let completed_epoch = now_epoch_secs();
    match result {
        Ok(run) => ProviderResult {
            provider: provider.to_string(),
            output: Ok(run.text),
            session_id: run.session_id,
            digest: run.digest,
            completed_epoch,
            sandbox: effective_sandbox(provider, sandbox),
            writable_roots: root_paths,
            model: model.map(str::to_string),
            effort: effort.map(str::to_string),
            served: run.served,
            grok_trust: trust_grant,
            leftovers: run.leftovers,
        },
        Err(e) => ProviderResult {
            provider: provider.to_string(),
            output: Err(e),
            session_id: None,
            digest: None,
            completed_epoch,
            // A launch that failed still launched with these permissions
            // requested, and a row that omitted them would read as a run with
            // none.
            sandbox: effective_sandbox(provider, sandbox),
            writable_roots: root_paths,
            model: model.map(str::to_string),
            effort: effort.map(str::to_string),
            served: Served::default(),
            // Requested, as `sandbox` is: if grok got as far as running, the
            // grant happened before whatever failed.
            grok_trust: trust_grant,
            // A runner that failed after its launch left its verdict in the
            // ledger (`contained::Contained`'s drop), which the exit status
            // reads; this row has no outcome to carry.
            leftovers: crate::contained::Leftovers::default(),
        },
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
    provider: &str,
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

/// The writable roots a profile `config` entry sets, when one does.
///
/// Profile overrides are passed after the derived `-c`, so codex takes the last
/// one and a profile restating this key replaces the derivation entirely. The
/// last entry wins here for the same reason. Returns `None` when no entry
/// touches the key, and `Some(vec![])` for an explicit empty override - which is
/// a real, recordable state, not the absence of one.
fn config_writable_roots_override(config: &[String]) -> Option<Vec<String>> {
    const KEY: &str = "sandbox_workspace_write.writable_roots";
    // Keep this override supported: the typed `writable_roots` field adds to
    // derived roots, while codex `-c` deliberately replaces the whole list.
    // Match the complete key before parsing its value. A neighboring setting
    // such as `sandbox_workspace_write.writable_roots_extra` must not hide an
    // earlier override of the actual roots key.
    let value = config.iter().rev().find_map(|entry| {
        let (key, value) = entry.split_once('=')?;
        (key.trim() == KEY).then_some(value.trim())
    })?;
    // Parse rather than string-scrape: the value is TOML, and a hand-written
    // profile can spell an array in ways a regex would get wrong. Codex falls
    // back to a string for values that are not TOML, but this setting requires
    // an array of paths; such a string is not an effective writable-roots
    // override.
    let parsed = format!("{KEY} = {value}").parse::<toml::Table>().ok()?;
    let roots = parsed
        .get("sandbox_workspace_write")?
        .get("writable_roots")?
        .as_array()?;
    Some(
        roots
            .iter()
            .filter_map(|v| v.as_str().map(ToString::to_string))
            .collect(),
    )
}

/// The sandbox level a run will actually be launched under, as opposed to the
/// one the caller asked for.
///
/// The two differ wherever the caller passes no level: the runners still pass
/// `--sandbox read-only`, so the run genuinely is read-only, and recording the
/// caller's `None` left such rows invisible to the exact query the sidecar
/// fields exist to answer, `jq 'select(.sandbox=="read-only")'`. On the
/// `review resume` path the caller is `app::message::inherited_settings`, which
/// supplies the level the session was created with; `None` there means the
/// session's record carries no level to inherit (a row written before levels
/// were recorded). Claude has no filesystem sandbox on this axis, so it records
/// none.
// Keep the native selector here for launch and resume. `sessions::record`
// normalizes the query field and saves this selector separately.
pub(crate) fn effective_sandbox(provider: &str, sandbox: Option<&str>) -> Option<String> {
    match provider {
        "codex" | "grok" => Some(sandbox.unwrap_or("read-only").to_string()),
        _ => None,
    }
}

/// Write prompt to stdin on a spawned task. Returns an error if the write fails.
async fn write_stdin(
    stdin: tokio::process::ChildStdin,
    prompt_bytes: Vec<u8>,
) -> Result<(), anyhow::Error> {
    let handle = tokio::spawn(async move {
        let mut stdin = stdin;
        let result = stdin.write_all(&prompt_bytes).await;
        drop(stdin);
        result
    });

    match handle.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(anyhow::anyhow!("failed to write prompt to stdin: {e}")),
        Err(e) => Err(anyhow::anyhow!("stdin write task panicked: {e}")),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("...");
    out
}

/// Normalize known Grok sandbox names for persisted queries, retaining custom
/// names verbatim. The native name must also be saved for exact session resume.
pub(crate) fn recorded_sandbox<'a>(provider: &str, sandbox: Option<&'a str>) -> Option<&'a str> {
    match (provider, sandbox) {
        ("grok", Some("workspace")) => Some("workspace-write"),
        ("grok", Some("none")) => Some("danger-full-access"),
        ("grok", Some(name)) if grok::is_generated_profile_name(name) => Some("workspace-write"),
        _ => sandbox,
    }
}
