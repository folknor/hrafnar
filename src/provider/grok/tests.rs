//! Unit tests of grok provider behavior.

use super::super::tests::exited;

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
        super::Served {
            model: Some("grok-4.7-build".to_string()),
            cost_usd: Some(0.02397),
        }
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
        review_error,
        text,
        session_id,
    } = outcome
    else {
        panic!("a cancelled turn is not an answer");
    };
    assert!(!review_error, "grok stated the stop reason");
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
    let digest = super::no_answer_digest(reason, Some(&exited(1)));
    assert_eq!(
        digest.exit_code,
        Some(1),
        "the digest reports the status we observed"
    );
    assert!(!digest.captured, "nothing was captured");
    assert!(
        !digest.recovered_from_transcript,
        "grok has no rollout to recover from"
    );
    assert!(
        digest.summary().turn_error.is_some(),
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
        usage.input_tokens,
        6716 + 30592,
        "codex-shaped: input includes cached"
    );
    assert_eq!(usage.cached_input_tokens, 30592);
    assert_eq!(usage.output_tokens, 154);
    assert_eq!(usage.reasoning_output_tokens, 82);
    let (turns, usage) = super::grok_usage("not json");
    assert_eq!((turns, usage.input_tokens), (0, 0));
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
        Some("permission_cancelled: permission for `search_replace` was refused"),
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
        review_error,
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
        review_error,
        "the missing stop reason is review's diagnosis"
    );
    let digest = super::review_no_answer_digest(reason, Some(&exited(0)));
    assert!(digest.turn_error.is_none());
    assert!(
        digest
            .review_error
            .as_deref()
            .is_some_and(|message| message.contains("stopReason: missing"))
    );
    assert!(digest.failed());
}

#[test]
fn json_that_is_not_a_result_object_is_a_launch_failure() {
    // Every field of a result object is optional, so *any* JSON object
    // deserializes into an all-default `GrokResult`. Reading that as "a turn
    // ran but did not answer" would infer the fact from successful parsing
    // rather than from evidence - and `NoAnswer` returns `Ok`, which tells
    // `review resume` the prompt cache was warmed. An unrecognised pre-turn failure
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

#[test]
fn review_verification_failure_preserves_a_real_answer_and_observed_status() {
    let digest = super::wake_failure_digest("unread background results".to_string(), None);
    assert!(digest.captured, "verification cannot retract an answer");
    assert_eq!(
        digest.exit_code,
        Some(0),
        "the completed turn exited cleanly"
    );
    assert!(
        digest.turn_error.is_none(),
        "grok did not state this diagnosis"
    );
    assert!(digest.failed(), "unverified work still fails the run");
    let summary = digest.summary();
    assert_eq!(
        summary.review_error.as_deref(),
        Some("unread background results")
    );
    assert!(summary.captured);
}

#[test]
fn review_verification_failure_preserves_provider_failure_and_usage() {
    let mut last = super::no_answer_digest("cancelled".to_string(), Some(&exited(7)));
    last.turns = 3;
    last.usage.output_tokens = 42;
    let digest = super::wake_failure_digest("cannot read update log".to_string(), Some(last));
    assert!(!digest.captured);
    assert_eq!(digest.exit_code, Some(7));
    assert_eq!(digest.turn_error.as_deref(), Some("cancelled"));
    assert_eq!(digest.turns, 3);
    assert_eq!(digest.usage.output_tokens, 42);
    assert!(digest.failed());
}

#[test]
fn review_verification_failure_preserves_a_signal_after_an_answer() {
    use std::os::unix::process::ExitStatusExt;
    let status = std::process::ExitStatus::from_raw(libc::SIGTERM);
    let mut last = super::no_answer_digest(String::new(), Some(&status));
    last.turn_error = None;
    last.captured = true;
    assert!(
        !last.failed(),
        "post-answer process status alone is not a failed turn"
    );
    let digest = super::wake_failure_digest("cannot verify results".to_string(), Some(last));
    assert!(digest.captured);
    assert_eq!(digest.exit_code, None);
    assert_eq!(digest.signal.as_deref(), Some("SIGTERM"));
    assert!(digest.turn_error.is_none());
    assert!(digest.failed());
}
