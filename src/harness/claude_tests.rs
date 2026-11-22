//! Unit tests of claude harness behavior.

use super::*;
use std::path::Path;

#[tokio::test]
async fn claude_output_past_the_cap_keeps_its_final_result_and_middle_metrics() {
    // The stream ends in the result: keeping only the head would lose it.
    let mut stream = format!("{CLAUDE_INIT}\n");
    let middle = r#"{"type":"result","subtype":"success","is_error":false,"result":"MIDDLE-ANSWER","session_id":"s","num_turns":3,"usage":{"input_tokens":10,"cache_creation_input_tokens":20,"cache_read_input_tokens":30,"output_tokens":40,"output_tokens_details":{"thinking_tokens":5}},"total_cost_usd":0.1,"modelUsage":{"claude-sonnet-4":{"costUSD":0.1}}}"#;
    stream.push_str(middle);
    stream.push('\n');
    for i in 0..1000 {
        stream.push_str(&format!("{{\"type\":\"assistant\",\"n\":{i}}}\n"));
    }
    stream.push_str(CLAUDE_OK);
    stream.push('\n');
    let head = new_shared_buf();
    let metrics = std::sync::Arc::new(std::sync::Mutex::new(super::ClaudeMetrics::default()));
    // A lease with no marker: `ready` only records the fact.
    let owner = Lease::learned(
        Harness::Claude,
        Path::new("/nonexistent"),
        &crate::data::DataDir::unavailable(),
        &crate::sessions::handoff::RunId::default(),
        None,
        None,
    );
    let lease = Lease::clone(&owner);
    // The keep must exceed the result line, as production's 8 MiB does; the
    // stream is several times larger, so the tail really is trimmed.
    let keep = 1024;
    assert!(CLAUDE_OK.len() < keep && stream.len() > 4 * keep);
    let capture = Capture::head_tail(std::sync::Arc::clone(&head), 100, keep);
    read_stream(
        stream.as_bytes(),
        capture.clone(),
        ClaudeObserver {
            metrics: std::sync::Arc::clone(&metrics),
            lease: lease.clone(),
        },
    )
    .await;
    assert!(lease.is_ready(), "the init event is seen");
    let head = head.lock().expect("head").clone();
    let joined_bytes = capture.snapshot();
    assert_eq!(head.len(), 100, "the head is capped");
    let joined = String::from_utf8_lossy(&joined_bytes);
    assert!(
        !joined.contains("MIDDLE-ANSWER"),
        "the head-tail capture trims the middle event"
    );
    let outcome =
        super::interpret_claude_output(Dialect::Claude, &joined, "").expect("the result survived");
    assert!(matches!(outcome, super::ClaudeOutcome::Answered { .. }));
    let metrics = metrics.lock().expect("metrics").clone();
    assert_eq!(metrics.turns, 5);
    assert_eq!(metrics.usage.total_input, 10 + 20 + 30 + 4 + 25470 + 22520);
    assert_eq!(metrics.usage.cached_input, 30 + 22520);
    assert_eq!(metrics.usage.output, 40 + 149);
    assert_eq!(metrics.usage.reasoning_output, 5);
    assert_eq!(
        metrics.served().model_label().as_deref(),
        Some("claude-opus-5-5,claude-sonnet-4")
    );
    let cost = metrics.served().cost_usd.expect("cost");
    assert!((cost - 0.31126).abs() < 1e-12);
}

#[test]
fn a_missing_claude_result_does_not_dump_the_stream_into_the_error() {
    let big = "x".repeat(100_000);
    let stdout = claude_stream(&[
        CLAUDE_INIT,
        &format!(r#"{{"type":"user","tool_result":"{big}"}}"#),
    ]);
    let err = super::interpret_claude_output(Dialect::Claude, &stdout, "").expect_err("no result");
    assert!(
        err.to_string().len() < 2000 + 200,
        "error is {} chars",
        err.to_string().len()
    );
}

/// claude's first `stream-json` event, trimmed.
const CLAUDE_INIT: &str = r#"{"type":"system","subtype":"init","session_id":"2f8b2c2f-d939-4823-a615-7ebd6637f958","model":"claude-opus-5-5"}"#;

/// A clean claude result event, trimmed from a real Claude Code 2.1.284 run.
const CLAUDE_OK: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"LAUNCHED","session_id":"2f8b2c2f-d939-4823-a615-7ebd6637f958","num_turns":2,"terminal_reason":"completed","api_error_status":null,"total_cost_usd":0.21126,"usage":{"input_tokens":4,"cache_creation_input_tokens":25470,"cache_read_input_tokens":22520,"output_tokens":149,"output_tokens_details":{"thinking_tokens":0}},"modelUsage":{"claude-opus-5-5":{"costUSD":0.21126}}}"#;

/// `events` as claude's stream: one event per line.
fn claude_stream(events: &[&str]) -> String {
    events.join("\n") + "\n"
}

#[test]
fn claude_clean_result_is_an_answer() {
    let stdout = claude_stream(&[CLAUDE_INIT, CLAUDE_OK]);
    let outcome =
        super::interpret_claude_output(Dialect::Claude, &stdout, "").expect("a result object");
    let super::ClaudeOutcome::Answered { text, session_id } = outcome else {
        panic!("is_error false is an answer");
    };
    assert_eq!(text, "LAUNCHED");
    assert_eq!(
        session_id.as_deref(),
        Some("2f8b2c2f-d939-4823-a615-7ebd6637f958")
    );
}

#[test]
fn claude_api_error_is_a_stated_failure_that_keeps_its_session() {
    // Real output for `--model no-such-model-xyz`: exit 1, and a result object
    // that still says `subtype: "success"` - only `is_error` gives it away. The
    // session exists on disk, so the id must survive; text mode discarded it.
    let unknown_model = claude_stream(&[
        CLAUDE_INIT,
        r#"{"type":"result","subtype":"success","is_error":true,"result":"There's an issue with the selected model (no-such-model-xyz). It may not exist or you may not have access to it. Run --model to pick a different model.","session_id":"ff2d6c14-1b45-481f-85a2-24e367d9e6ad","num_turns":1,"terminal_reason":"api_error","api_error_status":404,"total_cost_usd":0}"#,
    ]);
    let outcome = super::interpret_claude_output(
        Dialect::Claude,
        &unknown_model,
        r#"[claude-code:unrecognized_model] {"model":"no-such-model-xyz"}"#,
    )
    .expect("an error result is still a result, not a launch failure");
    let super::ClaudeOutcome::NoAnswer {
        stated: Some(reason),
        unverified: None,
        session_id,
        ..
    } = outcome
    else {
        panic!("is_error true is a stated failure, whatever the subtype says");
    };
    assert!(reason.contains("api_error"), "{reason}");
    assert!(reason.contains("404"), "{reason}");
    assert!(
        reason.contains("issue with the selected model"),
        "claude's own explanation is persisted: {reason}"
    );
    assert_eq!(
        session_id.as_deref(),
        Some("ff2d6c14-1b45-481f-85a2-24e367d9e6ad")
    );
}

#[test]
fn claude_result_without_is_error_fails_closed() {
    // A claude that stopped reporting the flag must not pass every run off as
    // answered.
    let unflagged = r#"{"type": "result", "result": "maybe", "session_id": "s"}"#;
    let outcome =
        super::interpret_claude_output(Dialect::Claude, unflagged, "").expect("a result object");
    let super::ClaudeOutcome::NoAnswer {
        stated: None,
        unverified: Some(reason),
        ..
    } = outcome
    else {
        panic!("a result with no is_error is hrafnar's diagnosis, not an answer");
    };
    assert_eq!(
        reason,
        "hrafnar could not classify claude result (no is_error flag): maybe"
    );
}

#[test]
fn claude_output_without_a_result_object_is_a_launch_failure() {
    let err = super::interpret_claude_output(Dialect::Claude, "", "Error: not logged in")
        .expect_err("no result object: nothing ran");
    assert!(err.to_string().contains("not logged in"), "{err}");
    // A stream cut off after its first event is not a result either - the
    // shape a claude killed mid-turn leaves. (`run_claude_compat` still keeps that
    // run's session: see `a_claude_compatible_run_that_dies_after_starting_keeps_its_session`.)
    let err = super::interpret_claude_output(Dialect::Claude, &claude_stream(&[CLAUDE_INIT]), "")
        .expect_err("a system event is not a result");
    assert!(err.to_string().contains("no result object"), "{err}");
}

#[test]
fn claude_interrupted_turn_is_not_an_answer() {
    // Real shape of a SIGINT mid-command: exit 0, and a result that says
    // `error_during_execution` / `aborted_tools` with no `result` text at all.
    // Only `is_error` stops it reading as an empty answer.
    let stdout = claude_stream(&[
        CLAUDE_INIT,
        r#"{"type":"result","subtype":"error_during_execution","is_error":true,"session_id":"4397a6d5-4943-4b52-a0b6-d20c75078bd9","num_turns":3,"terminal_reason":"aborted_tools","stop_reason":"tool_use"}"#,
    ]);
    let outcome =
        super::interpret_claude_output(Dialect::Claude, &stdout, "").expect("a result object");
    let super::ClaudeOutcome::NoAnswer {
        stated: Some(reason),
        session_id,
        ..
    } = outcome
    else {
        panic!("an aborted turn is not an answer");
    };
    assert!(reason.contains("aborted_tools"), "{reason}");
    assert_eq!(
        session_id.as_deref(),
        Some("4397a6d5-4943-4b52-a0b6-d20c75078bd9")
    );
}

#[test]
fn claude_last_result_in_the_stream_is_the_answer() {
    // A background subagent's result is handed back in a second turn, and the
    // stream then carries one result per turn: the first is the model's
    // "launched" note, the last its answer.
    let first = CLAUDE_OK;
    let last = r#"{"type":"result","subtype":"success","is_error":false,"result":"PROBE-B-DONE","session_id":"2f8b2c2f-d939-4823-a615-7ebd6637f958","num_turns":1}"#;
    let stdout = claude_stream(&[CLAUDE_INIT, first, CLAUDE_INIT, last]);
    let outcome =
        super::interpret_claude_output(Dialect::Claude, &stdout, "").expect("a result object");
    let super::ClaudeOutcome::Answered { text, .. } = outcome else {
        panic!("the last result is an answer");
    };
    assert_eq!(text, "PROBE-B-DONE");
}

#[test]
fn claude_usage_and_served_are_read_from_the_result() {
    let (turns, usage) = super::claude_usage(CLAUDE_OK);
    assert_eq!(turns, 2);
    // Uncached input, cache writes and cache reads together, as codex counts
    // input; the reads are the cached share.
    assert_eq!(usage.total_input, 4 + 25470 + 22520);
    assert_eq!(usage.cached_input, 22520);
    assert_eq!(usage.output, 149);
    assert_eq!(
        super::claude_served(CLAUDE_OK),
        super::Served::new(["claude-opus-5-5".to_string()], Some(0.21126))
    );
    assert_eq!(super::claude_usage("not json").0, 0);
}

#[test]
fn claude_served_models_and_cost_sum_every_result() {
    let second = r#"{"type":"result","modelUsage":{"claude-sonnet-4":{}},"total_cost_usd":0.05}"#;
    let stdout = claude_stream(&[CLAUDE_OK, second]);
    let served = super::claude_served(&stdout);
    assert_eq!(
        served.model_label().as_deref(),
        Some("claude-opus-5-5,claude-sonnet-4")
    );
    let cost = served.cost_usd.expect("cost");
    assert!((cost - 0.26126).abs() < 1e-12);
}

#[test]
fn claude_usage_counts_every_result_and_ignores_other_events() {
    let last = r#"{"type":"result","num_turns":1,"usage":{"input_tokens":2,"cache_creation_input_tokens":3,"cache_read_input_tokens":5,"output_tokens":7,"output_tokens_details":{"thinking_tokens":4}}}"#;
    let assistant =
        r#"{"type":"assistant","num_turns":99,"usage":{"input_tokens":999,"output_tokens":999}}"#;
    let stdout = claude_stream(&[CLAUDE_INIT, CLAUDE_OK, assistant, "not json", last]);
    let (turns, usage) = super::claude_usage(&stdout);
    assert_eq!(turns, 3);
    assert_eq!(usage.total_input, 4 + 25470 + 22520 + 2 + 3 + 5);
    assert_eq!(usage.cached_input, 22520 + 5);
    assert_eq!(usage.output, 149 + 7);
    assert_eq!(usage.reasoning_output, 4);
    let (turns, usage) = super::claude_usage(r#"{"type":"result"}"#);
    assert_eq!((turns, usage.total_input), (0, 0));
}

#[tokio::test]
async fn claude_eof_metrics_and_abort_facts_are_independent_of_capture() {
    use tokio::io::AsyncWriteExt;
    let owner = Lease::learned(
        Harness::Claude,
        Path::new("/nonexistent"),
        &crate::data::DataDir::unavailable(),
        &crate::sessions::handoff::RunId::default(),
        None,
        None,
    );
    let lease = Lease::clone(&owner);
    let metrics: SharedClaudeMetrics = Default::default();
    read_stream(
        CLAUDE_OK.as_bytes(),
        Capture::head(new_shared_buf(), 0),
        ClaudeObserver {
            metrics: std::sync::Arc::clone(&metrics),
            lease: lease.clone(),
        },
    )
    .await;
    assert_eq!(
        metrics.lock().expect("test operation succeeds").turns,
        2,
        "EOF partial result counted"
    );
    let (mut writer, reader) = tokio::io::duplex(4096);
    let task = tokio::spawn(read_stream(
        reader,
        Capture::head(new_shared_buf(), 0),
        ClaudeObserver {
            metrics: std::sync::Arc::clone(&metrics),
            lease: lease.clone(),
        },
    ));
    writer
        .write_all(format!("{CLAUDE_INIT}\n{CLAUDE_OK}\nunfinished").as_bytes())
        .await
        .expect("test operation succeeds");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while metrics.lock().expect("metrics").turns != 4 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("metrics published before abort");
    task.abort();
    let _ = task.await;
    assert!(lease.is_ready());
    assert_eq!(metrics.lock().expect("test operation succeeds").turns, 4);
}

#[tokio::test]
async fn claude_init_does_not_accept_an_oversized_line_suffix() {
    let owner = Lease::learned(
        Harness::Claude,
        Path::new("/nonexistent"),
        &crate::data::DataDir::unavailable(),
        &crate::sessions::handoff::RunId::default(),
        None,
        None,
    );
    let lease = Lease::clone(&owner);
    let mut bytes = vec![b' '; CLAUDE_INIT_SCAN_MAX + 1];
    bytes.extend_from_slice(CLAUDE_INIT.as_bytes());
    bytes.push(b'\n');
    read_stream(
        bytes.as_slice(),
        Capture::head(new_shared_buf(), 0),
        ClaudeObserver {
            metrics: Default::default(),
            lease: lease.clone(),
        },
    )
    .await;
    assert!(!lease.is_ready());
    read_stream(
        CLAUDE_INIT.as_bytes(),
        Capture::head(new_shared_buf(), 0),
        ClaudeObserver {
            metrics: Default::default(),
            lease: lease.clone(),
        },
    )
    .await;
    assert!(lease.is_ready(), "eligible EOF init accepted");
}

/// maki's `init`, trimmed from a probe of maki's `-p` stream-json output.
const MAKI_INIT: &str = r#"{"type":"system","subtype":"init","cwd":"/p","session_id":"3f17c0b4-5f22-43a4-a5af-1c8573d9ae40","tools":["read","bash"],"model":"m"}"#;

/// A maki result event with the given `is_error`, `stop_reason` (raw JSON, or
/// `None` to omit the key) and result text.
fn maki_result(is_error: &str, stop_reason: Option<&str>, text: &str) -> String {
    let stop = stop_reason.map_or(String::new(), |r| format!(r#","stop_reason":{r}"#));
    format!(
        r#"{{"type":"result","subtype":"x","is_error":{is_error},"num_turns":1,"result":{text}{stop},"session_id":"3f17c0b4-5f22-43a4-a5af-1c8573d9ae40","total_cost_usd":0.0,"usage":{{"input_tokens":1,"output_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}}"#,
        text = serde_json::Value::String(text.into())
    )
}

fn maki_verdict(result: &str) -> super::ClaudeOutcome {
    super::interpret_claude_output(Dialect::Maki, &claude_stream(&[MAKI_INIT, result]), "")
        .expect("a result object")
}

#[test]
fn maki_answers_only_on_end_turn() {
    let super::ClaudeOutcome::Answered { text, session_id } =
        maki_verdict(&maki_result("false", Some(r#""end_turn""#), "DONE"))
    else {
        panic!("is_error false with end_turn is an answer");
    };
    assert_eq!(text, "DONE");
    assert_eq!(
        session_id.as_deref(),
        Some("3f17c0b4-5f22-43a4-a5af-1c8573d9ae40")
    );
}

#[test]
fn maki_unfinished_endings_are_stated_failures_that_keep_their_text() {
    // maki sets is_error for `cancelled` alone, so a truncated answer would
    // otherwise pass as complete.
    for reason in ["max_tokens", "max_turns", "dropped", "compact"] {
        let outcome = maki_verdict(&maki_result(
            "false",
            Some(&format!("\"{reason}\"")),
            "PARTIAL",
        ));
        let super::ClaudeOutcome::NoAnswer {
            stated: Some(stated),
            unverified: None,
            text,
            ..
        } = outcome
        else {
            panic!("{reason} is not an answer");
        };
        assert!(stated.contains(reason), "{stated}");
        assert_eq!(
            text, "PARTIAL",
            "the partial text is kept as the turn's text"
        );
    }
}

#[test]
fn maki_harness_failure_is_stated_with_its_api_error() {
    // Probed: no credits on the configured backend. Exit 0, is_error true,
    // stop_reason null, the API error as the result text.
    let outcome = maki_verdict(&maki_result(
        "true",
        Some("null"),
        "API error (402): You have no credits remaining.",
    ));
    let super::ClaudeOutcome::NoAnswer {
        stated: Some(stated),
        unverified: None,
        session_id,
        ..
    } = outcome
    else {
        panic!("an API failure is maki's stated failure");
    };
    assert!(stated.contains("API error (402)"), "{stated}");
    assert!(session_id.is_some(), "the session survives an API failure");
    // A cancelled run that hrafnar did not interrupt: someone else signalled it.
    let outcome = maki_verdict(&maki_result("true", Some(r#""cancelled""#), ""));
    let super::ClaudeOutcome::NoAnswer {
        stated: Some(stated),
        ..
    } = outcome
    else {
        panic!("cancelled is stated");
    };
    assert!(stated.contains("stop_reason: cancelled"), "{stated}");
}

#[test]
fn maki_results_review_cannot_classify_fail_closed() {
    for (is_error, stop, what) in [
        ("false", Some("null"), "is_error false with no ending"),
        ("false", Some(r#""new_reason""#), "an unfamiliar ending"),
        ("false", Some("7"), "a non-string ending"),
        ("false", None, "a missing stop_reason"),
    ] {
        let outcome = maki_verdict(&maki_result(is_error, stop, "maybe"));
        let super::ClaudeOutcome::NoAnswer {
            stated: None,
            unverified: Some(unverified),
            text,
            ..
        } = outcome
        else {
            panic!("{what} is hrafnar's diagnosis, not an answer");
        };
        assert!(
            unverified.contains("could not classify maki"),
            "{what}: {unverified}"
        );
        assert_eq!(text, "maybe", "{what}: the text survives");
    }
    // A stated error without the key keeps both facts.
    let outcome = maki_verdict(&maki_result("true", None, "boom"));
    let super::ClaudeOutcome::NoAnswer {
        stated: Some(stated),
        unverified: Some(unverified),
        ..
    } = outcome
    else {
        panic!("both facts survive");
    };
    assert!(stated.contains("boom"), "{stated}");
    assert!(unverified.contains("no stop_reason"), "{unverified}");
    let unflagged = r#"{"type":"result","result":"x","stop_reason":"end_turn"}"#;
    assert!(matches!(
        maki_verdict(unflagged),
        super::ClaudeOutcome::NoAnswer {
            stated: None,
            unverified: Some(_),
            ..
        }
    ));
}

#[test]
fn maki_setup_failure_detail_is_free_of_terminal_escapes() {
    // Probed: an unknown --resume id exits 1 with nothing on stdout and a
    // coloured banner on stderr.
    let err = super::interpret_claude_output(
        Dialect::Maki,
        "",
        "\n\u{1b}[1;31m\u{2716} not found: 1111114bZ6BZRUqUqZeo\u{1b}[0m\n",
    )
    .expect_err("no result object");
    let message = err.to_string();
    assert!(
        message.starts_with("maki produced no result object"),
        "{message}"
    );
    assert!(
        message.contains("not found: 1111114bZ6BZRUqUqZeo"),
        "{message}"
    );
    assert!(!message.contains('\u{1b}'), "{message:?}");
}

#[test]
fn maki_zero_cost_is_unknown_and_no_served_model_is_invented() {
    let zero = maki_result("false", Some(r#""end_turn""#), "x");
    let metrics = super::summarize_output(Dialect::Maki, &claude_stream(&[MAKI_INIT, &zero]));
    assert_eq!(metrics.turns, 1);
    assert_eq!(metrics.usage.output, 2);
    assert_eq!(
        metrics.served(),
        super::Served::default(),
        "maki's 0.0 is unpriced, not free; its init model is the requested one"
    );
    let priced = zero.replace(r#""total_cost_usd":0.0"#, r#""total_cost_usd":0.25"#);
    let metrics = super::summarize_output(Dialect::Maki, &priced);
    assert_eq!(metrics.served().cost_usd, Some(0.25));
    // claude's zero is a real zero.
    let metrics = super::summarize_output(Dialect::Claude, &zero);
    assert_eq!(metrics.served().cost_usd, Some(0.0));
}

#[test]
fn maki_argv_never_carries_a_flag_maki_would_drop() {
    let fresh = SessionStart::Fresh {
        id: Some(SessionId::parse("0b6c1f3e-4a2d-4e8b-9c51-7d2f8a6e3b10").expect("id")),
    };
    let resume = SessionStart::Resume(SessionId::parse("1111114bZ6BZRUqUqZeo").expect("id"));
    let selection = ModelSpec::new(Harness::Maki, Some("openrouter/qwen"), None, &[]);
    let fresh_args = super::launch_args(Dialect::Maki, &fresh, &selection);
    assert_eq!(
        fresh_args,
        [
            "--session-id",
            "0b6c1f3e-4a2d-4e8b-9c51-7d2f8a6e3b10",
            "-p",
            "--output-format",
            "stream-json",
            "--model",
            "openrouter/qwen"
        ]
    );
    let resume_args = super::launch_args(Dialect::Maki, &resume, &selection);
    assert_eq!(resume_args[..2], ["--resume", "1111114bZ6BZRUqUqZeo"]);
    for args in [fresh_args, resume_args] {
        for arg in &args {
            assert!(
                !super::MAKI_IGNORED_FLAGS.contains(&arg.as_str()),
                "maki ignores {arg}"
            );
            assert!(
                arg != "--permission-mode" && arg != "--trust" && arg != "--yolo",
                "{arg} must never reach maki"
            );
        }
    }
    assert!(super::forced_env(Dialect::Maki).is_empty());
}

/// The events claude 2.1.330 printed for an expired, unrefreshable login,
/// reduced to the fields the classification reads.
const AUTH_HOOK: &str = r#"{"type":"system","subtype":"hook_started"}"#;
const AUTH_INIT: &str = r#"{"type":"system","subtype":"init","session_id":"s"}"#;
const AUTH_ERROR: &str = r#"{"type":"assistant","error":"authentication_failed","parent_tool_use_id":null,"message":{"content":[{"type":"text","text":"Failed to authenticate"}]}}"#;
const AUTH_RESULT: &str = r#"{"type":"result","subtype":"success","is_error":true,"terminal_reason":"api_error","result":"Failed to authenticate: OAuth session expired and could not be refreshed","session_id":"s"}"#;

fn auth_of(lines: &[&str], eof: bool) -> Option<AuthFailure> {
    let mut metrics = ClaudeMetrics::new(Dialect::Claude);
    for line in lines {
        metrics.record_line(line.as_bytes());
    }
    if eof {
        metrics.eof();
    }
    metrics.auth_failure()
}

#[test]
fn an_auth_failure_before_any_work_is_replayable() {
    assert_eq!(
        auth_of(&[AUTH_HOOK, AUTH_INIT, AUTH_ERROR, AUTH_RESULT], true),
        Some(AuthFailure::BeforeWork)
    );
}

/// Model output or tool activity first means a resend could repeat work.
#[test]
fn an_auth_failure_after_work_is_not_replayable() {
    let text = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#;
    let tool = r#"{"type":"user","message":{"content":[{"type":"tool_result"}]}}"#;
    for work in [text, tool] {
        assert_eq!(
            auth_of(&[AUTH_INIT, work, AUTH_ERROR, AUTH_RESULT], true),
            Some(AuthFailure::Unknown),
            "{work}"
        );
        // An early auth error the run got past, then work, is not replayable.
        assert_eq!(
            auth_of(&[AUTH_INIT, AUTH_ERROR, work, AUTH_RESULT], true),
            Some(AuthFailure::Unknown),
            "{work}"
        );
    }
}

/// The auth error must belong to the result that ended the run: one closed by
/// some other ending, or by a turn before a later failure, is not the run's.
#[test]
fn an_auth_error_not_closed_by_the_last_result_is_not_the_runs() {
    let other = r#"{"type":"result","is_error":true,"terminal_reason":"max_turns"}"#;
    let completed = r#"{"type":"result","is_error":false,"terminal_reason":"completed"}"#;
    assert_eq!(auth_of(&[AUTH_INIT, AUTH_ERROR, other], true), None);
    assert_eq!(
        auth_of(&[AUTH_INIT, AUTH_ERROR, completed, AUTH_RESULT], true),
        None
    );
    // An auth error with no result after it at all: reported, not replayed.
    assert_eq!(
        auth_of(&[AUTH_INIT, AUTH_ERROR], true),
        Some(AuthFailure::Unknown)
    );
}

/// A partial view of the stream cannot prove nothing ran.
#[test]
fn an_incompletely_observed_auth_failure_is_not_replayable() {
    assert_eq!(
        auth_of(&[AUTH_INIT, AUTH_ERROR, AUTH_RESULT], false),
        Some(AuthFailure::Unknown)
    );
    assert_eq!(
        auth_of(&[AUTH_INIT, "not json", AUTH_ERROR, AUTH_RESULT], true),
        Some(AuthFailure::Unknown)
    );
    let mut metrics = ClaudeMetrics::new(Dialect::Claude);
    metrics.record_line(AUTH_INIT.as_bytes());
    metrics.skipped();
    metrics.record_line(AUTH_ERROR.as_bytes());
    metrics.eof();
    assert_eq!(metrics.auth_failure(), Some(AuthFailure::Unknown));
}

/// A subagent's system event, or tool use carried on the auth event itself,
/// is work a resend would repeat.
#[test]
fn subagent_and_tool_activity_beside_the_auth_error_are_work() {
    let sub_system =
        r#"{"type":"system","subtype":"task_notification","parent_tool_use_id":"toolu_1"}"#;
    let acting_error = r#"{"type":"assistant","error":"authentication_failed","parent_tool_use_id":null,"message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#;
    // The documented task events carry no parent_tool_use_id at all.
    let task = r#"{"type":"system","subtype":"task_started","tool_use_id":"toolu_1"}"#;
    for activity in [sub_system, task] {
        assert_eq!(
            auth_of(&[AUTH_INIT, activity, AUTH_ERROR, AUTH_RESULT], true),
            Some(AuthFailure::Unknown),
            "{activity}"
        );
    }
    assert_eq!(
        auth_of(&[AUTH_INIT, acting_error, AUTH_RESULT], true),
        Some(AuthFailure::Unknown)
    );
}

/// A subagent's credential error is not the run's, and is itself work.
#[test]
fn a_subagent_auth_error_is_not_the_runs() {
    let sub =
        r#"{"type":"assistant","error":"authentication_failed","parent_tool_use_id":"toolu_1"}"#;
    assert_eq!(auth_of(&[AUTH_INIT, sub, AUTH_RESULT], true), None);
    assert_eq!(
        auth_of(&[AUTH_INIT, sub, AUTH_ERROR, AUTH_RESULT], true),
        Some(AuthFailure::Unknown)
    );
}

#[test]
fn an_ordinary_api_error_is_not_an_auth_failure() {
    assert_eq!(auth_of(&[AUTH_INIT, AUTH_RESULT], true), None);
}

use crate::harness::claude_stream::{Denial, denials_in};

fn denial(id: &str, description: &str) -> Denial {
    Denial {
        id: Some(id.into()),
        description: description.into(),
    }
}

/// claude's result lists refused calls in `permission_denials`. The entry
/// shape is the Agent SDK's `SDKPermissionDenial`, not yet captured from a
/// real dontAsk run.
const CLAUDE_DENIED_RESULT: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"s","num_turns":2,"permission_denials":[{"tool_name":"Bash","tool_use_id":"toolu_1","tool_input":{"command":"rm -rf\n build","timeout":5000}},{"tool_name":"Write","tool_use_id":"toolu_2","tool_input":{"file_path":"/etc/x","content":"y"}},{"tool_name":"WebSearch","tool_use_id":"toolu_3","tool_input":{"query":"q"}}]}"#;

/// maki 0.7.3's stream-json `user` event for a refused call: the tool result
/// is the gate's message, flagged as an error.
const MAKI_DENIED_USER: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"Permission denied for `bash` (head -3): deny rule `head *` for `bash`. Do not retry. Try a different approach or ask the user for guidance.","is_error":true},{"type":"tool_result","tool_use_id":"t2","content":"ok"}]},"session_id":"s"}"#;

#[test]
fn claude_permission_denials_are_decoded_from_the_result() {
    assert_eq!(
        denials_in(Dialect::Claude, CLAUDE_DENIED_RESULT.as_bytes()),
        vec![
            denial("toolu_1", "Bash (rm -rf build)"),
            denial("toolu_2", "Write (/etc/x)"),
            denial("toolu_3", "WebSearch"),
        ]
    );
    // An entry in an unknown shape still counts.
    let odd = r#"{"type":"result","permission_denials":["Bash"]}"#;
    assert_eq!(
        denials_in(Dialect::Claude, odd.as_bytes()),
        vec![Denial {
            id: None,
            description: "Bash".into()
        }]
    );
    // Only a result carries the list, and an empty one is no denial.
    let empty = r#"{"type":"result","permission_denials":[]}"#;
    let elsewhere = r#"{"type":"assistant","permission_denials":[{"tool_name":"Bash"}]}"#;
    for line in [empty, elsewhere, CLAUDE_OK] {
        assert!(
            denials_in(Dialect::Claude, line.as_bytes()).is_empty(),
            "{line}"
        );
    }
    // maki's marker means nothing to claude.
    assert!(denials_in(Dialect::Claude, MAKI_DENIED_USER.as_bytes()).is_empty());
}

#[test]
fn maki_permission_denials_are_decoded_from_tool_results() {
    assert_eq!(
        denials_in(Dialect::Maki, MAKI_DENIED_USER.as_bytes()),
        vec![denial("t1", "bash (head -3)")]
    );
    // The marker in a result that is not an error, or in model text, is not
    // a refusal.
    let not_error = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"Permission denied for `bash` (ls): x."}]}}"#;
    let text = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Permission denied for `bash` (ls): x."}]}}"#;
    for line in [not_error, text] {
        assert!(
            denials_in(Dialect::Maki, line.as_bytes()).is_empty(),
            "{line}"
        );
    }
    // A message after the prefix in another shape is shown as it reads.
    let other = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t9","content":"Permission denied for plugin call\nsecond line","is_error":true}]}}"#;
    assert_eq!(
        denials_in(Dialect::Maki, other.as_bytes()),
        vec![denial("t9", "plugin call second line")]
    );
    assert!(denials_in(Dialect::Maki, CLAUDE_DENIED_RESULT.as_bytes()).is_empty());
}

/// Denials are counted once per call across every result, bounded in what
/// is shown, and leave the answer standing.
#[test]
fn denials_are_counted_once_and_bounded_beside_an_answer() {
    let mut metrics = ClaudeMetrics::new(Dialect::Claude);
    metrics.record_line(CLAUDE_DENIED_RESULT.as_bytes());
    // A later result repeating the list adds nothing.
    metrics.record_line(CLAUDE_DENIED_RESULT.as_bytes());
    assert_eq!(metrics.denials.count, 3);
    let many: Vec<String> = (0..9)
        .map(|i| {
            format!(
                r#"{{"tool_name":"Bash","tool_use_id":"m{i}","tool_input":{{"command":"c{i}"}}}}"#
            )
        })
        .collect();
    let line = format!(
        r#"{{"type":"result","permission_denials":[{}]}}"#,
        many.join(",")
    );
    metrics.record_line(line.as_bytes());
    assert_eq!(metrics.denials.count, 12);
    assert_eq!(metrics.denials.shown.len(), Denials::SHOWN);
    let rendered = metrics.denials.line().expect("a denial line");
    assert!(
        rendered.starts_with("permission denied: 12 tool calls: Bash (rm -rf build); "),
        "{rendered}"
    );
    assert!(rendered.ends_with("(+7 more)"), "{rendered}");

    let mut maki = ClaudeMetrics::new(Dialect::Maki);
    maki.record_line(MAKI_DENIED_USER.as_bytes());
    assert_eq!(
        maki.denials.line().as_deref(),
        Some("permission denied: 1 tool call: bash (head -3)")
    );

    // The outcome is decided by the result alone.
    let outcome =
        interpret_claude_output(Dialect::Claude, CLAUDE_DENIED_RESULT, "").expect("result");
    assert!(
        matches!(outcome, ClaudeOutcome::Answered { .. }),
        "{outcome:?}"
    );
}

#[test]
fn denials_survive_the_persisted_digest() {
    let mut denials = Denials::default();
    denials.push("Bash (ls)");
    let digest = Digest {
        denials: denials.clone(),
        ..Digest::default()
    };
    let summary = digest.summary(&Outcome::answered(Answer::Captured));
    assert_eq!(summary.permission_denials, 1);
    let row = serde_json::to_value(&summary).expect("row");
    let back: DigestSummary = serde_json::from_value(row).expect("read back");
    assert_eq!(back.denials(), denials);
    // A run with none writes neither field, so old rows read unchanged.
    let none = serde_json::to_value(Digest::default().summary(&Outcome::no_answer())).expect("row");
    assert!(none.get("permission_denials").is_none() && none.get("denied").is_none());
}
