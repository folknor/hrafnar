//! Codex rollout progress source and its measured silence/answer policy.
use crate::watchdog::{Evidence, Kind, Source, Timings, Verdict, monitor};
use std::path::PathBuf;

/// Watch `session_id`'s rollout until it looks stranded, then resolve.
///
/// Resolves *only* on the stranded verdict - on any other condition (no session
/// id yet, no rollout file, file still advancing) it simply keeps polling
/// forever, so the caller can hold it in a `select!` against `child.wait()`
/// without it ever pre-empting a healthy run.
///
/// `session_rx` is a watch channel because a fresh run does not know its session
/// id until `thread.started` arrives on the NDJSON stream; on a resume it is
/// populated from the start. `baseline` is the rollout's size at spawn, which
/// scopes the answer scan to this run's own events.
pub async fn wait_for_stranded_completion(
    session_rx: tokio::sync::watch::Receiver<Option<String>>,
    codex_home: Option<PathBuf>,
    baseline: Option<u64>,
    timings: Timings,
) -> Verdict {
    // Unknown scope cannot license either answer-based stopping or a stall.
    let baseline = match baseline {
        Some(baseline) => baseline,
        None => std::future::pending().await,
    };
    let mut source = CodexRollout {
        session_rx,
        home: codex_home,
        baseline,
        cached_transcript: None,
        unreadable_session: None,
    };
    let detection = monitor(&mut source, timings).await;
    let quiet_secs = detection.quiet_secs;
    let last_event = detection.evidence.last_event;
    let path = source
        .cached_transcript
        .as_ref()
        .map(|(_, path)| path.display().to_string())
        .unwrap_or_default();
    let reason = match detection.kind {
        Kind::Stranded => format!(
            "codex has written a final answer to its rollout but has not exited, \
             and the rollout has not advanced in {quiet_secs}s.\n  \
             Treating the run as complete and terminating codex.\n  transcript: {path}"
        ),
        Kind::Stalled => format!(
            "stall timeout: codex has written nothing to its rollout in \
             {quiet_secs}s and has produced no final answer.\n  \
             codex wakes itself every few minutes even while waiting on a \
             long tool call, so silence this long means the run is wedged, not working.\n  \
             Terminating it and reporting failure.\n  last rollout event: {}\n  transcript: {path}",
            last_event.as_deref().unwrap_or("(none)")
        ),
    };
    Verdict {
        kind: detection.kind,
        final_answer: detection.evidence.answer.map(|answer| answer.0),
        reason,
        quiet_secs,
        last_event,
    }
}

/// Codex's append-only rollout length is progress. Any length change, including
/// truncation, resets patience. Equal-length replacement is not detected.
/// Partial/malformed bytes count as progress but never as answer evidence.
struct CodexRollout {
    session_rx: tokio::sync::watch::Receiver<Option<String>>,
    home: Option<PathBuf>,
    baseline: u64,
    cached_transcript: Option<(String, PathBuf)>,
    unreadable_session: Option<String>,
}

impl Source for CodexRollout {
    type Answer = super::rollout::PhaseAnswer;
    async fn revision(&mut self) -> Option<u64> {
        // Still waiting to learn which session this is (fresh run, pre-
        // `thread.started`). Nothing to watch yet.
        let session_id = self.session_rx.borrow_and_update().clone()?;

        let codex_home = self.home.as_deref()?;

        let (path, cache_matches) = match self.cached_transcript.as_ref() {
            Some((cached_id, path)) if cached_id == &session_id => (path.clone(), true),
            _ => {
                match crate::transcript::find_transcript_path_in_home_lookup(
                    &session_id,
                    codex_home,
                ) {
                    crate::transcript::Lookup::Found(path) => {
                        self.unreadable_session = None;
                        (path, false)
                    }
                    crate::transcript::Lookup::Absent | crate::transcript::Lookup::UnknownHome => {
                        return None;
                    }
                    crate::transcript::Lookup::Unreadable(error) => {
                        if self.unreadable_session.as_deref() != Some(session_id.as_str()) {
                            eprintln!(
                                "warning: rollout search for codex session {session_id} is unreadable: {error}"
                            );
                            self.unreadable_session = Some(session_id);
                        }
                        return None;
                    }
                }
            }
        };

        let Ok(meta) = tokio::fs::metadata(&path).await else {
            // If the path vanished after it was cached, allow the next poll to
            // locate it again in case codex replaced the rollout.
            if cache_matches {
                self.cached_transcript = None;
            }
            return None;
        };
        if !cache_matches {
            self.cached_transcript = Some((session_id, path.clone()));
        }
        Some(meta.len())
    }

    async fn evidence(&mut self) -> Option<Evidence<Self::Answer>> {
        let (_, path) = self.cached_transcript.as_ref()?;
        let bytes = tokio::fs::read(path).await.ok()?;
        let ours = scan_from_baseline(&bytes, self.baseline);
        let folded = crate::provider::codex::rollout::fold(&String::from_utf8_lossy(ours));
        Some(Evidence {
            answer: folded.latest_phase_answer,
            last_event: folded.current_turn.last_event,
        })
    }
}

fn scan_from_baseline(bytes: &[u8], baseline: u64) -> &[u8] {
    crate::transcript::slice_from_offset(bytes, Some(baseline))
}

#[cfg(test)]
fn final_answer(bytes: &[u8]) -> Option<String> {
    crate::provider::codex::rollout::fold(&String::from_utf8_lossy(bytes))
        .latest_phase_answer
        .map(|answer| answer.0)
}

#[cfg(test)]
fn last_event(bytes: &[u8]) -> Option<String> {
    crate::provider::codex::rollout::fold(&String::from_utf8_lossy(bytes))
        .current_turn
        .last_event
}

#[cfg(test)]
mod tests {
    use super::*;

    const TASK_STARTED: &str = r#"{"timestamp":"2026-08-11T07:25:08.000Z","type":"event_msg","payload":{"type":"task_started"}}"#;
    const FINAL_ANSWER: &str = r#"{"timestamp":"2026-08-11T07:42:20.000Z","type":"event_msg","payload":{"type":"agent_message","message":"Done.","phase":"final_answer"}}"#;
    const COMMENTARY: &str = r#"{"timestamp":"2026-08-11T07:30:00.000Z","type":"event_msg","payload":{"type":"agent_message","message":"I am now regenerating the pilot.","phase":"commentary"}}"#;

    fn scan(rollout: &str, baseline: u64) -> bool {
        final_answer(scan_from_baseline(rollout.as_bytes(), baseline)).is_some()
    }

    #[test]
    fn detects_a_final_answer_from_this_run() {
        let rollout = format!("{TASK_STARTED}\n{FINAL_ANSWER}\n");
        assert!(scan(&rollout, 0));
    }

    // codex 0.157.0 writes no `agent_message` event; the phase-tagged answer is
    // an assistant `message` response item. A stranded 0.157.0 run must still be
    // recognised as answered.
    #[test]
    fn detects_a_final_answer_as_an_assistant_message() {
        let answer = r#"{"timestamp":"2026-09-29T06:28:09.406Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done."}],"phase":"final_answer"}}"#;
        let rollout = format!("{TASK_STARTED}\n{answer}\n");
        assert!(scan(&rollout, 0));
    }

    #[test]
    fn latest_answer_survives_trailing_turns() {
        let later = FINAL_ANSWER.replace("Done.", "Latest.");
        let rollout =
            format!("{FINAL_ANSWER}\n{TASK_STARTED}\n{later}\n{TASK_STARTED}\n{COMMENTARY}\n");
        assert_eq!(final_answer(rollout.as_bytes()).as_deref(), Some("Latest."));
    }

    #[test]
    fn commentary_alone_is_not_an_answer() {
        // The mid-turn wedge case: interim notes only. Killing here would throw
        // away a run that never reported.
        let rollout = format!("{TASK_STARTED}\n{COMMENTARY}\n");
        assert!(!scan(&rollout, 0));
    }

    #[test]
    fn a_previous_turns_answer_does_not_count() {
        // A resume appends to the same rollout. The prior turn's answer must not
        // make us kill a resume that has produced nothing yet.
        let prior = format!("{TASK_STARTED}\n{FINAL_ANSWER}\n");
        let baseline = u64::try_from(prior.len()).expect("test fixture fits in u64");
        let rollout = format!("{prior}{TASK_STARTED}\n{COMMENTARY}\n");
        assert!(!scan(&rollout, baseline));
        // Sanity: without the baseline the stale answer would have counted,
        // which is exactly the misfire the offset prevents.
        assert!(scan(&rollout, 0));
    }

    // The offset is immune to clock resolution: an earlier turn that completed
    // in the *same second* as this run's launch is still excluded, where a
    // second-truncated timestamp comparison would have let it through and
    // licensed killing a resume that had produced nothing.
    #[test]
    fn a_prior_answer_in_the_same_second_does_not_count() {
        let same_second_answer = r#"{"timestamp":"2026-08-11T07:25:08.100Z","type":"event_msg","payload":{"type":"agent_message","message":"Old answer.","phase":"final_answer"}}"#;
        let prior = format!("{same_second_answer}\n");
        let baseline = u64::try_from(prior.len()).expect("test fixture fits in u64");
        let launched_same_second = r#"{"timestamp":"2026-08-11T07:25:08.900Z","type":"event_msg","payload":{"type":"task_started"}}"#;
        let rollout = format!("{prior}{launched_same_second}\n");
        assert!(!scan(&rollout, baseline));
    }

    // The last-event probe must be scoped to this run's bytes, like the answer
    // check. A resume that stalls before writing anything parseable must report
    // no event of its own rather than inheriting the previous turn's - that
    // field's whole purpose is saying what *this* run was doing when it froze.
    #[test]
    fn last_event_is_scoped_to_this_run() {
        let prior = format!("{TASK_STARTED}\n{FINAL_ANSWER}\n");
        let baseline = u64::try_from(prior.len()).expect("test fixture fits in u64");

        // A resume that has written nothing of its own yet.
        let bytes = prior.clone().into_bytes();
        assert_eq!(
            last_event(scan_from_baseline(&bytes, baseline)),
            None,
            "a run that wrote nothing must not inherit the prior turn's event"
        );
        // Unscoped, it would have reported the previous turn's answer event -
        // the misreport this scoping prevents.
        assert_eq!(
            last_event(&bytes).as_deref(),
            Some("event_msg/agent_message")
        );

        // Once the run does write, its own last event is reported.
        let tool_call =
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"exec"}}"#;
        let resumed = format!("{prior}{TASK_STARTED}\n{tool_call}\n");
        let bytes = resumed.into_bytes();
        assert_eq!(
            last_event(scan_from_baseline(&bytes, baseline)).as_deref(),
            Some("response_item/custom_tool_call")
        );
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let rollout = format!("not json at all\n{FINAL_ANSWER}\n");
        assert!(scan(&rollout, 0));
    }

    // A baseline landing mid-line leaves an unparseable fragment, which must be
    // skipped without hiding the answer that follows it.
    #[test]
    fn a_baseline_inside_a_line_skips_only_that_fragment() {
        let rollout = format!("{TASK_STARTED}\n{FINAL_ANSWER}\n");
        let baseline = u64::try_from(TASK_STARTED.len() / 2).expect("fits");
        assert!(scan(&rollout, baseline));
    }

    // A rollout shorter than the baseline was rotated or replaced; scanning it
    // whole could read another run's turn, so it must yield nothing.
    #[test]
    fn a_truncated_rollout_yields_nothing() {
        let rollout = format!("{FINAL_ANSWER}\n");
        let baseline = u64::try_from(rollout.len() + 1000).expect("fits");
        assert!(!scan(&rollout, baseline));
    }
}
