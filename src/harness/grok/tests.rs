//! Unit tests of grok harness behavior.

use crate::process::Exit;

// ---------------------------------------------------------------------------
// grok output classification
//
// These need no child process: grok reports the outcome of a turn in its own
// result object, so the whole decision is a pure function of the two streams.
// That is the substantive difference from codex, where the equivalent question
// ("is this text an answer?") can only be settled by reading the rollout, and
// where the tests consequently have to drive a real process.
// ---------------------------------------------------------------------------

/// The shape of a healthy run, trimmed to the fields we read.
const GROK_OK: &str = r#"{
  "text": "OK",
  "stopReason": "end_turn",
  "sessionId": "3f1c9a2e-7b40-4d51-9c68-2a5e10d3b7f4",
  "usage": { "total_tokens": 22628 },
  "num_turns": 1
}"#;

#[test]
fn grok_end_turn_is_an_answer() {
    let super::GrokOutcome::Answered(answer) =
        super::interpret_grok_output(GROK_OK, "").expect("end_turn is an answer")
    else {
        panic!("end_turn must classify as an answer");
    };
    assert_eq!(answer.text, "OK");
    assert_eq!(
        answer.session_id.as_deref(),
        Some("3f1c9a2e-7b40-4d51-9c68-2a5e10d3b7f4"),
        "the echoed session id is what we record, not the one we asked for"
    );
}

#[test]
fn grok_served_model_and_cost_are_read_from_model_usage() {
    // Trimmed from a real grok 1.0.40 run under the default `-m grok-4.7`: the
    // served name differs from anything `grok models` lists, which is why it is
    // recorded separately from `model` rather than replacing it.
    let out = r#"{
      "text": "ok",
      "stopReason": "end_turn",
      "sessionId": "3b8f1c22-9d4e-4a51-b7c6-2e0f9a8d7c11",
      "num_turns": 1,
      "total_cost_usd": 0.02397,
      "modelUsage": { "grok-4.7-build": { "modelCalls": 1, "costUSD": 0.02397 } }
    }"#;
    assert_eq!(
        super::result_served(out),
        super::Served::new(["grok-4.7-build".to_string()], Some(0.02397))
    );
    // Absent fields and non-JSON record nothing rather than failing the run.
    assert_eq!(super::result_served(GROK_OK), super::Served::default());
    assert_eq!(super::result_served("not json"), super::Served::default());
}

#[test]
fn grok_cancelled_turn_is_not_an_answer() {
    // Observed by running a tool-using prompt under `--max-turns 1`: a full
    // result object, exit 1, and `text` holding the model's opening remark.
    // Persisting that as the response is the exact failure this gate prevents.
    let cut_off = r#"{
      "text": "I'll list everything in `src/` first, then read each file.",
      "stopReason": "cancelled",
      "sessionId": "019ffbc5-2b48-7611-a7f2-48db1fa2b0ed",
      "num_turns": 1
    }"#;
    let outcome = super::interpret_grok_output(cut_off, "Error: max turns reached")
        .expect("a cancelled turn still ran - it is not a launch failure");
    let super::GrokOutcome::NoAnswer {
        reason,
        hrafnar_error,
        text,
        session_id,
    } = outcome
    else {
        panic!("a cancelled turn is not an answer");
    };
    assert!(!hrafnar_error, "grok stated the stop reason");
    assert!(
        reason.contains("cancelled"),
        "names the stop reason: {reason}"
    );
    assert!(
        text.contains("list everything"),
        "the interim commentary is kept as a clue: {text}"
    );
    // The session id is the point of returning this rather than an error: the
    // turn that was cut off is the one most worth resuming, and `invoke` drops
    // the id on every error path.
    assert_eq!(
        session_id.as_deref(),
        Some("019ffbc5-2b48-7611-a7f2-48db1fa2b0ed"),
        "a cut-off turn keeps its session id"
    );

    // It must still read as a failure everywhere that matters: exit code,
    // greppable digest, and no auto-resume (a stated reason is not a wedge).
    let digest = super::status_digest(Exit::Code(1));
    assert_eq!(
        digest.exit.code(),
        Some(1),
        "the digest reports the status we observed"
    );
    let outcome = super::Outcome::no_answer().stated(reason);
    assert!(outcome.failed() && outcome.stated_failure().is_some());
    let summary = digest.summary(&outcome);
    assert!(!summary.captured, "nothing was captured");
    assert!(
        !summary.recovered_from_transcript,
        "grok has no rollout to recover from"
    );
    assert!(
        summary.turn_error.is_some(),
        "the reason persists flat, so these stay greppable"
    );
}

#[test]
fn grok_turns_and_usage_are_read_from_the_result() {
    // Trimmed from a real grok 1.0.40 run. Before this was read, every grok
    // digest said 0 turns and 0 tokens, which reads as "nothing ran" on a run
    // that spent real money.
    let out = r#"{
      "text": "",
      "stopReason": "cancelled",
      "sessionId": "01a0c86b-18a3-7c70-9851-66dd509787b0",
      "usage": {
        "input_tokens": 6716,
        "cache_read_input_tokens": 30592,
        "output_tokens": 154,
        "reasoning_tokens": 82
      },
      "num_turns": 2
    }"#;
    let (turns, usage) = super::grok_usage(out);
    assert_eq!(turns, 2);
    assert_eq!(
        usage.total_input,
        6716 + 30592,
        "codex-shaped: input includes cached"
    );
    assert_eq!(usage.cached_input, 30592);
    assert_eq!(usage.output, 154);
    assert_eq!(usage.reasoning_output, 82);
    let (turns, usage) = super::grok_usage("not json");
    assert_eq!((turns, usage.total_input), (0, 0));
}

#[test]
fn grok_no_answer_reason_carries_the_cause_and_the_trust_hint() {
    let base = "grok ended the turn without an answer (stopReason: cancelled)".to_string();
    assert_eq!(
        super::explain_grok_no_answer(base.clone(), None, None),
        base
    );
    let full = super::explain_grok_no_answer(
        base,
        Some(&super::state::CancelCause {
            category: "permission_cancelled".into(),
            refused_tool: Some("search_replace".into()),
        }),
        Some("/p is not in grok's trusted folders"),
    );
    assert!(full.contains("stopReason: cancelled"), "{full}");
    assert!(full.contains("search_replace"), "{full}");
    assert!(full.contains("trusted folders"), "{full}");
}

#[test]
fn grok_missing_stop_reason_is_not_an_answer() {
    // A future grok that drops or renames the field must not be read as
    // success: the whole gate rests on that field being present and known.
    // This object still identifies itself as a result (it carries a session
    // id), so the turn demonstrably ran - it just did not say how it ended.
    let outcome = super::interpret_grok_output(
        r#"{"text": "half a thought", "sessionId": "019ffbc5-2b48-7611-a7f2-48db1fa2b0ed"}"#,
        "",
    )
    .expect("an unlabelled turn still ran");
    let super::GrokOutcome::NoAnswer {
        reason,
        hrafnar_error,
        ..
    } = outcome
    else {
        panic!("an unlabelled turn is not a proven answer");
    };
    assert!(
        reason.contains("missing"),
        "says the reason was absent: {reason}"
    );
    assert!(
        hrafnar_error,
        "the missing stop reason is hrafnar's diagnosis"
    );
    let outcome = super::Outcome::no_answer().unverified(reason);
    assert!(outcome.stated.is_none());
    assert!(
        outcome
            .unverified
            .as_deref()
            .is_some_and(|message| message.contains("stopReason: missing"))
    );
    assert!(outcome.failed());
}

#[test]
fn json_that_is_not_a_result_object_is_a_launch_failure() {
    // Every field of a result object is optional, so *any* JSON object
    // deserializes into an all-default `GrokResult`. Reading that as "a turn
    // ran but did not answer" would infer the fact from successful parsing
    // rather than from evidence - and `NoAnswer` returns `Ok`, which tells
    // `hrafnar resume` the prompt cache was warmed. An unrecognised pre-turn failure
    // would then mark a cold session warm and get the *next* resume refused.
    let err = super::interpret_grok_output(r#"{"unexpected": "shape"}"#, "Error: something else")
        .expect_err("nothing here says a turn ran");
    let msg = err.to_string();
    assert!(
        msg.contains("no recognisable result"),
        "distinct from a labelled non-answer: {msg}"
    );
    assert!(msg.contains("something else"), "quotes the reason: {msg}");
}

#[test]
fn grok_without_a_result_object_reports_the_failure() {
    // Unknown model / unknown session / auth failure: grok never reaches a
    // turn, so there is no object to parse and the message is all we have.
    let err = super::interpret_grok_output("", "Error: Couldn't set model 'nope-9000'")
        .expect_err("no result object is a failure");
    let msg = err.to_string();
    assert!(msg.contains("no result object"), "distinct class: {msg}");
    assert!(msg.contains("nope-9000"), "quotes grok's reason: {msg}");
}

#[test]
fn grok_typed_error_line_keeps_its_message() {
    // Every field of a result object is optional, so this line parses as a
    // perfectly valid empty result. Read as one it reports only "no stop
    // reason" and throws away the sentence that says what went wrong - which is
    // the whole content of a pre-turn failure.
    let err = super::interpret_grok_output(r#"{"type":"error","message":"boom"}"#, "")
        .expect_err("a typed error line is not an answer");
    assert!(
        err.to_string().contains("boom"),
        "an error printed only on stdout keeps its message: {err}"
    );
}

/// A grok run as `run_grok_turn` returns it, for the wake-failure tests.
fn grok_run(outcome: super::Outcome, digest: Option<super::Digest>) -> super::RunOutput {
    super::RunOutput {
        text: String::new(),
        outcome,
        session_id: None,
        codex_home: None,
        digest,
        served: super::Served::default(),
        leftovers: crate::contained::Leftovers::default(),
    }
}

#[test]
fn wake_turns_fold_into_one_run_and_a_final_wake_failure_carries_the_totals() {
    let usage = |n| super::Usage::from_parts(n, 0, 1, n, 0);
    let mut first = grok_run(super::Outcome::answered(super::Answer::Captured), None);
    first.session_id = Some("first".into());
    first.text = "interim".into();
    first.served = super::Served::new(["grok-a".to_string()], Some(0.5));
    let mut run = super::GrokTurn {
        out: first,
        turns: 2,
        usage: usage(10),
    };
    let mut wake = grok_run(super::Outcome::answered(super::Answer::Captured), None);
    wake.session_id = Some("echoed".into());
    wake.text = "final".into();
    wake.served = super::Served::new(["grok-b".to_string()], None);
    run.absorb(super::GrokTurn {
        out: wake,
        turns: 3,
        usage: usage(5),
    });
    run.out.digest = Some(super::wake_failure(&mut run.out, "unverifiable".into()));
    let out = run.finish();
    assert_eq!(out.text, "final", "the wake's answer is the run's");
    assert_eq!(out.session_id.as_deref(), Some("first"));
    assert_eq!(out.served.model_label().as_deref(), Some("grok-a,grok-b"));
    assert_eq!(out.served.cost_usd, Some(0.5));
    assert!(out.outcome.failed(), "the wake failure fails the run");
    let digest = out.digest.expect("the wake failure made a digest");
    assert_eq!(digest.turns, 5);
    assert_eq!(digest.usage.total_input, 10 + 1 + 5 + 1);
    assert_eq!(digest.usage.cached_input, 2);
}

#[test]
fn review_verification_failure_preserves_a_real_answer_and_observed_status() {
    let mut run = grok_run(super::Outcome::answered(super::Answer::Captured), None);
    let digest = super::wake_failure(&mut run, "unread background results".to_string());
    assert!(
        run.outcome.has_answer(),
        "verification cannot retract an answer"
    );
    assert_eq!(
        digest.exit.code(),
        Some(0),
        "the completed turn exited cleanly"
    );
    assert!(
        run.outcome.stated.is_none(),
        "grok did not state this diagnosis"
    );
    assert!(run.outcome.failed(), "unverified work still fails the run");
    let summary = digest.summary(&run.outcome);
    assert_eq!(
        summary.hrafnar_error.as_deref(),
        Some("unread background results")
    );
    assert!(summary.captured);
}

#[test]
fn review_verification_failure_preserves_harness_failure_and_usage() {
    let mut last = super::status_digest(Exit::Code(7));
    last.turns = 3;
    last.usage.output = 42;
    let mut run = grok_run(
        super::Outcome::no_answer().stated("cancelled".to_string()),
        Some(last),
    );
    let digest = super::wake_failure(&mut run, "cannot read update log".to_string());
    assert!(!run.outcome.has_answer());
    assert_eq!(digest.exit.code(), Some(7));
    assert_eq!(run.outcome.stated.as_deref(), Some("cancelled"));
    assert_eq!(
        run.outcome.unverified.as_deref(),
        Some("cannot read update log")
    );
    assert_eq!(digest.turns, 3);
    assert_eq!(digest.usage.output, 42);
    assert!(run.outcome.failed());
}

#[test]
fn review_verification_failure_preserves_a_signal_after_an_answer() {
    let status = Exit::Signal(libc::SIGTERM);
    let answered = super::Outcome::answered(super::Answer::Captured);
    assert!(
        !answered.failed(),
        "post-answer process status alone is not a failed turn"
    );
    let mut run = grok_run(answered, Some(super::status_digest(status)));
    let digest = super::wake_failure(&mut run, "cannot verify results".to_string());
    assert!(run.outcome.has_answer());
    assert_eq!(digest.exit.code(), None);
    assert_eq!(digest.exit.signal_name().as_deref(), Some("SIGTERM"));
    assert!(run.outcome.stated.is_none());
    assert!(run.outcome.failed());
}

/// Preparation commits only successful native setup and keeps the provenance of
/// derived roots; an intervening explicit distrust must still refuse spawning.
#[tokio::test]
async fn grok_preparation_retains_request_and_rechecks_distrust_before_spawn() {
    use super::*;
    let scratch = || {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(crate::ids::generate_uuid());
        std::fs::create_dir_all(&dir).expect("scratch");
        std::fs::canonicalize(dir).expect("canonical scratch")
    };
    let home = scratch();
    let project = scratch();
    let env = std::collections::BTreeMap::from([
        ("GROK_HOME".into(), home.to_string_lossy().into_owned()),
        ("HOME".into(), "/home/nobody-here".into()),
    ]);
    let mut launch = LaunchSpec {
        harness: Harness::Grok,
        start: SessionStart::Fresh { id: None },
        settings: RunSettings::from_profile(Harness::Grok, None),
        prompt: "test".into(),
        root: project.clone(),
    };
    launch.start.prepare(Harness::Grok).expect("identity");
    launch.settings.env = Some(env);
    launch.settings.permissions = Permissions::profile(
        Harness::Grok,
        Some(&crate::config::SandboxLevel::WorkspaceWrite),
    );
    let mut plan = super::super::permissions::PermissionPlan::new(
        launch.settings.permissions.clone(),
        &project,
        &[],
    );
    let original_paths = plan.paths.clone();
    let mut prepared = prepare(&launch, &mut plan).await.expect("setup");
    assert_eq!(launch.settings.permissions.selector(), Some("workspace"));
    assert_eq!(plan.paths, original_paths);
    assert!(plan.derives());
    assert!(
        plan.permissions
            .selector()
            .expect("native profile")
            .starts_with("hrafnar-ws-")
    );
    assert!(prepared.trust_grant.is_some());
    assert!(prepared.verified_grant.is_none());
    let key = prepared.grok.workspace_key(&project);
    let store = home.join("trusted_folders.toml");
    std::fs::write(
        &store,
        format!("[folders.\"{}\"]\ntrusted = false\n", key.display()),
    )
    .expect("decline after setup");
    let runtime = HarnessRuntime {
        data: crate::data::DataDir::at(home.join("data")),
        ..Default::default()
    };
    let error = match prepared
        .run(&launch, plan.permissions.selector(), None, &runtime)
        .await
    {
        Err(e) => e,
        Ok(_) => panic!("intervening distrust must refuse launch"),
    };
    assert!(
        error.to_string().contains("explicitly untrusted"),
        "{error:#}"
    );
    assert!(prepared.verified_grant.is_none());
    // A setup failure must not replace the request with a generated selector.
    let mut failed = super::super::permissions::PermissionPlan::new(
        launch.settings.permissions.clone(),
        &project,
        &[],
    );
    assert!(prepare(&launch, &mut failed).await.is_err());
    assert_eq!(failed.permissions, launch.settings.permissions);
    assert_eq!(failed.paths, original_paths);
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(project);
}

/// A trusted replacement workspace does not prove the originally announced key
/// was granted. Check admission and post-run verification independently.
#[tokio::test]
async fn grok_trust_key_drift_is_not_a_verified_grant() {
    use super::*;
    let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-scratch")
        .join(crate::ids::generate_uuid());
    std::fs::create_dir_all(base.join("project/.git")).expect("fixture project");
    std::fs::create_dir_all(base.join("main/.git/worktrees/linked"))
        .expect("fixture main checkout");
    let base = std::fs::canonicalize(base).expect("canonical fixture");
    let project = base.join("project");
    let home = base.join("grok-home");
    let mut launch = LaunchSpec {
        harness: Harness::Grok,
        start: SessionStart::Fresh { id: None },
        settings: RunSettings::from_profile(Harness::Grok, None),
        prompt: "test".into(),
        root: project.clone(),
    };
    launch.start.prepare(Harness::Grok).expect("identity");
    launch.settings.env = Some(std::collections::BTreeMap::from([
        ("GROK_HOME".into(), home.to_string_lossy().into_owned()),
        ("HOME".into(), "/home/nobody-here".into()),
    ]));
    launch.settings.permissions = Permissions::profile(
        Harness::Grok,
        Some(&crate::config::SandboxLevel::WorkspaceWrite),
    );
    let mut plan = super::super::permissions::PermissionPlan::new(
        launch.settings.permissions.clone(),
        &project,
        &[],
    );
    let mut prepared = prepare(&launch, &mut plan).await.expect("setup A");
    assert_eq!(prepared.trust_grant.as_deref(), Some(project.as_path()));
    prepared
        .check_grant_target(&project)
        .expect("original target allowed");
    // Model the workspace identity changing after the launch-time check.
    std::fs::remove_dir(project.join(".git")).expect("remove git directory");
    std::fs::write(
        project.join(".git"),
        format!(
            "gitdir: {}\n",
            base.join("main/.git/worktrees/linked").display()
        ),
    )
    .expect("new worktree identity");
    let replacement = base.join("main");
    std::fs::write(
        home.join("trusted_folders.toml"),
        format!("[folders.\"{}\"]\ntrusted = true\n", replacement.display()),
    )
    .expect("B already trusted");
    prepared.verify_trust(&project);
    assert!(
        prepared.verified_grant.is_none(),
        "B being trusted says nothing about A"
    );
    // Independently model the same drift before launch-time admission.
    let error = prepared
        .check_grant_target(&project)
        .expect_err("changed grant target");
    assert!(error.to_string().contains("trust key changed"), "{error:#}");
    std::fs::remove_dir_all(base).expect("cleanup");
}

/// A recorded generated profile is recreated from its recorded set, never
/// from the current host's derived grants. No harness is needed for setup.
#[tokio::test]
async fn generated_profile_resume_recreates_recorded_roots_and_refuses_a_mismatch() {
    use super::*;
    let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-scratch")
        .join(crate::ids::generate_uuid());
    std::fs::create_dir_all(base.join("project/.git")).expect("project");
    std::fs::create_dir_all(base.join("grok-home")).expect("grok home");
    let base = std::fs::canonicalize(base).expect("canonical scratch");
    let project = base.join("project");
    let home = base.join("grok-home");
    std::fs::write(
        home.join("trusted_folders.toml"),
        format!("[folders.\"{}\"]\ntrusted = true\n", project.display()),
    )
    .expect("trusted");
    let launch = |name: &str, roots: &[String]| {
        // Exercise the same sidecar row decoder and inheritance path as message.
        let row: crate::sessions::SessionRow = serde_json::from_value(serde_json::json!({
            "timestamp": "2026-01-01T00:00:00Z", "project": project,
            "hostname": "host", "audit_id": "audit", "provider": "grok",
            "archetype": "bare", "session_id": "historical-grok",
            "sandbox": "workspace-write", "provider_sandbox": name,
            "writable_roots": roots, "operator_prompt": "p",
            "assembled_prompt": "p", "hrafnar_version": "0.0.0"
        }))
        .expect("historical row");
        let mut settings = RunSettings::from_inherited(
            Harness::Grok,
            InheritedSettings::from_record(Some(&row), Harness::Grok),
        );
        // Inject child locations; this does not change production inheritance.
        settings.env = Some(std::collections::BTreeMap::from([
            ("GROK_HOME".into(), home.to_string_lossy().into_owned()),
            ("HOME".into(), "/home/nobody-here".into()),
        ]));
        LaunchSpec {
            harness: Harness::Grok,
            start: SessionStart::Resume(SessionId::parse("historical-grok").expect("id")),
            settings,
            prompt: "continue".into(),
            root: project.clone(),
        }
    };
    for (name, recorded) in [
        ("hrafnar-ws-cbf29ce484222325", vec![]),
        (
            "hrafnar-ws-a4924c479efe6e4c",
            vec![
                "/home/u/.cargo".into(),
                "/home/u/.brokkr".into(),
                "/home/u/.cargo".into(),
            ],
        ),
    ] {
        let expected = RootSet::new(&recorded);
        for _ in 0..2 {
            let launch = launch(name, &recorded);
            let mut plan = super::super::permissions::PermissionPlan::new(
                launch.settings.permissions.clone(),
                &project,
                &launch.settings.writable_roots,
            );
            prepare(&launch, &mut plan).await.expect("resume prepared");
            assert_eq!(plan.permissions.native_to_record(), Some(name));
            assert_eq!(
                plan.paths,
                expected.paths(),
                "only the recorded set, no host grants"
            );
            assert!(plan.uses_recorded_roots());
            assert!(!plan.derives());
            let config: toml::Table = std::fs::read_to_string(home.join("sandbox.toml"))
                .expect("recreated profile")
                .parse()
                .expect("toml");
            let stored: Vec<_> = config["profiles"][name]["read_write"]
                .as_array()
                .expect("roots")
                .iter()
                .map(|v| v.as_str().expect("root"))
                .collect();
            assert_eq!(stored, expected.paths());
            std::fs::remove_file(home.join("sandbox.toml")).expect("delete generated file");
        }
    }
    std::fs::remove_file(home.join("trusted_folders.toml")).expect("undecided project");
    let launch = launch("hrafnar-ws-cbf29ce484222325", &["/different".into()]);
    let mut plan = super::super::permissions::PermissionPlan::new(
        launch.settings.permissions.clone(),
        &project,
        &launch.settings.writable_roots,
    );
    let err = match prepare(&launch, &mut plan).await {
        Err(e) => e,
        Ok(_) => panic!("mismatch must refuse"),
    };
    assert!(err.to_string().contains("do not match"), "{err}");
    assert!(
        !home.join("sandbox.toml").exists(),
        "no mismatched profile written"
    );
    assert!(
        !home.join("trusted_folders.toml").exists(),
        "no trust write"
    );
    let _ = std::fs::remove_dir_all(base);
}
