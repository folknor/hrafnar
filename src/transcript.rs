//! Codex on-disk transcript forensics.
//!
//! Codex writes a rollout transcript per session under `$CODEX_HOME/sessions`
//! (default `~/.codex/sessions`), named `rollout-<ts>-<session_id>.jsonl`. It
//! carries the same turn events as the `--json` stream plus tool calls, and it
//! survives a frozen/halted stream. When a fresh codex run ends without a clean
//! capture, we read this to classify *why* it stopped: a `stream_error`, or a
//! terminal `function_call` with no matching output (the tool that was running
//! when it died).

use std::{
    ffi::CStr,
    fmt, io,
    io::{BufRead, Read},
    os::unix::ffi::OsStrExt,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

/// A tool call with no matching output in the Codex rollout.
#[derive(Clone)]
pub struct InFlightTool {
    pub call_id: Option<String>,
    pub name: String,
    pub arguments: String,
}

impl fmt::Display for InFlightTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ", self.name)?;
        let mut chars = self.arguments.chars();
        for _ in 0..200 {
            let Some(ch) = chars.next() else {
                return Ok(());
            };
            write!(f, "{ch}")?;
        }
        if chars.next().is_some() {
            f.write_str("...")?;
        }
        Ok(())
    }
}

/// Physical event kind, open to future Codex event names. Display is only the
/// historical diagnostic projection; it is not a parser or a closed vocabulary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventKind {
    pub top: Option<String>,
    pub payload: Option<String>,
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{}",
            self.top.as_deref().unwrap_or("?"),
            self.payload.as_deref().unwrap_or("?")
        )
    }
}

/// Byte boundary measured before a launch, not a timestamp or a turn number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RolloutOffset(pub u64);

/// Authority to attribute rollout bytes. WholeFile is for session inspection;
/// an unknown launch baseline must never become permission to read old turns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    WholeFile,
    From(RolloutOffset),
    Untrusted,
}

impl Scope {
    pub fn bytes(self, bytes: &[u8]) -> &[u8] {
        match self {
            Self::WholeFile => bytes,
            Self::Untrusted => &[],
            Self::From(RolloutOffset(offset)) => match usize::try_from(offset) {
                Ok(offset) if offset <= bytes.len() => &bytes[offset..],
                _ => &[],
            },
        }
    }
}

/// Result of looking for or reading a rollout artifact.
///
/// Callers can distinguish an absent artifact, an unreadable artifact, and an
/// unresolved Codex home without inferring the cause from `None`.
#[derive(Debug)]
pub enum Lookup<T> {
    Found(T),
    Absent,
    Unreadable(io::Error),
    UnknownHome,
}

pub struct TranscriptSummary {
    pub path: PathBuf,
    /// A `task_complete` event was seen (codex reported the turn done).
    pub task_complete: bool,
    /// A `stream_error` event was seen (internal-tool error froze the stream).
    pub stream_error: bool,
    /// `type/payload.type` of the final transcript event.
    pub last_event: Option<EventKind>,
    /// The last tool call with no matching output.
    pub last_in_flight_tool: Option<InFlightTool>,
    /// The last `final_answer`-phase agent message this turn, if any. codex
    /// tags interim progress notes `phase="commentary"` and the real report
    /// `phase="final_answer"`. When a shutdown-exit truncates the `--json`
    /// stream and `-o` file (captured=false) but the rollout still reached the
    /// answer, this is the authoritative response to recover. `None` means the
    /// turn ended without emitting a final answer (e.g. it stopped on a tool).
    pub final_answer: Option<String>,
}

/// Resolve the Codex home as the child sees it. Relative paths are based on
/// the harness working directory, not the caller's current directory.
pub(crate) fn codex_home(
    override_home: Option<&str>,
    home_override: Option<&str>,
    working_dir: &Path,
) -> Option<PathBuf> {
    let code_home = if override_home.is_some() {
        None
    } else {
        std::env::var("CODEX_HOME").ok()
    };
    let home = std::env::var("HOME").ok();
    let effective_home = home_override.or(home.as_deref());
    let needs_passwd_home = override_home.is_none_or(str::is_empty)
        && code_home.as_deref().is_none_or(str::is_empty)
        && effective_home.is_none_or(str::is_empty);
    let passwd_home = needs_passwd_home.then(home_directory_from_passwd).flatten();
    resolve_codex_home(
        override_home,
        code_home.as_deref(),
        effective_home,
        passwd_home.as_deref(),
        working_dir,
    )
}

fn resolve_codex_home(
    override_home: Option<&str>,
    code_home: Option<&str>,
    home: Option<&str>,
    passwd_home: Option<&Path>,
    working_dir: &Path,
) -> Option<PathBuf> {
    let configured = override_home
        .filter(|dir| !dir.is_empty())
        .or_else(|| code_home.filter(|dir| !dir.is_empty()));
    let home_dir = home
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| passwd_home.map(Path::to_path_buf));
    let path = if let Some(dir) = configured {
        PathBuf::from(dir)
    } else {
        home_dir?.join(".codex")
    };
    if path.is_absolute() {
        Some(path)
    } else if working_dir.is_absolute() {
        Some(working_dir.join(path))
    } else {
        Some(std::env::current_dir().ok()?.join(working_dir).join(path))
    }
}

fn home_directory_from_passwd() -> Option<PathBuf> {
    let uid = unsafe { libc::geteuid() };
    let mut capacity = 16 * 1024;
    loop {
        let mut buffer = vec![0u8; capacity];
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::zeroed();
        let mut result = std::ptr::null_mut();
        let code = unsafe {
            libc::getpwuid_r(
                uid,
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if code == libc::ERANGE && capacity < 1 << 20 {
            capacity *= 2;
            continue;
        }
        if code != 0 || result.is_null() {
            return None;
        }
        let entry = unsafe { entry.assume_init() };
        if entry.pw_dir.is_null() {
            return None;
        }
        let bytes = unsafe { CStr::from_ptr(entry.pw_dir) }.to_bytes();
        if bytes.is_empty() {
            return None;
        }
        return Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)));
    }
}

/// Locate candidates by filename suffix, then require exact session identity
/// from their first session_meta record. This also supports opaque IDs. Bounded
/// recursive walk of the date-nested `sessions/` tree. A missing override uses
/// the current process environment and the account home from the system user
/// database when `HOME` is unset.
pub fn find_transcript_path_lookup(
    session_id: &str,
    override_home: Option<&str>,
) -> Lookup<PathBuf> {
    let Ok(id) = crate::harness::SessionId::parse(session_id) else {
        return Lookup::Absent;
    };
    let Some(home) = codex_home(override_home, None, &PathBuf::new()) else {
        return Lookup::UnknownHome;
    };
    find_session_path(&id, &home)
}

/// Locate and summarize a rollout under an already resolved home.
pub(crate) fn summarize_session_in_home_lookup(
    session_id: &str,
    home: &Path,
    scope: Scope,
) -> Lookup<TranscriptSummary> {
    match find_transcript_path_in_home_lookup(session_id, home) {
        Lookup::Found(path) => summarize_lookup(&path, scope),
        Lookup::Absent => Lookup::Absent,
        Lookup::Unreadable(error) => Lookup::Unreadable(error),
        Lookup::UnknownHome => Lookup::UnknownHome,
    }
}

pub(crate) fn find_transcript_path_in_home_lookup(
    session_id: &str,
    home: &Path,
) -> Lookup<PathBuf> {
    let Ok(id) = crate::harness::SessionId::parse(session_id) else {
        return Lookup::Absent;
    };
    find_transcript_path_checked_in_home(&id, home)
}

pub(crate) fn find_transcript_path_checked_in_home(
    session_id: &crate::harness::SessionId,
    home: &Path,
) -> Lookup<PathBuf> {
    find_session_path(session_id, home)
}

// Suffixes only select candidates. Metadata supplies identity at discovery time;
// a later path read is not pinned against replacement, nor confined by symlinks.
fn find_session_path(session_id: &crate::harness::SessionId, home: &Path) -> Lookup<PathBuf> {
    let session_id = session_id.as_str();
    let sessions = home.join("sessions");
    let needle = format!("-{session_id}.jsonl");
    let mut stack = vec![(sessions, 0u32)];
    let mut unreadable = None;
    while let Some((dir, depth)) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // A missing root or a directory that disappeared during the walk
            // is absence. Other errors mean the search may be incomplete.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                if unreadable.is_none() {
                    unreadable = Some(error);
                }
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    if unreadable.is_none() {
                        unreadable = Some(error);
                    }
                    continue;
                }
            };
            let path = entry.path();
            let metadata = match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    if unreadable.is_none() {
                        unreadable = Some(error);
                    }
                    continue;
                }
            };
            if metadata.is_dir() {
                // sessions/YYYY/MM/DD/file.jsonl -> cap the descent.
                if depth < 5 {
                    stack.push((path, depth + 1));
                }
            } else if has_transcript_name(&path, &needle) {
                match identifies_session(&path, session_id) {
                    Ok(true) => return Lookup::Found(path),
                    Ok(false) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        unreadable.get_or_insert(error);
                    }
                }
            }
        }
    }
    match unreadable {
        Some(error) => Lookup::Unreadable(error),
        None => Lookup::Absent,
    }
}

// Bound the first physical record, including its optional LF, before decoding.
// A complete JSON record at EOF is valid. Never scan ahead or decode lossily.
const HEADER_LIMIT: u64 = 1 << 20;

fn identifies_session(path: &Path, requested: &str) -> io::Result<bool> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "rollout is not a regular file",
        ));
    }
    let mut header = Vec::new();
    io::BufReader::new(file.take(HEADER_LIMIT + 1)).read_until(b'\n', &mut header)?;
    if header.len() as u64 > HEADER_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "rollout metadata header is oversized",
        ));
    }
    metadata_identifies(&header, requested)
}

fn metadata_identifies(header: &[u8], requested: &str) -> io::Result<bool> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid rollout session metadata",
        )
    };
    let event: serde_json::Value = serde_json::from_slice(header).map_err(|_| invalid())?;
    if event.get("type").and_then(|v| v.as_str()) != Some("session_meta") {
        return Err(invalid());
    }
    let payload = event
        .get("payload")
        .and_then(|v| v.as_object())
        .ok_or_else(invalid)?;
    let id = payload
        .get("id")
        .map(|v| v.as_str().ok_or_else(invalid))
        .transpose()?;
    let legacy = payload
        .get("session_id")
        .map(|v| v.as_str().ok_or_else(invalid))
        .transpose()?;
    match (id, legacy) {
        (Some(id), Some(legacy)) if id != legacy => Err(invalid()),
        (Some(id), _) | (_, Some(id)) => Ok(id == requested),
        (None, None) => Err(invalid()),
    }
}

fn has_transcript_name(path: &Path, needle: &str) -> bool {
    path.file_name()
        .is_some_and(|name| name.as_bytes().ends_with(needle.as_bytes()))
}

pub(crate) fn summarize_lookup(path: &Path, scope: Scope) -> Lookup<TranscriptSummary> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Lookup::Absent,
        Err(error) => return Lookup::Unreadable(error),
    };
    // Lossy is safe: an offset landing mid-line leaves one unparseable fragment,
    // which is skipped like any other malformed line.
    let content = String::from_utf8_lossy(scope.bytes(&bytes));
    let mut summary = parse(&content);
    summary.path = path.to_path_buf();
    Lookup::Found(summary)
}

/// Current real turn, using the same evidence fold as the watchdog and bundles.
fn parse(content: &str) -> TranscriptSummary {
    crate::harness::codex::rollout::fold(content).current_turn
}

/// Find and summarize the transcript for a codex session id, if one exists.
/// `override_home` is the run's effective `CODEX_HOME` (a profile `env`
/// override), if any.
/// Scope restricts the summary to the run's own events. A From offset is the
/// rollout's size at launch, restricting the
/// summary to that run's own events - used by recovery so a prior turn's
/// `final_answer` can never be reported as this run's result. Use WholeFile to
/// summarize the whole rollout (e.g. `review sessions <id>`).
pub fn summarize_session_lookup(
    session_id: &str,
    override_home: Option<&str>,
    scope: Scope,
) -> Lookup<TranscriptSummary> {
    match find_transcript_path_lookup(session_id, override_home) {
        Lookup::Found(path) => summarize_lookup(&path, scope),
        Lookup::Absent => Lookup::Absent,
        Lookup::Unreadable(error) => Lookup::Unreadable(error),
        Lookup::UnknownHome => Lookup::UnknownHome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_fixtures as fixtures;

    // A run that ran one tool to completion, started a second, then hit a
    // stream_error and never reached task_complete - the frozen-stream case.
    const FROZEN: &str = r#"
{"type":"session_meta","payload":{"originator":"codex_exec","session_id":"abc"}}
{"type":"event_msg","payload":{"type":"task_started"}}
{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"ls\"}","call_id":"call_1"}}
{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_1","output":"ok"}}
{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"sleep 999\"}","call_id":"call_2"}}
{"type":"event_msg","payload":{"type":"stream_error"}}
"#;

    #[test]
    fn frozen_run_surfaces_last_in_flight_tool() {
        let s = parse(FROZEN);
        assert!(!s.task_complete);
        assert!(s.stream_error);
        assert_eq!(
            s.last_event.as_ref().map(ToString::to_string).as_deref(),
            Some("event_msg/stream_error")
        );
        let tool = s.last_in_flight_tool.expect("in-flight tool");
        assert_eq!(tool.name, "exec_command");
        assert!(
            tool.arguments.contains("sleep 999"),
            "args were: {}",
            tool.arguments
        );
    }

    #[test]
    fn in_flight_tool_display_truncates_long_arguments_with_ellipsis() {
        let tool = InFlightTool {
            call_id: None,
            name: "exec_command".to_string(),
            arguments: "a".repeat(201),
        };
        assert_eq!(
            format!("{tool}"),
            format!("exec_command {}...", "a".repeat(200))
        );
    }

    #[test]
    fn clean_run_has_no_in_flight_tool() {
        let clean = format!(
            "{}{}{}{}{}{}{}",
            fixtures::event(Some("event_msg"), "task_started"),
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"c1"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ok"}}"#,
            "\n",
            fixtures::event(Some("event_msg"), "task_complete")
        );
        let s = parse(&clean);
        assert!(s.task_complete);
        assert!(!s.stream_error);
        assert!(s.last_in_flight_tool.is_none());
    }

    // A truncated run: the model emitted an interim commentary message, ran a
    // final tool, then a real final_answer, and reached task_complete - but the
    // exec stream/-o were cut off. The final_answer must be recoverable.
    #[test]
    fn recovers_final_answer_from_transcript() {
        let run = format!(
            "{}{}{}{}{}{}{}{}{}{}{}",
            fixtures::event(Some("event_msg"), "task_started"),
            "\n",
            fixtures::legacy_message("I am now running the checks.", "commentary"),
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"c1"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ok"}}"#,
            "\n",
            fixtures::legacy_message("All checks passed. No files changed.", "final_answer"),
            "\n",
            fixtures::event(Some("event_msg"), "task_complete")
        );
        let s = parse(&run);
        assert!(s.task_complete);
        assert_eq!(
            s.final_answer.as_deref(),
            Some("All checks passed. No files changed.")
        );
    }

    // codex 0.157.0's shape, taken from a real rollout: no `agent_message`
    // event, the answer is an assistant `message` response item (preceded by an
    // `item_completed` event carrying the same text, which is not read - so
    // here it carries different text, to show it). A commentary-phase assistant
    // message earlier in the turn must not count.
    #[test]
    fn recovers_final_answer_from_an_assistant_message() {
        let run = format!(
            "{}{}{}{}{}{}{}{}{}{}{}",
            fixtures::event(Some("event_msg"), "task_started"),
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Review this."}]}}"#,
            "\n",
            fixtures::assistant_message("Reading the fixtures now.", "commentary"),
            "\n",
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","content":[{"type":"Text","text":"NOT READ"}],"phase":"final_answer"}}}"#,
            "\n",
            fixtures::assistant_message("No problems.", "final_answer"),
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"No problems."}}"#
        );
        let s = parse(&run);
        assert!(s.task_complete);
        assert_eq!(s.final_answer.as_deref(), Some("No problems."));
    }

    #[test]
    fn an_empty_final_answer_message_is_not_an_answer() {
        let payload = serde_json::json!({
            "type": "message", "role": "assistant", "phase": "final_answer",
            "content": [{"type": "reasoning_text", "text": "thinking"}],
        });
        assert!(
            crate::harness::codex::rollout::fold(
                &serde_json::json!({"payload": payload}).to_string()
            )
            .current_turn
            .final_answer
            .is_none()
        );
    }

    #[test]
    fn commentary_assistant_message_is_not_a_final_answer() {
        let run = format!(
            "{}{}{}{}{}",
            fixtures::event(Some("event_msg"), "task_started"),
            "\n",
            fixtures::assistant_message("Reading the fixtures now.", "commentary"),
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":null}}"#
        );
        assert!(parse(&run).final_answer.is_none());
    }

    // A run that ended on a tool with only commentary - no final answer exists.
    #[test]
    fn no_final_answer_when_only_commentary() {
        let run = format!(
            "{}{}{}{}{}{}{}{}{}",
            fixtures::event(Some("event_msg"), "task_started"),
            "\n",
            fixtures::legacy_message("I am moving through the checks now.", "commentary"),
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"c1"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ok"}}"#,
            "\n",
            fixtures::event(Some("event_msg"), "task_complete")
        );
        let s = parse(&run);
        assert!(s.task_complete);
        assert!(s.final_answer.is_none());
    }

    // A resumed rollout: the first turn completed, then a second turn started
    // (resume), ran a tool, and hit a stream_error without completing. The
    // summary must reflect the current turn, not the completed earlier one.
    #[test]
    fn resumed_turn_does_not_inherit_prior_completion() {
        let resumed = format!(
            "{}{}{}{}{}{}{}{}{}",
            fixtures::event(Some("event_msg"), "task_started"),
            "\n",
            fixtures::event(Some("event_msg"), "task_complete"),
            "\n",
            fixtures::event(Some("event_msg"), "task_started"),
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"sleep 999\"}","call_id":"c9"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"stream_error"}}"#
        );
        let s = parse(&resumed);
        assert!(!s.task_complete, "second turn must not report complete");
        assert!(s.stream_error);
        let tool = s.last_in_flight_tool.expect("in-flight tool from turn 2");
        assert_eq!(tool.name, "exec_command");
    }

    // codex-cli 0.147.0 opens a turn at `codex exec` teardown and aborts it
    // milliseconds later. Transcribed from the field (session 019fefb0), which
    // is why it includes the `turn_context` and the synthetic developer message
    // - the phantom is NOT an empty event sequence:
    //   07:23:23.220  event_msg     task_started   turn_id 67a4fabf-...
    //   07:23:23.228  turn_context  (no payload.type)
    //   07:23:23.265  response_item message        role "developer", <turn_aborted>
    //   07:23:23.266  event_msg     turn_aborted   reason "interrupted"
    // The summary must still describe the real (completed) turn underneath.
    #[test]
    fn phantom_shutdown_turn_does_not_mask_real_completion() {
        let rollout = format!(
            "{}{}{}{}{}{}{}{}{}{}{}{}{}{}{}",
            fixtures::event(Some("event_msg"), "task_started"),
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"do the thing"}}"#,
            "\n",
            fixtures::legacy_message("The real report.", "final_answer"),
            "\n",
            fixtures::event(Some("event_msg"), "task_complete"),
            "\n",
            fixtures::event(Some("event_msg"), "task_started"),
            "\n",
            r#"{"type":"turn_context","payload":{"cwd":"/home/folk/Programs/mogwai"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<turn_aborted>"}]}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}"#
        );
        let s = parse(&rollout);
        assert!(s.task_complete, "phantom turn must not clear task_complete");
        assert_eq!(s.final_answer.as_deref(), Some("The real report."));
    }

    // A real turn cancelled before it emitted any agent message or tool call is
    // a genuine early cancellation. It must NOT be rolled back into the previous
    // turn's completion - doing so would report a cancelled turn as finished,
    // with a stale answer attached.
    #[test]
    fn early_cancelled_turn_with_a_user_message_is_not_a_phantom() {
        let rollout = format!(
            "{}{}{}{}{}{}{}{}{}{}{}",
            fixtures::event(Some("event_msg"), "task_started"),
            "\n",
            fixtures::legacy_message("Old answer.", "final_answer"),
            "\n",
            fixtures::event(Some("event_msg"), "task_complete"),
            "\n",
            fixtures::event(Some("event_msg"), "task_started"),
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"now do the next thing"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}"#
        );
        let s = parse(&rollout);
        assert!(
            !s.task_complete,
            "a cancelled real turn must not inherit the prior turn's completion"
        );
        assert!(
            s.final_answer.is_none(),
            "a cancelled real turn must not surface the prior turn's answer"
        );
    }

    // Same, for a turn cancelled after only reasoning events - no user_message
    // in this rollout at all, so the reasoning is what proves it was real.
    #[test]
    fn turn_cancelled_after_only_reasoning_is_not_a_phantom() {
        let rollout = format!(
            "{}{}{}{}{}{}{}{}{}",
            fixtures::event(Some("event_msg"), "task_started"),
            "\n",
            fixtures::event(Some("event_msg"), "task_complete"),
            "\n",
            fixtures::event(Some("event_msg"), "task_started"),
            "\n",
            r#"{"type":"response_item","payload":{"type":"reasoning","summary":[]}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}"#
        );
        let s = parse(&rollout);
        assert!(
            !s.task_complete,
            "reasoning proves the turn was real, not a teardown phantom"
        );
    }

    // The rollback must not swallow a *genuine* interruption: a turn that ran
    // tools and was then aborted really did fail, and must report as such.
    #[test]
    fn aborted_turn_with_content_is_not_rolled_back() {
        let rollout = format!(
            "{}{}{}{}{}{}{}{}{}",
            fixtures::event(Some("event_msg"), "task_started"),
            "\n",
            fixtures::event(Some("event_msg"), "task_complete"),
            "\n",
            fixtures::event(Some("event_msg"), "task_started"),
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"c1"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}"#
        );
        let s = parse(&rollout);
        assert!(
            !s.task_complete,
            "a real aborted turn must not inherit the prior turn's completion"
        );
        let tool = s.last_in_flight_tool.expect("in-flight tool from turn 2");
        assert_eq!(tool.name, "exec_command");
    }

    // A prior turn completed with a real final_answer; a later resume died
    // before emitting any new event. Scoped to the bytes this run appended,
    // recovery must NOT surface the stale final_answer as its result - doing so
    // would report someone else's answer, suppress auto-resume, and write the
    // wrong response into the sidecar.
    fn parse_from(rollout: &str, from_offset: Option<u64>) -> TranscriptSummary {
        let bytes = rollout.as_bytes();
        let scope = from_offset.map_or(Scope::WholeFile, |offset| {
            Scope::From(RolloutOffset(offset))
        });
        parse(&String::from_utf8_lossy(scope.bytes(bytes)))
    }

    #[test]
    fn offset_excludes_a_prior_turns_final_answer() {
        let prior = format!(
            "{}{}{}{}{}{}",
            fixtures::timestamp(
                &fixtures::event(Some("event_msg"), "task_started"),
                "2026-07-17T10:00:00.000Z"
            ),
            "\n",
            fixtures::timestamp(
                &fixtures::legacy_message("Old answer from turn one.", "final_answer"),
                "2026-07-17T10:00:05.000Z"
            ),
            "\n",
            fixtures::timestamp(
                &fixtures::event(Some("event_msg"), "task_complete"),
                "2026-07-17T10:00:06.000Z"
            ),
            "\n"
        );
        let offset = u64::try_from(prior.len()).expect("test fixture fits in u64");
        // The resume died before writing anything of its own.
        let rollout = prior.clone();

        // Whole-file view (e.g. `review sessions <id>`): the old answer shows.
        assert_eq!(
            parse_from(&rollout, None).final_answer.as_deref(),
            Some("Old answer from turn one.")
        );
        // Scoped to this run's bytes: nothing of ours, so nothing to recover.
        let s = parse_from(&rollout, Some(offset));
        assert!(
            s.final_answer.is_none(),
            "stale final_answer must be gated out"
        );
        assert!(!s.task_complete, "prior task_complete must be gated out");
    }

    // The offset is immune to clock resolution, which a timestamp filter was
    // not: a prior answer written in the *same second* as the resume's launch
    // is still excluded.
    #[test]
    fn offset_excludes_a_prior_answer_from_the_same_second() {
        let prior = format!(
            "{}{}",
            fixtures::timestamp(
                &fixtures::legacy_message("Old answer.", "final_answer"),
                "2026-07-17T10:00:05.100Z"
            ),
            "\n"
        );
        let offset = u64::try_from(prior.len()).expect("test fixture fits in u64");
        let rollout = format!(
            "{prior}{}\n",
            fixtures::timestamp(
                &fixtures::event(Some("event_msg"), "task_started"),
                "2026-07-17T10:00:05.900Z"
            )
        );
        let s = parse_from(&rollout, Some(offset));
        assert!(s.final_answer.is_none());
    }

    // A rollout shorter than the offset was rotated or replaced under us;
    // attributing its contents to this run would be a lie.
    #[test]
    fn offset_past_the_end_yields_nothing() {
        let rollout = format!(
            "{}{}",
            fixtures::legacy_message("Answer.", "final_answer"),
            "\n"
        );
        let offset = u64::try_from(rollout.len() + 1000).expect("fits");
        assert!(parse_from(&rollout, Some(offset)).final_answer.is_none());
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    #[test]
    fn malformed_identity_never_searches_even_with_unknown_home() {
        for id in ["", "../escape", "a/b", "a.b"] {
            assert!(matches!(
                find_transcript_path_lookup(id, Some("relative")),
                Lookup::Absent
            ));
            assert!(matches!(
                find_transcript_path_in_home_lookup(id, Path::new("/missing")),
                Lookup::Absent
            ));
        }
    }
}

#[cfg(test)]
mod scope_and_metadata_tests {
    use super::*;
    use crate::codex_fixtures as fixtures;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target/test-scratch")
                .join(crate::config::generate_uuid());
            std::fs::create_dir_all(path.join("sessions")).expect("scratch");
            Self(path)
        }
        fn put(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join("sessions").join(name);
            std::fs::write(&path, bytes).expect("fixture");
            path
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn scopes_distinguish_inspection_attribution_and_unknown() {
        let bytes = b"old\nnew\n";
        assert_eq!(Scope::WholeFile.bytes(bytes), bytes);
        assert_eq!(Scope::Untrusted.bytes(bytes), b"");
        for (offset, expected) in [
            (0, &bytes[..]),
            (4, &bytes[4..]),
            (8, &b""[..]),
            (9, &b""[..]),
            (u64::MAX, &b""[..]),
        ] {
            assert_eq!(Scope::From(RolloutOffset(offset)).bytes(bytes), expected);
        }
    }

    #[test]
    fn metadata_checks_presence_types_and_raw_identity_without_normalizing() {
        assert!(
            metadata_identifies(fixtures::metadata("a-b-c").as_bytes(), "a-b-c").expect("header")
        );
        assert!(
            !metadata_identifies(fixtures::metadata("bar-foo").as_bytes(), "foo")
                .expect("different")
        );
        assert!(!metadata_identifies(fixtures::metadata("Foo").as_bytes(), "foo").expect("case"));
        assert!(!metadata_identifies(fixtures::metadata(" foo").as_bytes(), "foo").expect("space"));
        for payload in [
            r#"{"session_id":"foo"}"#,
            r#"{"id":"foo","session_id":"foo"}"#,
        ] {
            assert!(
                metadata_identifies(
                    format!(r#"{{"type":"session_meta","payload":{payload}}}"#).as_bytes(),
                    "foo"
                )
                .expect("legacy or agreeing IDs")
            );
        }
        for header in [
            "",
            "\n",
            "{",
            "[]",
            "{}",
            r#"{"type":"event_msg","payload":{"id":"foo"}}"#,
            r#"{"type":"session_meta","payload":[]}"#,
            r#"{"type":"session_meta","payload":{}}"#,
            r#"{"type":"session_meta","payload":{"id":null,"session_id":"foo"}}"#,
            r#"{"type":"session_meta","payload":{"id":"foo","session_id":1}}"#,
            r#"{"type":"session_meta","payload":{"id":"foo","session_id":"bar"}}"#,
        ] {
            assert_eq!(
                metadata_identifies(header.as_bytes(), "foo")
                    .expect_err(header)
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
        assert!(
            metadata_identifies(
                b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"\xff\"}}",
                "foo"
            )
            .is_err()
        );
    }

    #[test]
    fn discovery_verifies_metadata_and_continues_past_wrong_or_bad_candidates() {
        let scratch = Scratch::new();
        let wrong = scratch.put(
            "rollout-date-bar-foo.jsonl",
            fixtures::metadata("bar-foo").as_bytes(),
        );
        assert!(matches!(
            find_transcript_path_in_home_lookup("foo", &scratch.0),
            Lookup::Absent
        ));
        assert!(
            matches!(find_transcript_path_in_home_lookup("bar-foo", &scratch.0), Lookup::Found(p) if p == wrong)
        );
        scratch.put("broken-foo.jsonl", b"not json\n");
        assert!(matches!(
            find_transcript_path_in_home_lookup("foo", &scratch.0),
            Lookup::Unreadable(_)
        ));
        // The metadata is authoritative even when the candidate's prefix was renamed.
        let exact = scratch.put(
            "renamed-prefix-foo.jsonl",
            b"{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"foo\"}}\r\n",
        );
        assert!(
            matches!(find_transcript_path_in_home_lookup("foo", &scratch.0), Lookup::Found(p) if p == exact)
        );
        std::fs::remove_file(exact).expect("remove exact");
        scratch.put(
            "rollout-date-bar-foo.jsonl",
            fixtures::metadata("foo").as_bytes(),
        );
        assert!(
            matches!(find_transcript_path_in_home_lookup("foo", &scratch.0), Lookup::Found(p) if p == wrong)
        );
    }

    #[test]
    fn discovery_and_summary_preserve_non_utf8_home_and_filename() {
        let scratch = Scratch::new();
        let home = scratch.0.join(std::ffi::OsStr::from_bytes(b"home-\xff"));
        std::fs::create_dir_all(home.join("sessions")).expect("native home");
        let path = home
            .join("sessions")
            .join(std::ffi::OsStr::from_bytes(b"rollout-\xfe-foo.jsonl"));
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                fixtures::metadata("foo"),
                fixtures::legacy_message("native answer", "final_answer")
            ),
        )
        .expect("native rollout");
        assert!(
            matches!(find_transcript_path_in_home_lookup("foo", &home), Lookup::Found(p) if p == path)
        );
        match summarize_session_in_home_lookup("foo", &home, Scope::WholeFile) {
            Lookup::Found(summary) => {
                assert_eq!(summary.path, path);
                assert_eq!(summary.final_answer.as_deref(), Some("native answer"));
            }
            _ => panic!("native path summary"),
        }
    }

    #[test]
    fn physical_header_is_bounded_and_nonregular_candidates_do_not_block() {
        let scratch = Scratch::new();
        let header = fixtures::metadata("foo");
        let mut bytes = header.as_bytes().to_vec();
        bytes.resize(usize::try_from(HEADER_LIMIT).expect("fits"), b' ');
        let path = scratch.put("limit-foo.jsonl", &bytes);
        assert!(identifies_session(&path, "foo").expect("limit includes optional LF"));
        bytes.push(b'\n');
        std::fs::write(&path, &bytes).expect("oversize");
        assert_eq!(
            identifies_session(&path, "foo")
                .expect_err("over limit")
                .kind(),
            io::ErrorKind::InvalidData
        );
        std::fs::write(&path, format!("\n{header}\n")).expect("empty first line");
        assert!(identifies_session(&path, "foo").is_err());
        let fifo = scratch.0.join("sessions/fifo-foo.jsonl");
        let cpath = std::ffi::CString::new(fifo.as_os_str().as_bytes()).expect("path");
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        assert_eq!(
            identifies_session(&fifo, "foo")
                .expect_err("FIFO refused without reading")
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
}
