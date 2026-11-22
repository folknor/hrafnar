//! End-to-end tests for all harness runners, driven by stub harnesses.
//!
//! An integration test rather than a unit test because every harness runs
//! under a keeper, and the keeper is the built `review` binary: Cargo hands an
//! integration test its path (`CARGO_BIN_EXE_review`) and gives a unit test
//! nothing of the kind, so only here do the runners run under exactly the
//! keeper that ships.
//!
//! # Why a stub rather than mocks
//!
//! The behaviour under test is a *process* behaviour: codex finishing its work
//! and then refusing to exit, or wedging mid-turn, or leaving a descendant that
//! holds the stdout pipe open and ignores `SIGTERM`. None of that can be
//! exercised by faking a return value - it needs a real child process, a real
//! pipe, a real process group and a real kill. So these tests run the actual
//! `invoke` against a shell script that reproduces each hang shape
//! deterministically.
//!
//! This matters because the watchdog *kills a live codex*. Before this file the
//! kill path had never run outside hand-written parse fixtures.
//!
//! # How it stays fast
//!
//! `HarnessRuntime` makes the binary, the watchdog timings, the drain grace and
//! the keeper's teardown graces injectable, so a hang that takes 3 minutes to
//! detect and 10 seconds to escalate in production resolves in under a second
//! here. Nothing about the code path itself changes.
//!
//! # Isolation
//!
//! Scratch files live under `target/test-scratch/<uuid>/` (per the project rule
//! that data lives in the repo rather than `/tmp`) and are removed on success.
//!
//! `HarnessRuntime::data` is also redirected there, and that is load-bearing
//! rather than tidiness: `CODEX_HOME` only redirects the *child*, so `review`'s
//! own data paths still resolved from the test process's real
//! `HOME`/`XDG_DATA_HOME`. Before the override existed, running this suite
//! deposited stub incident bundles in the operator's real
//! `~/.local/share/review/incidents`.
//!
//! Any process a stub deliberately leaves running records its pid to
//! `$LINGER_PID`, so the test can assert on it and clean it up rather than
//! leaking it into the rest of the suite.
//!
//! The generated stub is passed as data to `/bin/sh`; it is never exec'd
//! directly. In a parallel test process, another thread can fork while
//! `write_stub` still has a write descriptor open, and that child inherits the
//! descriptor until its own exec. Directly exec'ing the freshly-written stub in
//! that window fails with `ETXTBSY`. Exec'ing the system shell avoids the race
//! by construction while preserving the real child/process-group behaviour.
//! Suspicious runs consequently probe `/bin/sh --version`, not the script;
//! `incident::version_probe` must keep its stdin set to null so a shell that
//! handles an unknown option by reading stdin cannot hang the bundle write.

#[path = "support/codex.rs"]
mod fixtures;

use anyhow::Result;
use review::harness::{
    Digest, Harness, HarnessCommand, HarnessCommands, HarnessResult, HarnessRuntime, invoke,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Retain the invocation reservation through receipt publication and assertions.
/// Assertion fields may be moved independently; the owner stays in this scope.
struct FixtureRun {
    text: String,
    outcome: review::harness::Outcome,
    session_id: Option<String>,
    digest: Option<Digest>,
    leftovers: review::contained::Leftovers,
    owner: HarnessResult,
}

fn fixture_run(mut owner: HarnessResult) -> Result<FixtureRun> {
    let turn = std::mem::replace(
        &mut owner.output,
        Err(anyhow::anyhow!("fixture extracted turn")),
    )?;
    Ok(FixtureRun {
        text: turn.text,
        outcome: turn.outcome,
        session_id: owner.session_id.take(),
        digest: owner.digest.take(),
        leftovers: std::mem::take(&mut owner.leftovers),
        owner,
    })
}

/// The flat persisted form of a run: its outcome over its forensics (or
/// none), which is what the assertions below read the outcome facts from.
fn summary_of(run: &FixtureRun) -> review::harness::DigestSummary {
    match &run.digest {
        Some(d) => d.summary(&run.outcome),
        None => Digest::default().summary(&run.outcome),
    }
}

/// A stub session id. Fixed rather than random so the stub script and the
/// assertions can both name the rollout file.
const STUB_SESSION: &str = "01920000-0000-7000-8000-000000000001";

/// Per-test scratch directory, removed when the test finishes.
///
/// `Drop` also SIGKILLs any process a stub deliberately left running, unless the
/// test disarmed it by cleaning up itself. That belt-and-braces matters because
/// these fixtures leave processes alive for 30-300 seconds: if `run_stub` or any
/// assertion before the cleanup line panics, an explicit kill at the end of the
/// test body never runs, and the leftover process leaks into the rest of the
/// suite. `Drop` runs on unwind, so it covers every failure path.
struct Scratch {
    root: PathBuf,
    /// Set once a test has cleaned up its own lingering process. Suppresses the
    /// kill in `Drop`, which would otherwise be a signal aimed at a pid that has
    /// already been reaped - and could therefore hit an unrelated process that
    /// reused the number.
    linger_disarmed: std::cell::Cell<bool>,
}

impl Scratch {
    fn new() -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(review::config::generate_uuid());
        std::fs::create_dir_all(root.join("codex-home")).expect("create scratch dirs");
        std::fs::create_dir_all(root.join("project")).expect("create project dir");
        Self {
            root,
            linger_disarmed: std::cell::Cell::new(false),
        }
    }

    /// Take responsibility for the lingering process: `Drop` will not signal it.
    /// Call only after confirming it is gone.
    fn disarm_linger(&self) {
        self.linger_disarmed.set(true);
    }

    fn codex_home(&self) -> PathBuf {
        self.root.join("codex-home")
    }

    fn project(&self) -> PathBuf {
        self.root.join("project")
    }

    /// Isolated XDG data root for in-flight markers and incident bundles.
    ///
    /// Redirecting `CODEX_HOME` only redirects the *child process*; `review`'s
    /// own data paths resolve from this process's real `HOME`/`XDG_DATA_HOME`.
    /// Without this override the suite wrote stub incident bundles straight into
    /// the operator's real `~/.local/share/review/incidents`.
    fn data_root(&self) -> PathBuf {
        self.root.join("data")
    }

    /// Pid a stub recorded for a process it deliberately left running, so the
    /// test can assert on it and clean it up.
    fn linger_pid(&self) -> Option<i32> {
        std::fs::read_to_string(self.root.join("linger.pid"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Incident bundles written by a run under this scratch root.
    fn incident_dirs(&self) -> Vec<PathBuf> {
        let base = self.data_root().join("review").join("incidents");
        match std::fs::read_dir(base) {
            Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Write a stub shell script and return its path.
    ///
    /// The harness passes this path to `/bin/sh` as data; it must not exec the
    /// freshly-written file directly (see the module-level `ETXTBSY` note). The
    /// executable mode is retained only so a failed fixture is convenient to
    /// invoke by hand while debugging.
    fn write_stub(&self, body: &str) -> String {
        let path = self.root.join("codex-stub.sh");
        let script = format!("#!/bin/sh\n{body}\n");
        std::fs::write(&path, script).expect("write stub");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub");
        path.to_string_lossy().into_owned()
    }

    /// Path the stub is expected to write its rollout to, matching the real
    /// codex layout (`sessions/YYYY/MM/DD/rollout-<ts>-<id>.jsonl`) that
    /// `transcript::find_transcript_path` walks.
    fn rollout_path(&self) -> PathBuf {
        self.codex_home()
            .join("sessions/2026/01/01")
            .join(format!("rollout-2026-01-01T00-00-00-{STUB_SESSION}.jsonl"))
    }

    /// Seed one completed prior turn and a rate-limit snapshot for resume tests.
    fn seed_previous_rollout(&self) {
        let rollout = self.rollout_path();
        std::fs::create_dir_all(rollout.parent().expect("rollout parent"))
            .expect("create rollout directory");
        std::fs::write(
            rollout,
            format!("{}\n{}\n{}\n{}\n{}\n",
                fixtures::metadata(STUB_SESSION),
                fixtures::event(Some("event_msg"), "task_started"),
                fixtures::legacy_message("OLD PREVIOUS ANSWER", "final_answer"),
                fixtures::event(Some("event_msg"), "task_complete"),
                r#"{"type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":99.0}}}}"#,
            ),
        )
        .expect("seed previous rollout turn");
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Reap a lingering fixture process the test did not get to clean up
        // (a panic before its cleanup line, including inside `run_stub`).
        if !self.linger_disarmed.get()
            && let Some(pid) = self.linger_pid()
        {
            // SAFETY: a pid this fixture's stub spawned and recorded. Only
            // reached when the test did *not* disarm, so it has not been reaped
            // by us and the number cannot have been recycled behind our back.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Shell fragment: emit `thread.started` on stdout and create the rollout with
/// a `task_started`. Shared by every stub.
fn stub_preamble() -> String {
    let metadata = fixtures::metadata(STUB_SESSION);
    let started = fixtures::event(Some("event_msg"), "task_started");
    let message = fixtures::legacy_message("MESSAGE_PLACEHOLDER", "PHASE_PLACEHOLDER");
    format!(
        r#"
# Record our own pid so a test that deliberately leaves this process unreaped
# can kill its process group afterwards.
printf '%s\n' "$$" > "$CODEX_HOME/pid"
# Drain the prompt so review's stdin write completes rather than seeing EPIPE.
cat > /dev/null
ROLL_DIR="$CODEX_HOME/sessions/2026/01/01"
mkdir -p "$ROLL_DIR"
ROLL="$ROLL_DIR/rollout-2026-01-01T00-00-00-{STUB_SESSION}.jsonl"
rollout_started() {{ printf '%s\n' '{started}' >> "$ROLL"; }}
rollout_message() {{
    printf '%s\n' '{message}' | sed "s/MESSAGE_PLACEHOLDER/$1/;s/PHASE_PLACEHOLDER/$2/" >> "$ROLL"
}}
if [ ! -s "$ROLL" ]; then printf '%s\n' '{metadata}' > "$ROLL"; fi
printf '%s\n' '{{"type":"thread.started","thread_id":"{STUB_SESSION}"}}'
rollout_started
"#
    )
}

/// Fast timings: the whole watchdog cycle completes in well under a second,
/// while preserving the ordering the production values encode (poll interval
/// comfortably shorter than the quiet grace).
fn fast_runtime(scratch: &Scratch) -> HarnessRuntime {
    HarnessRuntime {
        claude_token: None,
        run_id: review::sessions::handoff::RunId::default(),
        reservation: None,
        predecessor: None,
        // Execute the stable system shell, not the fixture file another test
        // thread may still have open for writing. The script path is argv[0]
        // from the shell's perspective; see `run_stub_with`.
        commands: {
            let mut commands = HarnessCommands::default();
            commands.set(
                Harness::Codex,
                HarnessCommand {
                    program: "/bin/sh".to_string(),
                    leading_args: Vec::new(),
                },
            );
            commands
        },
        timings: review::watchdog::Timings {
            poll_interval: std::time::Duration::from_millis(50),
            quiet_grace: std::time::Duration::from_millis(250),
            // Mirrors the production ordering (3 min vs 15 min) for realism.
            // It is *not* required for correctness: the two verdicts are
            // mutually exclusive on whether an answer exists, so a shorter
            // stall grace cannot steal an answered run - see
            // `an_answered_run_is_stranded_even_with_a_shorter_stall_grace`.
            stall_grace: Some(std::time::Duration::from_millis(600)),
        },
        drain_grace: std::time::Duration::from_millis(300),
        data: review::data::DataDir::at(scratch.data_root()),
        log_family: review::data::LogFamily::Public,
        // One domain per fixture, shared by its turns and version probes.
        lifecycle: review::supervisor::Lifecycle::default(),
        teardown: fast_teardown(),
    }
}

/// The real keeper, paced for tests: graces short enough to keep the suite
/// fast, long enough that a cooperative process still gets a real chance to
/// honour SIGTERM first.
fn fast_teardown() -> review::contained::Settings {
    review::contained::Settings {
        keeper: PathBuf::from(env!("CARGO_BIN_EXE_review")),
        schedule: review::keeper::TeardownSchedule {
            leftover_grace: std::time::Duration::from_millis(200),
            terminate_grace: std::time::Duration::from_millis(400),
            kill_phase: std::time::Duration::from_millis(600),
            beat: std::time::Duration::from_millis(20),
            flush: std::time::Duration::from_secs(2),
        },
        startup: std::time::Duration::from_secs(10),
        probe_timeout: std::time::Duration::from_secs(10),
    }
}

/// Run the stub through the real codex path as a fresh (oneshot) run, with the
/// default fast timings.
async fn run_stub(scratch: &Scratch, script: String) -> Result<FixtureRun> {
    let runtime = fast_runtime(scratch);
    run_stub_with(scratch, &script, &runtime).await
}

/// As `run_stub`, but with an explicit runtime - for tests that vary the
/// timings themselves.
async fn run_stub_with(
    scratch: &Scratch,
    script: &str,
    runtime: &HarnessRuntime,
) -> Result<FixtureRun> {
    run_codex_stub(scratch, script, runtime, None, None).await
}

// This prefix learns the output path from the real adapter argv, rather than
// manufacturing a second launch specification in the test.
const CODEX_STUB_ARGS: &str = r#"
printf '%s\n' "$@" > "$CODEX_HOME/args"
LAST_MSG=
expect_output=0
output_flags=0
for a in "$@"; do
    if [ "$expect_output" = 1 ]; then
        LAST_MSG=$a
        expect_output=0
    elif [ "$a" = -o ]; then
        output_flags=$((output_flags+1))
        expect_output=1
    fi
done
[ "$expect_output" = 0 ] && [ "$output_flags" = 1 ] && [ -n "$LAST_MSG" ] || exit 92
export LAST_MSG
"#;

async fn run_codex_stub(
    scratch: &Scratch,
    script: &str,
    runtime: &HarnessRuntime,
    resume: Option<&str>,
    launched: Option<review::harness::LaunchSignal>,
) -> Result<FixtureRun> {
    let shim = scratch.root.join("codex-argv.sh");
    std::fs::write(&shim, format!("{CODEX_STUB_ARGS}\n. \"$STUB_BODY\"\n"))
        .expect("write argv shim");
    let mut runtime = runtime.clone();
    let mut command = runtime.commands.get(Harness::Codex).clone();
    // This scenario helper accepts a bare shell, or an invalid executable for
    // refusal tests. Arbitrary prefixes are exercised separately through invoke.
    assert!(
        command.leading_args.is_empty(),
        "scenario helper needs a bare interpreter"
    );
    command
        .leading_args
        .push(shim.to_string_lossy().into_owned());
    runtime.commands.set(Harness::Codex, command);
    let env = BTreeMap::from([
        (
            "CODEX_HOME".into(),
            scratch.codex_home().to_string_lossy().into_owned(),
        ),
        ("STUB_BODY".into(), script.to_owned()),
        (
            "LINGER_PID".into(),
            scratch
                .root
                .join("linger.pid")
                .to_string_lossy()
                .into_owned(),
        ),
    ]);
    let start = match resume {
        Some(id) => review::harness::SessionStart::Resume(review::harness::SessionId::parse(id)?),
        None => review::harness::SessionStart::Fresh { id: None },
    };
    fixture_run(
        invoke(
            review::harness::LaunchSpec {
                harness: Harness::Codex,
                start,
                settings: fixture_settings(
                    Harness::Codex,
                    review::config::Profile {
                        env: Some(env),
                        ..Default::default()
                    },
                ),
                prompt: if resume.is_some() {
                    "continue review"
                } else {
                    "review this"
                }
                .into(),
                root: scratch.project(),
            },
            launched,
            &runtime,
            None,
        )
        .await,
    )
}

/// Resume with the real pre-launch rollout baseline and production argv.
async fn run_resume_stub(scratch: &Scratch, script: &str) -> Result<FixtureRun> {
    run_codex_stub(
        scratch,
        script,
        &fast_runtime(scratch),
        Some(STUB_SESSION),
        None,
    )
    .await
}

/// Is `pid` in the zombie state - i.e. dead, but not yet reaped by its parent?
///
/// `/proc/<pid>/stat` is `pid (comm) state ...`, and `comm` may itself contain
/// spaces and parentheses, so the state is the first token *after the last*
/// `)`, not the third whitespace-separated field.
fn is_zombie(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(after_comm) = stat.rsplit_once(')') else {
        return false;
    };
    after_comm.1.split_whitespace().next() == Some("Z")
}

/// Is `pid` still running? Used to assert that group escalation actually reaped
/// a deliberately stubborn descendant, rather than trusting that it did.
///
/// A zombie counts as terminated. `kill(pid, 0)` succeeds against zombies, and
/// these fixtures create them by design: killing the stub makes its stubborn
/// child an orphan, and whether that orphan is reaped promptly depends on
/// whether the environment's PID 1 does so. In a container that does not, the
/// SIGKILL would have worked perfectly and the test would still have failed.
fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 delivers nothing; it only reports existence.
    let exists = unsafe { libc::kill(pid, 0) == 0 };
    exists && !is_zombie(pid)
}

/// 1. Stranded completion: codex writes a real final answer, then hangs
///    forever. The watchdog must kill the group, recovery must surface the
///    answer, and the digest must record that *we* ended the run.
#[tokio::test]
async fn stranded_completion_is_killed_and_recovered() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
rollout_message "STRANDED ANSWER" "final_answer"
printf '%s\n' '{{"type":"event_msg","payload":{{"type":"task_complete"}}}}' >> "$ROLL"
# Finished the work, now refuse to exit - the 0.147.0 hang.
sleep 3600
"#,
        preamble = stub_preamble()
    ));

    let run = run_stub(&scratch, stub).await.expect("run completes");

    assert_eq!(
        run.text, "STRANDED ANSWER",
        "the answer on disk must be recovered and reported"
    );
    let summary = summary_of(&run);
    assert!(run.digest.is_some(), "codex runs carry a digest");
    assert!(
        summary.recovered_from_transcript,
        "answer came from the rollout, not the -o backstop"
    );
    assert!(
        summary
            .terminated_by_review
            .as_deref()
            .is_some_and(|why| why.contains("watchdog")),
        "digest must record the watchdog kill, got {:?}",
        summary.terminated_by_review
    );
    assert_eq!(
        run.session_id.as_deref(),
        Some(STUB_SESSION),
        "session id must survive the kill"
    );
}

/// A real trailing turn resets the summary, but must not erase the answer
/// that licensed the watchdog kill or make the result eligible for auto-resume.
#[tokio::test]
async fn stranded_answer_survives_a_trailing_turn_with_content() {
    for trailing in [
        r#"{"type":"event_msg","payload":{"type":"user_message","message":"continue"}}"#,
        r#"{"type":"response_item","payload":{"type":"reasoning"}}"#,
        r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
        r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"tail"}}"#,
    ] {
        let scratch = Scratch::new();
        let stub = scratch.write_stub(&format!(
            r#"{preamble}
rollout_message "STRANDED ANSWER" "final_answer"
printf '%s\n' '{{"type":"event_msg","payload":{{"type":"task_complete"}}}}' >> "$ROLL"
rollout_started
printf '%s\n' '{trailing}' >> "$ROLL"
printf '%s\n' '{{"type":"event_msg","payload":{{"type":"task_complete"}}}}' >> "$ROLL"
sleep 3600
"#,
            preamble = stub_preamble()
        ));
        let run = run_stub(&scratch, stub).await.expect("run completes");
        assert_eq!(run.text, "STRANDED ANSWER", "trailing event: {trailing}");
        let summary = summary_of(&run);
        let digest = run.digest.expect("digest");
        assert!(!summary.captured);
        assert!(summary.recovered_from_transcript);
        assert!(!summary.interrupted);
        assert!(
            summary
                .terminated_by_review
                .as_deref()
                .is_some_and(|why| why.contains("stranded"))
        );
        assert!(
            digest
                .codex()
                .expect("codex")
                .transcript
                .as_ref()
                .expect("summary")
                .final_answer
                .is_none(),
            "the current-turn summary must still reflect the trailing turn"
        );
    }
}

/// The two verdicts are mutually exclusive on whether an answer exists, so
/// `stall_grace` shorter than `quiet_grace` is not a misconfiguration: an
/// answered run matches neither branch until `quiet_grace` and is then
/// `Stranded`, with its answer recovered. Pinned because the alternative -
/// the stall branch stealing an answered run and reporting a recovered result
/// as a failure - is silent and plausible-looking, the worst kind of wrong.
#[tokio::test]
async fn an_answered_run_is_stranded_even_with_a_shorter_stall_grace() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
rollout_message "ANSWER" "final_answer"
sleep 3600
"#,
        preamble = stub_preamble()
    ));

    let mut runtime = fast_runtime(&scratch);
    // Inverted relative to production: the stall grace elapses first.
    runtime.timings.quiet_grace = std::time::Duration::from_millis(500);
    runtime.timings.stall_grace = Some(std::time::Duration::from_millis(100));

    let run = run_stub_with(&scratch, &stub, &runtime)
        .await
        .expect("run completes");

    let summary = summary_of(&run);

    assert!(run.digest.is_some(), "digest");
    let why = summary.terminated_by_review.unwrap_or_default();
    assert!(
        why.contains("stranded"),
        "an answered run must be stranded, not stalled; got {why:?}"
    );
    assert!(
        !why.contains("stall timeout"),
        "the stall branch must not claim a run that produced an answer"
    );
    assert_eq!(run.text, "ANSWER", "the answer must still be recovered");
}

/// 2. Mid-turn wedge: codex goes silent having written only interim commentary,
///    never a final answer. The stall timeout must kill it, leave an incident
///    bundle, record the silence duration and last event, and - critically -
///    recover *no* answer, so the run reads as the failure it is.
#[tokio::test]
async fn a_mid_turn_wedge_trips_the_stall_timeout() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
# An interim note only. codex tags these "commentary"; treating one as a result
# would report "I am now doing X" as the review.
rollout_message "I am now regenerating the pilot." "commentary"
printf '%s\n' '{{"type":"response_item","payload":{{"type":"custom_tool_call","name":"exec","call_id":"c1","input":"{{}}"}}}}' >> "$ROLL"
# Wedged mid-tool-call: silent from here, exactly like the field rollout that
# stopped dead with a background cell still running.
sleep 3600
"#,
        preamble = stub_preamble()
    ));

    let run = run_stub(&scratch, stub).await.expect("run completes");

    let summary = summary_of(&run);

    let digest = run.digest.expect("digest");
    assert!(
        summary
            .terminated_by_review
            .as_deref()
            .is_some_and(|why| why.contains("stall timeout")),
        "the stall timeout must be what ended this run, got {:?}",
        summary.terminated_by_review
    );
    // The whole point of the answer gate: nothing was produced, so nothing may
    // be reported as a result.
    assert!(
        !summary.captured && !summary.recovered_from_transcript,
        "a wedged run must not report an answer (captured={}, recovered={})",
        summary.captured,
        summary.recovered_from_transcript
    );
    assert!(
        !run.text.contains("regenerating the pilot"),
        "interim commentary must not be surfaced as the result, got {:?}",
        run.text
    );
    // Forensics that make a misfire on a healthy run diagnosable.
    assert!(
        digest.codex().expect("codex").quiet_secs.is_some(),
        "the observed silence duration must be recorded"
    );
    assert_eq!(
        digest
            .codex()
            .expect("codex")
            .last_rollout_event
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("response_item/custom_tool_call"),
        "the last thing codex was doing must be recorded"
    );
    assert_eq!(
        scratch.incident_dirs().len(),
        1,
        "a stalled run must leave exactly one forensic bundle"
    );
}

/// A resumed run's incident excerpt and metadata must use the same byte scope
/// and final-answer recognizer as recovery. Previous turns must not leak into
/// this bundle, commentary is not an answer, and a trailing Codex phantom turn
/// must not erase a real answer from the current run.
#[tokio::test]
async fn incident_bundle_scopes_resumed_rollout_and_uses_phase_aware_answers() {
    let cases = [
        (
            "commentary-only",
            r#"rollout_message "CURRENT COMMENTARY" "commentary"
printf '%s\n' '{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"CURRENT COMMENTARY"}}' >> "$ROLL"
"#,
            false,
            "CURRENT COMMENTARY",
        ),
        (
            "answer-before-phantom",
            r#"rollout_message "CURRENT FINAL" "final_answer"
printf '%s\n' '{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"CURRENT FINAL"}}' >> "$ROLL"
rollout_started
printf '%s\n' '{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<turn_aborted>"}]}}' >> "$ROLL"
printf '%s\n' '{"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}' >> "$ROLL"
"#,
            true,
            "CURRENT FINAL",
        ),
    ];

    for (case, current_events, expected_answer, current_marker) in cases {
        let scratch = Scratch::new();
        scratch.seed_previous_rollout();
        let stub = scratch.write_stub(&format!(
            "{preamble}\n{current_events}\nsleep 3600\n",
            preamble = stub_preamble(),
        ));

        let run = run_resume_stub(&scratch, &stub)
            .await
            .unwrap_or_else(|error| panic!("{case}: {error}"));
        let summary = summary_of(&run);
        assert!(run.digest.is_some(), "codex digest");
        if expected_answer {
            assert!(
                summary.recovered_from_transcript,
                "{case}: answer recovered"
            );
        } else {
            assert!(
                !summary.recovered_from_transcript,
                "{case}: no answer recovered"
            );
        }

        let dirs = scratch.incident_dirs();
        assert_eq!(dirs.len(), 1, "{case}: one incident bundle");
        let meta: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dirs[0].join("meta.json")).expect("read meta.json"),
        )
        .expect("parse meta.json");
        assert_eq!(
            meta["final_answer_present"].as_bool(),
            Some(expected_answer),
            "{case}: answer classification belongs to the resumed run"
        );
        assert!(
            meta["rate_limits"].is_null(),
            "{case}: old rate-limit data must not be copied into this run"
        );
        let tail = std::fs::read_to_string(dirs[0].join("transcript.tail.jsonl"))
            .expect("read transcript tail");
        assert!(
            tail.contains(current_marker),
            "{case}: current lines retained"
        );
        assert!(
            !tail.contains("OLD PREVIOUS ANSWER"),
            "{case}: previous turn must not appear in the excerpt"
        );
    }
}

#[tokio::test]
async fn incident_bundle_excludes_previous_turn_when_resume_adds_no_rollout_bytes() {
    let scratch = Scratch::new();
    scratch.seed_previous_rollout();
    let stub = scratch.write_stub(&format!(
        r#"
cat > /dev/null
printf '%s\n' '{{"type":"thread.started","thread_id":"{STUB_SESSION}"}}'
sleep 3600
"#
    ));

    let run = run_resume_stub(&scratch, &stub)
        .await
        .expect("resume completes after watchdog kill");
    let summary = summary_of(&run);
    assert!(run.digest.is_some(), "codex digest");
    assert!(!summary.recovered_from_transcript);

    let dirs = scratch.incident_dirs();
    assert_eq!(dirs.len(), 1, "one incident bundle");
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dirs[0].join("meta.json")).expect("read meta.json"))
            .expect("parse meta.json");
    assert_eq!(meta["final_answer_present"].as_bool(), Some(false));
    assert!(meta["rate_limits"].is_null());
    assert_eq!(
        std::fs::read_to_string(dirs[0].join("transcript.tail.jsonl"))
            .expect("read transcript tail"),
        ""
    );
}

/// The stall timeout is disableable, because it rests on an empirical codex
/// property rather than a documented contract. With it off, the same wedge must
/// hang rather than be killed - proving the switch actually reaches the poller.
#[tokio::test]
async fn a_disabled_stall_timeout_never_fires() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
rollout_message "working" "commentary"
sleep 3600
"#,
        preamble = stub_preamble()
    ));

    let mut runtime = fast_runtime(&scratch);
    runtime.timings.stall_grace = None;

    // Give the poller many times the stall grace it *would* have used. The run
    // must still be going: with the branch disabled, nothing can end it.
    let outcome = tokio::time::timeout(
        std::time::Duration::from_millis(2_000),
        run_stub_with(&scratch, &stub, &runtime),
    )
    .await;
    assert!(
        outcome.is_err(),
        "with the stall timeout disabled, a wedged run must not be killed"
    );

    // Nothing reaped the stub, so clean it up via its own process group. The
    // pid is the group leader (`process_group(0)` at spawn).
    let stub_pid = std::fs::read_to_string(scratch.codex_home().join("pid"))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok());
    if let Some(pid) = stub_pid {
        // SAFETY: negative pid addresses the group this test's stub leads.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
}

/// 3. A run that keeps writing to its rollout must never be touched, however
///    long it goes on.
///
///    The stub writes its final answer *early* and then keeps working for well
///    past the quiet grace. That is the important shape: the answer gate alone
///    would happily kill this run, so the only thing protecting it is the
///    advancement check. It also mirrors real codex, which appends trailing
///    micro-turns after the substantive answer.
#[tokio::test]
async fn an_advancing_rollout_is_never_killed() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
# The answer exists from here on, so the answer gate is satisfied for the whole
# remainder of the run.
rollout_message "REAL ANSWER" "final_answer"
# Now keep working, well past quiet_grace, exactly as a live codex does (it
# cannot stay silent for long). Killing here would be a false positive.
i=0
while [ $i -lt 12 ]; do
  printf '%s\n' '{{"type":"event_msg","payload":{{"type":"token_count","info":{{}}}}}}' >> "$ROLL"
  sleep 0.1
  i=$((i+1))
done
printf '%s\n' '{{"type":"event_msg","payload":{{"type":"task_complete"}}}}' >> "$ROLL"
printf 'REAL ANSWER' > "$LAST_MSG"
exit 0
"#,
        preamble = stub_preamble()
    ));

    let run = run_stub(&scratch, stub).await.expect("run completes");

    let summary = summary_of(&run);

    let digest = run.digest.expect("digest");
    assert!(
        summary.terminated_by_review.is_none(),
        "an active run must never be killed even once its answer exists, got {:?}",
        summary.terminated_by_review
    );
    assert_eq!(
        digest.exit.code(),
        Some(0),
        "stub must have exited on its own, not been killed"
    );
    assert!(
        digest.exit.signal_name().is_none(),
        "no signal: nothing killed this run, got {:?}",
        digest.exit
    );
    assert!(
        summary.captured,
        "the stub wrote the -o backstop, so this is a clean capture"
    );
    assert_eq!(run.text, "REAL ANSWER");
}

/// 4a. Pipe retention *only*: a descendant outlives its parent and holds the
///     stdout pipe open, so EOF never arrives. The parent exits normally, so
///     nothing is killed - this isolates "reaping is authoritative, and output
///     captured before the drain grace expired is preserved" from any question
///     of signal handling.
///
///     The descendant is killed explicitly at the end rather than left to time
///     out, so the test does not leak a process into the rest of the suite.
#[tokio::test]
async fn a_pipe_retaining_child_does_not_block_reaping_or_lose_output() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
rollout_message "PARTIAL" "final_answer"
# A non-JSON stdout line, which the digest surfaces as a log line. It is written
# *before* the pipe is stranded, so it is the evidence that output captured
# ahead of the drain-grace expiry is preserved rather than discarded.
printf '%s\n' 'ERROR: codex log line before the hang'
# A grandchild that keeps stdout open well past the drain grace. It inherits
# stdout, so EOF cannot arrive while it lives. Its pid is recorded so the test
# can clean it up instead of leaking it into the rest of the suite.
sleep 30 &
echo $! > "$LINGER_PID"
# Parent exits at once: review reaps it while the pipe is still held open.
exit 0
"#,
        preamble = stub_preamble()
    ));

    let started = std::time::Instant::now();
    let run = run_stub(&scratch, stub).await.expect("run completes");
    let elapsed = started.elapsed();

    // The run must not block on the grandchild's 30s sleep.
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "reaping must not wait for pipe EOF; took {elapsed:?}"
    );
    let digest = run.digest.expect("digest");
    assert_eq!(
        digest.exit.code(),
        Some(0),
        "the stub itself exited cleanly"
    );
    // The heart of this test: stdout never reached EOF, so the reader was
    // aborted on the grace expiry. What it had already buffered must survive.
    // An earlier version returned an empty buffer here, silently discarding the
    // NDJSON stream, the log lines and the forensic `stdout.jsonl` - in exactly
    // the scenario the code exists to diagnose.
    assert!(
        digest
            .codex()
            .expect("codex")
            .log_lines
            .iter()
            .any(|l| l.contains("codex log line before the hang")),
        "output captured before the drain grace expired must be preserved, got {:?}",
        digest.codex().expect("codex").log_lines
    );
    // The answer was written to the rollout before the hang, so recovery still
    // works even though the stream was cut short.
    assert_eq!(run.text, "PARTIAL");
    assert!(
        scratch.rollout_path().exists(),
        "sanity: the stub wrote its rollout"
    );

    // The pipe-holder is a leftover: the keeper ended it before the run
    // returned, and it is recorded, not reported.
    let lingering = scratch
        .linger_pid()
        .expect("stub recorded its background pid");
    let gone = !pid_alive(lingering);
    if gone {
        scratch.disarm_linger();
    }
    assert!(
        gone,
        "the keeper must end the pipe-holder before the run returns"
    );
    assert!(run.leftovers.is_clean(), "{:?}", run.leftovers);
    assert!(
        run.leftovers
            .killed
            .iter()
            // By pid: its recorded command line is whatever it was when last
            // seen alive, and a keeper that catches it between fork and exec
            // ends it there, still a copy of the stub shell (or `[sleep]`
            // mid-exec).
            .any(|p| u32::try_from(lingering).is_ok_and(|l| l == p.pid)),
        "the killed leftover is named in the record: {:?}",
        run.leftovers
    );
}

/// 4b. Escalation: a descendant that ignores `SIGTERM` while its parent hangs
///     after producing an answer. The watchdog fires, the keeper SIGTERMs the
///     run (which the descendant ignores) and escalates to `SIGKILL`. Asserts
///     the descendant is actually gone when the run returns, rather than
///     assuming the signal landed.
///
///     Split from 4a deliberately: that test exercises a leftover that dies on
///     its first SIGTERM after the harness exited on its own, this one a
///     terminate that has to escalate.
#[tokio::test]
async fn group_escalation_kills_a_sigterm_ignoring_descendant() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
rollout_message "ANSWER" "final_answer"
# A descendant that refuses SIGTERM outright. Only the keeper's SIGKILL
# escalation can reap it.
sh -c 'trap "" TERM; echo $$ > "$LINGER_PID"; sleep 300' &
# Parent hangs too, so the watchdog is what ends this run.
sleep 300
"#,
        preamble = stub_preamble()
    ));

    let run = run_stub(&scratch, stub).await.expect("run completes");

    let summary = summary_of(&run);

    assert!(run.digest.is_some(), "digest");
    assert!(
        summary
            .terminated_by_review
            .as_deref()
            .is_some_and(|why| why.contains("watchdog")),
        "the watchdog must be what ended this run, got {:?}",
        summary.terminated_by_review
    );

    // The keeper is reaped only after its teardown, so the descendant is gone
    // by the time the run returns.
    let stubborn = scratch
        .linger_pid()
        .expect("stub recorded its background pid");
    let reaped = !pid_alive(stubborn);
    // Disarm before asserting: if the escalation did *not* work, this process is
    // still alive with a 300-second sleep ahead of it, and the assertion below
    // would unwind past any cleanup written after it. `Scratch::drop` kills it
    // on the way out - which is exactly the case the drop guard exists for, so
    // the guard must stay armed here.
    if reaped {
        scratch.disarm_linger();
    }
    assert!(
        reaped,
        "a SIGTERM-ignoring descendant must be reaped by the SIGKILL escalation"
    );
    assert!(run.leftovers.is_clean(), "{:?}", run.leftovers);

    // A killed run is suspicious, so it must leave a forensic bundle - in the
    // scratch data root, never the operator's real one.
    assert_eq!(
        scratch.incident_dirs().len(),
        1,
        "a watchdog kill must write exactly one incident bundle"
    );
}

/// 5. A refused turn: codex states why it stopped and exits non-zero, having
///    produced only interim commentary. The real shape, from a run whose
///    recursion-depth hardening tripped the upstream cybersecurity classifier -
///    two `agent_message`s of working commentary, then `error` + `turn.failed`
///    carrying the same message, then exit 1 with no `final_answer` on disk.
///
///    Before `turn_error`, both events were dropped on the floor: the run was
///    reported as "died without a final answer", filed as a mystery death, and
///    auto-resumed straight into the identical refusal - which is why every
///    occurrence in the field left *two* incident bundles seconds apart.
#[tokio::test]
async fn a_stated_turn_failure_is_surfaced_not_reported_as_a_mystery_death() {
    let scratch = Scratch::new();
    const REFUSAL: &str = "This content was flagged for possible cybersecurity risk. \
If this seems wrong, try rephrasing your request.";
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
# Interim commentary only - no final_answer is ever written to the rollout,
# which is what makes this indistinguishable from a mid-turn death without the
# error events.
printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"I am now mapping the parser tests."}}}}'
rollout_message "I am now mapping the parser tests." "commentary"
# Codex says why it is stopping, as a pair of events carrying the same message.
printf '%s\n' '{{"type":"error","message":"{refusal}"}}'
printf '%s\n' '{{"type":"turn.failed","error":{{"message":"{refusal}"}}}}'
exit 1
"#,
        preamble = stub_preamble(),
        refusal = REFUSAL
    ));

    let run = run_stub(&scratch, stub).await.expect("run completes");
    let summary = summary_of(&run);
    let digest = run.digest.expect("digest");

    // The point of the fix: codex's stated reason is carried, not discarded.
    assert_eq!(
        summary.turn_error.as_deref(),
        Some(REFUSAL),
        "the turn.failed / error message must be captured"
    );
    // The surrounding failure signature is unchanged - this still *is* a run
    // that produced no answer, and must keep reporting as one.
    assert_eq!(digest.exit.code(), Some(1));
    assert!(!summary.captured, "no -o file was written");
    assert!(
        !summary.recovered_from_transcript,
        "there is no final_answer to recover - a refusal is not a truncation"
    );
    // It is persisted, so refusals stay greppable in the sidecar and audit logs.
    assert_eq!(summary.turn_error.as_deref(), Some(REFUSAL));

    // The interim note is still returned (it is all there is), but the reported
    // text must not be the bare "died without a final answer" fabrication.
    assert!(
        run.text.contains("mapping the parser tests"),
        "the interim commentary is still surfaced, got {:?}",
        run.text
    );

    // A refusal is still suspicious enough to bundle - the evidence is worth
    // keeping - but the bundle now carries the reason on its face, so
    // `review incidents` can say what happened instead of guessing "died".
    let dirs = scratch.incident_dirs();
    assert_eq!(dirs.len(), 1, "a refused run still writes one bundle");
    let meta = std::fs::read_to_string(dirs[0].join("meta.json")).expect("meta.json");
    assert!(
        meta.contains("This content was flagged"),
        "meta.json must record the stated reason, got {meta}"
    );
}

/// A stub that answers `SIGINT` the way `codex exec` does: it aborts the turn
/// and exits 1. `before` runs first - e.g. writing a final answer.
fn interruptible_stub(scratch: &Scratch, before: &str) -> String {
    scratch.write_stub(&format!(
        r#"{preamble}
rollout_message "I am now working." "commentary"
{before}
trap 'printf "%s\n" "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"turn_aborted\",\"reason\":\"interrupted\"}}}}" >> "$ROLL"; exit 1' INT
# Short sleeps, so the trap runs promptly: sh defers it until the foreground
# command returns.
while :; do sleep 0.05; done
"#,
        preamble = stub_preamble()
    ))
}

/// Run `stub` as the owner and `interrupt_cmd::interrupt` against it, the way
/// `review interrupt` would from another process. The owner's sidecar row -
/// which `invoke` does not write itself - is appended when its run
/// returns, as the fan-out task does.
fn publish_stub_handoff(runtime: &HarnessRuntime, run: &FixtureRun) {
    // These tests drive invoke without the app recording layer. Model the app's durable append
    // before publishing the exact invocation handoff, rather than fake a log
    // polling closure; app recording itself has an injected-domain test.
    let path = runtime
        .data
        .sessions(review::data::LogFamily::Public)
        .expect("sessions path");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let row = serde_json::json!({
        "timestamp": "t", "epoch_secs": 1, "project": "/p", "hostname": "h",
        "audit_id": "a", "provider": run.owner.harness.as_str(), "archetype": "bare",
        "session_id": run.session_id, "operator_prompt": "", "assembled_prompt": "", "review_version": "0",
    });
    use std::io::Write;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("log")
        .write_all(format!("{row}\n").as_bytes())
        .expect("append");
    review::sessions::handoff::publish(
        &runtime.data,
        &review::sessions::handoff::Handoff {
            schema: review::sessions::handoff::PROTOCOL,
            requested: run.owner.run_id.clone().expect("invoke run id"),
            interrupted: run.outcome.interrupted(),
            final_session: run.session_id.clone(),
            sidecar_written: true,
        },
    )
    .expect("publish handoff");
}

async fn interrupt_stub(
    scratch: &Scratch,
    stub: &str,
) -> (FixtureRun, Result<review::interrupt_cmd::Outcome>) {
    let mut runtime = fast_runtime(scratch);
    // The stub is silent while it waits to be interrupted; the stall timeout
    // must not be what ends it.
    runtime.timings.stall_grace = None;
    let data_root = scratch.data_root();

    let owner = async {
        let run = run_stub_with(scratch, stub, &runtime)
            .await
            .expect("run completes");
        publish_stub_handoff(&runtime, &run);
        run
    };
    let interrupter = async {
        // `review interrupt` refuses a run whose pid is not offered yet, so
        // wait for it the way an operator would retry.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while review::inflight::live_marker(STUB_SESSION, Some(&data_root))
            .filter(|m| m.signal_ready)
            .and_then(|m| m.keeper.map(review::process::ProcessIdentity::pid))
            .is_none()
        {
            assert!(
                std::time::Instant::now() < deadline,
                "codex's pid never offered"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        review::interrupt_cmd::interrupt(STUB_SESSION, Some(&data_root)).await
    };
    tokio::join!(owner, interrupter)
}

/// `review interrupt` end to end, against a run that has no answer yet. From
/// the exit status alone this is a mid-turn death; the request is what must
/// turn it into an interrupt: flagged on the digest (so the fan-out does not
/// auto-resume past it), no incident bundle, the request consumed, and the verb
/// reporting an interrupted session.
#[tokio::test]
async fn an_operator_interrupt_is_not_reported_as_a_death() {
    let scratch = Scratch::new();
    let stub = interruptible_stub(&scratch, "");
    let (run, outcome) = interrupt_stub(&scratch, &stub).await;
    let outcome = outcome.expect("the verb succeeds");
    assert!(outcome.interrupted, "the verb reports the interrupt");
    let summary = summary_of(&run);
    let digest = run.digest.expect("digest");

    assert!(summary.interrupted, "the digest must record the interrupt");
    assert!(summary.interrupted, "and persist it");
    assert!(
        summary
            .terminated_by_review
            .as_deref()
            .is_some_and(|why| why.contains("operator interrupt")),
        "got {:?}",
        summary.terminated_by_review
    );
    assert_eq!(digest.exit.code(), Some(1));
    assert_eq!(run.session_id.as_deref(), Some(STUB_SESSION));
    assert!(
        scratch.incident_dirs().is_empty(),
        "an interrupt is explained, so it is not an incident"
    );
    assert!(
        !review::inflight::take_interrupt_request(STUB_SESSION, Some(&scratch.data_root())),
        "the run consumed the request"
    );
}

/// A signalled `review` (an orchestrator stopping its background task) ends the
/// turn the way `review interrupt` does instead of killing codex: the run comes
/// back interrupted, with its session id, so it can be recorded and continued.
#[tokio::test]
async fn a_stop_signal_ends_the_turn_as_an_interrupt() {
    let scratch = Scratch::new();
    let stub = interruptible_stub(&scratch, "");
    let mut runtime = fast_runtime(&scratch);
    runtime.timings.stall_grace = None;
    let data_root = scratch.data_root();

    let stopper = async {
        // As in production, the SIGINT path is only taken once codex has
        // spoken; before that the group is killed instead.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while review::inflight::live_marker(STUB_SESSION, Some(&data_root))
            .filter(|m| m.signal_ready)
            .and_then(|m| m.keeper.map(review::process::ProcessIdentity::pid))
            .is_none()
        {
            assert!(
                std::time::Instant::now() < deadline,
                "codex's pid never offered"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        runtime.lifecycle.request_stop();
    };
    let (run, ()) = tokio::join!(run_stub_with(&scratch, &stub, &runtime), stopper);
    let run = run.expect("run completes");
    let summary = summary_of(&run);
    let digest = run.digest.expect("digest");

    assert!(summary.interrupted, "the digest must record the interrupt");
    assert_eq!(
        digest.exit.code(),
        Some(1),
        "codex answered SIGINT, not a kill"
    );
    assert!(
        summary
            .terminated_by_review
            .as_deref()
            .is_some_and(|why| why.contains("signalled")),
        "got {:?}",
        summary.terminated_by_review
    );
    assert_eq!(run.session_id.as_deref(), Some(STUB_SESSION));
    assert!(scratch.incident_dirs().is_empty());

    // Once stopped, nothing new launches.
    let refused = run_stub_with(&scratch, &stub, &runtime).await;
    assert!(
        refused.is_err_and(|e| e.to_string().contains("signalled to stop")),
        "a launch after the stop must be refused"
    );
}

/// A request is not an interrupt when the run already has its answer - codex
/// finished before the signal landed, or answered and then took the signal.
/// Calling that interrupted would report a completed run as cut short.
#[tokio::test]
async fn a_run_that_already_answered_is_not_marked_interrupted() {
    let scratch = Scratch::new();
    let stub = interruptible_stub(&scratch, r#"printf '%s' 'THE ANSWER' > "$LAST_MSG""#);
    let (run, outcome) = interrupt_stub(&scratch, &stub).await;

    let summary = summary_of(&run);

    assert!(run.digest.is_some(), "digest");
    assert!(summary.captured, "the answer was written before the signal");
    assert!(!summary.interrupted, "so the run is not an interrupt");
    assert!(summary.terminated_by_review.is_none());
    assert_eq!(run.text, "THE ANSWER");
    assert!(
        !outcome.expect("the verb succeeds").interrupted,
        "and the verb says the run ended first"
    );
}

// ---------------------------------------------------------------------------
// claude runner, against a stub `claude`
//
// The same process behaviours as codex - signals, process groups, the in-flight
// marker, interrupt requests - driven through `invoke`. The stub
// speaks claude's `stream-json`: an `init` event, then a `result`.
// ---------------------------------------------------------------------------

/// Shell fragment every claude stub starts with. Records its pid, environment
/// and arguments under `$STUB_DIR`, learns the session id from its argv as
/// claude does (`--session-id` fresh, `--resume` otherwise), and drains the
/// prompt. Prints nothing: each stub decides when claude "starts".
const CLAUDE_STUB_PREAMBLE: &str = r#"
printf '%s\n' "$$" > "$STUB_DIR/pid"
env > "$STUB_DIR/env"
printf '%s\n' "$*" > "$STUB_DIR/args"
SID=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--session-id" ] || [ "$prev" = "--resume" ]; then SID="$a"; fi
  prev="$a"
done
cat > /dev/null
"#;

/// Shell line printing claude's first event, which carries the session id.
/// maki's `init` has the same shape.
const CLAUDE_STUB_INIT: &str =
    r#"printf '%s\n' "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"$SID\"}""#;

/// Shell line printing a result event for this session.
fn claude_stub_result(is_error: bool, extra: &str) -> String {
    format!(
        r#"printf '%s\n' "{{\"type\":\"result\",\"is_error\":{is_error},\"session_id\":\"$SID\",\"num_turns\":1{extra}}}""#
    )
}

/// The Claude-compatible harnesses, which share one runner and therefore
/// every lifecycle scenario below; only their wire fixtures differ.
const CLAUDE_COMPAT: [Harness; 2] = [Harness::Claude, Harness::Maki];

/// Shell line printing `harness`'s answer `text`. maki answers only with
/// `stop_reason: "end_turn"`.
fn stub_answer(harness: Harness, text: &str) -> String {
    match harness {
        Harness::Maki => claude_stub_result(
            false,
            &format!(r#",\"result\":\"{text}\",\"stop_reason\":\"end_turn\""#),
        ),
        _ => claude_stub_result(false, &format!(r#",\"result\":\"{text}\""#)),
    }
}

/// Shell line printing `harness`'s stated failure `text`. maki's harness
/// failures carry `stop_reason: null` (probed: an API 402).
fn stub_failure(harness: Harness, text: &str) -> String {
    match harness {
        Harness::Maki => claude_stub_result(
            true,
            &format!(r#",\"subtype\":\"error\",\"result\":\"{text}\",\"stop_reason\":null"#),
        ),
        _ => claude_stub_result(true, &format!(r#",\"result\":\"{text}\""#)),
    }
}

/// The exit status `harness` ends with after answering SIGINT itself:
/// claude exits 0, maki 128+SIGINT.
fn sigint_exit(harness: Harness) -> i32 {
    match harness {
        Harness::Maki => 130,
        _ => 0,
    }
}

/// Shell fragment: on SIGINT, end the turn as `harness` does - print its
/// aborted result and exit - then wait for it.
fn stub_await_sigint(harness: Harness) -> String {
    let result = match harness {
        Harness::Maki => claude_stub_result(
            true,
            r#",\"subtype\":\"error\",\"stop_reason\":\"cancelled\""#,
        ),
        _ => claude_stub_result(
            true,
            r#",\"subtype\":\"error_during_execution\",\"terminal_reason\":\"aborted_tools\""#,
        ),
    };
    format!(
        "trap '{result}; exit {code}' INT\nwhile :; do sleep 0.05; done",
        code = sigint_exit(harness)
    )
}

/// Run `body` as claude through `invoke`: a fresh run, or a resume
/// of `resume`.
async fn run_claude_stub(
    scratch: &Scratch,
    body: &str,
    runtime: &HarnessRuntime,
    resume: Option<&str>,
) -> Result<FixtureRun> {
    run_compat_stub(Harness::Claude, scratch, body, runtime, resume).await
}

/// Run `body` as `harness` (claude or maki) through `invoke`.
async fn run_compat_stub(
    harness: Harness,
    scratch: &Scratch,
    body: &str,
    runtime: &HarnessRuntime,
    resume: Option<&str>,
) -> Result<FixtureRun> {
    let script = scratch.write_stub(&format!("{CLAUDE_STUB_PREAMBLE}\n{body}"));
    let mut runtime = runtime.clone();
    runtime.commands.set(
        harness,
        HarnessCommand {
            program: "/bin/sh".into(),
            leading_args: vec![script],
        },
    );
    let mut env = BTreeMap::new();
    env.insert(
        "STUB_DIR".to_string(),
        scratch.root.to_string_lossy().into_owned(),
    );
    fixture_run(
        invoke(
            review::harness::LaunchSpec {
                harness,
                start: {
                    match resume {
                        Some(id) => review::harness::SessionStart::Resume(
                            review::harness::SessionId::parse(id)?,
                        ),
                        None => review::harness::SessionStart::Fresh { id: None },
                    }
                },
                settings: fixture_settings(
                    harness,
                    review::config::Profile {
                        env: Some(env.clone()),
                        ..Default::default()
                    },
                ),
                prompt: ("review this").to_owned(),
                root: scratch.project(),
            },
            None,
            &runtime,
            None,
        )
        .await,
    )
}

#[tokio::test]
async fn a_claude_resume_reports_the_session_id_it_was_given() {
    let scratch = Scratch::new();
    let sid = "01920000-0000-7000-8000-000000000002";
    let body = format!(
        "{CLAUDE_STUB_INIT}\nprintf '%s\\n' '{{\"type\":\"result\",\"is_error\":false,\"result\":\"continued\"}}'"
    );
    let run = run_claude_stub(&scratch, &body, &fast_runtime(&scratch), Some(sid))
        .await
        .expect("resume completes");
    assert_eq!(run.session_id.as_deref(), Some(sid));
}

/// The session id of the one `harness` run in flight under `scratch`, once
/// its pid is offered for signalling - which is what `review interrupt` waits
/// for.
async fn compat_signalable(harness: Harness, scratch: &Scratch) -> String {
    let data_root = scratch.data_root();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(m) = review::inflight::read_live(Some(&data_root))
            .into_iter()
            .find(|m| m.harness == harness.as_str() && m.signal_ready)
        {
            return m.session_id.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{harness}'s pid never offered"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Run `body` as the owner and `review interrupt` against it from "another
/// process", appending the owner's sidecar row when its run returns, as the
/// fan-out task does. The interrupt is sent once the harness's pid is offered.
async fn compat_interrupt_stub(
    harness: Harness,
    scratch: &Scratch,
    body: &str,
) -> (FixtureRun, Result<review::interrupt_cmd::Outcome>) {
    compat_interrupt_stub_at(harness, scratch, body, true).await
}

/// As `compat_interrupt_stub`, but `after_pid: false` sends the interrupt as
/// soon as the run is marked in flight - before the first event, as a second
/// `review message` does when its launch lock hands over.
async fn compat_interrupt_stub_at(
    harness: Harness,
    scratch: &Scratch,
    body: &str,
    after_pid: bool,
) -> (FixtureRun, Result<review::interrupt_cmd::Outcome>) {
    let runtime = fast_runtime(scratch);
    let data_root = scratch.data_root();
    let owner = async {
        let run = run_compat_stub(harness, scratch, body, &runtime, None)
            .await
            .expect("run completes");
        publish_stub_handoff(&runtime, &run);
        run
    };
    let interrupter = async {
        let sid = if after_pid {
            compat_signalable(harness, scratch).await
        } else {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if let Some(m) = review::inflight::read_live(Some(&data_root)).pop() {
                    assert!(!m.signal_ready, "the stub has not spoken yet");
                    break m.session_id.clone();
                }
                assert!(std::time::Instant::now() < deadline, "no marker");
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        };
        review::interrupt_cmd::interrupt(&sid, Some(&data_root)).await
    };
    tokio::join!(owner, interrupter)
}

#[tokio::test]
async fn a_claude_run_launches_with_background_off_and_reports_its_answer() {
    let scratch = Scratch::new();
    let body = format!(
        "{CLAUDE_STUB_INIT}\n{}",
        claude_stub_result(false, r#",\"result\":\"THE ANSWER\""#)
    );
    let run = run_claude_stub(&scratch, &body, &fast_runtime(&scratch), None)
        .await
        .expect("run completes");
    assert_eq!(run.text, "THE ANSWER");
    assert!(run.digest.is_none(), "a clean answer carries no digest");
    let args = std::fs::read_to_string(scratch.root.join("args")).expect("args");
    let sid = run.session_id.expect("a fresh run reports its session");
    assert!(
        args.contains(&format!("--session-id {sid}")),
        "the id recorded is the one claude was given: {args}"
    );
    assert!(
        args.contains("--output-format stream-json --verbose"),
        "{args}"
    );
    let env = std::fs::read_to_string(scratch.root.join("env")).expect("env");
    for var in [
        "CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1",
        "BASH_DEFAULT_TIMEOUT_MS=1200000",
        "BASH_MAX_TIMEOUT_MS=1200000",
    ] {
        assert!(env.lines().any(|l| l == var), "{var} must reach claude");
    }
    assert!(
        review::inflight::read_live(Some(&scratch.data_root()))
            .iter()
            .all(|m| m.phase == review::inflight::Phase::HandoffPending),
        "invoke retains pending evidence until recording or result drop"
    );
}

#[tokio::test]
async fn review_interrupt_ends_a_claude_compatible_turn_and_keeps_its_session() {
    for harness in CLAUDE_COMPAT {
        let scratch = Scratch::new();
        let body = format!("{CLAUDE_STUB_INIT}\n{}", stub_await_sigint(harness));
        let (run, outcome) = compat_interrupt_stub(harness, &scratch, &body).await;
        let outcome = outcome.expect("the verb succeeds");
        assert!(
            outcome.interrupted,
            "{harness}: the verb reports the interrupt"
        );
        assert_eq!(outcome.harness, harness);
        let summary = summary_of(&run);
        let digest = run.digest.expect("digest");
        assert!(summary.interrupted, "{harness}");
        assert!(
            summary.turn_error.is_none(),
            "{harness}'s complaint about the abort is not a stated failure"
        );
        assert_eq!(
            digest.exit.code(),
            Some(sigint_exit(harness)),
            "{harness} answered SIGINT itself"
        );
        assert!(
            summary
                .terminated_by_review
                .as_deref()
                .is_some_and(|why| why.contains("operator interrupt")),
            "{harness}: got {:?}",
            summary.terminated_by_review
        );
        assert!(
            run.text.contains("interrupted"),
            "{harness}: got {:?}",
            run.text
        );
        let sid = run.session_id.expect("the session survives the interrupt");
        assert!(
            !review::inflight::take_interrupt_request(&sid, Some(&scratch.data_root())),
            "{harness}: the run consumed the request"
        );
    }
}

#[tokio::test]
async fn review_interrupt_waits_for_a_claude_compatible_run_that_has_not_spoken_yet() {
    // A second `review message` meets a run in the second after it launched,
    // before the first event offers its pid. Refusing then lost the message;
    // the verb waits for the pid instead.
    for harness in CLAUDE_COMPAT {
        let scratch = Scratch::new();
        let body = format!(
            "sleep 1\n{CLAUDE_STUB_INIT}\n{}",
            stub_await_sigint(harness)
        );
        let (run, outcome) = compat_interrupt_stub_at(harness, &scratch, &body, false).await;
        assert!(
            outcome
                .expect("the verb waits rather than refusing")
                .interrupted,
            "{harness}: and then interrupts"
        );
        assert!(run.outcome.interrupted(), "{harness}");
    }
}

#[tokio::test]
async fn a_claude_compatible_answer_that_beats_the_interrupt_stands() {
    for harness in CLAUDE_COMPAT {
        let scratch = Scratch::new();
        let body = format!(
            "{CLAUDE_STUB_INIT}\n{}\ntrap 'exit 0' INT\nwhile :; do sleep 0.05; done",
            stub_answer(harness, "THE ANSWER")
        );
        let (run, outcome) = compat_interrupt_stub(harness, &scratch, &body).await;
        assert_eq!(run.text, "THE ANSWER", "{harness}");
        assert!(!run.outcome.interrupted(), "{harness}");
        assert!(
            !outcome.expect("the verb succeeds").interrupted,
            "{harness}: the verb says the run ended first"
        );
    }
}

#[tokio::test]
async fn a_stop_signal_ends_a_started_claude_compatible_turn_as_an_interrupt() {
    for harness in CLAUDE_COMPAT {
        let scratch = Scratch::new();
        let runtime = fast_runtime(&scratch);
        let body = format!("{CLAUDE_STUB_INIT}\n{}", stub_await_sigint(harness));
        let stopper = async {
            compat_signalable(harness, &scratch).await;
            runtime.lifecycle.request_stop();
        };
        let (run, ()) = tokio::join!(
            run_compat_stub(harness, &scratch, &body, &runtime, None),
            stopper
        );
        let run = run.expect("run completes");
        let summary = summary_of(&run);
        let digest = run.digest.expect("digest");
        assert!(summary.interrupted, "{harness}");
        assert_eq!(
            digest.exit.code(),
            Some(sigint_exit(harness)),
            "{harness}: SIGINT, not a group kill"
        );
        assert!(
            summary
                .terminated_by_review
                .as_deref()
                .is_some_and(|why| why.contains("signalled")),
            "{harness}"
        );
        assert!(run.session_id.is_some(), "{harness}");

        // Once stopped, nothing new launches - a run waiting on the launch
        // lock must not start only to be killed.
        let refused = run_compat_stub(harness, &scratch, &body, &runtime, None).await;
        assert!(
            refused.is_err_and(|e| e.to_string().contains("not launched")),
            "{harness}: a launch after the stop must be refused"
        );
    }
}

#[tokio::test]
async fn a_claude_compatible_run_stopped_before_its_first_event_names_no_session() {
    // Killed before the harness could have written a session: recording one
    // would print a resume command for a session that does not exist.
    for harness in CLAUDE_COMPAT {
        let scratch = Scratch::new();
        let runtime = fast_runtime(&scratch);
        let data_root = scratch.data_root();
        let stopper = async {
            // The marker goes up at spawn; the stub never speaks, so no pid is
            // offered.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while review::inflight::read_live(Some(&data_root)).is_empty() {
                assert!(std::time::Instant::now() < deadline, "no marker");
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            runtime.lifecycle.request_stop();
        };
        let body = "while :; do sleep 0.05; done";
        let (run, ()) = tokio::join!(
            run_compat_stub(harness, &scratch, body, &runtime, None),
            stopper
        );
        let Err(err) = run else {
            panic!("{harness}: a run stopped before it started has no session to report");
        };
        assert!(err.to_string().contains("not started"), "{harness}: {err}");
        assert!(
            err.to_string().contains(harness.as_str()),
            "{harness}: {err}"
        );
        assert!(review::inflight::read_live(Some(&data_root)).is_empty());
    }
}

#[tokio::test]
async fn a_claude_compatible_run_that_dies_after_starting_keeps_its_session() {
    // Killed from outside or crashed after its first event: no result, but the
    // session may exist and is the turn most worth resuming.
    for harness in CLAUDE_COMPAT {
        let scratch = Scratch::new();
        let body = format!("{CLAUDE_STUB_INIT}\nexit 3");
        let run = run_compat_stub(harness, &scratch, &body, &fast_runtime(&scratch), None)
            .await
            .expect("a death after starting is not a launch failure");
        let summary = summary_of(&run);
        let digest = run.digest.expect("digest");
        assert!(!summary.captured, "{harness}");
        assert!(!summary.interrupted, "{harness}");
        assert!(
            summary.turn_error.is_none(),
            "{harness} stated no reason, so this is a death, not a stated failure"
        );
        assert_eq!(digest.exit.code(), Some(3), "{harness}");
        assert!(
            run.session_id.is_some(),
            "{harness}: the session id is kept"
        );
        assert!(
            run.text
                .contains(&format!("{harness} ended without a result")),
            "got {:?}",
            run.text
        );
    }
}

#[tokio::test]
async fn output_before_init_is_not_a_claude_compatible_start() {
    // A stray byte or diagnostic, then a crash, before the `init` event: the
    // harness never reached the point of having a session, so none is kept.
    for harness in CLAUDE_COMPAT {
        let scratch = Scratch::new();
        let body = "printf 'booting'\nexit 1";
        let run = run_compat_stub(harness, &scratch, body, &fast_runtime(&scratch), None).await;
        assert!(
            run.is_err(),
            "{harness}: no init event, no session: a launch failure, not a death"
        );
    }
}

#[tokio::test]
async fn a_stale_interrupt_request_does_not_mark_a_claude_compatible_resume_interrupted() {
    // Left by a `review` killed before it consumed it. A resume that then fails
    // for its own reason must report that reason, not an interrupt.
    for harness in CLAUDE_COMPAT {
        let scratch = Scratch::new();
        let sid = "0c1a0000-0000-4000-8000-000000000001";
        std::fs::create_dir_all(scratch.data_root().join("review/inflight")).expect("dir");
        review::inflight::request_interrupt(sid, Some(&scratch.data_root())).expect("request");
        let body = format!(
            "{CLAUDE_STUB_INIT}\n{}\nexit 1",
            stub_failure(harness, "rate limited")
        );
        let run = run_compat_stub(harness, &scratch, &body, &fast_runtime(&scratch), Some(sid))
            .await
            .expect("run completes");
        let summary = summary_of(&run);
        assert!(run.digest.is_some(), "{harness}: digest");
        assert!(
            !summary.interrupted,
            "{harness}: the request was not aimed at this run"
        );
        assert!(
            summary
                .turn_error
                .as_deref()
                .is_some_and(|e| e.contains("rate limited")),
            "{harness}'s reason is kept: {:?}",
            summary.turn_error
        );
    }
}

#[tokio::test]
async fn a_maki_error_result_with_exit_zero_is_a_stated_failure() {
    // Probed: maki exits 0 when its result says `is_error: true` (an API 402).
    // The exit code says nothing; the result decides.
    let scratch = Scratch::new();
    let body = format!(
        "{CLAUDE_STUB_INIT}\n{}\nexit 0",
        stub_failure(Harness::Maki, "API error (402): no credits")
    );
    let run = run_compat_stub(
        Harness::Maki,
        &scratch,
        &body,
        &fast_runtime(&scratch),
        None,
    )
    .await
    .expect("an error result is a turn");
    let summary = summary_of(&run);
    assert!(!summary.captured);
    assert!(
        summary
            .turn_error
            .as_deref()
            .is_some_and(|e| e.contains("API error (402)")),
        "{:?}",
        summary.turn_error
    );
    assert_eq!(run.digest.expect("digest").exit.code(), Some(0));
    assert!(run.session_id.is_some());
}

#[tokio::test]
async fn a_maki_cancelled_by_someone_else_is_a_stated_cancellation() {
    // A SIGTERM delivered to maki directly, not through review: maki prints a
    // `cancelled` result and exits 143. With no interrupt request, that is
    // maki's stated cancellation, not an interrupt by the operator.
    let scratch = Scratch::new();
    let body = format!(
        "{CLAUDE_STUB_INIT}\n{}\nexit 143",
        claude_stub_result(
            true,
            r#",\"subtype\":\"error\",\"stop_reason\":\"cancelled\""#
        )
    );
    let run = run_compat_stub(
        Harness::Maki,
        &scratch,
        &body,
        &fast_runtime(&scratch),
        None,
    )
    .await
    .expect("a cancelled result is a turn");
    let summary = summary_of(&run);
    assert!(!summary.interrupted);
    assert!(
        summary
            .turn_error
            .as_deref()
            .is_some_and(|e| e.contains("stop_reason: cancelled")),
        "{:?}",
        summary.turn_error
    );
    assert_eq!(run.digest.expect("digest").exit.code(), Some(143));
    assert!(run.session_id.is_some());
}

#[tokio::test]
async fn a_truncated_maki_answer_is_not_an_answer() {
    let scratch = Scratch::new();
    let body = format!(
        "{CLAUDE_STUB_INIT}\n{}",
        claude_stub_result(
            false,
            r#",\"result\":\"half an answ\",\"stop_reason\":\"max_tokens\""#
        )
    );
    let run = run_compat_stub(
        Harness::Maki,
        &scratch,
        &body,
        &fast_runtime(&scratch),
        None,
    )
    .await
    .expect("a truncated result is a turn");
    let summary = summary_of(&run);
    assert!(
        !summary.captured,
        "max_tokens with is_error false is not an answer"
    );
    assert!(
        summary
            .turn_error
            .as_deref()
            .is_some_and(|e| e.contains("max_tokens")),
        "{:?}",
        summary.turn_error
    );
    assert_eq!(run.text, "half an answ", "the partial text is kept");
}

/// Whether `session_id`'s marker is readable once this task sees `launched`
/// fire. `review message` releases its session launch lock on that signal, so
/// a marker written any later leaves a window in which a second message sees
/// no turn in flight and launches beside this one.
///
/// What this can and cannot catch: it runs when the test task is next
/// scheduled after the signal, not at the signal itself. It reliably catches
/// a marker written from a task spawned after launch - the shape the runners
/// had, which failed it 5 times in 5 - but not one written synchronously just
/// after the signal, before the runner yields. That the marker is written
/// before the spawn is stated where each runner writes it; this test guards
/// against the realistic regression, not every conceivable one.
async fn marked_at_launch(
    launched: tokio::sync::oneshot::Receiver<()>,
    session_id: &str,
    data_root: &std::path::Path,
) -> bool {
    launched.await.expect("the run signals its launch");
    review::inflight::live_marker(session_id, Some(data_root)).is_some()
}

#[tokio::test]
async fn a_claude_resume_is_marked_in_flight_before_it_launches() {
    let scratch = Scratch::new();
    let sid = "0c1a0000-0000-4000-8000-000000000002";
    let body = format!(
        "{CLAUDE_STUB_PREAMBLE}\n{CLAUDE_STUB_INIT}\n{}",
        claude_stub_result(false, r#",\"result\":\"ok\""#)
    );
    let mut runtime = fast_runtime(&scratch);
    runtime.commands.set(
        Harness::Claude,
        HarnessCommand {
            program: "/bin/sh".into(),
            leading_args: vec![scratch.write_stub(&body)],
        },
    );
    let mut env = BTreeMap::new();
    env.insert(
        "STUB_DIR".to_string(),
        scratch.root.to_string_lossy().into_owned(),
    );
    let (tx, rx) = tokio::sync::oneshot::channel();
    let data_root = scratch.data_root();
    let project = scratch.project();
    let start = review::harness::SessionStart::Resume(
        review::harness::SessionId::parse(sid).expect("test id"),
    );
    let selection =
        review::harness::ModelSpec::new(review::harness::Harness::Claude, None, None, &[]);
    let launch = review::harness::LaunchSpec {
        harness: Harness::Claude,
        start: start.clone(),
        settings: fixture_settings(
            Harness::Claude,
            review::config::Profile {
                model: selection.model().map(str::to_owned),
                effort: selection.effort().map(str::to_owned),
                env: Some(env.clone()),
                ..Default::default()
            },
        ),
        prompt: ("more").to_owned(),
        root: project.clone(),
    };
    let (run, marked) = tokio::join!(
        invoke(launch, Some(tx), &runtime, None),
        marked_at_launch(rx, sid, &data_root)
    );
    assert!(run.output.is_ok_and(|r| r.text == "ok"));
    assert!(marked, "the marker must exist when the launch is signalled");
}

#[tokio::test]
async fn a_codex_resume_is_marked_in_flight_before_it_launches() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        "{}\nprintf '%s' 'ok' > \"$LAST_MSG\"",
        stub_preamble()
    ));
    let runtime = fast_runtime(&scratch);
    let (tx, rx) = tokio::sync::oneshot::channel();
    let data_root = scratch.data_root();
    let (run, marked) = tokio::join!(
        run_codex_stub(&scratch, &stub, &runtime, Some(STUB_SESSION), Some(tx)),
        marked_at_launch(rx, STUB_SESSION, &data_root)
    );
    assert!(run.is_ok());
    assert!(marked, "the marker must exist when the launch is signalled");
}

/// The ticket's case: the agent started a hung script in a session of its own
/// (codex's sandbox runs bwrap with `--new-session`), and codex finished. The
/// script escaped every process group `review` knew about and used to run for
/// hours after `review` reported the run over. The keeper adopts it as a
/// subreaper and ends it - SIGKILL, since it ignores SIGTERM - before the run
/// returns, and the record names it.
#[tokio::test]
async fn a_hung_process_in_its_own_session_is_ended_when_the_run_ends() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
setsid sh -c 'trap "" TERM; echo $$ > "$LINGER_PID"; while :; do sleep 1; done' &
# Wait until it has recorded itself, so the test can find it.
while [ ! -s "$LINGER_PID" ]; do sleep 0.01; done
rollout_message "DONE" "final_answer"
printf '%s' 'DONE' > "$LAST_MSG"
exit 0
"#,
        preamble = stub_preamble()
    ));
    let run = run_stub(&scratch, stub).await.expect("run completes");
    assert_eq!(run.text, "DONE");
    // The teardown takes longer than the stub's quiet grace (the script needs
    // the SIGKILL rounds), and must not read as codex refusing to exit.
    let summary = summary_of(&run);
    assert!(run.digest.is_some(), "codex digest");
    assert_eq!(summary.terminated_by_review, None);
    assert!(
        scratch.incident_dirs().is_empty(),
        "a clean run is no incident"
    );
    let hung = scratch.linger_pid().expect("stub recorded the hung script");
    let gone = !pid_alive(hung);
    if gone {
        scratch.disarm_linger();
    }
    assert!(gone, "the run must not return while the hung script lives");
    assert!(run.leftovers.is_clean(), "{:?}", run.leftovers);
    assert!(
        run.leftovers
            .killed
            .iter()
            .any(|p| u32::try_from(hung).is_ok_and(|h| h == p.pid)),
        "the killed script is recorded: {:?}",
        run.leftovers
    );
}

/// A daemonising double fork reparents to the nearest subreaper, which is the
/// keeper, so it is torn down like any other leftover.
#[tokio::test]
async fn a_double_forked_daemon_is_ended_when_the_run_ends() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
( setsid sleep 300 & echo $! > "$LINGER_PID" )
while [ ! -s "$LINGER_PID" ]; do sleep 0.01; done
printf '%s' 'DONE' > "$LAST_MSG"
exit 0
"#,
        preamble = stub_preamble()
    ));
    let run = run_stub(&scratch, stub).await.expect("run completes");
    let daemon = scratch.linger_pid().expect("stub recorded the daemon");
    let gone = !pid_alive(daemon);
    if gone {
        scratch.disarm_linger();
    }
    assert!(gone, "the daemon must be ended before the run returns");
    assert!(run.leftovers.is_clean(), "{:?}", run.leftovers);
}

/// A keeper that dies cannot vouch for anything: the run's teardown outcome is
/// unknown, which goes in the ledger (failing the command) and never reads as
/// clean. The run itself is still `Ok`, so its session survives to be
/// inspected or continued, as for any death.
#[tokio::test]
async fn a_keeper_that_dies_leaves_the_outcome_unknown() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
echo $$ > "$LINGER_PID"
# This shell's parent is its keeper.
kill -9 $PPID
sleep 1
"#,
        preamble = stub_preamble()
    ));
    let runtime = fast_runtime(&scratch);
    let run = run_stub_with(&scratch, &stub, &runtime)
        .await
        .expect("the run is still Ok");
    assert_eq!(
        run.session_id.as_deref(),
        Some(STUB_SESSION),
        "the session survives"
    );
    assert!(run.leftovers.unknown.is_some(), "{:?}", run.leftovers);
    let digest = run.digest.as_ref().expect("digest");
    assert_eq!(
        digest.exit.code(),
        None,
        "an unreported status is not invented"
    );
    assert!(
        runtime
            .lifecycle
            .unresolved()
            .iter()
            .any(|u| u.leftovers.unknown.is_some() && u.label.contains("codex")),
        "the ledger records the unknown outcome"
    );
    assert!(
        fast_runtime(&scratch).lifecycle.unresolved().is_empty(),
        "another runtime cannot inherit the unknown teardown"
    );
}

/// A harness that cannot be executed fails the launch, with the reason.
#[tokio::test]
async fn a_harness_that_cannot_exec_fails_the_launch() {
    let scratch = Scratch::new();
    let mut runtime = fast_runtime(&scratch);
    runtime.commands.set(
        Harness::Codex,
        HarnessCommand {
            program: scratch
                .root
                .join("no-such-codex")
                .to_string_lossy()
                .into_owned(),
            leading_args: Vec::new(),
        },
    );
    let err = match run_stub_with(&scratch, "unused", &runtime).await {
        Ok(_) => panic!("a missing binary must fail the launch"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("failed to launch"), "{err}");
}

#[tokio::test]
async fn claude_answer_with_nonzero_exit_keeps_answer_and_all_turn_usage() {
    let scratch = Scratch::new();
    let body = format!(
        "{CLAUDE_STUB_INIT}\n{}\n{}\nexit 7",
        claude_stub_result(
            false,
            r#",\"result\":\"LAUNCHED\",\"usage\":{\"input_tokens\":11,\"output_tokens\":13}"#,
        ),
        claude_stub_result(
            false,
            r#",\"result\":\"THE ANSWER\",\"usage\":{\"input_tokens\":17,\"output_tokens\":19}"#,
        ),
    );
    let run = run_claude_stub(&scratch, &body, &fast_runtime(&scratch), None)
        .await
        .expect("the answer survives the exit");
    assert_eq!(run.text, "THE ANSWER");
    assert!(run.session_id.is_some());
    let summary = summary_of(&run);
    let digest = run.digest.expect("nonzero status is recorded");
    assert!(summary.captured);
    assert_eq!(digest.exit.code(), Some(7));
    assert!(summary.turn_error.is_none());
    assert!(summary.review_error.is_none());
    assert!(!run.outcome.failed());
    assert_eq!(digest.turns, 2);
    assert_eq!(digest.usage.total_input, 28);
    assert_eq!(digest.usage.output, 32);
}

#[tokio::test]
async fn model_selection_arguments_and_record_share_the_prepared_spec() {
    use review::harness::{Harness, SessionStart};
    let scratch = Scratch::new();
    let script = scratch.write_stub(&format!(
        "{CLAUDE_STUB_PREAMBLE}\n{CLAUDE_STUB_INIT}\n{}",
        claude_stub_result(false, r#",\"result\":\"ok\""#)
    ));
    let mut runtime = fast_runtime(&scratch);
    runtime.commands.set(
        Harness::Claude,
        HarnessCommand {
            program: "/bin/sh".into(),
            leading_args: vec![script],
        },
    );
    let env = BTreeMap::from([(
        "STUB_DIR".into(),
        scratch.root.to_string_lossy().into_owned(),
    )]);
    let result = review::harness::invoke(
        review::harness::LaunchSpec {
            harness: Harness::Claude,
            start: SessionStart::Fresh { id: None },
            settings: fixture_settings(
                Harness::Claude,
                review::config::Profile {
                    model: Some("chosen-model".into()),
                    effort: Some("high".into()),
                    env: Some(env.clone()),
                    ..Default::default()
                },
            ),
            prompt: ("review this").to_owned(),
            root: scratch.project(),
        },
        None,
        &runtime,
        None,
    )
    .await;
    assert_eq!(result.output.expect("answer").text, "ok");
    assert_eq!(result.model.as_deref(), Some("chosen-model"));
    assert_eq!(result.effort.as_deref(), Some("high"));
    let args = std::fs::read_to_string(scratch.root.join("args")).expect("stub args");
    assert!(
        args.contains("--model chosen-model --effort high"),
        "{args}"
    );
}

#[tokio::test]
async fn invalid_model_selection_is_refused_before_a_session_launches() {
    use review::harness::{Harness, SessionStart};
    let scratch = Scratch::new();
    let result = review::harness::invoke(
        review::harness::LaunchSpec {
            harness: Harness::Codex,
            start: SessionStart::Fresh { id: None },
            settings: fixture_settings(
                Harness::Codex,
                review::config::Profile {
                    model: Some("chosen".into()),
                    effort: Some("high".into()),
                    env: None,
                    config: vec!["model_reasoning_effort=123".into()],
                    ..Default::default()
                },
            ),
            prompt: ("review this").to_owned(),
            root: scratch.project(),
        },
        None,
        &fast_runtime(&scratch),
        None,
    )
    .await;
    let error = result
        .output
        .expect_err("invalid config refused")
        .to_string();
    assert!(error.contains("must be a string"), "{error}");
    assert!(result.session_id.is_none());
    assert!(result.permissions.is_none());
    assert!(result.model.is_none());
    assert!(result.effort.is_none());
    assert!(!scratch.codex_home().join("pid").exists());
}

#[test]
fn command_prefix_defaults_and_replacement_cover_every_harness() {
    let mut commands = HarnessCommands::default();
    for &harness in Harness::all() {
        assert_eq!(commands.get(harness).program, harness.binary());
        assert!(commands.get(harness).leading_args.is_empty());
    }
    let original = commands.clone();
    commands.set(
        Harness::Codex,
        HarnessCommand {
            program: "/bin/sh".into(),
            leading_args: vec!["fixture with spaces".into()],
        },
    );
    assert_eq!(commands.get(Harness::Claude), original.get(Harness::Claude));
    assert_eq!(commands.get(Harness::Grok), original.get(Harness::Grok));
    assert_eq!(
        commands.get(Harness::Codex).leading_args,
        ["fixture with spaces"]
    );
}

#[tokio::test]
async fn codex_prefix_is_preserved_in_spawn_probe_and_replay_metadata() {
    let scratch = Scratch::new();
    let script = scratch.root.join("provider with spaces.sh");
    std::fs::write(&script, r#"
[ "$1" = 'prefix with spaces' ] || exit 61
shift
if [ "$1" = --version ]; then
    [ "$#" = 1 ] || exit 62
    printf 'fixture-version\n'
    exit 0
fi
[ "$1" = exec ] || exit 63
printf '%s\n' "$@" > "$CODEX_HOME/args"
cat >/dev/null
printf '%s\n' '{"type":"error","message":"fixture refusal"}' '{"type":"turn.failed","error":{"message":"fixture refusal"}}'
exit 1
"#).expect("write prefix fixture");
    let prefix = vec![
        script.to_string_lossy().into_owned(),
        "prefix with spaces".into(),
    ];
    let mut runtime = fast_runtime(&scratch);
    runtime.commands.set(
        Harness::Codex,
        HarnessCommand {
            program: "/bin/sh".into(),
            leading_args: prefix.clone(),
        },
    );
    let run = fixture_run(
        invoke(
            review::harness::LaunchSpec {
                harness: Harness::Codex,
                start: review::harness::SessionStart::Fresh { id: None },
                settings: fixture_settings(
                    Harness::Codex,
                    review::config::Profile {
                        env: Some(BTreeMap::from([(
                            "CODEX_HOME".into(),
                            scratch.codex_home().to_string_lossy().into_owned(),
                        )])),
                        ..Default::default()
                    },
                ),
                prompt: ("prompt").to_owned(),
                root: scratch.project(),
            },
            None,
            &runtime,
            None,
        )
        .await,
    )
    .expect("fixture ran");
    let args: Vec<String> = std::fs::read_to_string(scratch.codex_home().join("args"))
        .expect("observed argv")
        .lines()
        .map(str::to_owned)
        .collect();
    assert!(
        run.outcome
            .stated
            .expect("stated failure")
            .contains("fixture refusal")
    );
    let dirs = scratch.incident_dirs();
    assert_eq!(dirs.len(), 1);
    let meta: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dirs[0].join("meta.json")).expect("metadata"),
    )
    .expect("metadata json");
    assert_eq!(meta["codex_version"], "fixture-version");
    let expected: Vec<String> = std::iter::once("/bin/sh".to_string())
        .chain(prefix)
        .chain(args)
        .collect();
    assert_eq!(meta["argv"], serde_json::json!(expected));
    assert!(
        meta["command"]
            .as_str()
            .expect("replay command")
            .contains("'prefix with spaces'")
    );
}

#[tokio::test]
async fn empty_command_program_fails_to_launch() {
    let scratch = Scratch::new();
    let mut runtime = fast_runtime(&scratch);
    runtime.commands.set(
        Harness::Codex,
        HarnessCommand {
            program: String::new(),
            leading_args: Vec::new(),
        },
    );
    let err = match run_stub_with(&scratch, "unused", &runtime).await {
        Ok(_) => panic!("empty executable must fail"),
        Err(error) => error,
    };
    assert!(
        err.to_string().contains("invalid keeper launch bytes"),
        "{err:#}"
    );
}

#[tokio::test]
async fn grok_prefix_drives_fresh_resume_and_stated_failure_through_invoke() {
    use review::harness::{SessionId, SessionStart};
    let scratch = Scratch::new();
    let script = scratch.write_stub(r#"
[ "$1" = 'prefix with spaces' ] || exit 71
shift
printf '%s\n' "$@" > "$STUB_DIR/args"
prompt= sid= mode=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --session-id|--resume)
            [ "$#" -ge 2 ] && [ -z "$sid" ] && [ -n "$2" ] || exit 73
            mode=$1; sid=$2; shift 2 ;;
        --prompt-file)
            [ "$#" -ge 2 ] && [ -z "$prompt" ] && [ -n "$2" ] || exit 74
            prompt=$2; shift 2 ;;
        *) shift ;;
    esac
done
[ -n "$sid" ] && [ -f "$prompt" ] || exit 72
cat "$prompt" > "$STUB_DIR/prompt"
printf '%s\n' "$PWD" > "$STUB_DIR/cwd"
printf '%s\n' "$mode" "$sid" > "$STUB_DIR/session"
printf '%s %s\n' "$mode" "$sid" >> "$STUB_DIR/trace"
mkdir -p "$GROK_HOME/sessions/fixture/$sid"
: > "$GROK_HOME/sessions/fixture/$sid/updates.jsonl"
if [ "$STUB_WAKE" = 1 ] && [ "$mode" = --session-id ]; then
    printf '%s\n' \
      '{"params":{"update":{"sessionUpdate":"turn_completed"}}}' \
      '{"params":{"update":{"sessionUpdate":"task_completed","task_snapshot":{"task_id":"fixture-task","command":"fixture build","exit_code":0,"output":"fixture build passed"}}}}' \
      > "$GROK_HOME/sessions/fixture/$sid/updates.jsonl"
fi
printf '{"text":"fixture answer","stopReason":"%s","sessionId":"%s","numTurns":1}\n' "$STUB_STOP" "$sid"
"#);
    let mut runtime = fast_runtime(&scratch);
    runtime.commands.set(
        Harness::Grok,
        HarnessCommand {
            program: "/bin/sh".into(),
            leading_args: vec![script, "prefix with spaces".into()],
        },
    );
    let sid = "grok-prefix-session";
    let mut env = BTreeMap::from([
        (
            "GROK_HOME".into(),
            scratch
                .root
                .join("grok-home")
                .to_string_lossy()
                .into_owned(),
        ),
        (
            "STUB_DIR".into(),
            scratch.root.to_string_lossy().into_owned(),
        ),
        ("STUB_STOP".into(), "end_turn".into()),
    ]);
    let assert_args = |mode: &str, expected_sid: &str| {
        let args = std::fs::read_to_string(scratch.root.join("args")).expect("args");
        let lines: Vec<_> = args.lines().collect();
        for (flag, value) in [
            (mode, expected_sid),
            ("--sandbox", "read-only"),
            ("-m", "chosen-model"),
            ("--reasoning-effort", "high"),
            ("--permission-mode", "dontAsk"),
            ("--output-format", "json"),
        ] {
            assert_eq!(
                lines.iter().filter(|arg| **arg == flag).count(),
                1,
                "{args}"
            );
            let at = lines.iter().position(|arg| *arg == flag).expect("flag");
            assert_eq!(lines[at + 1], value, "{args}");
        }
        assert_eq!(
            lines
                .iter()
                .filter(|arg| **arg == "--session-id" || **arg == "--resume")
                .count(),
            1,
            "one session mode: {args}"
        );
        assert_eq!(
            lines.iter().filter(|arg| **arg == "--prompt-file").count(),
            1,
            "{args}"
        );
        assert!(
            !args.contains("prefix with spaces"),
            "prefix consumed exactly once"
        );
        let prompt_at = lines
            .iter()
            .position(|s| *s == "--prompt-file")
            .expect("prompt flag");
        assert!(
            !std::path::Path::new(lines[prompt_at + 1]).exists(),
            "prompt removed on return"
        );
        assert!(
            review::inflight::live_marker(expected_sid, Some(&scratch.data_root()))
                .is_some_and(|m| m.phase == review::inflight::Phase::HandoffPending),
            "provider return keeps handoff pending until the app records and publishes"
        );
    };
    for (start, mode, stop) in [
        (
            SessionStart::Fresh {
                id: Some(SessionId::parse(sid).expect("id")),
            },
            "--session-id",
            "end_turn",
        ),
        (
            SessionStart::Resume(SessionId::parse(sid).expect("id")),
            "--resume",
            "end_turn",
        ),
        (
            SessionStart::Resume(SessionId::parse(sid).expect("id")),
            "--resume",
            "cancelled",
        ),
    ] {
        env.insert("STUB_STOP".into(), stop.into());
        let result = review::harness::invoke(
            review::harness::LaunchSpec {
                harness: Harness::Grok,
                start,
                settings: fixture_settings(
                    Harness::Grok,
                    review::config::Profile {
                        model: Some("chosen-model".into()),
                        effort: Some("high".into()),
                        env: Some(env.clone()),
                        ..Default::default()
                    },
                ),
                prompt: ("inspect this prompt").to_owned(),
                root: scratch.project(),
            },
            None,
            &runtime,
            None,
        )
        .await;
        assert!(result.output.is_ok(), "fixture ran");
        assert_eq!(result.session_id.as_deref(), Some(sid));
        let turn = result.output.expect("fixture ran");
        assert_eq!(turn.text, "fixture answer");
        if stop == "cancelled" {
            let summary = turn
                .summary(result.digest.as_ref())
                .expect("failure summary");
            let digest = result.digest.expect("failure digest");
            assert_eq!(digest.exit.code(), Some(0));
            assert!(turn.outcome.failed());
            assert!(
                summary
                    .turn_error
                    .expect("stated stop")
                    .contains("cancelled")
            );
            assert!(
                summary.review_error.is_none(),
                "background verification succeeded"
            );
        } else {
            assert!(
                result.digest.is_none(),
                "successful answer needs no diagnostic digest"
            );
        }
        assert_eq!(
            std::fs::read_to_string(scratch.root.join("session")).expect("session"),
            format!("{mode}\n{sid}\n")
        );
        assert_eq!(
            std::fs::read_to_string(scratch.root.join("prompt")).expect("prompt"),
            "inspect this prompt"
        );
        // `PWD` is set to the project root as given (a logical spelling, as a
        // shell's own `PWD` may be), so compare the directories it names.
        let pwd = std::fs::read_to_string(scratch.root.join("cwd")).expect("cwd");
        assert_eq!(
            Path::new(pwd.trim()).canonicalize().expect("canonical PWD"),
            scratch.project().canonicalize().expect("canonical project")
        );
        assert_args(mode, sid);
    }

    // The wake is a second contained launch inside one invoke, using the same
    // configured prefix and checked session rather than a real Grok executable.
    std::fs::write(scratch.root.join("trace"), "").expect("clear trace");
    env.insert("STUB_WAKE".into(), "1".into());
    env.insert("STUB_STOP".into(), "end_turn".into());
    let wake_sid = "grok-wake-session";
    let result = review::harness::invoke(
        review::harness::LaunchSpec {
            harness: Harness::Grok,
            start: SessionStart::Fresh {
                id: Some(SessionId::parse(wake_sid).expect("id")),
            },
            settings: fixture_settings(
                Harness::Grok,
                review::config::Profile {
                    model: Some("chosen-model".into()),
                    effort: Some("high".into()),
                    env: Some(env.clone()),
                    ..Default::default()
                },
            ),
            prompt: ("start background work").to_owned(),
            root: scratch.project(),
        },
        None,
        &runtime,
        None,
    )
    .await;
    assert_eq!(result.session_id.as_deref(), Some(wake_sid));
    assert!(result.digest.is_none(), "wake verification succeeded");
    assert!(
        result
            .output
            .expect("wake answer")
            .text
            .contains("grok woken 1 time(s)")
    );
    assert_eq!(
        std::fs::read_to_string(scratch.root.join("trace")).expect("launch trace"),
        format!("--session-id {wake_sid}\n--resume {wake_sid}\n")
    );
    assert!(
        std::fs::read_to_string(scratch.root.join("prompt"))
            .expect("wake prompt")
            .contains("fixture build passed")
    );
    assert_args("--resume", wake_sid);
}

// Fixture settings go through the same profile constructor as production.
fn fixture_settings(
    harness: Harness,
    profile: review::config::Profile,
) -> review::harness::RunSettings {
    review::harness::RunSettings::from_profile(
        harness,
        Some(&review::config::HarnessEntry {
            harness,
            profile,
            layer: review::config::Layer::Local,
            table: "[fixture]".into(),
        }),
    )
}

/// Durable grants belong to adapter post-processing, including when Grok emits
/// a pre-turn error. A merely requested or process-local grant is never a fact.
#[tokio::test]
async fn grok_prepared_trust_is_verified_even_after_returned_error() {
    use review::harness::{LaunchSpec, SessionStart};
    for durable in [false, true] {
        let scratch = Scratch::new();
        let base = std::fs::canonicalize(&scratch.root).expect("canonical scratch");
        let home = base.join("grok-home");
        let script = scratch.write_stub(
            r#"
seen_trust=0
for arg in "$@"; do
    [ "$arg" != --trust ] || seen_trust=1
done
[ "$seen_trust" = 1 ] || exit 81
printf '%s\n' launched > "$STUB_DIR/launched"
if [ "$DURABLE" = 1 ]; then
    printf '[folders."%s"]\ntrusted = true\n' "$TRUST_KEY" > "$GROK_HOME/trusted_folders.toml"
fi
printf '%s\n' '{"type":"error","message":"fixture auth failure after trust"}'
"#,
        );
        let mut runtime = fast_runtime(&scratch);
        runtime.commands.set(
            Harness::Grok,
            HarnessCommand {
                program: "/bin/sh".into(),
                leading_args: vec![script],
            },
        );
        // Give this fixture its own repository boundary, independent of the
        // target directory's symlink or any enclosing repository.
        std::fs::create_dir(scratch.project().join(".git")).expect("fixture git root");
        let key = std::fs::canonicalize(scratch.project()).expect("project root");
        let env = BTreeMap::from([
            ("GROK_HOME".into(), home.to_string_lossy().into_owned()),
            ("HOME".into(), "/home/nobody-here".into()),
            ("STUB_DIR".into(), base.to_string_lossy().into_owned()),
            ("TRUST_KEY".into(), key.to_string_lossy().into_owned()),
            ("DURABLE".into(), if durable { "1" } else { "0" }.into()),
        ]);
        let result = review::harness::invoke(
            LaunchSpec {
                harness: Harness::Grok,
                start: SessionStart::Fresh { id: None },
                settings: fixture_settings(
                    Harness::Grok,
                    review::config::Profile {
                        sandbox: Some(review::config::SandboxLevel::WorkspaceWrite),
                        env: Some(env),
                        ..Default::default()
                    },
                ),
                prompt: "test".into(),
                root: scratch.project(),
            },
            None,
            &runtime,
            None,
        )
        .await;
        assert!(base.join("launched").exists());
        let error = result.output.expect_err("typed pre-turn failure");
        assert!(
            error.to_string().contains("fixture auth failure"),
            "{error:#}"
        );
        assert_eq!(
            result.grok_trust,
            durable.then(|| key.to_string_lossy().into_owned())
        );
        assert!(
            result
                .permissions
                .expect("prepared permissions")
                .native_to_record()
                .expect("native selector")
                .starts_with("review-ws-")
        );
        assert!(home.join("sandbox.toml").exists());
    }
}

#[tokio::test]
async fn grok_invalid_home_is_refused_before_even_a_read_only_launch() {
    use review::harness::{LaunchSpec, SessionStart};
    let scratch = Scratch::new();
    let result = review::harness::invoke(
        LaunchSpec {
            harness: Harness::Grok,
            start: SessionStart::Fresh { id: None },
            settings: fixture_settings(
                Harness::Grok,
                review::config::Profile {
                    env: Some(BTreeMap::from([(
                        "GROK_HOME".into(),
                        "relative-home".into(),
                    )])),
                    ..Default::default()
                },
            ),
            prompt: "test".into(),
            root: scratch.project(),
        },
        None,
        &fast_runtime(&scratch),
        None,
    )
    .await;
    assert!(
        result
            .output
            .expect_err("invalid home")
            .to_string()
            .contains("relative")
    );
    assert!(result.session_id.is_none());
    assert!(result.grok_trust.is_none());
    assert_eq!(
        result
            .permissions
            .expect("requested permissions")
            .selector(),
        Some("read-only")
    );
}

/// One fixture per wire protocol, sharing only process observation/transport.
/// Every adapter still builds its entire argv and permission plan in invoke.
fn protocol_fixture(
    scratch: &Scratch,
    harness: Harness,
    resume: bool,
    level: Option<review::config::SandboxLevel>,
    mode: &str,
) -> (review::harness::LaunchSpec, HarnessRuntime) {
    let protocol = match harness {
        Harness::Codex => {
            r#"
printf '%s\n' '{"type":"thread.started","thread_id":"01920000-0000-7000-8000-000000000001"}'
[ "$STUB_MODE" != death ] || exit 7
printf '%s' 'fixture answer' > "$out"
"#
        }
        Harness::Claude => {
            r#"
printf '{"type":"system","subtype":"init","session_id":"%s"}\n' "$sid"
[ "$STUB_MODE" != death ] || exit 7
printf '{"type":"result","is_error":false,"result":"fixture answer","session_id":"%s","num_turns":1}\n' "$sid"
"#
        }
        // maki's stream is claude's shape, but only `end_turn` is an answer.
        Harness::Maki => {
            r#"
printf '{"type":"system","subtype":"init","session_id":"%s","model":"m"}\n' "$sid"
[ "$STUB_MODE" != death ] || exit 7
printf '{"type":"result","subtype":"success","is_error":false,"result":"fixture answer","stop_reason":"end_turn","session_id":"%s","num_turns":1,"total_cost_usd":0.0}\n' "$sid"
"#
        }
        // Grok has a single result, not an init stream. EOF before that object
        // establishes no turn; this intentionally differs from the other two.
        Harness::Grok => {
            r#"
[ "$STUB_MODE" != death ] || exit 7
mkdir -p "$GROK_HOME/sessions/fixture/$sid"
: > "$GROK_HOME/sessions/fixture/$sid/updates.jsonl"
printf '{"text":"fixture answer","stopReason":"end_turn","sessionId":"%s","numTurns":1}\n' "$sid"
"#
        }
    };
    let script = scratch.write_stub(&format!(
        r#"
[ "$1" = 'prefix with spaces' ] || exit 81
shift
if [ "$1" = --version ]; then printf 'fixture-version\n'; exit 0; fi
printf '%s\n' "$@" > "$STUB_DIR/argv"
printf '%s\n' "$PWD" > "$STUB_DIR/pwd"
pwd > "$STUB_DIR/cwd"
printf '%s\n' "$$" > "$STUB_DIR/child-pid"
out= prompt= sid=
prev=
for a in "$@"; do
    case "$prev" in
        -o) [ -z "$out" ] || exit 82; out=$a ;;
        --prompt-file) [ -z "$prompt" ] || exit 83; prompt=$a ;;
        --session-id|--resume) sid=$a ;;
    esac
    prev=$a
done
if [ -n "$prompt" ]; then cat "$prompt" > "$STUB_DIR/prompt"; else cat > "$STUB_DIR/prompt"; fi
if [ "$STUB_MODE" = barrier ]; then
    n=0
    while [ ! -f "$STUB_DIR/release" ]; do
        n=$((n+1)); [ "$n" -lt "$STUB_BARRIER_POLLS" ] || exit 84
        sleep "$STUB_BARRIER_BEAT"
    done
fi
{protocol}
"#
    ));
    let mut runtime = fast_runtime(scratch);
    runtime.commands.set(
        harness,
        HarnessCommand {
            program: "/bin/sh".into(),
            leading_args: vec![script, "prefix with spaces".into()],
        },
    );
    let external = scratch.root.join("external root");
    std::fs::create_dir_all(&external).expect("external root");
    // A separate automatic derivation input, independent of configured roots
    // and ambient HOME/CARGO_TARGET_DIR. Keep its native target observable.
    let automatic = scratch.root.join("automatic target");
    std::fs::create_dir_all(&automatic).expect("automatic target");
    std::os::unix::fs::symlink(
        std::fs::canonicalize(&automatic).expect("native target"),
        scratch.project().join("target"),
    )
    .expect("project target symlink");
    let env = BTreeMap::from([
        (
            "STUB_DIR".into(),
            scratch.root.to_string_lossy().into_owned(),
        ),
        (
            "CODEX_HOME".into(),
            scratch.codex_home().to_string_lossy().into_owned(),
        ),
        (
            "GROK_HOME".into(),
            std::fs::canonicalize(&scratch.root)
                .expect("native scratch path")
                .join("grok-home")
                .to_string_lossy()
                .into_owned(),
        ),
        ("STUB_MODE".into(), mode.into()),
        ("STUB_BARRIER_POLLS".into(), "1000".into()),
        ("STUB_BARRIER_BEAT".into(), "0.01".into()),
        ("PWD".into(), "/wrong-profile-pwd".into()),
    ]);
    let config = match harness {
        Harness::Codex if mode != "derived" => vec![
            "model=\"raw-model\"".into(),
            "model_reasoning_effort=\"low\"".into(),
            "sandbox_workspace_write.network_access=false".into(),
            format!(
                "sandbox_workspace_write.writable_roots={}",
                toml::Value::Array(vec![toml::Value::String(
                    external.to_string_lossy().into_owned()
                )])
            ),
        ],
        Harness::Codex | Harness::Claude | Harness::Grok | Harness::Maki => Vec::new(),
    };
    // maki refuses an effort it cannot honour, and a launch carrying one never
    // starts; every other adapter renders the profile's.
    let effort = match harness {
        Harness::Maki => None,
        Harness::Codex | Harness::Claude | Harness::Grok => Some("high".into()),
    };
    let launch = review::harness::LaunchSpec {
        harness,
        start: if resume {
            review::harness::SessionStart::Resume(
                review::harness::SessionId::parse(STUB_SESSION).expect("id"),
            )
        } else {
            review::harness::SessionStart::Fresh { id: None }
        },
        settings: fixture_settings(
            harness,
            review::config::Profile {
                model: Some("chosen-model".into()),
                effort,
                sandbox: level,
                env: Some(env),
                config,
                writable_roots: vec![external.to_string_lossy().into_owned()],
            },
        ),
        prompt: "prompt with spaces\nand a second line".into(),
        root: scratch.project(),
    };
    (launch, runtime)
}

fn observed_args(scratch: &Scratch) -> Vec<String> {
    std::fs::read_to_string(scratch.root.join("argv"))
        .expect("argv")
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Normalize only nondeterministic transport values, preserving every flag,
/// argument boundary, duplication and precedence position in the oracle.
fn normalized_args(scratch: &Scratch, result: &HarnessResult) -> Vec<String> {
    let mut args = observed_args(scratch);
    let mut previous = String::new();
    for arg in &mut args {
        let flag = arg.clone();
        if previous == "-o" || previous == "--prompt-file" {
            assert!(!Path::new(arg).exists(), "transport file removed on return");
            *arg = "<file>".into();
        } else if previous == "--session-id" || previous == "--resume" {
            assert_eq!(Some(arg.as_str()), result.session_id.as_deref());
            *arg = "<session>".into();
        }
        previous = flag;
    }
    args
}

#[tokio::test]
async fn codex_launch_argv_table_covers_permissions_overrides_and_resume_output() {
    use review::config::SandboxLevel;
    for requested in std::iter::once(None).chain(SandboxLevel::NAMED.into_iter().map(Some)) {
        let level = requested.clone().unwrap_or_default();
        for resume in [false, true] {
            let scratch = Scratch::new();
            let (launch, runtime) = protocol_fixture(
                &scratch,
                Harness::Codex,
                resume,
                requested.clone(),
                "answer",
            );
            assert!(
                launch.settings.origin().is_some(),
                "fixture selects a real profile"
            );
            let result = invoke(launch, None, &runtime, None).await;
            if resume {
                assert_eq!(result.session_id.as_deref(), Some(STUB_SESSION));
            }
            assert_eq!(result.output.as_ref().expect("turn").text, "fixture answer");
            assert_eq!(result.model.as_deref(), Some("chosen-model"));
            assert_eq!(result.effort.as_deref(), Some("low"));
            assert_eq!(
                result.permissions.as_ref().and_then(|p| p.selector()),
                Some(level.as_str())
            );
            let external = scratch
                .root
                .join("external root")
                .to_string_lossy()
                .into_owned();
            assert_eq!(
                result.writable_roots.as_slice(),
                std::slice::from_ref(&external)
            );
            let mut expected: Vec<String> = [
                "exec",
                "--skip-git-repo-check",
                "-c",
                "approval_policy=\"never\"",
                "--ignore-rules",
                "--ignore-user-config",
                "-c",
                "model_reasoning_summary=\"detailed\"",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect();
            match level {
                SandboxLevel::ReadOnly => expected.extend(
                    [
                        "-c",
                        "permissions.review-read-only.extends=\":read-only\"",
                        "-c",
                        "permissions.review-read-only.network.enabled=true",
                        "-c",
                        "default_permissions=\"review-read-only\"",
                    ]
                    .into_iter()
                    .map(str::to_owned),
                ),
                SandboxLevel::WorkspaceWrite | SandboxLevel::DangerFullAccess => {
                    expected.extend(["--sandbox".into(), level.as_str().into()]);
                }
                SandboxLevel::Custom(_) => panic!("named levels only"),
            }
            expected.extend(
                [
                    "-m",
                    "chosen-model",
                    "-c",
                    "model_reasoning_effort=\"high\"",
                ]
                .into_iter()
                .map(str::to_owned),
            );
            // The generated effective roots and original raw override both reach
            // the process. The independent literal pins their order/spelling.
            let roots = format!("sandbox_workspace_write.writable_roots=[\"{external}\"]");
            expected.extend(["-c".into(), roots]);
            if level == SandboxLevel::WorkspaceWrite {
                expected.extend([
                    "-c".into(),
                    "sandbox_workspace_write.network_access=true".into(),
                ]);
            }
            expected.extend(
                [
                    "-c",
                    "model=\"raw-model\"",
                    "-c",
                    "model_reasoning_effort=\"low\"",
                    "-c",
                    "sandbox_workspace_write.network_access=false",
                ]
                .into_iter()
                .map(str::to_owned),
            );
            expected.extend([
                "-c".into(),
                format!(
                    "sandbox_workspace_write.writable_roots={}",
                    toml::Value::Array(vec![toml::Value::String(external)])
                ),
            ]);
            if resume {
                expected.extend(["resume".into(), STUB_SESSION.into()]);
            }
            expected.extend(["--json", "-o", "<file>"].into_iter().map(str::to_owned));
            assert_eq!(
                normalized_args(&scratch, &result),
                expected,
                "{level:?} resume={resume}"
            );
            assert_eq!(
                std::fs::read_to_string(scratch.root.join("prompt")).expect("prompt"),
                "prompt with spaces\nand a second line"
            );
        }
    }
}

#[tokio::test]
async fn claude_launch_argv_table_keeps_permissions_and_session_modes_explicit() {
    for requested in
        std::iter::once(None).chain(review::config::SandboxLevel::NAMED.into_iter().map(Some))
    {
        for resume in [false, true] {
            let scratch = Scratch::new();
            let (launch, runtime) = protocol_fixture(
                &scratch,
                Harness::Claude,
                resume,
                requested.clone(),
                "answer",
            );
            let result = invoke(launch, None, &runtime, None).await;
            if resume {
                assert_eq!(result.session_id.as_deref(), Some(STUB_SESSION));
            }
            assert_eq!(result.output.as_ref().expect("turn").text, "fixture answer");
            assert_eq!(result.model.as_deref(), Some("chosen-model"));
            assert_eq!(result.effort.as_deref(), Some("high"));
            assert_eq!(result.permissions.as_ref().and_then(|p| p.selector()), None);
            assert!(result.writable_roots.is_empty());
            let expected: Vec<_> = [
                if resume { "--resume" } else { "--session-id" },
                "<session>",
                "--print",
                "--permission-mode",
                "dontAsk",
                "--output-format",
                "stream-json",
                "--verbose",
                "--model",
                "chosen-model",
                "--effort",
                "high",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect();
            assert_eq!(normalized_args(&scratch, &result), expected);
            assert_eq!(
                std::fs::read_to_string(scratch.root.join("prompt")).expect("prompt"),
                "prompt with spaces\nand a second line"
            );
        }
    }
}

#[tokio::test]
async fn maki_launch_argv_table_carries_no_permission_or_effort_flags() {
    for requested in
        std::iter::once(None).chain(review::config::SandboxLevel::NAMED.into_iter().map(Some))
    {
        for resume in [false, true] {
            let scratch = Scratch::new();
            let (launch, runtime) =
                protocol_fixture(&scratch, Harness::Maki, resume, requested.clone(), "answer");
            let result = invoke(launch, None, &runtime, None).await;
            if resume {
                assert_eq!(result.session_id.as_deref(), Some(STUB_SESSION));
            }
            assert_eq!(result.output.as_ref().expect("turn").text, "fixture answer");
            assert_eq!(result.model.as_deref(), Some("chosen-model"));
            assert_eq!(result.effort, None);
            assert_eq!(result.permissions.as_ref().and_then(|p| p.selector()), None);
            assert!(result.writable_roots.is_empty());
            assert_eq!(
                result.served,
                review::harness::Served::default(),
                "an unpriced zero and the requested model are not served facts"
            );
            let expected: Vec<_> = [
                if resume { "--resume" } else { "--session-id" },
                "<session>",
                "-p",
                "--output-format",
                "stream-json",
                "--model",
                "chosen-model",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect();
            assert_eq!(normalized_args(&scratch, &result), expected);
            assert_eq!(
                std::fs::read_to_string(scratch.root.join("prompt")).expect("prompt"),
                "prompt with spaces\nand a second line"
            );
        }
    }
}

#[tokio::test]
async fn grok_launch_argv_table_checks_generated_policy_and_prompt_transport() {
    use review::config::SandboxLevel;
    for requested in std::iter::once(None).chain(SandboxLevel::NAMED.into_iter().map(Some)) {
        let level = requested.clone().unwrap_or_default();
        for resume in [false, true] {
            let scratch = Scratch::new();
            let (launch, runtime) =
                protocol_fixture(&scratch, Harness::Grok, resume, requested.clone(), "answer");
            let result = invoke(launch, None, &runtime, None).await;
            if resume {
                assert_eq!(result.session_id.as_deref(), Some(STUB_SESSION));
            }
            assert_eq!(result.output.as_ref().expect("turn").text, "fixture answer");
            let selector = result
                .permissions
                .as_ref()
                .and_then(|p| p.selector())
                .expect("selector");
            match level {
                SandboxLevel::ReadOnly => {
                    assert_eq!(selector, "read-only");
                    assert!(result.writable_roots.is_empty());
                }
                SandboxLevel::DangerFullAccess => {
                    assert_eq!(selector, "none");
                    assert!(result.writable_roots.is_empty());
                }
                SandboxLevel::WorkspaceWrite => {
                    assert!(selector.starts_with("review-ws-"));
                    let automatic = std::fs::canonicalize(scratch.root.join("automatic target"))
                        .expect("automatic grant");
                    assert!(
                        result
                            .writable_roots
                            .iter()
                            .any(|p| Path::new(p) == automatic)
                    );
                    assert!(
                        result.writable_roots.contains(
                            &std::fs::canonicalize(scratch.root.join("external root"))
                                .expect("resolved grant")
                                .to_string_lossy()
                                .into_owned()
                        )
                    );
                    let profiles: toml::Table =
                        std::fs::read_to_string(scratch.root.join("grok-home/sandbox.toml"))
                            .expect("generated profiles")
                            .parse()
                            .expect("profiles TOML");
                    assert_eq!(
                        profiles["profiles"][selector]["extends"].as_str(),
                        Some("workspace")
                    );
                    let actual = profiles["profiles"][selector]["read_write"]
                        .as_array()
                        .expect("roots");
                    assert_eq!(
                        actual
                            .iter()
                            .map(|v| v.as_str().expect("path"))
                            .collect::<Vec<_>>(),
                        result
                            .writable_roots
                            .iter()
                            .map(String::as_str)
                            .collect::<Vec<_>>()
                    );
                }
                SandboxLevel::Custom(_) => panic!("named levels only"),
            }
            let mut expected: Vec<String> = [
                if resume { "--resume" } else { "--session-id" },
                "<session>",
                "--prompt-file",
                "<file>",
                "--output-format",
                "json",
                "--permission-mode",
                "dontAsk",
                "--sandbox",
                selector,
            ]
            .into_iter()
            .map(str::to_owned)
            .collect();
            if level == SandboxLevel::WorkspaceWrite {
                expected.push("--trust".into());
            }
            expected.extend(
                [
                    "-m",
                    "chosen-model",
                    "--reasoning-effort",
                    "high",
                    "--background-wait-timeout",
                    "1200",
                ]
                .into_iter()
                .map(str::to_owned),
            );
            assert_eq!(normalized_args(&scratch, &result), expected);
            assert_eq!(result.model.as_deref(), Some("chosen-model"));
            assert_eq!(result.effort.as_deref(), Some("high"));
            assert_eq!(
                std::fs::read_to_string(scratch.root.join("prompt")).expect("prompt"),
                "prompt with spaces\nand a second line"
            );
            assert!(
                result.grok_trust.is_none(),
                "a flag alone proves no durable grant"
            );
        }
    }
}

#[tokio::test]
async fn every_adapter_shares_launch_stop_pending_and_death_contracts() {
    use review::config::SandboxLevel;
    for harness in Harness::all() {
        let scratch = Scratch::new();
        let (launch, runtime) = protocol_fixture(
            &scratch,
            *harness,
            true,
            Some(SandboxLevel::ReadOnly),
            "barrier",
        );
        let (tx, rx) = tokio::sync::oneshot::channel();
        let observer = async {
            rx.await.expect("launch handshake");
            let marker = review::inflight::live_marker(STUB_SESSION, Some(&scratch.data_root()))
                .expect("resume marked by launch notification");
            assert_eq!(marker.phase, review::inflight::Phase::Running);
            std::fs::write(scratch.root.join("release"), "go").expect("release fixture");
        };
        let (result, ()) = tokio::join!(invoke(launch, Some(tx), &runtime, None), observer);
        assert!(
            !result
                .output
                .as_ref()
                .expect("established turn")
                .outcome
                .failed(),
            "{harness}"
        );
        let marker = review::inflight::live_marker(STUB_SESSION, Some(&scratch.data_root()))
            .expect("pending");
        assert_eq!(
            marker.run_id,
            result.run_id.as_ref().expect("run id").as_str()
        );
        assert_eq!(marker.phase, review::inflight::Phase::HandoffPending);
        for file in ["pwd", "cwd"] {
            assert_eq!(
                std::fs::read_to_string(scratch.root.join(file))
                    .expect("cwd observation")
                    .trim(),
                scratch.project().to_str().expect("project")
            );
        }
        let pid = std::fs::read_to_string(scratch.root.join("child-pid"))
            .expect("pid")
            .trim()
            .parse()
            .expect("numeric pid");
        assert!(!pid_alive(pid), "keeper reaped {harness}");
        assert!(result.leftovers.is_clean());
        assert!(runtime.lifecycle.unresolved().is_empty());
        drop(result);
        assert!(review::inflight::read_live(Some(&scratch.data_root())).is_empty());

        let scratch = Scratch::new();
        let (launch, runtime) = protocol_fixture(
            &scratch,
            *harness,
            false,
            Some(SandboxLevel::ReadOnly),
            "answer",
        );
        runtime.lifecycle.request_stop();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let result = invoke(launch, Some(tx), &runtime, None).await;
        assert!(
            result.output.is_err(),
            "stopped {harness} must refuse before spawn"
        );
        assert!(rx.await.is_err());
        assert!(!scratch.root.join("child-pid").exists());
        drop(result);
        assert!(review::inflight::read_live(Some(&scratch.data_root())).is_empty());

        let scratch = Scratch::new();
        let (launch, runtime) = protocol_fixture(
            &scratch,
            *harness,
            false,
            Some(SandboxLevel::ReadOnly),
            "death",
        );
        let result = invoke(launch, None, &runtime, None).await;
        match harness {
            Harness::Codex | Harness::Claude | Harness::Maki => {
                let turn = result
                    .output
                    .as_ref()
                    .expect("initial event established the turn");
                assert!(!turn.outcome.has_answer());
                assert!(turn.outcome.failed());
                assert_eq!(
                    result.digest.as_ref().expect("death digest").exit.code(),
                    Some(7)
                );
                assert!(result.session_id.is_some());
            }
            Harness::Grok => assert!(
                result.output.is_err(),
                "EOF without its result established no Grok turn"
            ),
        }
        assert!(runtime.lifecycle.unresolved().is_empty());
        drop(result);
        assert!(review::inflight::read_live(Some(&scratch.data_root())).is_empty());
    }
}

#[tokio::test]
async fn codex_derived_roots_reach_both_the_process_and_record_projection() {
    let scratch = Scratch::new();
    let (launch, runtime) = protocol_fixture(
        &scratch,
        Harness::Codex,
        false,
        Some(review::config::SandboxLevel::WorkspaceWrite),
        "derived",
    );
    let result = invoke(launch, None, &runtime, None).await;
    assert_eq!(result.output.as_ref().expect("turn").text, "fixture answer");
    let external =
        std::fs::canonicalize(scratch.root.join("external root")).expect("canonical root");
    assert!(
        result
            .writable_roots
            .iter()
            .any(|p| Path::new(p) == external)
    );
    let automatic =
        std::fs::canonicalize(scratch.root.join("automatic target")).expect("automatic grant");
    assert!(
        result
            .writable_roots
            .iter()
            .any(|p| Path::new(p) == automatic)
    );
    let args = observed_args(&scratch);
    let overrides: Vec<_> = args
        .iter()
        .filter(|s| s.starts_with("sandbox_workspace_write.writable_roots="))
        .collect();
    assert_eq!(overrides.len(), 1, "no raw override in this case");
    let parsed: toml::Table = overrides[0].parse().expect("generated TOML");
    let roots = parsed["sandbox_workspace_write"]["writable_roots"]
        .as_array()
        .expect("array");
    assert_eq!(
        roots
            .iter()
            .map(|s| s.as_str().expect("path"))
            .collect::<Vec<_>>(),
        result
            .writable_roots
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
    assert_eq!(result.effort.as_deref(), Some("high"));
}
