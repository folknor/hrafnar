//! The codex adapter. A fresh run is `codex exec --json -o FILE` (the stream
//! carries `thread_id`); a resume is `codex exec resume ID --json -o FILE`.
//! The prompt goes on stdin. `rollout` decodes rollouts and the exec stream,
//! `watch` is the progress watchdog's codex source, `roots` renders writable
//! roots as a config override, and `forensics` renders codex evidence into
//! digests and incident bundles.
//!
//! Argv order is a rule: the fixed overrides (`--skip-git-repo-check`,
//! `approval_policy`, `--ignore-rules`, `--ignore-user-config`, the reasoning
//! summary, the sandbox selection, derived writable roots, workspace network)
//! come before the profile's raw `config` entries. Codex takes the last `-c`,
//! so a profile can restate any fixed override and win. The reason for each
//! flag is on the line that passes it.
//!
//! The final message is picked in priority order:
//!
//! 1. The `-o` file, which codex writes only for a real final message.
//! 2. An answer recovered from the rollout: the exact answer the watchdog
//!    killed on, else the current turn's phase-tagged `final_answer`.
//! 3. The last unphased stream message, as interim text only.
//!
//! With none of them, `Outcome::placeholder` supplies the wording and stderr
//! is appended as detail. The runner never fails on a non-zero exit once a
//! turn started, so the session ID and digest survive a dead run.
//!
//! Rollout reads (watchdog, recovery, incident tail) all use the one `Scope`
//! computed here from the rollout's length before spawn, so none of them can
//! attribute an earlier turn's bytes to this run.

pub(crate) mod forensics;
pub(crate) mod rollout;
mod roots;
pub(crate) mod watch;

use super::*;
use crate::transcript::{RolloutOffset, Scope};

/// Nudge sent when auto-resuming a codex run that died without a final answer.
const RESUME_NUDGE: &str = "The previous turn ended without a final answer. \
Continue from exactly where you left off and produce your complete final response now.";

/// Codex's recovery from its mid-turn death: a run that ended with no real
/// final answer (no `-o` capture and nothing recovered from the rollout) is
/// resumed once with a nudge, cache still warm, because the work was real and
/// a warm resume can finish it. A run that captured or recovered an answer is
/// not retried. `harness::recovery` applies the vetoes before this: a stated
/// failure (codex's `error` + `turn.failed` pair) is a verdict on the request,
/// so resending it costs a second run to hear the same refusal, and an
/// operator interrupt is a decision, not a death.
pub(super) fn recovery(r: &HarnessResult) -> Option<&'static str> {
    r.output
        .as_ref()
        .is_ok_and(|turn| !turn.outcome.has_answer())
        .then_some(RESUME_NUDGE)
}

// Readiness is a byte-level fact; session discovery is a bounded line-level fact.
pub(super) struct CodexObserver {
    pub(super) lease: Lease,
    pub(super) scanning: bool,
}

impl super::stream::Observer for CodexObserver {
    fn first_bytes(&mut self) {
        self.lease.ready();
    }
    fn line_cap(&self) -> Option<usize> {
        self.scanning.then_some(1 << 20)
    }
    fn line(&mut self, line: &[u8]) {
        if let Ok(val) = serde_json::from_slice::<serde_json::Value>(line)
            && val.get("type").and_then(|t| t.as_str()) == Some("thread.started")
            && let Some(id) = val.get("thread_id").and_then(|t| t.as_str())
        {
            self.lease.set_session(id);
            self.scanning = false;
        }
    }
}

/// Name of the codex permissions profile `hrafnar` synthesises for `read-only`.
///
/// Never read from the operator's config - `--ignore-user-config` drops that
/// file, and the profile is defined entirely by the `-c` overrides below - so
/// the name only has to avoid colliding with a built-in (those all start `:`).
const CODEX_READ_ONLY_PROFILE: &str = "hrafnar-read-only";

/// The codex arguments that select a sandbox level.
///
/// Every level but `read-only` is `--sandbox <level>`. `read-only` is not,
/// and the reason is worth the paragraph.
///
/// A `read-only` run needs sockets for the same reason a `workspace-write` one
/// does: the seccomp filter codex installs when network access is off denies
/// `bind`, `connect`, `listen`, `accept`, `getsockname`, `getsockopt`,
/// `setsockopt` and `sendto` outright, and permits `socket()` only for
/// AF_UNIX - so a unix socket can be created and then used for nothing. That
/// filter is not scoped to a sandbox level; it is installed whenever the
/// network policy is `Restricted`, which `read-only` always was.
///
/// There is no `sandbox_read_only.network_access` to turn it off.
/// `SandboxPolicy::ReadOnly` does carry a `network_access` field, but nothing
/// in the legacy `sandbox_mode` pipeline ever sets it: `derive_permission_
/// profile` maps `SandboxMode::ReadOnly` to `PermissionProfile::read_only()`,
/// whose network policy is hardcoded `Restricted`. Only the workspace-write
/// arm consults config, which is why the sibling override below exists and has
/// no read-only counterpart.
///
/// The one surface that *can* express "read-only filesystem, network enabled"
/// is codex's named-permissions pipeline - a `[permissions.<id>]` profile
/// extending the built-in `:read-only` with `network.enabled = true`. The
/// catch is that `resolve_permission_config_syntax` returns `Legacy` the
/// instant a `sandbox_mode` override is present, and `Legacy` makes
/// `[permissions]` profiles inert. So selecting the profile means **not**
/// passing `--sandbox` at all; the two mechanisms cannot be combined.
///
/// Giving up `--sandbox` on the path that matters most deserved evidence
/// rather than a reading of the source, so `scripts/readonly_network_check.py`
/// measures three configurations against one witness program. Verified on
/// codex-cli 0.151.0:
///
/// ```text
///                      --sandbox read-only   profile, restricted   profile, enabled
///   write_cwd          EROFS                 EROFS                 EROFS
///   read_etc_hostname  ok                    ok                    ok
///   unix_connect       EPERM                 EPERM                 ok
///   unix_sockopt       EPERM                 EPERM                 ok
///   inet_socket_create EPERM                 EPERM                 ok
///   inet_bind_listen   EPERM                 EPERM                 ok
///   tcp_connect        EPERM                 EPERM                 ok
/// ```
///
/// The middle column is the load-bearing one. It is the same profile with
/// network left restricted, and it reproduces `--sandbox read-only` exactly -
/// which is what establishes that dropping the flag does not widen the
/// filesystem, separately from the network change. `write_cwd` stays `EROFS`
/// in all three: the guarantee `.hrafnar.toml` advertises for `read-only` is
/// intact, and only the socket lines move.
///
/// Note the witnesses are `connect()` and socket options, not `bind()` on a
/// unix path. Under `read-only` nothing is writable, so binding an AF_UNIX
/// socket cannot succeed on filesystem grounds however the filter is set; what
/// a read-only run gains is the ability to *reach* something already
/// listening, plus AF_INET egress.
///
/// Enabling network is as blunt here as it is for `workspace-write` - the
/// filter is all-or-nothing and codex exposes no AF_UNIX-only knob on Linux
/// (`NetworkToml` has `unix_sockets` and `allow_local_binding`, but only
/// `seatbelt.rs` reads them; the Linux path is `landlock.rs`, where network
/// `Enabled` with no proxy means no filter is installed at all). Deliberately
/// *not* configuring a proxy also keeps us out of `NetworkSeccompMode::
/// ProxyRouted`, which would be worse than the status quo: it denies AF_UNIX
/// `socket()` entirely.
///
/// An older codex without the permissions pipeline fails soft rather than
/// silently wide: it ignores `default_permissions` and falls through to
/// `SandboxMode::default()`, which is `ReadOnly`.
fn codex_sandbox_args(level: &str) -> Vec<String> {
    if level != "read-only" {
        return vec!["--sandbox".to_string(), level.to_string()];
    }
    vec![
        "-c".to_string(),
        format!(r#"permissions.{CODEX_READ_ONLY_PROFILE}.extends=":read-only""#),
        "-c".to_string(),
        format!("permissions.{CODEX_READ_ONLY_PROFILE}.network.enabled=true"),
        "-c".to_string(),
        format!(r#"default_permissions="{CODEX_READ_ONLY_PROFILE}""#),
    ]
}

pub(super) async fn run_codex(
    launch: &super::LaunchSpec,
    sandbox: &str,
    root_paths: &[String],
    launched: Option<LaunchSignal>,
    runtime: &HarnessRuntime,
) -> Result<RunOutput> {
    let session = &launch.start;
    let selection = &launch.settings.selection;

    selection.validate(Harness::Codex)?;
    session.validate(Harness::Codex)?;
    // The helper returns the exclusively-created path needed in argv. Cleanup
    // transfers to run_codex_json, which handles its pre-guard stop and guards
    // every fallible launch path after that.
    let last_msg_path = new_output_file()?;

    // Exec-level options shared by both paths.
    let mut args: Vec<String> = vec![
        "exec".to_string(),
        // `hrafnar` is routinely launched from a directory that is not itself a
        // git repo (an umbrella dir whose repos live in subdirectories). Codex
        // refuses to start there ("Not inside a trusted directory and
        // --skip-git-repo-check was not specified"), so pass it always.
        // Marking the directory `trust_level = "trusted"` does not help
        // (verified on 0.149.0): the repo check runs ahead of project trust,
        // so there is no config-side fix. The filesystem guarantee is the
        // sandbox selection, which this flag does not touch: it only waives
        // codex's own "am I in a repo?" precondition.
        "--skip-git-repo-check".to_string(),
        // `--sandbox` alone is not a filesystem guarantee (codex 0.150.x and
        // later). `codex exec` sets `approval_policy = "never"` for headless
        // runs, which is what makes a read-only sandbox absolute: the model's
        // `request_permissions` escalation is auto-denied under `never`. But
        // `build_exec_config` drops that headless default whenever the
        // resolved `approvals_reviewer` is `auto_review` (settable globally in
        // `~/.codex/config.toml`), falling back to `on-request`. Escalations
        // then go to codex's own auto-approving guardian, which grants them,
        // and additional write permissions are layered onto the read-only
        // profile for the turn. Verified on 0.150.1: a bare
        // `codex exec --sandbox read-only` wrote a file; with this override the
        // same prompt is rejected ("writing is blocked by read-only sandbox").
        // The `-c` survives because `build_exec_config` rewrites only the
        // harness overrides, never the CLI ones. Pinning the policy keeps the
        // invariant independent of the operator's global codex config.
        "-c".to_string(),
        "approval_policy=\"never\"".to_string(),
        // The second, independent hole in the same guarantee, and the one that
        // fires in the field. Codex's execpolicy `.rules` files are not
        // only an approval allowlist: a `prefix_rule(..., decision="allow")`
        // whose pattern matches *every* parsed segment of a command makes
        // `exec_policy.rs` return `Skip { bypass_sandbox: true }`, which the
        // orchestrator resolves to `SandboxType::None`. On Linux the sandbox is
        // purely an argv wrapper, so that is not a weakened sandbox - it is no
        // sandbox at all. `approval_policy="never"` does not gate this branch,
        // and `read-only` is exposed exactly as much as `workspace-write`
        // (`unsandboxed_execution_allowed` only tests for deny-*read* entries,
        // which neither level has). Verified on 0.151.0 against an operator
        // `~/.codex/rules/default.rules` carrying `["brokkr","status"]` and
        // `["ln","-s"]`: under `--sandbox read-only`, `ln -s /etc/hostname x`
        // created the link, and `brokkr status` wrote a row into
        // `~/.local/share/brokkr/history.db` - both outside every writable root.
        // Prefixing one non-allowlisted segment (`echo probe; ...`) made the
        // identical command sandboxed, so enforcement tracks the shape of the
        // command the model happened to type and reads as flaky. Upstream
        // documents this ("control which commands Codex can run outside the
        // sandbox"): codex works as intended, it is hrafnar's guarantee that
        // breaks. `--ignore-rules` drops the user and project rule layers for
        // this invocation only, leaving interactive codex untouched; the
        // root-owned `/etc/codex/rules/` layer still loads, correctly.
        // Redirecting `CODEX_HOME` would also kill the bypass but would move
        // auth, sessions and rollout discovery with it. An older codex
        // without the flag fails loudly on an unknown argument rather than
        // silently running unsandboxed.
        "--ignore-rules".to_string(),
        // The general form of the same rule: a `hrafnar` run is a function of
        // `.hrafnar.toml` plus argv, never of a file the operator edited for
        // unrelated reasons. Both holes above are ambient global codex config
        // voiding a profile's advertised guarantee, and pinning each hole as
        // it is found leaves the next one live, so drop
        // `$CODEX_HOME/config.toml` wholesale. Auth is unaffected: it resolves
        // from `CODEX_HOME` independently of this flag (verified: a run with
        // it authenticates and completes). It does not replace
        // `--ignore-rules`: rules live outside config.toml, and measured, this
        // flag alone leaves the bypass open. Two deliberate consequences: a
        // profile with no `model` takes codex's built-in default rather than
        // the operator's, and a profile naming a custom `model_provider` must
        // define it in its own `config`.
        "--ignore-user-config".to_string(),
        // Rollout reasoning items carry `encrypted_content` - sealed server-side
        // and undecryptable here - and on the default `auto` summary setting
        // their `summary` arrays come back empty, so a rollout records *what* a
        // run did and never *why*, and intent has to be inferred from the
        // command sequence. `detailed` fills the summaries, which `src/transcript.rs` forensics
        // and `hrafnar sessions <id>` can then surface. Passed before profile
        // `config` overrides, so a profile can still set `none` where the
        // summaries are not worth their tokens.
        "-c".to_string(),
        "model_reasoning_summary=\"detailed\"".to_string(),
    ];
    args.extend(codex_sandbox_args(sandbox));
    args.extend_from_slice(selection.args());
    // Writable roots the host needs for a build, derived by the caller (see
    // `invoke`, and `src/writable_roots/` for why they are derived at all).
    // Placed before profile `config` so a profile can restate
    // `sandbox_workspace_write.writable_roots` and win.
    if let Some(override_arg) = roots::config_override_paths(root_paths) {
        args.push("-c".to_string());
        args.push(override_arg);
    }
    // A `workspace-write` run gets network access, because without it codex
    // cannot use **unix domain sockets** - which has nothing to do with the
    // network and everything to do with how the filter is written.
    //
    // The seccomp filter codex installs when network access is off
    // (`linux-sandbox/src/landlock.rs`) unconditionally denies `bind`,
    // `connect`, `listen` and `accept`, returning `EPERM`. It carves out
    // AF_UNIX for `socket()` and `socketpair()` only, so an AF_UNIX socket can
    // be created and then used for nothing that needs a pathname. A daemon or
    // worker test suite that binds a socket under its own target directory
    // fails with `Operation not permitted` on a path that is demonstrably
    // writable: verified by binding in the cwd, which codex grants
    // unconditionally, and getting the same errno. So no `writable_roots`
    // entry can fix it; there is no path for which it works. `socketpair`
    // has the carve-out, so socketpair-only tests pass, which makes the
    // failure look flaky.
    //
    // Enabling network access is blunt: the filter is all-or-nothing, so this
    // restores AF_INET egress too, and codex offers no AF_UNIX-only knob. It
    // matches what an operator execpolicy allowlist gives a matching command
    // without `--ignore-rules` (no wrapper, so no filter), applied to every
    // command instead of the ones whose argv matched a rule. The upstream
    // asymmetry looks like an oversight: `landlock.rs` keeps `recvfrom`
    // allowed so `cargo clippy`'s socketpair machinery works, and misses the
    // four pathname syscalls.
    //
    // Scoped to `workspace-write` because this key only exists for that level.
    // `read-only` needs the same grant (the filter denies `connect` there too)
    // and gets it from the permissions profile in `codex_sandbox_args`, there
    // being no `sandbox_read_only.network_access`.
    if sandbox == "workspace-write" {
        args.push("-c".to_string());
        args.push("sandbox_workspace_write.network_access=true".to_string());
    }

    // Keep raw config after generated settings; ModelSpec owns its selector
    // precedence and the values recorded for restoration.
    args.extend_from_slice(selection.config_args());

    // A fresh run captures the new session id from the stream; the `resume`
    // subcommand carries the id we already know. Both stream `--json` and take
    // `-o` (placed at the subcommand level for resume, exec level for fresh).
    let known_session_id = if session.is_fresh() {
        args.push("--json".to_string());
        args.push("-o".to_string());
        args.push(last_msg_path.clone());
        None
    } else {
        args.push("resume".to_string());
        let session_id = session.id().expect("resume identity").as_str();
        args.push(session_id.to_string());
        args.push("--json".to_string());
        args.push("-o".to_string());
        args.push(last_msg_path.clone());
        session.id().cloned()
    };

    run_codex_json(
        launch,
        args,
        &last_msg_path,
        known_session_id,
        launched,
        runtime,
    )
    .await
}

/// Shared codex runner. `args` must include `--json` and `-o <last_msg_path>`.
/// Pipes the prompt on stdin and distills a `Digest` (usage, turns, captured,
/// exit/signal, log lines, transcript) from the NDJSON stream plus the `-o`
/// backstop. `known_session_id` is `Some` on resume (we already know it) and
/// `None` on a fresh run (parsed from `thread.started`). Never bails on a
/// non-zero exit: a halted/errored run still reports what it produced.
async fn run_codex_json(
    launch: &super::LaunchSpec,
    args: Vec<String>,
    last_msg_path: &str,
    known_session_id: Option<SessionId>,
    launched: Option<LaunchSignal>,
    runtime: &HarnessRuntime,
) -> Result<RunOutput> {
    let env = launch.settings.env.as_ref();
    let prompt = launch.prompt.as_str();
    let project_root = launch.root.as_path();
    let command = runtime.commands.get(Harness::Codex);
    // Launched under a keeper (`contained::spawn`), which puts codex in a
    // process group of its own, out of reach of a terminal SIGINT - the signal
    // supervisor stops runs instead - and tears down everything codex started
    // (exec-server, unified-exec cells, sandboxed shells in sessions of their
    // own, MCP servers) when the run ends, however it ends.
    let spec = ProcessSpec::new(
        command,
        args,
        project_root,
        // Make a codex panic legible: without this it exits 1 with no trace,
        // and the rollout records no reason for the death either. A
        // default, so an explicit profile override still wins; and part of the
        // spec, so the incident replay carries it by construction.
        &[("RUST_BACKTRACE", "1".to_string())],
        env,
        &[],
        Stdin::Pipe(prompt.as_bytes().to_vec()),
    );
    // Size of the rollout transcript *before* this run appends to it. A resume
    // writes into the same file as every earlier turn, so the watchdog needs to
    // know where this run's events begin - otherwise a previous turn's
    // `final_answer` would look like proof that this run had finished, and a
    // quiet mid-turn wedge would be killed as a "stranded completion".
    //
    // A byte offset rather than a timestamp on purpose: rollout timestamps carry
    // milliseconds but our own clock string is second-resolution, so a turn that
    // completed in the same second as this run's launch would slip through a
    // time-based filter. The offset has no such race - it is exact. Zero for a
    // fresh run, whose rollout does not exist yet.
    // An unknown baseline becomes Scope::Untrusted, which disables both the watchdog
    // and transcript recovery for this run. It is emphatically not the same as
    // Scope::From(0): falling back to zero for a *resume* whose rollout could not be
    // read would scope the scan to the entire session history, and an old
    // `final_answer` from a previous turn could then authorise killing a genuine
    // mid-turn wedge, or be recovered as this run's answer.
    // Resolve once so baseline, watchdog and recovery search the same home.
    let codex_home = crate::transcript::codex_home(
        env.and_then(|e| e.get("CODEX_HOME")).map(String::as_str),
        env.and_then(|e| e.get("HOME")).map(String::as_str),
        project_root,
    );
    let mut baseline_problem = None;
    let rollout_baseline: Option<u64> = match (codex_home.as_deref(), known_session_id.as_ref()) {
        (None, _) => {
            baseline_problem = Some("Codex home is unknown".to_string());
            None
        }
        // Fresh run: a new session id, so its rollout does not exist yet and
        // every byte in it will be ours. Zero is exact here, not a fallback.
        (Some(_), None) => Some(0),
        // Resume: the rollout already holds earlier turns, so we must know
        // exactly where they end. If we cannot stat it, we do not guess.
        (Some(home), Some(sid)) => {
            match crate::transcript::find_transcript_path_checked_in_home(sid, home) {
                crate::transcript::Lookup::Found(path) => match std::fs::metadata(path) {
                    Ok(metadata) => Some(metadata.len()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        baseline_problem =
                            Some("the rollout disappeared before it could be read".to_string());
                        None
                    }
                    Err(error) => {
                        baseline_problem = Some(format!("the rollout could not be read: {error}"));
                        None
                    }
                },
                crate::transcript::Lookup::Absent => {
                    baseline_problem = Some("the rollout was not found".to_string());
                    None
                }
                crate::transcript::Lookup::Unreadable(error) => {
                    baseline_problem = Some(format!("the rollout search was unreadable: {error}"));
                    None
                }
                crate::transcript::Lookup::UnknownHome => {
                    baseline_problem = Some("Codex home is unknown".to_string());
                    None
                }
            }
        }
    };
    let rollout_scope = rollout_baseline.map_or(Scope::Untrusted, |offset| {
        Scope::From(RolloutOffset(offset))
    });
    if let Some(problem) = baseline_problem {
        eprintln!(
            "warning: {problem} for session {}; completion watchdog and transcript recovery \
             are disabled for this run",
            known_session_id
                .as_ref()
                .map(SessionId::as_str)
                .unwrap_or("?")
        );
    }

    let start = std::time::Instant::now();
    // Installed before anything can return, so the `-o` file goes on every
    // path, the lifecycle's stop refusal included.
    let _output_cleanup = RemoveOnDrop(last_msg_path.to_string());
    // A resume knows its session id, so its in-flight marker goes up before the
    // spawn: `hrafnar message` releases its session launch lock when the launch
    // signal fires, and a second message must then find this marker to
    // interrupt the run rather than launch beside it. A fresh run's marker goes
    // up when `thread.started` reaches the reader (`Lease::set_session`);
    // nobody can message a session whose id has not been printed. The marker
    // shows `hrafnar sessions` the turn in flight instead of the previous
    // turn's response, so a hung run does not look like an idle session.
    //
    // The keeper's pid is offered on the marker only from codex's first stdout
    // byte. On an npm install the pid is a node wrapper (`codex-cli/bin/
    // codex.js`) that forwards one SIGINT to the native binary, but installs
    // that forwarding only after spawning it. A SIGINT before then kills the
    // wrapper and orphans codex, still holding the session's writer lock
    // (`$CODEX_HOME/thread-writer-locks/ID.lock`), so the resume `hrafnar
    // interrupt` hands back would fail "already has an active writer". The
    // interrupt signals the leader only, never the group: the wrapper already
    // forwards, and a group signal would hand the native binary a second copy.
    let session = match known_session_id.as_ref() {
        Some(sid) => SessionStart::Resume(sid.clone()),
        None => SessionStart::Fresh { id: None },
    };
    let owner = match session.id() {
        Some(sid) => Lease::known(
            sid,
            Harness::Codex,
            project_root,
            &runtime.data,
            &runtime.run_id,
            runtime.reservation.clone(),
            runtime.predecessor.as_ref(),
        ),
        None => Lease::learned(
            Harness::Codex,
            project_root,
            &runtime.data,
            &runtime.run_id,
            runtime.reservation.clone(),
            runtime.predecessor.as_ref(),
        ),
    };
    let lease = Lease::clone(&owner);
    let scanning_for_session_id = known_session_id.is_none();
    let (mut run, pipes) = Run::spawn(
        Harness::Codex,
        &spec,
        &session,
        LeaseUse::Owned(owner),
        runtime,
    )?;

    // Bound in-memory buffering: a runaway/noisy harness could otherwise OOM
    // `hrafnar` long before the 1 MiB forensic tail is applied. Buffer up to a
    // generous cap and keep draining past it so codex never blocks on a full
    // pipe. Real reviews are far under the cap.
    //
    // These are spawned rather than joined inline because the wait below has to
    // be interruptible, and because pipe EOF is *not* a reliable completion
    // signal - see the reaping comment further down.
    let stdout_shared = new_shared_buf();
    let stderr_shared = new_shared_buf();
    let stdout_task = tokio::spawn(super::stream::read_stream(
        pipes.stdout,
        super::stream::Capture::head(std::sync::Arc::clone(&stdout_shared), STDOUT_CAPTURE_CAP),
        CodexObserver {
            lease: lease.clone(),
            scanning: scanning_for_session_id,
        },
    ));
    let stderr_task = tokio::spawn(read_capped(
        pipes.stderr,
        STDERR_CAPTURE_CAP,
        std::sync::Arc::clone(&stderr_shared),
    ));
    run.track(&stdout_task);
    run.track(&stderr_task);
    run.launch(launched).await?;

    // Records that *we* ended the run, and why. See `Outcome::ended`.
    let mut terminated_by_hrafnar: Option<crate::harness::Termination> = None;
    // Forensics from the watchdog verdict, carried onto the digest and into the
    // incident bundle.
    let mut watchdog_answer: Option<String> = None;
    let mut quiet_secs: Option<u64> = None;
    let mut last_rollout_event: Option<crate::transcript::EventKind> = None;

    // Wait for codex, but stay interruptible: the wait ends on reap, and the
    // watchdog can pre-empt it. Everything `hrafnar` knows about a run (digest,
    // incident bundle, sidecar row) is computed below this point, so a wait
    // that blocks on a codex that finished its turn and never exits (observed
    // on 0.147.0: answer on disk, 10+ hours hung) produces nothing at all.
    // On a stranded completion the run is terminated, reaped, and the
    // recovered `final_answer` reported as usual. A signalled `hrafnar` ends
    // the turn as `hrafnar interrupt` would, through the lifecycle's stop arm.
    // The watchdog judges codex only, never the teardown of what it left.
    let watchdog = {
        let watchdog = crate::watchdog::wait_for_stranded_completion(
            lease.session_watch(),
            codex_home.clone(),
            rollout_scope,
            runtime.timings,
        );
        async move {
            let verdict = watchdog.await;
            eprintln!("watchdog: {}", verdict.reason);
            verdict
        }
    };
    if let Some(verdict) = run.wait(watchdog).await {
        terminated_by_hrafnar = Some(verdict.termination());
        watchdog_answer = verdict.final_answer;
        quiet_secs = Some(verdict.quiet_secs);
        last_rollout_event = verdict.last_event.clone();
    }
    // The keeper was waited for before the marker goes, so the session read
    // as in flight until nothing of the run was left. Whether the consumed
    // interrupt request *counts* waits until we know if an answer was produced.
    let ended = run.finish().await;
    let interrupt_requested = ended.interrupt_requested;
    let stopped_by_signal = ended.stopped_by_signal;
    let mut leftovers = ended.leftovers;
    // `None` when the keeper could not report it: the digest then shows no
    // exit code, the run reads as suspicious (bundled, transcript read), and
    // its session and any answer survive - the ledger already fails the
    // command for the unknown teardown.
    let status = ended.status;

    // The child is reaped, so the run is over regardless of what the pipes do.
    //
    // Waiting on pipe EOF here would reintroduce a hang of its own: EOF needs
    // *every* holder of the write end to close it, which includes any process
    // codex spawned that inherited the descriptors. Reaping is the authoritative
    // signal; give the readers a moment to drain what is already buffered, then
    // take what they have. (Codex 0.147.0 has no production `Stdio::inherit`
    // on the exec path: `codex-rs/core/src/spawn.rs` gives children null or
    // piped stdio, so this is defence in depth rather than a known live path.)
    let stdout_buf = collect_reader(
        stdout_task,
        &stdout_shared,
        runtime.drain_grace,
        "codex stdout",
    )
    .await;
    let stderr_buf = collect_reader(
        stderr_task,
        &stderr_shared,
        runtime.drain_grace,
        "codex stderr",
    )
    .await;
    let write_res = ended.stdin;
    // Read after the drain: a `thread.started` the reader saw late is still
    // this run's id, though it no longer puts a marker up.
    let watched_session_id = lease.session();
    let duration_ms = start.elapsed().as_millis();
    // A failed prompt write (e.g. broken pipe because codex exited first) is a
    // forensic signal, not a fatal error - the digest still reports what ran.
    let stdin_write_error = write_res.err().map(|e| e.to_string());

    // Parse the NDJSON stream: session id, streamed final message, turn count,
    // summed usage, and any non-JSON log lines. We do NOT bail on a non-zero
    // exit here - the whole point of the digest is that a halted or errored run
    // still yields whatever it produced, with the exit status recorded.
    let stdout = String::from_utf8_lossy(&stdout_buf);
    let rollout::Stream {
        session_id: parsed_session_id,
        interim_message: stream_message,
        turns,
        usage,
        log_lines,
        turn_error,
    } = rollout::stream(&stdout);

    // The -o file is written via a path separate from the NDJSON stream, only
    // on a real final message. A non-empty file is thus the authoritative
    // completed signal - it survives a frozen stream. Its absence means the run
    // ended without a final report (crashed, killed, or yielded out).
    let final_from_file = match tokio::fs::read_to_string(&last_msg_path).await {
        Ok(s) => {
            let s = s.trim().to_string();
            if s.is_empty() { None } else { Some(s) }
        }
        Err(_) => None,
    };
    // (the `-o` temp file is removed by `_output_cleanup` on scope exit)

    let captured = final_from_file.is_some();

    // On resume we already know the session id; on a fresh run it comes from
    // the stream. `watched_session_id` is the same id as published live by the
    // stdout reader, and is the fallback for when that reader was aborted before
    // EOF: `parsed_session_id` comes from re-parsing `stdout_buf`, which a
    // truncated capture can leave without the `thread.started` line. Losing the
    // id would cost us the transcript, the recovery path and the sidecar row.
    let session_id = known_session_id
        .map(|id| id.as_str().to_owned())
        .or(parsed_session_id)
        .or(watched_session_id);

    // Only pay for transcript forensics when the run looks wrong; a clean
    // captured run needs no post-mortem. Look under the run's effective
    // CODEX_HOME (a profile env override) so a custom codex home is searched.
    // Read before choosing the final message: codex routinely finishes the turn
    // on disk (task_complete) yet exits non-zero and truncates both the `--json`
    // stream and the `-o` file, so the rollout is the authoritative record.
    let suspicious = !captured || !status.success();
    let transcript = if suspicious {
        // Scoped to the bytes this run appended (see `rollout_baseline`), so a
        // previous turn's `final_answer` can never be recovered as ours. When
        // the baseline is untrustworthy this yields nothing rather than the
        // whole history, which is the safe direction: no recovery beats a wrong
        // answer that also suppresses auto-resume.
        match (session_id.as_deref(), codex_home.as_deref()) {
            (Some(sid), Some(home)) => {
                match crate::transcript::summarize_session_in_home_lookup(sid, home, rollout_scope)
                {
                    crate::transcript::Lookup::Found(summary) => Some(summary),
                    crate::transcript::Lookup::Absent => {
                        eprintln!(
                            "warning: rollout for codex session {sid} was not found during recovery"
                        );
                        None
                    }
                    crate::transcript::Lookup::Unreadable(error) => {
                        eprintln!(
                            "warning: rollout for codex session {sid} could not be read during recovery: {error}"
                        );
                        None
                    }
                    crate::transcript::Lookup::UnknownHome => {
                        eprintln!("warning: Codex home is unknown during transcript recovery");
                        None
                    }
                }
            }
            (Some(sid), None) => {
                eprintln!(
                    "warning: Codex home is unknown for session {sid}; transcript recovery is disabled"
                );
                None
            }
            _ => None,
        }
    } else {
        None
    };

    // Final-message priority is in the module doc. Recovery keys on the
    // rollout's phase-tagged `final_answer`, never on `task_complete`: codex
    // fires `task_complete` on aborted turns too, so it cannot tell a
    // truncated-but-completed run (answer on disk, exit 1 at shutdown, closing
    // stream events and `-o` lost) from a genuine mid-turn death. Both look
    // like `exit 1 / captured false / turns 0` on stdout.
    // A trailing turn can reset the summary's answer. Preserve the exact
    // scoped evidence that licensed our kill even if the rollout later changes.
    // Final rollout bytes alone cannot reconstruct this decision: detection's
    // saved answer is a separate temporal input. Pure fold/outcome tests cover
    // their own rules; invoke fixtures exercise this live composition.
    let recovered =
        watchdog_answer.or_else(|| transcript.as_ref().and_then(|t| t.final_answer.clone()));
    // The bundle's answer flag must agree with recovery: a trailing phantom
    // turn resets the summary's `final_answer`, but the watchdog's scoped
    // answer still proves this run produced one.
    let answer_in_scope = recovered.is_some();
    let recovered_from_transcript = final_from_file.is_none() && recovered.is_some();
    let final_message = final_from_file.or(recovered).or(stream_message);

    let answer = if captured {
        Answer::Captured
    } else if recovered_from_transcript {
        Answer::Recovered
    } else {
        Answer::None
    };
    let mut outcome = Outcome::answered(answer);
    outcome.stated = turn_error;
    let outcome = outcome.ended(
        interrupt_requested,
        stopped_by_signal,
        terminated_by_hrafnar,
    );
    let interrupted = outcome.interrupted();

    // Whether to bundle is this adapter's policy over its own evidence, not a
    // property of the outcome: a suspicious codex run is bundled even when it
    // answered (recovered, or captured with a bad exit), while a claude or grok
    // answer with a bad exit is not. So there is deliberately no
    // `is_incident` on `Outcome`; only the interrupt exclusion is shared.
    // Dump a full forensic bundle for the same suspicious runs we post-mortem -
    // stderr (with backtraces), the raw stream, the transcript tail, the exact
    // argv, and codex's version - so the next death is over-instrumented and
    // diagnosable without a repro, since codex records no reason for a death
    // in its rollout. A stated failure is still bundled; its stated reason
    // becomes the bundle's verdict, so it does not read as a hang.
    // An operator interrupt is not an incident: the missing answer is explained.
    let incident_path = if suspicious && !interrupted {
        // The version probe runs a harness binary too, so it is contained
        // like the run, and what it leaves joins the run's leftovers.
        let (codex_version, probe_leftovers) = crate::incident::version_probe(
            &runtime.lifecycle,
            &command.program,
            &command.leading_args,
            &runtime.teardown,
        )
        .await;
        leftovers.merge(probe_leftovers);
        match crate::incident::write_bundle(&crate::incident::Incident {
            harness_version: codex_version,
            harness: Harness::Codex,
            // The process exactly as it was spawned - program, argv, cwd and
            // every env override, the runner's own included - so the replay
            // in meta.json is the spawn by construction.
            process: &spec,
            session_id: session_id.as_deref(),
            prompt,
            exit: status,
            stdin_write_error: stdin_write_error.as_deref(),
            stdout: &stdout_buf,
            stderr: &stderr_buf,
            duration_ms,
            data: &runtime.data,
            outcome: &outcome,
            turns,
            forensics: Some(crate::incident::IncidentForensics::Codex(
                crate::incident::CodexIncident {
                    transcript_path: transcript.as_ref().map(|t| t.path.as_path()),
                    scope: rollout_scope,
                    final_answer_present: forensics::answer_presence(
                        answer_in_scope,
                        transcript.as_ref(),
                    ),
                    quiet_secs,
                    last_rollout_event: last_rollout_event.as_ref(),
                },
            )),
        }) {
            crate::incident::BundleWriteOutcome::Written(path) => Some(path),
            crate::incident::BundleWriteOutcome::Unavailable(reason) => {
                eprintln!("warning: incident bundle was not written: {reason}");
                None
            }
            crate::incident::BundleWriteOutcome::Incomplete {
                directory,
                failures,
            } => {
                for failure in failures {
                    let path = failure
                        .path
                        .as_deref()
                        .map(|path| format!(" {}", path.display()))
                        .unwrap_or_default();
                    eprintln!(
                        "warning: incident bundle write failed{path}: {}",
                        failure.message
                    );
                }
                if let Some(directory) = directory {
                    eprintln!(
                        "warning: incomplete incident bundle at {}",
                        directory.display()
                    );
                }
                None
            }
        }
    } else {
        None
    };

    let digest = Digest {
        exit: status,
        turns,
        usage,
        incident_path,
        denials: super::Denials::default(),
        harness: Some(super::HarnessForensics::Codex(super::CodexForensics {
            log_lines,
            transcript,
            quiet_secs,
            last_rollout_event,
        })),
    };

    // Even when no message came back (a hard freeze before any agent_message,
    // the very case forensics exist to explain), return the digest rather than a
    // bare error - otherwise `invoke` would drop the session id and transcript.
    let text = final_message.unwrap_or_else(|| {
        let base = outcome.placeholder(Harness::Codex);
        let stderr = String::from_utf8_lossy(&stderr_buf);
        let detail = stderr.trim();
        if detail.is_empty() {
            base
        } else {
            format!("{base}\n{detail}")
        }
    });
    Ok(RunOutput {
        text,
        outcome,
        session_id,
        codex_home: codex_home.map(|path| path.to_string_lossy().into_owned()),
        digest: Some(digest),
        served: Served::default(),
        leftovers,
    })
}

#[cfg(test)]
#[path = "codex_tests.rs"]
mod tests;

/// Resolve Codex's raw-config root precedence before arguments or records use it.
pub(super) fn prepare(launch: &super::LaunchSpec, plan: &mut super::permissions::PermissionPlan) {
    if let Some(paths) = config_writable_roots_override(&launch.settings.config) {
        plan.paths = paths;
    }
}

/// The writable roots a profile `config` entry sets, when one does.
///
/// Profile overrides are passed after the derived `-c`, so codex takes the last
/// one and a profile restating this key replaces the derivation entirely. The
/// last entry wins here for the same reason. Returns `None` when no entry
/// touches the key, and `Some(vec![])` for an explicit empty override - which is
/// a real, recordable state, not the absence of one.
pub(super) fn config_writable_roots_override(config: &[String]) -> Option<Vec<String>> {
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

/// Codex alone has a measured wake cadence; unmeasured sources default off.
pub(super) fn watchdog_timings() -> crate::watchdog::Timings {
    crate::watchdog::Timings {
        stall_grace: Some(crate::timings::STALL_GRACE),
        ..crate::watchdog::Timings::default()
    }
}
