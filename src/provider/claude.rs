use super::*;

type SharedClaudeMetrics = std::sync::Arc<std::sync::Mutex<ClaudeMetrics>>;

#[derive(Default, Clone)]
struct ClaudeMetrics {
    turns: u32,
    usage: Usage,
    models: std::collections::BTreeSet<String>,
    cost_usd: Option<f64>,
}

#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct ClaudeMetricEvent {
    #[serde(rename = "type")]
    kind: String,
    num_turns: u32,
    usage: ClaudeMetricUsage,
    #[serde(rename = "modelUsage")]
    model_usage: Option<serde_json::Map<String, serde_json::Value>>,
    total_cost_usd: Option<f64>,
}

#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct ClaudeMetricUsage {
    input_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    output_tokens: u64,
    output_tokens_details: Option<ClaudeMetricUsageDetails>,
}

#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct ClaudeMetricUsageDetails {
    thinking_tokens: u64,
}

impl ClaudeMetrics {
    fn record_line(&mut self, line: &[u8]) {
        let Ok(event) = serde_json::from_slice::<ClaudeMetricEvent>(line) else {
            return;
        };
        if event.kind != "result" {
            return;
        }
        self.turns += event.num_turns;
        self.usage.input_tokens += event.usage.input_tokens
            + event.usage.cache_creation_input_tokens
            + event.usage.cache_read_input_tokens;
        self.usage.cached_input_tokens += event.usage.cache_read_input_tokens;
        self.usage.output_tokens += event.usage.output_tokens;
        self.usage.reasoning_output_tokens += event
            .usage
            .output_tokens_details
            .map_or(0, |details| details.thinking_tokens);
        if let Some(model_usage) = event.model_usage {
            self.models
                .extend(model_usage.into_iter().map(|(name, _)| name));
        }
        if let Some(cost) = event.total_cost_usd {
            self.cost_usd = Some(self.cost_usd.unwrap_or_default() + cost);
        }
    }

    fn served(&self) -> Served {
        let models: Vec<&str> = self.models.iter().map(String::as_str).collect();
        Served {
            model: (!models.is_empty()).then(|| models.join(",")),
            cost_usd: self.cost_usd,
        }
    }
}

/// Keep the event scanner bounded like stdout capture. A result line longer
/// than this cannot survive the existing head/tail capture as a parseable line.
const CLAUDE_METRIC_LINE_CAP: usize = STDOUT_CAPTURE_CAP;

fn extend_claude_metric_line(pending: &mut Vec<u8>, too_long: &mut bool, bytes: &[u8]) {
    if *too_long {
        return;
    }
    if pending.len().saturating_add(bytes.len()) > CLAUDE_METRIC_LINE_CAP {
        pending.clear();
        *too_long = true;
    } else {
        pending.extend_from_slice(bytes);
    }
}

fn record_claude_metric_line(metrics: &SharedClaudeMetrics, line: &[u8]) {
    if let Ok(mut metrics) = metrics.lock() {
        metrics.record_line(line);
    }
}

fn record_claude_metric_bytes(
    bytes: &[u8],
    pending: &mut Vec<u8>,
    too_long: &mut bool,
    metrics: &SharedClaudeMetrics,
) {
    let mut start = 0;
    while let Some(relative_end) = bytes[start..].iter().position(|&byte| byte == b'\n') {
        let end = start + relative_end;
        extend_claude_metric_line(pending, too_long, &bytes[start..end]);
        if !*too_long {
            record_claude_metric_line(metrics, pending);
        }
        pending.clear();
        *too_long = false;
        start = end + 1;
    }
    extend_claude_metric_line(pending, too_long, &bytes[start..]);
}

/// Read claude's stream like `read_capped`, with two differences.
///
/// `first` flips once claude's `init` event has arrived: that event is what
/// makes it safe to signal, and what shows its session exists (see
/// `run_claude`).
///
/// Past `cap`, the most recent `tail_keep` to `2 * tail_keep` bytes go to
/// `tail` instead of being dropped. The stream *ends* in the result object,
/// the one line `review` needs, so keeping only the head - right for codex,
/// whose answer comes from `-o` - would lose the answer and the session id of
/// any run that outgrew the cap. Result metrics are accumulated as the stream
/// arrives so result events trimmed from the middle remain accounted for. The
/// buffers and metrics are shared rather than returned, so what was read
/// survives the reader being aborted.
async fn read_claude_stream<R>(
    mut r: R,
    cap: usize,
    tail_keep: usize,
    head: SharedBuf,
    tail: SharedBuf,
    metrics: SharedClaudeMetrics,
    first: tokio::sync::watch::Sender<bool>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut chunk = [0u8; 16384];
    let mut metric_line = Vec::new();
    let mut metric_line_too_long = false;
    // Lines read so far while looking for claude's `init` event; `None` once
    // it has been seen. Only that event counts as claude having started: it
    // carries the session id, so from then on the session exists. A stray byte
    // or diagnostic before it proves nothing, and treating one as a start
    // would keep the id of a session claude never wrote.
    let mut scanning: Option<Vec<u8>> = Some(Vec::new());
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                record_claude_metric_bytes(
                    &chunk[..n],
                    &mut metric_line,
                    &mut metric_line_too_long,
                    &metrics,
                );
                if let Some(pending) = scanning.as_mut() {
                    pending.extend_from_slice(&chunk[..n]);
                    let mut found = false;
                    while let Some(end) = pending.iter().position(|&b| b == b'\n') {
                        let line: Vec<u8> = pending.drain(..=end).collect();
                        if is_claude_init(&line) {
                            found = true;
                            break;
                        }
                    }
                    if found {
                        first.send_replace(true);
                        scanning = None;
                    } else if pending.len() > CLAUDE_INIT_SCAN_MAX {
                        // One unterminated line this long is not an init
                        // event; stop holding it.
                        pending.clear();
                    }
                }
                let mut bytes = &chunk[..n];
                if let Ok(mut h) = head.lock()
                    && h.len() < cap
                {
                    let take = (cap - h.len()).min(bytes.len());
                    h.extend_from_slice(&bytes[..take]);
                    bytes = &bytes[take..];
                }
                if !bytes.is_empty()
                    && let Ok(mut t) = tail.lock()
                {
                    t.extend_from_slice(bytes);
                    if t.len() > 2 * tail_keep {
                        let excess = t.len() - tail_keep;
                        t.drain(..excess);
                    }
                }
            }
        }
    }
    if !metric_line_too_long && !metric_line.is_empty() {
        record_claude_metric_line(&metrics, &metric_line);
    }
}

/// Whether `line` is claude's `init` event: `{"type":"system","subtype":"init",...}`.
fn is_claude_init(line: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(line).is_ok_and(|v| {
        v.get("type").and_then(|t| t.as_str()) == Some("system")
            && v.get("subtype").and_then(|s| s.as_str()) == Some("init")
    })
}

/// Longest unterminated line `read_claude_stream` holds while looking for the
/// `init` event, which is a few KiB.
const CLAUDE_INIT_SCAN_MAX: usize = 1 << 20; // 1 MiB

/// How much of claude's stream past `STDOUT_CAPTURE_CAP` is kept: enough for a
/// result line carrying a long answer.
const CLAUDE_TAIL_KEEP: usize = 8 << 20; // 8 MiB

/// Longest explanation `interpret_claude_output` puts in an error, in chars.
const CLAUDE_DETAIL_MAX: usize = 2000;

/// How long `review` lets a claude shell command run in the foreground: 20
/// minutes, the allowance grok gets (`GROK_COMMAND_WAIT_SECS`) for the same
/// reason - long enough for a full gate, short enough that a wedged command
/// cannot hold a run for hours. Set as both `BASH_DEFAULT_TIMEOUT_MS` and
/// `BASH_MAX_TIMEOUT_MS`; see `run_claude` for why the default is the one that
/// matters.
const CLAUDE_COMMAND_WAIT_SECS: u64 = 20 * 60;

#[allow(clippy::too_many_arguments)]
pub async fn run_claude(
    session_id: &str,
    model: Option<&str>,
    effort: Option<&str>,
    env: Option<&std::collections::BTreeMap<String, String>>,
    prompt: &str,
    project_root: &Path,
    oneshot: bool,
    launched: Option<LaunchSignal>,
    // Named for codex, whose runner it was written for; claude uses only the
    // stop channel, the data root and the two grace periods.
    runtime: &ProviderRuntime,
) -> Result<RunOutput> {
    // In oneshot mode, generate a UUID up front and pass it via --session-id
    // so the fresh session is persistable and the operator can follow up via
    // `review resume <id>`. (Previously used --no-session-persistence, which made
    // the session unreachable.)
    let oneshot_id = if oneshot {
        Some(crate::config::generate_uuid())
    } else {
        None
    };

    let mut args: Vec<String> = if let Some(ref id) = oneshot_id {
        vec!["--session-id".to_string(), id.clone()]
    } else {
        vec!["--resume".to_string(), session_id.to_string()]
    };
    // The stream ends in one result object per run, whatever the outcome,
    // carrying `is_error` - so the turn classifies itself, as grok's does, and a
    // failed run still names its session. Text output gave neither: a non-zero
    // exit was all `review` saw, and it discarded the session id with it.
    //
    // `stream-json` rather than `json` (which prints only that object) for its
    // first event: an `init` within half a second of launch, which is what
    // makes claude safe to signal (see the wait loop below). `--verbose` is
    // required with it under `--print`. When claude wakes the model for a
    // background subagent's result, the stream carries a result per turn and the
    // last is the answer - moot while background tasks are disabled below.
    for arg in [
        "--print",
        "--permission-mode",
        "dontAsk",
        "--output-format",
        "stream-json",
        "--verbose",
    ] {
        args.push(arg.to_string());
    }
    if let Some(m) = model {
        args.push("--model".to_string());
        args.push(m.to_string());
    }
    if let Some(e) = effort {
        args.push("--effort".to_string());
        args.push(e.to_string());
    }

    let (program, leading) = runtime
        .claude_command
        .split_first()
        .map_or(("claude", &[][..]), |(p, rest)| (p.as_str(), rest));
    let mut cmd = Command::new(program);
    // Launched under a keeper (`contained::spawn`), which gives claude its own
    // process group and tears down every shell it started when the run ends.
    cmd.args(leading).args(&args).current_dir(project_root);
    // Keep the model's shell commands in the foreground, where their output
    // reaches it. `--print` does not wait for a background Bash command when the
    // model ends its turn: it *kills* it and exits 0. Probed on Claude Code
    // 2.1.284, asked only to run a 60s command in the background and report its
    // output, the model replied "I'll report exactly what it prints when it
    // finishes", ended its turn, and the command was killed seconds later - a
    // clean-looking run with no report, the loss grok's wake loop exists for.
    // Claude's switch removes the `run_in_background` parameter altogether, so
    // the same prompt ran the command in the foreground and reported its output.
    // (A background *subagent* was already waited for and its result handed
    // back in a second turn; it now just runs in the foreground too.)
    //
    // The cost of the switch is Bash's 2-minute default timeout, which kills a
    // longer foreground command outright. The model can pass its own `timeout`,
    // but under `dontAsk` a permission allowlist that names commands (like
    // `Bash(python3 *)`) denies any call carrying one - observed - so the
    // default is the limit that actually applies, and it is raised to
    // `CLAUDE_COMMAND_WAIT_SECS`: a 150s command then ran to completion. The
    // maximum is raised with it, for a model that is allowed to pass a timeout.
    // Set before the profile env so a profile can restate any of them.
    let wait_ms = (CLAUDE_COMMAND_WAIT_SECS * 1000).to_string();
    cmd.env("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS", "1")
        .env("BASH_DEFAULT_TIMEOUT_MS", &wait_ms)
        .env("BASH_MAX_TIMEOUT_MS", &wait_ms);
    if let Some(vars) = env {
        cmd.envs(vars);
    }
    // A request left over from an earlier run of this session (its `review`
    // killed before consuming it) is not aimed at this one, and would mark a
    // failed resume as interrupted, hiding claude's stated reason. A fresh run
    // needs no check: its session id is new. As in `run_codex_json`.
    if !oneshot {
        let _ = crate::inflight::take_interrupt_request(session_id, runtime.data_root.as_deref());
    }
    // A run still waiting on the lock or its stagger when `review` was told to
    // stop would otherwise launch claude only to kill it before its first event
    // - and, a stopped run being recorded as interrupted, file a sidecar row and
    // print a resume command for a session claude may never have written.
    if *runtime.stop.borrow() {
        anyhow::bail!("not launched: review was signalled to stop");
    }
    // The session id is known before the run starts - generated for a fresh
    // run, passed in for a resume - so the in-flight marker goes up before the
    // spawn. That ordering is load-bearing: `review message` releases its
    // session launch lock when the launch signal below fires, and a second
    // message must then find this marker, or it launches beside this run
    // instead of interrupting it. Dropped (removing the marker) on any early
    // return below.
    let marker_sid = oneshot_id.clone().unwrap_or_else(|| session_id.to_string());
    let marker = crate::inflight::mark(
        &marker_sid,
        "claude",
        &project_root.to_string_lossy(),
        runtime.data_root.as_deref(),
    );
    let mut contained = crate::contained::spawn(
        &cmd,
        Stdio::piped(),
        &runtime.teardown,
        &format!("the claude run of session {marker_sid}"),
        runtime.orphan_log.clone(),
    )?;
    let mut marker = marker;
    marker.record_keeper(contained.pid());
    let stdin = contained
        .child
        .stdin
        .take()
        .context("failed to open claude stdin")?;
    let stdout_pipe = contained
        .child
        .stdout
        .take()
        .context("failed to open claude stdout")?;
    let stderr_pipe = contained
        .child
        .stderr
        .take()
        .context("failed to open claude stderr")?;
    let mut contained = contained.launch().await?;
    // Launch has happened: the caller may release the global lock. Dropping the
    // sender instead (on the `?` above) tells it the same thing.
    if let Some(signal) = launched {
        let _ = signal.send(());
    }
    // The keeper: what `review interrupt` signals, and which forwards the
    // SIGINT to claude once.
    let pid = Some(contained.pid());

    let (first_tx, first_rx) = tokio::sync::watch::channel(false);
    // Offers claude's pid on the marker for `review interrupt` only once claude
    // has printed its first event. A SIGINT before claude installs its handler
    // takes the default action and kills it with nothing printed; after, it
    // ends the turn with a result object (see the wait loop). The task holds
    // the marker until it is aborted, which deletes it.
    let marker_task = tokio::spawn({
        let mut first_rx = first_rx.clone();
        let mut guard = marker;
        async move {
            if first_rx.wait_for(|seen| *seen).await.is_ok() {
                guard.record_child_pid(pid);
            }
            std::future::pending::<()>().await;
        }
    });

    let stdout_shared = new_shared_buf();
    let stdout_tail = new_shared_buf();
    let claude_metrics = std::sync::Arc::new(std::sync::Mutex::new(ClaudeMetrics::default()));
    let stderr_shared = new_shared_buf();
    let stdout_task = tokio::spawn(read_claude_stream(
        stdout_pipe,
        STDOUT_CAPTURE_CAP,
        CLAUDE_TAIL_KEEP,
        std::sync::Arc::clone(&stdout_shared),
        std::sync::Arc::clone(&stdout_tail),
        std::sync::Arc::clone(&claude_metrics),
        first_tx,
    ));
    let stderr_task = tokio::spawn(read_capped(
        stderr_pipe,
        STDERR_CAPTURE_CAP,
        std::sync::Arc::clone(&stderr_shared),
    ));
    let write_task = tokio::spawn(write_stdin(stdin, prompt.as_bytes().to_vec()));

    // Wait for claude, but end the turn if `review` is signalled to stop.
    // Probed on Claude Code 2.1.284 mid-command: a SIGINT to claude ends the
    // turn at once, kills the running command, and still prints a result object
    // (`is_error: true`, `terminal_reason: "aborted_tools"`) naming the session,
    // which then resumes normally. A SIGTERM also leaves a resumable session but
    // prints nothing, so there would be no result to record. Before claude's
    // first event it may not have a handler yet, so then the group is killed
    // instead, as codex's is.
    let mut stop_rx = runtime.stop.clone();
    let mut stopped_by_signal = false;
    let keeper_status = loop {
        tokio::select! {
            status = contained.wait() => break status,
            Ok(_) = stop_rx.wait_for(|stop| *stop), if !stopped_by_signal => {
                stopped_by_signal = true;
                if *first_rx.borrow() {
                    contained.interrupt();
                } else {
                    contained.terminate();
                }
            }
        }
    };
    // Consumed whatever became of the run, before the marker goes (`read_live`
    // discards a request with no marker beside it): left behind, it would mark
    // the session's next run as interrupted.
    let interrupt_requested =
        crate::inflight::take_interrupt_request(&marker_sid, runtime.data_root.as_deref())
            || stopped_by_signal;
    // Awaited after the abort, which only requests cancellation: the marker
    // guard must be gone before this run returns, not whenever the runtime
    // next polls the task.
    marker_task.abort();
    let _ = marker_task.await;
    let finished = contained.finish(&keeper_status).await;
    let leftovers = finished.leftovers;
    // `None` when the keeper could not report it. The run still goes on to
    // what claude printed - its answer and session survive an unknown
    // teardown, which the ledger already fails the command for.
    let status = finished.status;
    let mut stdout_buf = collect_reader(
        stdout_task,
        &stdout_shared,
        runtime.drain_grace,
        "claude stdout",
    )
    .await;
    // The tail starts mid-line; the newline makes that fragment a line of its
    // own, which fails to parse and is skipped like any other.
    if let Ok(tail) = stdout_tail.lock()
        && !tail.is_empty()
    {
        stdout_buf.push(b'\n');
        stdout_buf.extend_from_slice(&tail);
    }
    let stderr_buf = collect_reader(
        stderr_task,
        &stderr_shared,
        runtime.drain_grace,
        "claude stderr",
    )
    .await;
    let write_res = match write_task.await {
        Ok(r) => r,
        Err(e) => Err(anyhow::anyhow!("stdin write task panicked: {e}")),
    };
    let stdout = String::from_utf8_lossy(&stdout_buf).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_buf).into_owned();

    // The echoed ID wins on a fresh run in case claude reassigned the requested
    // UUID; on resume, retain the ID the caller asked us to continue.
    let keep_id = |echoed: Option<String>| {
        invocation_session_id(oneshot, echoed, oneshot_id.clone(), session_id)
    };
    let metrics = claude_metrics
        .lock()
        .map(|metrics| metrics.clone())
        .unwrap_or_default();
    let served = metrics.served();
    let turns = metrics.turns;
    let usage = metrics.usage;
    let with_usage = |digest: Digest| Digest {
        turns,
        usage: usage.clone(),
        ..digest
    };
    // What an interrupted run's digest says instead of claude's own complaint
    // about it: the missing answer is explained, so no `turn_error`, and the
    // run is flagged so it is neither retried nor reported as a failure of
    // claude's.
    let interrupted_digest = || Digest {
        turn_error: None,
        interrupted: true,
        terminated_by_review: Some(if stopped_by_signal {
            "review was signalled to stop".to_string()
        } else {
            "operator interrupt (`review interrupt`)".to_string()
        }),
        ..with_usage(no_answer_digest(String::new(), status.as_ref()))
    };
    // What an interrupted run prints in place of an answer, as codex's does.
    let interrupted_text = |text: String| {
        if text.trim().is_empty() {
            "(interrupted by the operator before a final answer)".to_string()
        } else {
            text
        }
    };
    // Whether claude printed its first event, which carries the session id:
    // past that point the session exists (its transcript is on disk), so a run
    // that then ends without a result still has a session worth keeping.
    let started = *first_rx.borrow();

    let outcome = match interpret_claude_output(&stdout, &stderr) {
        Ok(outcome) => outcome,
        // No result, but claude had started: interrupted before it could
        // answer for itself, or died (killed from outside, crashed) after
        // starting. Either way the session exists, and dropping its id - as an
        // `Err` would, `invoke` setting none - makes the one turn most worth
        // resuming unresumable. A death is `Ok` with a death digest, as for
        // codex: no `turn_error`, because claude stated no reason.
        Err(e) if started => {
            let digest = if interrupt_requested {
                interrupted_digest()
            } else {
                Digest {
                    turn_error: None,
                    ..with_usage(no_answer_digest(String::new(), status.as_ref()))
                }
            };
            let text = if interrupt_requested {
                interrupted_text(String::new())
            } else {
                format!("(claude ended without a result: {e})")
            };
            return Ok(RunOutput {
                text,
                session_id: keep_id(None),
                codex_home: None,
                digest: Some(digest),
                served,
                leftovers,
            });
        }
        // Stopped before claude's first event: the group was killed before
        // claude could have written anything, so there is no session to name.
        // Recording one would print a resume command that fails.
        Err(_) if stopped_by_signal => {
            anyhow::bail!("not started: review was signalled to stop before claude's first event");
        }
        // No result object and claude never started: the prompt may never
        // have arrived, which is the more useful thing to say when it didn't.
        Err(e) => match write_res {
            Err(write_err) => return Err(write_err.context(e.to_string())),
            Ok(()) => return Err(e),
        },
    };
    Ok(match outcome {
        // An answer wins over an interrupt request: claude can finish between
        // the request and the signal, and calling that interrupted would report
        // a completed run as cut short.
        ClaudeOutcome::Answered { text, session_id } => RunOutput {
            text,
            session_id: keep_id(session_id),
            codex_home: None,
            served,
            leftovers,
            // As for grok: a real answer from a process that exited non-zero is
            // still an answer, but the status is the only sign something went
            // wrong after the turn, so it is kept. `captured: true` keeps it out
            // of `died_without_answer`.
            // Clean answers intentionally have no digest, as for grok. Digest
            // usage is retained for abnormal runs; widening clean-run records
            // changes the shared digest and sidecar contract.
            digest: answered_status_digest(status.as_ref()).map(with_usage),
        },
        ClaudeOutcome::NoAnswer {
            reason,
            review_error,
            text,
            session_id,
        } => {
            let (text, digest) = if interrupt_requested {
                (interrupted_text(text), interrupted_digest())
            } else if review_error {
                (
                    text,
                    with_usage(review_no_answer_digest(reason, status.as_ref())),
                )
            } else {
                (text, with_usage(no_answer_digest(reason, status.as_ref())))
            };
            RunOutput {
                text,
                session_id: keep_id(session_id),
                codex_home: None,
                digest: Some(digest),
                served,
                leftovers,
            }
        }
    })
}

/// The last result object in claude's `stream-json` output, as its own line.
/// Every event is one line; a run ends with a `result`, and a run in which
/// claude woke the model for a background subagent carries one per turn, the
/// last being the answer.
fn last_claude_result(stdout: &str) -> Option<&str> {
    stdout.lines().rev().find(|line| {
        serde_json::from_str::<ClaudeResult>(line)
            .is_ok_and(|r| r.kind.as_deref() == Some("result"))
    })
}

/// The result object that ends claude's `--output-format stream-json` output:
/// one per run, whatever the outcome. Only the fields that decide the outcome
/// are modelled here; `ClaudeMetrics` accumulates usage and served details while
/// the full stream is read.
#[derive(serde::Deserialize)]
struct ClaudeResult {
    /// `"result"`. Checked so that some other JSON on stdout is not mistaken
    /// for a result - every other field here is optional.
    #[serde(rename = "type", default)]
    kind: Option<String>,
    /// `"success"` even for an API error (a model claude could not find
    /// reported `subtype: "success"`, `is_error: true`) - so `is_error`, not
    /// this, decides. Named in the reason when it says something else.
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    /// `"completed"` on a normal turn, `"api_error"` on the unknown model.
    #[serde(default)]
    terminal_reason: Option<String>,
    #[serde(default)]
    api_error_status: Option<u32>,
}

/// What a finished claude run amounts to. An `Err` alongside these means no
/// result object came back at all.
#[derive(Debug)]
enum ClaudeOutcome {
    Answered {
        text: String,
        session_id: Option<String>,
    },
    /// Claude produced a result object that is either flagged as an error or
    /// missing the flag needed to trust it as an answer. The session exists, so
    /// this is `Ok` with its ID kept.
    NoAnswer {
        reason: String,
        /// `true` when review inferred a failure because Claude omitted the
        /// field that says whether the result is an error.
        review_error: bool,
        text: String,
        session_id: Option<String>,
    },
}

/// Decide whether a finished claude run produced an answer, from its streams
/// alone. Pure, so each shape is tested without a claude binary.
///
/// Only `is_error: false` is an answer. A result missing the flag is treated
/// as a failure rather than a success: a claude that stopped reporting it
/// should fail loudly, not pass every run off as answered.
fn interpret_claude_output(stdout: &str, stderr: &str) -> Result<ClaudeOutcome> {
    // Why there is no result, for the error message - which is printed and
    // persisted in `audit.jsonl`, so it is bounded. stderr is claude's own
    // account when it gives one. Otherwise only the last line of the stream:
    // the whole stream is every event of the run, tool output included, up to
    // `STDOUT_CAPTURE_CAP`.
    let detail = || {
        let source = if stderr.trim().is_empty() {
            stdout
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("(no output)")
        } else {
            stderr.trim()
        };
        truncate(source, CLAUDE_DETAIL_MAX)
    };
    let Some(result) =
        last_claude_result(stdout).and_then(|line| serde_json::from_str::<ClaudeResult>(line).ok())
    else {
        anyhow::bail!("claude produced no result object: {}", detail());
    };
    let text = result.result.unwrap_or_default();
    if result.is_error == Some(false) {
        return Ok(ClaudeOutcome::Answered {
            text,
            session_id: result.session_id,
        });
    }
    let mut facts: Vec<String> = Vec::new();
    if result.is_error.is_none() {
        facts.push("no is_error flag".to_string());
    }
    if let Some(s) = result.subtype.as_deref().filter(|s| *s != "success") {
        facts.push(format!("subtype: {s}"));
    }
    if let Some(t) = result
        .terminal_reason
        .as_deref()
        .filter(|t| *t != "completed")
    {
        facts.push(format!("terminal_reason: {t}"));
    }
    if let Some(status) = result.api_error_status {
        facts.push(format!("API status {status}"));
    }
    let mut reason = if result.is_error.is_none() {
        "review could not classify claude result".to_string()
    } else {
        "claude reported an error".to_string()
    };
    if !facts.is_empty() {
        reason.push_str(&format!(" ({})", facts.join(", ")));
    }
    // On an error the result text is claude's own explanation (the unknown
    // model: "There's an issue with the selected model ..."). It is persisted
    // with the provider reason; a missing `is_error` is described as review's
    // classification failure and routes to `review_error` below.
    if let Some(line) = text.lines().find(|l| !l.trim().is_empty()) {
        reason.push_str(&format!(": {}", line.trim()));
    }
    Ok(ClaudeOutcome::NoAnswer {
        reason,
        review_error: result.is_error.is_none(),
        text,
        session_id: result.session_id,
    })
}

/// Sum claude's result-event metrics, mapped onto the codex-shaped `Usage`:
/// codex's `input_tokens` includes the cached part, while claude reports
/// uncached input, cache writes and cache reads separately, so all three are
/// summed and the reads are the cached share. Only result events contribute.
#[cfg(test)]
fn summarize_claude_output(stdout: &str) -> ClaudeMetrics {
    let mut metrics = ClaudeMetrics::default();
    for line in stdout.lines() {
        metrics.record_line(line.as_bytes());
    }
    metrics
}

#[cfg(test)]
fn claude_usage(stdout: &str) -> (u32, Usage) {
    let metrics = summarize_claude_output(stdout);
    (metrics.turns, metrics.usage)
}

#[cfg(test)]
fn claude_served(stdout: &str) -> Served {
    summarize_claude_output(stdout).served()
}

#[cfg(test)]
#[path = "claude_tests.rs"]
mod tests;
