/// Prompt ids grok gives the turns it starts itself when background work
/// finishes (`notification_bridge.rs`), as opposed to the UUID of a turn a
/// caller started.
const SYNTHETIC_TURN_PREFIXES: &[&str] =
    &["task-completed-", "bash-completed-", "monitor-completed-"];

/// A background result the model never saw, with everything needed to hand it
/// over in a prompt - so delivering it does not depend on the model choosing
/// to go and read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unread {
    /// A shell command that finished after the model's turn ended.
    Finished {
        id: WorkId,
        command: String,
        exit_code: Option<i64>,
        signal: Option<String>,
        /// Grok's captured output (its own snapshot, possibly truncated).
        output: String,
        truncated: bool,
        output_file: Option<String>,
    },
    /// Work started in the latest turn that never finished: grok stopped it
    /// at exit (the background wait ran out) or lost it. A later grok process
    /// can never finish it - its task table starts empty.
    Unfinished {
        id: WorkId,
        command: Option<String>,
        output_file: Option<String>,
    },
    /// A background subagent that finished after the model's turn ended.
    /// The retained update fields do not include its result text.
    Subagent {
        id: WorkId,
        status: Option<String>,
        error: Option<String>,
    },
}

/// Identity of one kind of grok background work. Keeping the kind separate
/// from the provider id avoids collisions between task ids and subagent ids.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkId {
    Task(String),
    Subagent(String),
}

impl WorkId {
    fn raw_id(&self) -> &str {
        match self {
            Self::Task(id) | Self::Subagent(id) => id,
        }
    }
}

impl std::fmt::Display for WorkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Task(_) => f.write_str(self.raw_id()),
            Self::Subagent(_) => write!(f, "subagent:{}", self.raw_id()),
        }
    }
}

impl Unread {
    /// The existing display identifier: the task id, or `subagent:<id>`.
    pub fn id(&self) -> String {
        self.work_id().to_string()
    }

    fn work_id(&self) -> &WorkId {
        match self {
            Self::Finished { id, .. } | Self::Unfinished { id, .. } | Self::Subagent { id, .. } => {
                id
            }
        }
    }
}

/// Background results and completion notices in a session's `updates.jsonl`
/// that the model may not have seen.
///
/// Grok moves a long shell command to the background and hands the model a
/// task id; a model that then waits on it (`get_command_or_subagent_output`)
/// reads the result inside its turn, so the task's `task_completed` lands
/// *before* the turn's `turn_completed`. A model that ends its turn instead
/// never sees it: headless `-p` waits for the task to finish, queues a wake-up,
/// and exits without running that turn (`headless.rs` has no wake handling).
/// That shows up as a completion *after* the last turn end - the exact
/// signature of the live probe that lost a passing `brokkr test`, and absent
/// from a session whose model waited. Background subagents
/// (`subagent_spawned`/`subagent_finished`) have no read signal in the update
/// data we retain, so completion relative to a turn end is only a heuristic for
/// whether the result was seen.
///
/// - **Only a real turn end counts.** Grok also starts turns itself when
///   background work finishes (prompt ids in `SYNTHETIC_TURN_PREFIXES`); one
///   torn down before it ran writes a `turn_completed` that is not `end_turn`,
///   and counting it would hide the very completion it was started for.
/// - **"Never finished" is scoped to the latest turn**, because a later grok
///   process can never finish an earlier one's task. The wake prompt includes
///   shell output, but for a subagent it only includes completion metadata and
///   asks the model to fetch the result. That notice does not prove the model
///   read the result.
/// - **Persistent monitors are not work that must produce a result.** They
///   never complete by design (`cli.rs`: they "always wait the full timeout"),
///   so a `task_backgrounded` carrying `monitor_description` is ignored.
///
/// `Err` when the log cannot be trusted: a line that is not valid JSON. Grok
/// flushes its log before a headless process exits (`unified_log::flush_blocking`),
/// so a malformed line means something is wrong - and treating it as "nothing
/// unread" would make the guarantee fail open exactly when it cannot be checked.
pub fn unread_background_results(updates: &str) -> Result<Vec<Unread>, String> {
    let mut started: Vec<(usize, WorkId, serde_json::Value)> = Vec::new();
    let mut finished: Vec<(usize, Unread)> = Vec::new();
    // Shell completions the model was not waiting for (`will_wake`): grok
    // delivers those only as a wake-up message in a *later* turn.
    let mut woken: Vec<(usize, Unread)> = Vec::new();
    let mut turn_ends: Vec<usize> = Vec::new();
    let mut prompts: Vec<usize> = Vec::new();
    for (i, line) in updates.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v = serde_json::from_str::<serde_json::Value>(line)
            .map_err(|e| format!("grok update log line {} is not valid JSON: {e}", i + 1))?;
        let update = &v["params"]["update"];
        let text = |key: &str| update[key].as_str().map(str::to_string);
        match update["sessionUpdate"].as_str() {
            Some("turn_completed") => {
                let synthetic = update["prompt_id"]
                    .as_str()
                    .is_some_and(|p| SYNTHETIC_TURN_PREFIXES.iter().any(|s| p.starts_with(s)));
                if !synthetic || update["stop_reason"].as_str() == Some("end_turn") {
                    turn_ends.push(i);
                }
            }
            Some("user_message_chunk") => prompts.push(i),
            Some("task_backgrounded") => {
                if update["monitor_description"].is_string() {
                    continue;
                }
                if let Some(id) = text("task_id") {
                    started.push((i, WorkId::Task(id), update.clone()));
                }
            }
            Some("task_completed") => {
                let snap = &update["task_snapshot"];
                if let Some(id) = snap["task_id"].as_str() {
                    let item = Unread::Finished {
                        id: WorkId::Task(id.to_string()),
                        command: snap["command"].as_str().unwrap_or_default().to_string(),
                        exit_code: snap["exit_code"].as_i64(),
                        signal: snap["signal"].as_str().map(str::to_string),
                        output: snap["output"].as_str().unwrap_or_default().to_string(),
                        truncated: snap["truncated"].as_bool().unwrap_or(false),
                        output_file: snap["output_file"].as_str().map(str::to_string),
                    };
                    // `will_wake: false` means the model was blocked waiting on
                    // it (`block_waited`) and got it inside its turn. Anything
                    // else - including a grok that omits the field - is only
                    // delivered by a later turn, so it must be proven read.
                    if update["will_wake"].as_bool() == Some(false) {
                        finished.push((i, item));
                    } else {
                        woken.push((i, item.clone()));
                        finished.push((i, item));
                    }
                }
            }
            Some("subagent_spawned") => {
                if let Some(id) = text("subagent_id") {
                    started.push((i, WorkId::Subagent(id), update.clone()));
                }
            }
            Some("subagent_finished") => {
                if let Some(id) = text("subagent_id") {
                    // This retained update projection does not deliver the
                    // subagent's result text; the wake prompt can only ask for it.
                    finished.push((
                        i,
                        Unread::Subagent {
                            id: WorkId::Subagent(id),
                            status: text("status"),
                            error: text("error"),
                        },
                    ));
                }
            }
            _ => {}
        }
    }
    let last_end = turn_ends.last().copied();
    // The latest turn began after the real turn end before it.
    let latest_turn_start = turn_ends.iter().rev().nth(1).copied();
    let after = |i: usize, mark: Option<usize>| mark.is_none_or(|m| i > m);

    // Finished work the model never received. For anything completed after
    // the last real turn end, that is immediate. For a `will_wake` shell
    // completion it holds even if the model's turn ended *after* it: grok does
    // not hand the result to the running turn, it queues a wake-up message, so
    // the model has it only once a turn that *started* after the completion (a
    // new prompt) has ended. The live test that motivated this: a warm
    // `brokkr test` finished while the model was still writing "started", its
    // turn then ended cleanly, and the wake-up message sat unread after it.
    let read_by_later_turn = |i: usize| {
        prompts
            .iter()
            .find(|p| **p > i)
            .is_some_and(|p| turn_ends.iter().any(|t| t > p))
    };
    let mut unread: Vec<Unread> = finished
        .iter()
        .filter(|(i, _)| after(*i, last_end))
        .map(|(_, u)| u.clone())
        .collect();
    unread.extend(
        woken
            .iter()
            .filter(|(i, _)| !read_by_later_turn(*i))
            .map(|(_, u)| u.clone()),
    );
    for (i, id, update) in &started {
        if !after(*i, latest_turn_start) || finished.iter().any(|(_, f)| f.work_id() == id) {
            continue;
        }
        unread.push(Unread::Unfinished {
            id: id.clone(),
            command: update["command"]
                .as_str()
                .or_else(|| update["description"].as_str())
                .map(str::to_string),
            output_file: update["output_file"].as_str().map(str::to_string),
        });
    }
    unread.sort_by(|a, b| a.work_id().cmp(b.work_id()));
    unread.dedup_by(|a, b| a.work_id() == b.work_id());
    Ok(unread)
}

/// The most of a command's output a wake prompt carries inline: its tail, where
/// a build's verdict is. Grok keeps the full log at `output_file`.
const WAKE_OUTPUT_TAIL_BYTES: usize = 16 * 1024;

/// The prompt that hands over shell output and notifies the model about
/// subagent completions whose result it may need to fetch.
///
/// Shell output is **in the prompt**, not pointed at. Subagent results are not
/// available in the retained update fields, so that case can only ask the model
/// to fetch them. A clean turn boundary does not prove that it did.
pub fn wake_prompt(unread: &[Unread]) -> String {
    let mut out = String::from(
        "Your turn ended while background work you started was still running. Here is the \
         information available about it.\n\n",
    );
    for u in unread {
        match u {
            Unread::Finished {
                id,
                command,
                exit_code,
                signal,
                output,
                truncated,
                output_file,
            } => {
                let status = match (exit_code, signal) {
                    (Some(c), _) => format!("exit code {c}"),
                    (None, Some(s)) => format!("killed by {s}"),
                    (None, None) => "unknown exit status".to_string(),
                };
                out.push_str(&format!(
                    "## `{command}` (task {id}) finished: {status}\n\n"
                ));
                let tail = tail_bytes(output, WAKE_OUTPUT_TAIL_BYTES);
                if tail.len() < output.len() || *truncated {
                    out.push_str("Output (tail; the full log is at ");
                    out.push_str(output_file.as_deref().unwrap_or("its output file"));
                    out.push_str("):\n\n");
                } else {
                    out.push_str("Output:\n\n");
                }
                out.push_str("```\n");
                out.push_str(tail);
                if !tail.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("```\n\n");
            }
            Unread::Unfinished {
                id,
                command,
                output_file,
            } => {
                if let WorkId::Subagent(id) = id {
                    out.push_str(&format!(
                        "## Subagent {id} did NOT finish\n\nIt did not finish before the \
                         session ended, so no result is available. Start another subagent if \
                         you still need its result.\n\n",
                    ));
                } else {
                    out.push_str(&format!(
                        "## `{}` ({id}) did NOT finish\n\nIt had not finished when the session \
                         ended, so no result is available.",
                        command.as_deref().unwrap_or("background work")
                    ));
                    if let Some(path) = output_file {
                        out.push_str(&format!(" Partial output is in {path}."));
                    }
                    out.push_str(
                        " Re-run it in the foreground and wait for it if you still need it.\n\n",
                    );
                }
            }
            Unread::Subagent { id, status, error } => {
                out.push_str(&format!(
                    "## Subagent {} finished: {}{}\n\nThis wake record does not include \
                     its result here. Fetch and read it with get_command_or_subagent_output \
                     before ending this turn.\n\n",
                    id.raw_id(),
                    status.as_deref().unwrap_or("unknown status"),
                    error
                        .as_deref()
                        .map(|e| format!(" ({e})"))
                        .unwrap_or_default(),
                ));
            }
        }
    }
    out.push_str(
        "Continue the task you were given from here. Do not end your turn while any command or \
         subagent you start is still running - wait for it. Read each subagent result before \
         ending. Finish with the report the task asked for.",
    );
    out
}

/// The last `max` bytes of `s`, cut on a char boundary.
fn tail_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One `updates.jsonl` line of the given kind, shaped like grok 1.0.40's.
    fn update(kind: &str, task: Option<&str>) -> String {
        match (kind, task) {
            ("task_backgrounded", Some(id)) => format!(
                r#"{{"method":"_x.ai/session/update","params":{{"update":{{"sessionUpdate":"task_backgrounded","task_id":"{id}"}}}}}}"#
            ),
            // The model was not waiting: grok queues a wake-up (grok 1.0.40
            // writes `will_wake: true`, `block_waited: false`).
            ("task_completed", Some(id)) => format!(
                r#"{{"method":"_x.ai/session/update","params":{{"update":{{"sessionUpdate":"task_completed","task_snapshot":{{"task_id":"{id}","exit_code":0,"block_waited":false}},"will_wake":true}}}}}}"#
            ),
            // The model was blocked waiting on it and got it in its turn.
            ("task_waited", Some(id)) => format!(
                r#"{{"method":"_x.ai/session/update","params":{{"update":{{"sessionUpdate":"task_completed","task_snapshot":{{"task_id":"{id}","exit_code":0,"block_waited":true}},"will_wake":false}}}}}}"#
            ),
            ("subagent_spawned" | "subagent_finished", Some(id)) => format!(
                r#"{{"method":"session/update","params":{{"update":{{"sessionUpdate":"{kind}","subagent_id":"{id}"}}}}}}"#
            ),
            // A turn grok started itself for a finished task, torn down before
            // it ran (`task-completed-<id>`, not `end_turn`).
            ("synthetic_cancelled", Some(id)) => format!(
                r#"{{"method":"session/update","params":{{"update":{{"sessionUpdate":"turn_completed","prompt_id":"task-completed-{id}","stop_reason":"cancelled"}}}}}}"#
            ),
            _ => format!(
                r#"{{"method":"session/update","params":{{"update":{{"sessionUpdate":"{kind}"}}}}}}"#
            ),
        }
    }

    fn stream(lines: &[String]) -> String {
        lines.join("\n")
    }

    /// The ids of the unread results, for a log that must parse.
    fn ids(updates: &str) -> Vec<String> {
        unread_background_results(updates)
            .expect("well-formed log")
            .iter()
            .map(Unread::id)
            .collect()
    }

    #[test]
    fn a_result_completed_after_the_last_turn_is_unread() {
        // The live probe that lost a passing `brokkr test`: the model ended its
        // turn while the task ran, and the completion landed after it.
        let lost = stream(&[
            update("user_message_chunk", None),
            update("tool_call", None),
            update("task_backgrounded", Some("t1")),
            update("agent_message_chunk", None),
            update("turn_completed", None),
            update("task_completed", Some("t1")),
            update("background_tasks", None),
            update("user_message_chunk", None),
        ]);
        assert_eq!(ids(&lost), vec!["t1".to_string()]);

        // Session db7ea2b1: the model waited, so every completion precedes the
        // final turn boundary.
        let waited = stream(&[
            update("tool_call", None),
            update("task_backgrounded", Some("t1")),
            update("tool_call", None),
            update("task_waited", Some("t1")),
            update("agent_message_chunk", None),
            update("task_backgrounded", Some("t2")),
            update("tool_call", None),
            update("task_waited", Some("t2")),
            update("agent_message_chunk", None),
            update("turn_completed", None),
        ]);
        assert!(ids(&waited).is_empty());

        // Live test 8e45970e: the model backgrounded a warm build and ended its
        // turn; the build finished *before* the turn ended, but grok queued the
        // result as a wake-up rather than giving it to the running turn, so
        // the model never saw it. Completion-before-turn-end is not "read".
        let raced = stream(&[
            update("user_message_chunk", None),
            update("task_backgrounded", Some("t1")),
            update("task_completed", Some("t1")),
            update("agent_message_chunk", None),
            update("turn_completed", None),
            update("user_message_chunk", None),
        ]);
        assert_eq!(ids(&raced), vec!["t1".to_string()]);
        // Once a turn that started after it has ended, it has been delivered.
        let delivered = format!(
            "{raced}\n{}\n{}",
            update("agent_message_chunk", None),
            update("turn_completed", None)
        );
        assert!(ids(&delivered).is_empty());

        // A resume that read the result appends a new turn after it: clean.
        let resumed = format!(
            "{lost}\n{}\n{}",
            update("tool_call", None),
            update("turn_completed", None)
        );
        assert!(ids(&resumed).is_empty());

        // Backgrounded and never completed in the latest turn: lost, and the
        // model is told once.
        let never = stream(&[
            update("task_backgrounded", Some("t1")),
            update("turn_completed", None),
        ]);
        assert_eq!(ids(&never), vec!["t1".to_string()]);

        assert!(ids("").is_empty());
        assert!(ids(&update("turn_completed", None)).is_empty());
        // A log that cannot be parsed cannot be vouched for: an error, never
        // "nothing unread", or the guarantee would fail open exactly when it
        // cannot be checked.
        assert!(unread_background_results(&format!("{lost}\n{{\"params\": {{")).is_err());
    }

    #[test]
    fn persistent_monitors_are_not_work_that_must_finish() {
        let monitor = r#"{"method":"_x.ai/session/update","params":{"update":{"sessionUpdate":"task_backgrounded","task_id":"m1","monitor_description":"watch the server"}}}"#;
        let log = stream(&[monitor.to_string(), update("turn_completed", None)]);
        assert!(ids(&log).is_empty());
    }

    #[test]
    fn the_wake_prompt_carries_the_result_itself() {
        let finished = Unread::Finished {
            id: WorkId::Task("t1".to_string()),
            command: "brokkr test -p piners-vm reg".to_string(),
            exit_code: Some(0),
            signal: None,
            output: "[test]    PASS piners-vm::reg\n".to_string(),
            truncated: false,
            output_file: Some("/g/t1.log".to_string()),
        };
        let lost = Unread::Unfinished {
            id: WorkId::Task("t2".to_string()),
            command: Some("brokkr check".to_string()),
            output_file: Some("/g/t2.log".to_string()),
        };
        let prompt = wake_prompt(&[finished, lost]);
        // The output is in the prompt: delivered by sending it.
        assert!(prompt.contains("PASS piners-vm::reg"), "{prompt}");
        assert!(prompt.contains("exit code 0"), "{prompt}");
        // Work that never finished is stated as such, not implied.
        assert!(
            prompt.contains("`brokkr check` (t2) did NOT finish"),
            "{prompt}"
        );

        // A huge output is cut to its tail, and the prompt says where the rest is.
        let big = Unread::Finished {
            id: WorkId::Task("t3".to_string()),
            command: "brokkr check".to_string(),
            exit_code: Some(1),
            signal: None,
            output: format!("{}THE VERDICT", "x".repeat(WAKE_OUTPUT_TAIL_BYTES * 2)),
            truncated: false,
            output_file: Some("/g/t3.log".to_string()),
        };
        let prompt = wake_prompt(&[big]);
        assert!(prompt.contains("THE VERDICT"), "the tail is kept");
        assert!(prompt.contains("/g/t3.log"), "{prompt}");
        assert!(prompt.len() < WAKE_OUTPUT_TAIL_BYTES + 2048);
    }

    #[test]
    fn the_wake_prompt_is_explicit_about_subagent_results() {
        let prompt = wake_prompt(&[Unread::Subagent {
            id: WorkId::Subagent("s1".to_string()),
            status: Some("completed".to_string()),
            error: None,
        }]);
        assert!(
            prompt.contains("This wake record does not include its result here"),
            "{prompt}"
        );
        assert!(
            prompt.contains("get_command_or_subagent_output"),
            "{prompt}"
        );
        assert!(
            prompt.contains("Read each subagent result before ending"),
            "{prompt}"
        );

        let unfinished = wake_prompt(&[Unread::Unfinished {
            id: WorkId::Subagent("s2".to_string()),
            command: Some("research task".to_string()),
            output_file: None,
        }]);
        assert!(
            unfinished.contains("no result is available"),
            "{unfinished}"
        );
        assert!(!unfinished.contains("Partial output is in"), "{unfinished}");
        assert!(!unfinished.contains("foreground"), "{unfinished}");
    }

    #[test]
    fn a_cancelled_synthetic_turn_does_not_hide_the_result() {
        // Grok started its own wake turn for t1 and tore it down at exit: that
        // `turn_completed` is not the model reading anything.
        let torn_down = stream(&[
            update("task_backgrounded", Some("t1")),
            update("turn_completed", None),
            update("task_completed", Some("t1")),
            update("synthetic_cancelled", Some("t1")),
        ]);
        assert_eq!(ids(&torn_down), vec!["t1".to_string()]);
    }

    #[test]
    fn work_from_an_earlier_turn_that_never_finished_is_not_woken_forever() {
        // t1 was lost in the first process; the wake turn that followed ended
        // cleanly. A new grok process can never finish t1, so it must not count
        // again - otherwise every later resume of this session would wake.
        let stale = stream(&[
            update("task_backgrounded", Some("t1")),
            update("turn_completed", None),
            update("user_message_chunk", None),
            update("agent_message_chunk", None),
            update("turn_completed", None),
        ]);
        assert!(ids(&stale).is_empty());
    }

    #[test]
    fn background_subagents_count_like_shell_tasks() {
        let lost = stream(&[
            update("subagent_spawned", Some("s1")),
            update("turn_completed", None),
            update("subagent_finished", Some("s1")),
        ]);
        assert_eq!(ids(&lost), vec!["subagent:s1".to_string()]);
        let waited = stream(&[
            update("subagent_spawned", Some("s1")),
            update("subagent_finished", Some("s1")),
            update("turn_completed", None),
        ]);
        assert!(ids(&waited).is_empty());

        // The display strings can overlap, but typed identities keep a task
        // whose id starts with `subagent:` distinct from a subagent id.
        let colliding = stream(&[
            update("task_backgrounded", Some("subagent:s1")),
            update("subagent_spawned", Some("s1")),
            update("turn_completed", None),
            update("task_completed", Some("subagent:s1")),
            update("subagent_finished", Some("s1")),
        ]);
        let unread = unread_background_results(&colliding).expect("well-formed log");
        assert_eq!(unread.len(), 2);
        assert!(matches!(
            unread[0].work_id(),
            WorkId::Task(id) if id == "subagent:s1"
        ));
        assert!(matches!(
            unread[1].work_id(),
            WorkId::Subagent(id) if id == "s1"
        ));
    }
}
