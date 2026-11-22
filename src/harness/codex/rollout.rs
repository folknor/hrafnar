//! Codex evidence decoding. Callers scope bytes to the invocation before folding.
//! Current-turn state and any answer in that scope are deliberately distinct:
//! a later real turn clears the former, while the watchdog may use the latter.
use crate::transcript::{EventKind, InFlightTool, TranscriptSummary};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PhaseAnswer(pub(crate) String);

pub(crate) struct Rollout {
    pub current_turn: TranscriptSummary,
    pub latest_phase_answer: Option<PhaseAnswer>,
    pub rate_limits: Option<serde_json::Value>,
}

/// The text of a rollout event's payload if it is the turn's real report, i.e.
/// a `phase: "final_answer"` agent message. codex tags interim progress notes
/// `phase: "commentary"`, which never count. Shared with `watchdog` so the two
/// readers cannot disagree about what an answer looks like.
///
/// Two shapes, because codex versions differ in which one carries the phase:
/// - an `event_msg` payload of type `agent_message`, text in `message` (codex
///   0.147.0 and earlier);
/// - a `response_item` payload of type `message` with `role: "assistant"`,
///   text in the `output_text` items of `content` (codex 0.157.0, which
///   writes no `agent_message` event at all).
///
/// Both count, or a clean turn on one version reads as a death and the stall
/// watchdog is blind to its answer. Empty text is not a report for either.
/// `task_complete.last_agent_message` often echoes the answer but never counts
/// alone: `task_complete` fires on aborted turns too.
fn final_answer_text(payload: &serde_json::Value) -> Option<String> {
    if payload.get("phase").and_then(|p| p.as_str()) != Some("final_answer") {
        return None;
    }
    match payload.get("type").and_then(|t| t.as_str()) {
        Some("agent_message") => payload
            .get("message")
            .and_then(|m| m.as_str())
            .filter(|text| !text.is_empty())
            .map(str::to_string),
        Some("message") if payload.get("role").and_then(|r| r.as_str()) == Some("assistant") => {
            let text: String = payload
                .get("content")
                .and_then(|c| c.as_array())?
                .iter()
                .filter(|item| item.get("type").and_then(|t| t.as_str()) == Some("output_text"))
                .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                .collect();
            // A final-phase message with no text is not an answer: counting it
            // would let the watchdog call a run that said nothing Stranded
            // and kill it as finished.
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

/// Parse transcript NDJSON content into a summary. Split from file IO so the
/// event handling can be unit-tested directly. Scoping to a single run's events
/// is the caller's job, via `Scope::bytes`.
pub(crate) fn fold(content: &str) -> Rollout {
    // Per-turn state. A resumed session appends to the same rollout, so this is
    // snapshotted and reset at each turn boundary (see the `task_started` arm).
    #[derive(Default, Clone)]
    struct TurnState {
        task_complete: bool,
        stream_error: bool,
        final_answer: Option<String>,
        /// Raw optional call IDs (including empty strings), preserving insertion order
        /// and distinct id-less calls via a Vec.
        in_flight: Vec<InFlightTool>,
    }

    let mut latest_phase_answer = None;
    let mut rate_limits = None;
    let mut cur = TurnState::default();
    let mut last_event = None;
    // The turn we were in before the most recent `task_started`. Kept so an
    // empty turn that immediately aborts can be rolled back (see below).
    let mut prev: Option<TurnState> = None;
    // Did the current turn emit anything at all? Distinguishes a real turn from
    // codex's shutdown-time phantom turn.
    let mut turn_produced_content = false;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let event: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let top = event.get("type").and_then(|t| t.as_str());
        let payload = event.get("payload");
        let ptype = payload.and_then(|p| p.get("type")).and_then(|t| t.as_str());

        if let Some(answer) = payload.and_then(final_answer_text) {
            latest_phase_answer = Some(PhaseAnswer(answer));
        }
        if ptype == Some("token_count")
            && let Some(limits) = payload.and_then(|p| p.get("rate_limits"))
            && !limits.is_null()
        {
            rate_limits = Some(limits.clone());
        }

        last_event = Some(EventKind {
            top: top.map(str::to_string),
            payload: ptype.map(str::to_string),
        });

        match ptype {
            // A resumed session appends new turns to the same rollout file. Reset
            // per-turn state on each turn boundary so the summary reflects the
            // current (last) turn, not a completed earlier one. The outgoing turn
            // is stashed in `prev` in case this new turn turns out to be a
            // phantom (see the `turn_aborted` arm).
            Some("task_started") => {
                prev = Some(std::mem::take(&mut cur));
                turn_produced_content = false;
            }
            // A turn that aborts without ever carrying a user request or model
            // output is codex's shutdown-time phantom, not a real turn: on
            // codex-cli 0.147.0, `codex exec` opens a turn during teardown and
            // aborts it within milliseconds. Observed shape, in order:
            // `task_started`, a `turn_context`, a synthetic `role: "developer"`
            // message whose text is `<turn_aborted>`, then `turn_aborted` with
            // `reason: "interrupted"` - 46 ms end to end.
            //
            // Note it is *not* an empty event sequence, so "zero items" cannot
            // be the test. What distinguishes it is the absence of both a
            // `user_message` (every real turn is opened by one) and any model
            // output. `turn_produced_content` is therefore keyed on a positive
            // list of real-content events rather than on "any event at all";
            // see the arms below for what counts.
            //
            // Attributing this phantom as the session's final state would make
            // clean, fully-completed sessions report `task_complete=false`, a
            // truncated turn that never happened. Roll back to the real turn
            // instead. A turn that *did* carry content and then aborted is a
            // genuine interruption and is kept as-is.
            Some("turn_aborted") if !turn_produced_content => {
                if let Some(p) = prev.take() {
                    cur = p;
                }
            }
            // The operator's request. Its presence alone proves a real turn:
            // the phantom is synthesised at teardown and never carries one.
            Some("user_message") => turn_produced_content = true,
            // Model output short of an agent message. A turn cancelled after
            // only reasoning is a genuine early cancellation, not a phantom,
            // and must not be rolled back.
            Some("reasoning") => turn_produced_content = true,
            // A conversation message. Excluding `role: "developer"` is the
            // point: that is the synthetic `<turn_aborted>` note codex injects
            // into the phantom itself, so counting it would defeat the check.
            Some("message") => {
                let role = payload.and_then(|p| p.get("role")).and_then(|r| r.as_str());
                if role != Some("developer") {
                    turn_produced_content = true;
                }
                if let Some(text) = payload.and_then(final_answer_text) {
                    cur.final_answer = Some(text);
                }
            }
            Some("task_complete") => {
                turn_produced_content = true;
                cur.task_complete = true;
            }
            Some("stream_error") => {
                turn_produced_content = true;
                cur.stream_error = true;
            }
            // The older shape of the turn's report; see `final_answer_text`.
            Some("agent_message") => {
                turn_produced_content = true;
                if let Some(text) = payload.and_then(final_answer_text) {
                    cur.final_answer = Some(text);
                }
            }
            Some(pt) if pt.ends_with("_call") => {
                turn_produced_content = true;
                if let Some(p) = payload {
                    let call_id = p
                        .get("call_id")
                        .and_then(|c| c.as_str())
                        .map(str::to_string);
                    // A missing ID is not the same as an empty ID. Keep it
                    // unmatched rather than letting an explicit empty ID
                    // retire every call whose ID was absent.
                    let name = p
                        .get("name")
                        .and_then(|n| n.as_str())
                        // `pt` is the event kind (for example,
                        // `function_call`), not the tool's name.
                        .unwrap_or("(unknown tool)")
                        .to_string();
                    let args = p
                        .get("arguments")
                        .or_else(|| p.get("input"))
                        .map(ToString::to_string)
                        .unwrap_or_default();
                    cur.in_flight.push(InFlightTool {
                        call_id,
                        name,
                        arguments: args,
                    });
                }
            }
            Some(pt) if pt.ends_with("_call_output") => {
                turn_produced_content = true;
                let output_call_id = payload
                    .and_then(|p| p.get("call_id"))
                    .and_then(|c| c.as_str());
                // An output without a call_id cannot be safely paired to one
                // of several pending calls, so leave them unchanged.
                if let Some(call_id) = output_call_id {
                    cur.in_flight
                        .retain(|tool| tool.call_id.as_deref() != Some(call_id));
                }
            }
            _ => {}
        }
    }

    let last_in_flight_tool = cur.in_flight.pop();

    let current_turn = TranscriptSummary {
        path: std::path::PathBuf::new(),
        task_complete: cur.task_complete,
        stream_error: cur.stream_error,
        last_event,
        last_in_flight_tool,
        final_answer: cur.final_answer,
    };
    Rollout {
        current_turn,
        latest_phase_answer,
        rate_limits,
    }
}

/// Exec JSON is a different protocol from rollout JSON. Unphased stream text
/// is diagnostic fallback only, never PhaseAnswer and never proof of success.
pub(crate) struct Stream {
    pub session_id: Option<String>,
    pub interim_message: Option<String>,
    pub turns: u32,
    pub usage: super::super::Usage,
    pub log_lines: Vec<String>,
    pub turn_error: Option<String>,
}

pub(crate) fn stream(content: &str) -> Stream {
    use super::super::Usage;
    let mut session_id: Option<String> = None;
    let mut interim_message: Option<String> = None;
    let mut turns: u32 = 0;
    let mut usage = Usage::default();
    let mut log_lines: Vec<String> = Vec::new();
    let mut turn_error: Option<String> = None;
    for line in content.lines() {
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
                session_id = val
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
                    interim_message = item
                        .and_then(|i| i.get("text"))
                        .and_then(|t| t.as_str())
                        .map(String::from);
                }
            }
            // Codex stating why the turn ended: an upstream refusal (content
            // flagged, rate limit, auth) or a harness error. Kept, because
            // without it a fully explained refusal reads as an unexplained
            // death with a bare interim note, and auto-resume replays it into
            // the identical refusal (observed on 0.146.0; the process exited
            // promptly on its own). `turn.failed` nests the message; the bare `error` event carries it
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
                    usage.total_input += get("input_tokens");
                    usage.cached_input += get("cached_input_tokens");
                    usage.output += get("output_tokens");
                    usage.reasoning_output += get("reasoning_output_tokens");
                }
            }
            _ => {}
        }
    }

    Stream {
        session_id,
        interim_message,
        turns,
        usage,
        log_lines,
        turn_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_fixtures as fixtures;

    #[test]
    fn answer_projections_distinguish_real_turns_and_shutdown_phantoms() {
        let answer = format!(
            "{}\n{}\n{}\n",
            fixtures::event(None, "task_started"),
            r#"{"payload":{"type":"agent_message","phase":"final_answer","message":"A"}}"#,
            fixtures::event(None, "task_complete")
        );
        let phantom = format!(
            "{}\n{}\n{}\n",
            fixtures::event(None, "task_started"),
            r#"{"payload":{"type":"message","role":"developer"}}"#,
            r#"{"payload":{"type":"turn_aborted"}}"#
        );
        let rolled_back = fold(&format!("{answer}{phantom}"));
        assert_eq!(rolled_back.current_turn.final_answer.as_deref(), Some("A"));
        assert_eq!(
            rolled_back.latest_phase_answer,
            Some(PhaseAnswer("A".into()))
        );
        assert_eq!(
            rolled_back
                .current_turn
                .last_event
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("?/turn_aborted")
        );
        let real = format!(
            "{}\n{}\n{}\n{}\n",
            fixtures::event(None, "task_started"),
            r#"{"payload":{"type":"user_message","message":"next"}}"#,
            r#"{"payload":{"type":"reasoning"}}"#,
            r#"{"payload":{"type":"turn_aborted"}}"#
        );
        let later = fold(&format!("{answer}{real}"));
        assert!(later.current_turn.final_answer.is_none());
        assert!(!later.current_turn.task_complete);
        assert_eq!(later.latest_phase_answer, Some(PhaseAnswer("A".into())));
        let scoped = fold(&real);
        assert!(scoped.latest_phase_answer.is_none());
    }

    #[test]
    fn stream_interim_and_task_completion_never_become_phase_answers() {
        let raw = format!(
            "{}{}",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"Working\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"last_agent_message\":\"Working\"}}\n"
        );
        assert_eq!(stream(&raw).interim_message.as_deref(), Some("Working"));
        let rollout = fold(&raw);
        assert!(rollout.current_turn.task_complete);
        assert!(rollout.current_turn.final_answer.is_none());
        assert!(rollout.latest_phase_answer.is_none());
    }
}

#[cfg(test)]
mod forensic_value_tests {
    use super::*;
    #[test]
    fn unknown_event_kinds_and_raw_tool_ids_remain_evidence() {
        let event = fold(r#"{"type":"future/top","payload":{"type":"future/payload"}}"#)
            .current_turn
            .last_event
            .expect("event");
        assert_eq!(event.top.as_deref(), Some("future/top"));
        assert_eq!(event.payload.as_deref(), Some("future/payload"));
        assert_eq!(event.to_string(), "future/top/future/payload");
        assert_eq!(
            fold("{}")
                .current_turn
                .last_event
                .expect("unknown")
                .to_string(),
            "?/?"
        );
        let tools = concat!(
            "{\"payload\":{\"type\":\"function_call\",\"call_id\":\"\",\"name\":\"empty\"}}\n",
            "{\"payload\":{\"type\":\"function_call\",\"name\":\"unidentified\"}}\n",
            "{\"payload\":{\"type\":\"function_call_output\"}}\n"
        );
        let tool = fold(tools)
            .current_turn
            .last_in_flight_tool
            .expect("missing output ID matches nothing");
        assert_eq!(tool.name, "unidentified");
        assert!(tool.call_id.is_none());
        let tool = fold(&format!("{tools}{{\"payload\":{{\"type\":\"function_call\",\"call_id\":\"x\",\"name\":\"named\"}}}}\n{{\"payload\":{{\"type\":\"function_call_output\",\"call_id\":\"x\"}}}}\n")).current_turn.last_in_flight_tool.expect("unidentified stays pending");
        assert!(tool.call_id.is_none());
        let empty = fold(
            "{\"payload\":{\"type\":\"function_call\",\"call_id\":\"\",\"name\":\"empty\"}}\n",
        )
        .current_turn
        .last_in_flight_tool
        .expect("empty ID is raw evidence");
        assert_eq!(empty.call_id.as_deref(), Some(""));
    }
}
