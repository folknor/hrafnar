//! Harness-owned discovery and presentation of session artifacts.
use super::Harness;
use crate::{sessions::SessionRecord, transcript};

pub(crate) struct SessionArtifacts(Option<transcript::Lookup<transcript::TranscriptSummary>>);

pub(crate) fn session_artifacts(record: &SessionRecord) -> SessionArtifacts {
    let lookup = match record.harness.parse::<Harness>() {
        Ok(Harness::Codex) => {
            let home = transcript::codex_home(
                record.codex_home.as_deref(),
                None,
                std::path::Path::new(&record.project),
            );
            Some(match home.as_deref() {
                Some(home) => transcript::summarize_session_in_home_lookup(
                    &record.session_id,
                    home,
                    transcript::Scope::WholeFile,
                ),
                None => transcript::Lookup::UnknownHome,
            })
        }
        // Historical names are still displayed even when their harness was removed.
        Ok(Harness::Claude | Harness::Grok | Harness::Maki) | Err(_) => None,
    };
    SessionArtifacts(lookup)
}

impl SessionArtifacts {
    pub fn final_answer(&self) -> Option<&str> {
        match &self.0 {
            Some(transcript::Lookup::Found(t)) => t.final_answer.as_deref(),
            _ => None,
        }
    }

    pub fn print(&self) {
        match &self.0 {
            Some(transcript::Lookup::Found(t)) => {
                super::codex::forensics::print_transcript(t);
                if t.task_complete && t.final_answer.is_none() {
                    println!("note: rollout has no final answer - the turn produced no conclusion");
                }
            }
            Some(transcript::Lookup::Absent) => {
                println!("transcript: (not found under the resolved CODEX_HOME)");
            }
            Some(transcript::Lookup::Unreadable(error)) => {
                println!("transcript: (unreadable: {error})");
            }
            Some(transcript::Lookup::UnknownHome) => {
                println!("transcript: (Codex home could not be resolved)");
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_home_artifacts_use_current_turn_and_ignore_retired_harnesses() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(format!(
                "artifacts-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
        let home = root.join("custom-codex");
        std::fs::create_dir_all(home.join("sessions")).expect("fixture home");
        let path = home.join("sessions/rollout-test-session.jsonl");
        let initial = format!(
            "{}\n{}\n{}\n{}\n",
            crate::codex_fixtures::metadata("session"),
            crate::codex_fixtures::event(None, "task_started"),
            crate::codex_fixtures::legacy_message("answer", "final_answer"),
            crate::codex_fixtures::event(None, "task_complete")
        );
        std::fs::write(&path, &initial).expect("rollout");
        let mut record: SessionRecord = serde_json::from_value(serde_json::json!({
            "timestamp":"2026-01-01T00:00:00Z","hostname":"old-host","audit_id":"old",
            "provider":"codex","archetype":"bare","session_id":"session",
            "project":root, "operator_prompt":"request", "assembled_prompt":"request",
            "review_version":"0.1.0", "codex_home":home,
        }))
        .expect("historical row with saved home");
        assert_eq!(session_artifacts(&record).final_answer(), Some("answer"));
        std::fs::write(
            &path,
            format!(
                "{initial}{}\n{}\n",
                r#"{"payload":{"type":"task_started"}}"#,
                r#"{"payload":{"type":"user_message","message":"next"}}"#
            ),
        )
        .expect("later real turn");
        assert!(session_artifacts(&record).final_answer().is_none());
        record.harness = "removed-provider".into();
        assert!(session_artifacts(&record).0.is_none());
        std::fs::remove_dir_all(&root).expect("cleanup");
    }
}
