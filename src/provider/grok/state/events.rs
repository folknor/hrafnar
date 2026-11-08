use std::path::Path;

/// The cause of a cancelled turn, from a session's `events.jsonl` body.
///
/// Takes the *last* `turn_ended` (a resumed session holds several turns), and
/// the last permission refusal before it unless a later allowed permission
/// superseded it.
pub(in crate::provider::grok) fn cancel_cause(events: &str) -> Option<String> {
    let mut refused_tool: Option<String> = None;
    let mut cause: Option<String> = None;
    for line in events.lines() {
        let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match ev.get("type").and_then(|t| t.as_str()) {
            Some("turn_started") => {
                refused_tool = None;
                cause = None;
            }
            Some("permission_resolved") => {
                let decision = ev.get("decision").and_then(|d| d.as_str());
                if decision == Some("allow") {
                    refused_tool = None;
                } else if decision.is_some() {
                    refused_tool = ev
                        .get("tool_name")
                        .and_then(|t| t.as_str())
                        .map(str::to_string);
                }
            }
            Some("turn_ended") => {
                cause = ev
                    .get("cancellation_category")
                    .and_then(|c| c.as_str())
                    .map(str::to_string);
            }
            _ => {}
        }
    }
    let cause = cause?;
    Some(match refused_tool {
        Some(tool) => format!("{cause}: permission for `{tool}` was refused"),
        None => cause,
    })
}

/// The `events.jsonl` body for a session, found by id under any cwd directory.
pub(in crate::provider::grok) fn session_events(
    grok_home: &Path,
    session_id: &str,
) -> Option<String> {
    session_file(
        grok_home,
        &crate::provider::SessionId::parse(session_id).ok()?,
        "events.jsonl",
    )
}

/// The `updates.jsonl` body for a session (the ACP update stream: tool calls,
/// background task lifecycle, turn boundaries).
pub(in crate::provider::grok) fn session_updates(
    grok_home: &Path,
    session_id: &str,
) -> Option<String> {
    session_file(
        grok_home,
        &crate::provider::SessionId::parse(session_id).ok()?,
        "updates.jsonl",
    )
}

/// A per-session file, found by id under any cwd directory.
///
/// Located by scanning `sessions/*/<id>/` rather than by re-encoding the cwd,
/// so this does not depend on grok's path-encoding scheme.
fn session_file(
    grok_home: &Path,
    session_id: &crate::provider::SessionId,
    name: &str,
) -> Option<String> {
    let sessions = std::fs::read_dir(grok_home.join("sessions")).ok()?;
    sessions.flatten().find_map(|cwd_dir| {
        std::fs::read_to_string(cwd_dir.path().join(session_id.as_str()).join(name)).ok()
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::scratch;
    use super::*;

    /// Trimmed from the tail of a real grok 1.0.40 session that was cancelled
    /// at its first edit in an untrusted folder.
    const CANCELLED: &str = r#"{"ts":"2026-09-22T08:43:27.728Z","type":"phase_changed","phase":"streaming_text"}
{"ts":"2026-09-22T08:43:52.896Z","type":"tool_started","tool_name":"search_replace"}
{"ts":"2026-09-22T08:43:52.896Z","type":"permission_requested","tool_name":"search_replace"}
{"ts":"2026-09-22T08:43:52.896Z","type":"permission_resolved","tool_name":"search_replace","decision":"cancelled","wait_ms":0}
{"ts":"2026-09-22T08:43:52.946Z","type":"turn_ended","outcome":"cancelled","cancellation_category":"permission_cancelled"}"#;

    #[test]
    fn cancel_cause_names_the_category_and_the_refused_tool() {
        assert_eq!(
            cancel_cause(CANCELLED).as_deref(),
            Some("permission_cancelled: permission for `search_replace` was refused")
        );
    }

    #[test]
    fn allowed_permissions_are_not_blamed() {
        let events = r#"{"type":"permission_resolved","tool_name":"read_file","decision":"allow"}
{"type":"turn_ended","outcome":"cancelled","cancellation_category":"max_turns"}"#;
        assert_eq!(cancel_cause(events).as_deref(), Some("max_turns"));

        let refusal_then_allow = r#"{"type":"permission_resolved","tool_name":"search_replace","decision":"cancelled"}
{"type":"permission_resolved","tool_name":"read_file","decision":"allow"}
{"type":"turn_ended","outcome":"cancelled","cancellation_category":"max_turns"}"#;
        assert_eq!(
            cancel_cause(refusal_then_allow).as_deref(),
            Some("max_turns")
        );
        assert_eq!(
            cancel_cause(r#"{"type":"turn_ended","outcome":"end_turn"}"#),
            None
        );
        assert_eq!(cancel_cause(""), None);
    }

    #[test]
    fn session_events_are_found_under_any_cwd_dir_and_ids_are_validated() {
        let home = scratch();
        let sid = "7ada244b-4d05-4fc4-b2c3-3b7594ff0577";
        let dir = home.join("sessions/%2Fsome%2Fcwd").join(sid);
        std::fs::create_dir_all(&dir).expect("create session dir");
        std::fs::write(dir.join("events.jsonl"), CANCELLED).expect("write events");
        assert_eq!(session_events(&home, sid).as_deref(), Some(CANCELLED));
        let underscored = "ses_123";
        let underscored_dir = home.join("sessions/%2Fother%2Fcwd").join(underscored);
        std::fs::create_dir_all(&underscored_dir).expect("create underscored session dir");
        std::fs::write(underscored_dir.join("events.jsonl"), CANCELLED)
            .expect("write underscored session events");
        std::fs::write(underscored_dir.join("updates.jsonl"), "updates")
            .expect("write underscored session updates");
        assert_eq!(
            session_events(&home, underscored).as_deref(),
            Some(CANCELLED)
        );
        assert_eq!(
            session_updates(&home, underscored).as_deref(),
            Some("updates")
        );
        assert_eq!(session_events(&home, "../etc"), None);
        assert_eq!(session_events(&home, "missing"), None);
        let _ = std::fs::remove_dir_all(&home);
    }
}
