//! Unit tests of claude provider behavior.

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
    let owner = Lease::learned(Provider::Claude, Path::new("/nonexistent"), None);
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
        super::interpret_claude_output("claude", &joined, "").expect("the result survived");
    assert!(matches!(outcome, super::ClaudeOutcome::Answered { .. }));
    let metrics = metrics.lock().expect("metrics").clone();
    assert_eq!(metrics.turns, 5);
    assert_eq!(metrics.usage.input_tokens, 10 + 20 + 30 + 4 + 25470 + 22520);
    assert_eq!(metrics.usage.cached_input_tokens, 30 + 22520);
    assert_eq!(metrics.usage.output_tokens, 40 + 149);
    assert_eq!(metrics.usage.reasoning_output_tokens, 5);
    assert_eq!(
        metrics.served().model.as_deref(),
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
    let err = super::interpret_claude_output("claude", &stdout, "").expect_err("no result");
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
    let outcome = super::interpret_claude_output("claude", &stdout, "").expect("a result object");
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
        "claude",
        &unknown_model,
        r#"[claude-code:unrecognized_model] {"model":"no-such-model-xyz"}"#,
    )
    .expect("an error result is still a result, not a launch failure");
    let super::ClaudeOutcome::NoAnswer {
        reason, session_id, ..
    } = outcome
    else {
        panic!("is_error true is not an answer, whatever the subtype says");
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
    let outcome = super::interpret_claude_output("claude", unflagged, "").expect("a result object");
    let super::ClaudeOutcome::NoAnswer {
        reason,
        review_error,
        ..
    } = outcome
    else {
        panic!("a result with no is_error is not trusted as an answer");
    };
    assert!(reason.contains("no is_error flag"), "{reason}");
    assert!(review_error, "the missing flag is review's diagnosis");
    let outcome = super::Outcome::no_answer().unverified(reason);
    assert!(outcome.stated.is_none());
    assert_eq!(
        outcome.unverified.as_deref(),
        Some("review could not classify claude result (no is_error flag): maybe")
    );
    assert!(outcome.failed());
}

#[test]
fn claude_output_without_a_result_object_is_a_launch_failure() {
    let err = super::interpret_claude_output("claude", "", "Error: not logged in")
        .expect_err("no result object: nothing ran");
    assert!(err.to_string().contains("not logged in"), "{err}");
    // A stream cut off after its first event is not a result either - the
    // shape a claude killed mid-turn leaves. (`run_claude` still keeps that
    // run's session: see `a_claude_that_dies_after_starting_keeps_its_session`.)
    let err = super::interpret_claude_output("claude", &claude_stream(&[CLAUDE_INIT]), "")
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
    let outcome = super::interpret_claude_output("claude", &stdout, "").expect("a result object");
    let super::ClaudeOutcome::NoAnswer {
        reason, session_id, ..
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
    let outcome = super::interpret_claude_output("claude", &stdout, "").expect("a result object");
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
    assert_eq!(usage.input_tokens, 4 + 25470 + 22520);
    assert_eq!(usage.cached_input_tokens, 22520);
    assert_eq!(usage.output_tokens, 149);
    assert_eq!(
        super::claude_served(CLAUDE_OK),
        super::Served {
            model: Some("claude-opus-5-5".to_string()),
            cost_usd: Some(0.21126),
        }
    );
    assert_eq!(super::claude_usage("not json").0, 0);
}

#[test]
fn claude_served_models_and_cost_sum_every_result() {
    let second = r#"{"type":"result","modelUsage":{"claude-sonnet-4":{}},"total_cost_usd":0.05}"#;
    let stdout = claude_stream(&[CLAUDE_OK, second]);
    let served = super::claude_served(&stdout);
    assert_eq!(
        served.model.as_deref(),
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
    assert_eq!(usage.input_tokens, 4 + 25470 + 22520 + 2 + 3 + 5);
    assert_eq!(usage.cached_input_tokens, 22520 + 5);
    assert_eq!(usage.output_tokens, 149 + 7);
    assert_eq!(usage.reasoning_output_tokens, 4);
    let (turns, usage) = super::claude_usage(r#"{"type":"result"}"#);
    assert_eq!((turns, usage.input_tokens), (0, 0));
}

#[tokio::test]
async fn claude_eof_metrics_and_abort_facts_are_independent_of_capture() {
    use tokio::io::AsyncWriteExt;
    let owner = Lease::learned(Provider::Claude, Path::new("/nonexistent"), None);
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
    let owner = Lease::learned(Provider::Claude, Path::new("/nonexistent"), None);
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
