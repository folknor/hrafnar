use super::*;

/// Read codex's stdout to EOF exactly like `read_capped`, but additionally scan
/// the NDJSON for the `thread.started` event and publish the session id on
/// `sid_tx` the moment it appears.
///
/// The watchdog (`crate::watchdog`) needs the session id to locate the rollout
/// transcript. On a resume we already know it, but on a fresh run codex only
/// reveals it mid-stream - and the old code could not see it until the whole
/// stream had hit EOF, which is precisely the thing that never happens on a
/// wedged run. Scanning stops as soon as the id is found, so the steady-state
/// cost is the same byte-copy loop as before.
///
/// The published id also survives a reader that never reaches EOF: it goes out
/// on the watch channel as soon as it is seen, so a truncated `stdout_buf` can
/// no longer cost us the session id (and with it the transcript, the recovery
/// path and the sidecar row).
async fn read_capped_stdout<R>(
    mut r: R,
    cap: usize,
    out: SharedBuf,
    sid_tx: tokio::sync::watch::Sender<Option<String>>,
    first_output: tokio::sync::watch::Sender<bool>,
    mut scanning: bool,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut chunk = [0u8; 16384];
    // Carry for a line split across two reads; only maintained while scanning.
    let mut partial: Vec<u8> = Vec::new();
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                // `send_if_modified` so only the first chunk wakes the watcher.
                first_output.send_if_modified(|seen| !std::mem::replace(seen, true));
                if scanning {
                    partial.extend_from_slice(&chunk[..n]);
                    // Process whole lines; keep the trailing fragment.
                    while let Some(nl) = partial.iter().position(|b| *b == b'\n') {
                        let line: Vec<u8> = partial.drain(..=nl).collect();
                        if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&line)
                            && val.get("type").and_then(|t| t.as_str()) == Some("thread.started")
                            && let Some(id) = val.get("thread_id").and_then(|t| t.as_str())
                        {
                            // A closed receiver just means nobody is watching.
                            let _ = sid_tx.send(Some(id.to_string()));
                            scanning = false;
                            partial.clear();
                            break;
                        }
                    }
                    // Guard against a pathological unterminated first line
                    // growing without bound while we wait for a newline.
                    if partial.len() > 1 << 20 {
                        partial.clear();
                    }
                }
                // Past the cap this keeps draining but stops buffering, so the
                // child never blocks on a full pipe.
                push_capped(&out, &chunk[..n], cap);
            }
        }
    }
}

/// Name of the codex permissions profile `review` synthesises for `read-only`.
///
/// Never read from the operator's config - `--ignore-user-config` drops that
/// file, and the profile is defined entirely by the `-c` overrides below - so
/// the name only has to avoid colliding with a built-in (those all start `:`).
const CODEX_READ_ONLY_PROFILE: &str = "review-read-only";

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
/// in all three: the guarantee `.review.toml` advertises for `read-only` is
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

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_codex(
    session_id: &str,
    model: Option<&str>,
    effort: Option<&str>,
    sandbox: Option<&str>,
    granted_roots: &[crate::writable_roots::GrantedRoot],
    env: Option<&std::collections::BTreeMap<String, String>>,
    config: &[String],
    prompt: &str,
    project_root: &Path,
    oneshot: bool,
    launched: Option<LaunchSignal>,
    runtime: &ProviderRuntime,
) -> Result<RunOutput> {
    let last_msg_path = new_output_file()?;

    // Exec-level options shared by both paths.
    let mut args: Vec<String> = vec![
        "exec".to_string(),
        // `review` is routinely launched from a directory that is not itself a
        // git repo (an umbrella dir whose repos live in subdirectories). Codex
        // refuses to start there - "Not inside a trusted directory and
        // --skip-git-repo-check was not specified" - so pass it always. The
        // filesystem guarantee we rely on is `--sandbox`, which is unaffected:
        // this flag only waives codex's own "am I in a repo?" precondition.
        "--skip-git-repo-check".to_string(),
        // `--sandbox` alone stopped being a filesystem guarantee in codex
        // 0.150.x. `codex exec` sets `approval_policy = "never"` for headless
        // runs, which is what made a read-only sandbox absolute: the model's
        // `request_permissions` escalation is auto-denied under `never`. But
        // `build_exec_config` now *drops* that headless default whenever the
        // resolved `approvals_reviewer` is `auto_review` (settable globally in
        // `~/.codex/config.toml`), falling back to `on-request`. Escalations
        // then go to codex's own auto-approving guardian, which grants them,
        // and additional write permissions are layered onto the read-only
        // profile for the turn. Verified on 0.150.1: a bare
        // `codex exec --sandbox read-only` wrote a file; with this override the
        // same prompt is rejected ("writing is blocked by read-only sandbox").
        // Pinning the policy here restores the invariant regardless of the
        // operator's global codex config. Passed before profile `config`
        // overrides so a profile can still opt back in deliberately.
        "-c".to_string(),
        "approval_policy=\"never\"".to_string(),
        // The second, independent hole in the same guarantee - and the one that
        // actually fires in the field. Codex's execpolicy `.rules` files are not
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
        // identical command sandboxed, which is why this looked like flaky,
        // per-run behaviour for months: enforcement tracked the shape of the
        // command the model happened to type. `--ignore-rules` drops the user
        // and project rule layers for this invocation only, leaving the
        // operator's interactive codex untouched. An older codex without the
        // flag fails loudly on an unknown argument rather than silently running
        // unsandboxed, which is the right way round.
        "--ignore-rules".to_string(),
        // And the general form of the same lesson: a `review` run must be a
        // function of `.review.toml` plus argv, never of a file the operator
        // edited for unrelated reasons. Twice now the guarantee a profile
        // advertised was silently voided by ambient global codex config - once
        // by `approvals_reviewer = "auto_review"` rewriting the approval
        // policy, once by the execpolicy rules above. Pinning each hole as it
        // is found leaves the next one live, so drop `$CODEX_HOME/config.toml`
        // wholesale. Auth is unaffected: it resolves from `CODEX_HOME`
        // independently of this flag (verified - a run with it authenticates
        // and completes normally). Two consequences, both deliberate: a run
        // whose profile sets no `model` now takes codex's built-in default
        // rather than the operator's configured one (identical on this host
        // today, but no longer a silent coupling), and a profile whose `config`
        // overrides name a custom `model_provider` defined in the operator's
        // config.toml must now define it in the profile too.
        "--ignore-user-config".to_string(),
        // Rollout reasoning items carry `encrypted_content` - sealed server-side
        // and undecryptable here - and on the default `auto` summary setting
        // their `summary` arrays come back empty, so a rollout records *what* a
        // run did and never *why*. That cost real time: reconstructing why an
        // agent relocated a build meant inferring intent from the command
        // sequence alone, because the transcript held no prose at any point.
        // `detailed` fills the summaries, which `src/transcript.rs` forensics
        // and `review sessions <id>` can then surface. Passed before profile
        // `config` overrides, so a profile can still set `none` where the
        // summaries are not worth their tokens.
        "-c".to_string(),
        "model_reasoning_summary=\"detailed\"".to_string(),
    ];
    args.extend(codex_sandbox_args(sandbox.unwrap_or("read-only")));
    if let Some(m) = model {
        args.push("-m".to_string());
        args.push(m.to_string());
    }
    if let Some(e) = effort {
        args.push("-c".to_string());
        args.push(format!("model_reasoning_effort=\"{e}\""));
    }
    // Writable roots the host needs for a build, derived by the caller (see
    // `invoke`, and `src/writable_roots/` for why they are derived at all).
    // Placed before profile `config` so a profile can restate
    // `sandbox_workspace_write.writable_roots` and win.
    if let Some(override_arg) = crate::writable_roots::config_override(granted_roots) {
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
    // be created and then used for nothing that needs a pathname. A daemon /
    // worker test suite that binds a socket under its own target directory
    // fails with `Operation not permitted` on a path that is demonstrably
    // writable - verified by binding in the *cwd*, which codex grants
    // unconditionally, and getting the same errno. So no `writable_roots`
    // entry can fix it; there is no path for which it works.
    //
    // Enabling network access is a blunt instrument - the filter is
    // all-or-nothing, so this restores AF_INET egress too, and codex offers no
    // AF_UNIX-only knob. It is nevertheless not a widening of what these runs
    // have actually had: until `--ignore-rules` landed in a commit earlier that
    // day, an operator execpolicy allowlist made build commands bypass the
    // sandbox wrapper entirely, so no seccomp filter was installed and every
    // `workspace-write` run in this tool's history ran with full network
    // access. This makes that explicit and, unlike the bypass, it applies to
    // every command rather than the ones whose argv happened to match a rule.
    //
    // Scoped to `workspace-write` because this key only exists for that level.
    // `read-only` needs the same grant - the filter denies `connect` there too -
    // and gets it from the permissions profile in `codex_sandbox_args`, there
    // being no `sandbox_read_only.network_access`. Passed before profile
    // `config`, so a profile can restate the key and win.
    if sandbox == Some("workspace-write") {
        args.push("-c".to_string());
        args.push("sandbox_workspace_write.network_access=true".to_string());
    }

    // Profile `config` overrides, each a verbatim `-c key=value`. Placed after
    // effort so a profile could even override reasoning effort if it wanted.
    for c in config {
        args.push("-c".to_string());
        args.push(c.clone());
    }

    // A fresh run captures the new session id from the stream; the `resume`
    // subcommand carries the id we already know. Both stream `--json` and take
    // `-o` (placed at the subcommand level for resume, exec level for fresh).
    let known_session_id = if oneshot {
        args.push("--json".to_string());
        args.push("-o".to_string());
        args.push(last_msg_path.clone());
        None
    } else {
        args.push("resume".to_string());
        args.push(session_id.to_string());
        args.push("--json".to_string());
        args.push("-o".to_string());
        args.push(last_msg_path.clone());
        Some(session_id.to_string())
    };

    run_codex_json(
        args,
        &last_msg_path,
        known_session_id,
        env,
        prompt,
        project_root,
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
// Every argument here is an independent axis of one invocation (argv, the `-o`
// path, the known session id, env, prompt, cwd, the launch handshake, the
// injectable runtime). Bundling them into a struct would add a type whose only
// purpose is to be destructured immediately, so the lint is waived rather than
// worked around.
#[allow(clippy::too_many_arguments)]
pub async fn run_codex_json(
    args: Vec<String>,
    last_msg_path: &str,
    known_session_id: Option<String>,
    env: Option<&std::collections::BTreeMap<String, String>>,
    prompt: &str,
    project_root: &Path,
    launched: Option<LaunchSignal>,
    runtime: &ProviderRuntime,
) -> Result<RunOutput> {
    let mut cmd = Command::new(&runtime.codex_binary);
    cmd.args(&args)
        .current_dir(project_root)
        // Make a codex panic legible: without this it exits 1 with no trace,
        // which is exactly the "no recorded reason" we kept hitting. Set before
        // the profile env so an explicit override still wins.
        .env("RUST_BACKTRACE", "1");
    // Launched under a keeper (`contained::spawn`), which puts codex in a
    // process group of its own, out of reach of a terminal SIGINT - the signal
    // supervisor stops runs instead - and tears down everything codex started
    // (exec-server, unified-exec cells, sandboxed shells in sessions of their
    // own, MCP servers) when the run ends, however it ends.
    if let Some(vars) = env {
        cmd.envs(vars);
    }
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
    // `None` means "no trustworthy baseline", which disables both the watchdog
    // and transcript recovery for this run. It is emphatically not the same as
    // `Some(0)`: falling back to zero for a *resume* whose rollout could not be
    // read would scope the scan to the entire session history, and an old
    // `final_answer` from a previous turn could then authorise killing a genuine
    // mid-turn wedge, or be recovered as this run's answer.
    let codex_home_for_baseline = env.and_then(|e| e.get("CODEX_HOME")).map(String::as_str);
    let rollout_baseline: Option<u64> = match known_session_id.as_deref() {
        // Fresh run: a new session id, so its rollout does not exist yet and
        // every byte in it will be ours. Zero is exact here, not a fallback.
        None => Some(0),
        // Resume: the rollout already holds earlier turns, so we must know
        // exactly where they end. If we cannot stat it, we do not guess.
        Some(sid) => crate::transcript::find_transcript_path(sid, codex_home_for_baseline)
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len()),
    };
    if rollout_baseline.is_none() {
        eprintln!(
            "warning: could not read the rollout for session {} before launch; \
             completion watchdog and transcript recovery are disabled for this run",
            known_session_id.as_deref().unwrap_or("?")
        );
    }

    // A request left over from an earlier run of this session (its `review`
    // killed before consuming it) is not aimed at this one. A fresh run needs
    // no such check: its session id is new.
    if let Some(sid) = known_session_id.as_deref() {
        let _ = crate::inflight::take_interrupt_request(sid, runtime.data_root.as_deref());
    }

    let start = std::time::Instant::now();
    // (Gating transcript recovery to this run's turn is done by byte offset -
    // `rollout_baseline` above - not by a wall-clock stamp, which was too coarse
    // to separate a resume from an answer written in the same second.)
    // A run still waiting on the lock or its stagger when `review` was told to
    // stop would otherwise start a turn nobody will see the end of.
    if *runtime.stop.borrow() {
        let _ = std::fs::remove_file(last_msg_path);
        anyhow::bail!("not launched: review was signalled to stop");
    }
    // A resume knows its session id, so its in-flight marker goes up before the
    // spawn: `review message` releases its session launch lock when the launch
    // signal below fires, and a second message must then find this marker to
    // interrupt the run rather than launch beside it. A fresh run's marker waits
    // for the id in the marker task below; nobody can message a session whose
    // id has not been printed. Dropped (removing the marker) on any early
    // return.
    let early_marker = known_session_id.as_deref().map(|sid| {
        crate::inflight::mark(
            sid,
            "codex",
            &project_root.to_string_lossy(),
            runtime.data_root.as_deref(),
        )
    });
    // Clean up the `-o` temp file on every exit path, including an error
    // before the read below.
    let _output_cleanup = RemoveOnDrop(last_msg_path.to_string());
    let label = match known_session_id.as_deref() {
        Some(sid) => format!("the codex run of session {sid}"),
        None => "a codex run".to_string(),
    };
    let mut contained = crate::contained::spawn(
        &cmd,
        Stdio::piped(),
        &runtime.teardown,
        &label,
        runtime.orphan_log.clone(),
    )?;
    let mut early_marker = early_marker;
    if let Some(marker) = early_marker.as_mut() {
        marker.record_keeper(contained.pid());
    }
    let stdin = contained
        .child
        .stdin
        .take()
        .context("failed to open codex stdin")?;
    let stdout_pipe = contained
        .child
        .stdout
        .take()
        .context("failed to open codex stdout")?;
    let stderr_pipe = contained
        .child
        .stderr
        .take()
        .context("failed to open codex stderr")?;
    let mut contained = contained.launch().await?;
    // Launch has happened: the caller may release the global lock. Dropping the
    // sender instead (on the `?` above) tells it the same thing.
    if let Some(signal) = launched {
        let _ = signal.send(());
    }

    // Publishes the session id as soon as it is known: immediately on a resume,
    // or when `thread.started` arrives on stdout for a fresh run. The watchdog
    // needs it to find the rollout transcript.
    let (sid_tx, sid_rx) = tokio::sync::watch::channel(known_session_id.clone());
    let scanning_for_session_id = known_session_id.is_none();
    // Kept for after the wait, so the id survives a stdout reader that never
    // reached EOF (see where `session_id` is resolved).
    let sid_rx_final = sid_rx.clone();
    // The keeper: what the in-flight marker records for `review interrupt` to
    // signal, and which forwards that SIGINT to codex once.
    let keeper_pid = contained.pid();
    let child_pid = Some(keeper_pid);
    // Flips on codex's first stdout. Until then the pid may be a node wrapper
    // that has not yet installed the handler forwarding SIGINT to the native
    // binary, so interrupting it could kill the wrapper and orphan codex -
    // still holding the session's writer lock, which would make the resume
    // `review interrupt` hands back fail. Output from codex proves the wrapper
    // got that far, so the marker only offers the pid from then on.
    let (first_output_tx, mut first_output_rx) = tokio::sync::watch::channel(false);
    // The same safety question for a signal-driven stop, which asks it later.
    let first_output_seen = first_output_rx.clone();

    // Mark the run as in flight so `review sessions` can report "turn in flight
    // since <time>" instead of showing the *previous* turn's response while this
    // one runs - the exact ambiguity that made a 10-hour hang look identical to
    // an idle session.
    //
    // This lives in its own task because a fresh run does not learn its session
    // id until `thread.started` arrives mid-stream. The task parks forever
    // holding the marker guard; aborting it after the run drops the guard, which
    // deletes the marker. That covers every normal exit path, and `read_live`
    // discards markers whose owning pid is gone for the ones it cannot.
    let marker_task = tokio::spawn({
        let mut rx = sid_rx.clone();
        let project = project_root.to_string_lossy().into_owned();
        let data_root = runtime.data_root.clone();
        async move {
            let mut guard = match early_marker {
                Some(guard) => guard,
                None => {
                    let sid = loop {
                        if let Some(sid) = rx.borrow_and_update().clone() {
                            break sid;
                        }
                        if rx.changed().await.is_err() {
                            return;
                        }
                    };
                    let mut guard =
                        crate::inflight::mark(&sid, "codex", &project, data_root.as_deref());
                    guard.record_keeper(keeper_pid);
                    guard
                }
            };
            if first_output_rx.wait_for(|seen| *seen).await.is_ok() {
                guard.record_child_pid(child_pid);
            }
            // Hold the marker until this task is aborted.
            std::future::pending::<()>().await;
        }
    });

    // Bound in-memory buffering: a runaway/noisy provider could otherwise OOM
    // `review` long before the 1 MiB forensic tail is applied. Buffer up to a
    // generous cap and keep draining past it so codex never blocks on a full
    // pipe. Real reviews are far under the cap.
    //
    // These are spawned rather than joined inline because the wait below has to
    // be interruptible, and because pipe EOF is *not* a reliable completion
    // signal - see the reaping comment further down.
    let stdout_shared = new_shared_buf();
    let stderr_shared = new_shared_buf();
    let stdout_task = tokio::spawn(read_capped_stdout(
        stdout_pipe,
        STDOUT_CAPTURE_CAP,
        std::sync::Arc::clone(&stdout_shared),
        sid_tx,
        first_output_tx,
        scanning_for_session_id,
    ));
    let stderr_task = tokio::spawn(read_capped(
        stderr_pipe,
        STDERR_CAPTURE_CAP,
        std::sync::Arc::clone(&stderr_shared),
    ));
    let write_task = tokio::spawn(write_stdin(stdin, prompt.as_bytes().to_vec()));

    // Records that *we* ended the run, and why. See `Digest::terminated_by_review`.
    let mut terminated_by_review: Option<String> = None;
    // Forensics from the watchdog verdict, carried onto the digest and into the
    // incident bundle.
    let mut quiet_secs: Option<u64> = None;
    let mut last_rollout_event: Option<String> = None;
    // Set when `review` itself was signalled and ended this turn in response
    // (see `install_signal_supervisor`); counts as an interrupt request.
    let mut stopped_by_signal = false;
    let mut stop_rx = runtime.stop.clone();

    // Wait for codex, but stay interruptible.
    //
    // Before this loop the code was a plain `join!` on the child plus both
    // pipes, which had two failure modes, both observed:
    //   - codex finishes its turn (final answer on disk, `task_complete`
    //     recorded) and then never exits. `review` blocked for 10+ hours with
    //     the answer sitting in the rollout the whole time, and produced no
    //     digest, no incident bundle and no sidecar row, because all of that is
    //     computed below this point.
    //   - the operator kills `review`, and codex - a separate process - keeps
    //     running detached, still modifying the tree.
    // The watchdog arm below fixes the first; the second is handled process-wide
    // by `install_signal_supervisor`.
    {
        let watchdog = crate::watchdog::wait_for_stranded_completion(
            sid_rx,
            env.and_then(|e| e.get("CODEX_HOME")).cloned(),
            rollout_baseline,
            runtime.timings,
        );
        tokio::pin!(watchdog);
        loop {
            tokio::select! {
                // Normal path: codex exited on its own. The teardown of what it
                // left is waited for below, outside the watchdog's view.
                () = contained.wait_provider() => break,

                // codex has demonstrably produced its answer but will not exit.
                // Kill the group; the loop then reaps it on the next iteration
                // and the recovered `final_answer` is reported as usual. The
                // guard disables this arm once it has fired, so the completed
                // future is never polled again.
                verdict = &mut watchdog, if terminated_by_review.is_none() => {
                    eprintln!("watchdog: {}", verdict.reason);
                    terminated_by_review = Some(match verdict.kind {
                        crate::watchdog::Kind::Stranded => {
                            "watchdog: stranded completion".to_string()
                        }
                        // Named a stall *timeout* on purpose: it is one, and
                        // saying so keeps a future codex cadence change
                        // diagnosable instead of mysterious.
                        crate::watchdog::Kind::Stalled => {
                            format!("watchdog: stall timeout ({}s silent)", verdict.quiet_secs)
                        }
                    });
                    quiet_secs = Some(verdict.quiet_secs);
                    last_rollout_event = verdict.last_event.clone();
                    contained.terminate();
                }

                // `review` was signalled: end the turn as `review interrupt`
                // would - SIGINT to the leader alone - so codex frees the
                // session and we get to record it. Before codex's first output
                // the leader may be a node wrapper that would die on SIGINT and
                // orphan codex, so then the group is killed instead.
                // (An `Err` - no sender left, so no stop can ever come - does
                // not match, and select disables the arm.)
                Ok(_) = stop_rx.wait_for(|stop| *stop), if !stopped_by_signal => {
                    stopped_by_signal = true;
                    if *first_output_seen.borrow() {
                        contained.interrupt();
                    } else {
                        contained.terminate();
                    }
                }
            }
        }
    }
    // codex has exited; its keeper exits once whatever it left is torn down.
    // Waited for before the marker goes, so the session reads as in flight
    // until nothing of the run is left: a message arriving meanwhile waits for
    // it rather than launching beside the teardown.
    let keeper_status = contained.wait().await;
    let watched_session_id = sid_rx_final.borrow().clone();
    // Consumed now that codex is reaped, whatever became of it: a request left
    // behind would mark the session's *next* run as interrupted. Before the
    // marker goes, because `read_live` discards a request with no marker beside
    // it. Whether it *counts* waits until we know if an answer was produced.
    let interrupt_requested = watched_session_id.as_deref().is_some_and(|sid| {
        crate::inflight::take_interrupt_request(sid, runtime.data_root.as_deref())
    }) || stopped_by_signal;
    // Run is over: drop the in-flight marker, before the `?` below so a failed
    // wait cannot leak it. `abort` only requests cancellation - the guard drops
    // when the runtime next polls the task - so the handle is awaited, or the
    // marker could outlive the run that returned.
    marker_task.abort();
    let _ = marker_task.await;
    if let Some(sid) = watched_session_id.as_deref() {
        contained.set_label(format!("the codex run of session {sid}"));
    }
    let finished = contained.finish(&keeper_status).await;
    let mut leftovers = finished.leftovers;
    // `None` when the keeper could not report it: the digest then shows no
    // exit code, the run reads as suspicious (bundled, transcript read), and
    // its session and any answer survive - the ledger already fails the
    // command for the unknown teardown.
    let status = finished.status;

    // The child is reaped, so the run is over regardless of what the pipes do.
    //
    // Waiting on pipe EOF here would reintroduce a hang of its own: EOF needs
    // *every* holder of the write end to close it, which includes any process
    // codex spawned that inherited the descriptors. Reaping is the authoritative
    // signal; give the readers a moment to drain what is already buffered, then
    // take what they have. (Investigation of codex 0.147.0 found no production
    // `Stdio::inherit` on the exec path - `codex-rs/core/src/spawn.rs` gives
    // children null/piped stdio - so this is defence in depth rather than a
    // known live path.)
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
    let write_res = match write_task.await {
        Ok(r) => r,
        Err(e) => Err(anyhow::anyhow!("stdin write task panicked: {e}")),
    };
    let duration_ms = start.elapsed().as_millis();
    // A failed prompt write (e.g. broken pipe because codex exited first) is a
    // forensic signal, not a fatal error - the digest still reports what ran.
    let stdin_write_error = write_res.err().map(|e| e.to_string());

    // Parse the NDJSON stream: session id, streamed final message, turn count,
    // summed usage, and any non-JSON log lines. We do NOT bail on a non-zero
    // exit here - the whole point of the digest is that a halted or errored run
    // still yields whatever it produced, with the exit status recorded.
    let stdout = String::from_utf8_lossy(&stdout_buf);
    let mut parsed_session_id: Option<String> = None;
    let mut stream_message: Option<String> = None;
    let mut turns: u32 = 0;
    let mut usage = Usage::default();
    let mut log_lines: Vec<String> = Vec::new();
    let mut turn_error: Option<String> = None;
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let val: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                // Plain-text log line (codex ERROR/WARN, apply_patch dump). The
                // harness can halt NDJSON emission here, so keep it visible.
                log_lines.push(line.to_string());
                continue;
            }
        };
        match val.get("type").and_then(|t| t.as_str()) {
            Some("thread.started") => {
                parsed_session_id = val
                    .get("thread_id")
                    .and_then(|t| t.as_str())
                    .map(String::from);
            }
            Some("item.completed") => {
                let item = val.get("item");
                // Only agent_message items carry the reportable final text;
                // reasoning / command items also arrive as item.completed.
                if item.and_then(|i| i.get("type")).and_then(|t| t.as_str())
                    == Some("agent_message")
                {
                    stream_message = item
                        .and_then(|i| i.get("text"))
                        .and_then(|t| t.as_str())
                        .map(String::from);
                }
            }
            // Codex stating why the turn ended: an upstream refusal (content
            // flagged, rate limit, auth) or a harness error. Dropping these was
            // what made a policy refusal - fully explained, at the end of the
            // stream - surface as an unexplained death with a bare interim note,
            // then get pointlessly auto-resumed into the identical refusal.
            // `turn.failed` nests the message; the bare `error` event carries it
            // at the top level. They arrive as a pair saying the same thing, so
            // last-writer-wins is fine; a genuine second failure is more
            // informative than the first anyway.
            Some("error") => {
                if let Some(m) = val.get("message").and_then(|m| m.as_str()) {
                    turn_error = Some(m.to_string());
                }
            }
            Some("turn.failed") => {
                if let Some(m) = val
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                {
                    turn_error = Some(m.to_string());
                }
            }
            Some("turn.completed") => {
                turns += 1;
                if let Some(u) = val.get("usage") {
                    let get = |k: &str| u.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
                    usage.input_tokens += get("input_tokens");
                    usage.cached_input_tokens += get("cached_input_tokens");
                    usage.output_tokens += get("output_tokens");
                    usage.reasoning_output_tokens += get("reasoning_output_tokens");
                }
            }
            _ => {}
        }
    }

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
        .or(parsed_session_id)
        .or(watched_session_id);

    let exit_code = status.and_then(|s| s.code());
    let signal = status.as_ref().and_then(signal_name);
    // Only pay for transcript forensics when the run looks wrong; a clean
    // captured run needs no post-mortem. Look under the run's effective
    // CODEX_HOME (a profile env override) so a custom codex home is searched.
    // Read before choosing the final message: codex routinely finishes the turn
    // on disk (task_complete) yet exits non-zero and truncates both the `--json`
    // stream and the `-o` file, so the rollout is the authoritative record.
    let suspicious = !captured || exit_code != Some(0) || signal.is_some();
    let codex_home = env.and_then(|e| e.get("CODEX_HOME")).map(String::as_str);
    // An untrustworthy baseline must not degrade to "scan everything": scope to
    // a point past the end of the file so nothing is attributed to this run.
    // `u64::MAX` is exact for that purpose - `slice_from_offset` yields an empty
    // slice for any offset beyond the file.
    let recovery_scope = Some(rollout_baseline.unwrap_or(u64::MAX));
    let transcript = if suspicious {
        // Scoped to the bytes this run appended (see `rollout_baseline`), so a
        // previous turn's `final_answer` can never be recovered as ours. When
        // the baseline is untrustworthy this yields nothing rather than the
        // whole history, which is the safe direction: no recovery beats a wrong
        // answer that also suppresses auto-resume.
        session_id
            .as_deref()
            .and_then(|sid| crate::transcript::summarize_session(sid, codex_home, recovery_scope))
    } else {
        None
    };

    // Final-message priority:
    //   1. the `-o` backstop - authoritative when codex managed to write it.
    //   2. the transcript's recovered `final_answer` - the real report salvaged
    //      when the stream/`-o` were truncated but the rollout reached the end.
    //   3. the last streamed message - interim commentary, a last resort.
    let recovered = transcript.as_ref().and_then(|t| t.final_answer.clone());
    let recovered_from_transcript = final_from_file.is_none() && recovered.is_some();
    let final_message = final_from_file.or(recovered).or(stream_message);

    // A request only makes this an interrupt if the run has no answer. codex
    // can finish on its own between `review interrupt` writing the request and
    // its signal landing, or answer before the signal is read; calling either
    // interrupted would report a completed run as cut short.
    let interrupted = interrupt_requested && !captured && !recovered_from_transcript;
    if interrupted && terminated_by_review.is_none() {
        terminated_by_review = Some(if stopped_by_signal {
            "review was signalled to stop".to_string()
        } else {
            "operator interrupt (`review interrupt`)".to_string()
        });
    }

    // Dump a full forensic bundle for the same suspicious runs we post-mortem -
    // stderr (with backtraces), the raw stream, the transcript tail, the exact
    // argv, and codex's version - so the next death is over-instrumented.
    // An operator interrupt is not an incident: the missing answer is explained.
    let incident_path = if suspicious && !interrupted {
        // The version probe runs a provider binary too, so it is contained
        // like the run, and what it leaves joins the run's leftovers.
        let (codex_version, probe_leftovers) =
            crate::contained::version_probe(&runtime.codex_binary, &runtime.teardown).await;
        leftovers.merge(probe_leftovers);
        crate::incident::write_bundle(&crate::incident::Incident {
            codex_version,
            provider: "codex",
            // The binary actually executed, which is not always "codex": a
            // stub overrides it. `command` in meta.json has to replay the
            // process that really ran.
            binary: &runtime.codex_binary,
            session_id: session_id.as_deref(),
            argv: &args,
            prompt,
            cwd: project_root,
            env,
            exit_code,
            signal: signal.as_deref(),
            stdin_write_error: stdin_write_error.as_deref(),
            stdout: &stdout_buf,
            stderr: &stderr_buf,
            transcript_path: transcript.as_ref().map(|t| t.path.as_str()),
            duration_ms,
            data_root: runtime.data_root.as_deref(),
            captured,
            recovered_from_transcript,
            turns,
            terminated_by_review: terminated_by_review.as_deref(),
            quiet_secs,
            last_rollout_event: last_rollout_event.as_deref(),
            turn_error: turn_error.as_deref(),
        })
        .map(|p| p.to_string_lossy().into_owned())
    } else {
        None
    };

    let digest = Digest {
        exit_code,
        signal,
        captured,
        recovered_from_transcript,
        turns,
        usage,
        log_lines,
        transcript,
        incident_path,
        terminated_by_review,
        quiet_secs,
        last_rollout_event,
        turn_error,
        review_error: None,
        interrupted,
    };

    // Even when no message came back (a hard freeze before any agent_message,
    // the very case forensics exist to explain), return the digest rather than a
    // bare error - otherwise `invoke` would drop the session id and transcript.
    let text = final_message.unwrap_or_else(|| {
        // No final answer at all - not even an interim message. task_complete
        // is not a success signal (it fires on aborted turns with a null final
        // answer), so a non-zero exit/signal here means the run died mid-turn.
        // A stated reason outranks our inference: when codex told us why the turn
        // ended, say that instead of guessing from the exit status.
        let stated = digest
            .turn_error
            .as_ref()
            .map(|m| format!("(codex ended the turn: {m})"));
        let base = stated.unwrap_or_else(|| {
            if digest.interrupted {
                "(interrupted by the operator before a final answer)".to_string()
            } else if digest.exit_code == Some(0) && digest.signal.is_none() {
                "(codex produced no final message)".to_string()
            } else {
                "(codex died without a final answer)".to_string()
            }
        });
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
        session_id,
        digest: Some(digest),
        served: Served::default(),
        leftovers,
    })
}

#[cfg(test)]
#[path = "codex_tests.rs"]
mod tests;
