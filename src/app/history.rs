//! Session artifacts, recent sessions, and incident listings.

use anyhow::{Result, bail};

use super::selected_root;
use crate::{config, incident, inflight, project, provider, sessions, transcript};

/// `review incidents` - list recent forensic bundles (see `incident.rs`) so a
/// dead run is triageable at a glance instead of buried under the incidents dir.
pub(super) fn run_incidents(limit: usize) -> Result<()> {
    let listing = incident::list_recent(limit);
    for failure in &listing.failures {
        let path = failure
            .path
            .as_deref()
            .map(|path| format!(" {}", path.display()))
            .unwrap_or_default();
        eprintln!(
            "warning: could not read incident data{path}: {}",
            failure.message
        );
    }
    if listing.incidents.is_empty() {
        if listing.failures.is_empty() {
            eprintln!(
                "no incident bundles recorded (none of your codex runs have looked suspicious)"
            );
        } else {
            eprintln!("no readable incident bundles found");
        }
        return Ok(());
    }
    for inc in &listing.incidents {
        let m = &inc.meta;
        let exit = match m.exit_code {
            Some(c) => c.to_string(),
            None => "-".to_string(),
        };
        // A stated reason wins over every inferred verdict: these bundles are
        // explained, and listing them as "died" sends the reader hunting for a
        // cause codex already gave. Truncated so one refusal can't wrap the line.
        let stated = m
            .turn_error
            .as_deref()
            .map(|e| format!("turn failed: {}", first_line_truncated(e, 90)));
        let verdict = match &stated {
            Some(s) => s.as_str(),
            None if m.recovered_from_transcript => "recovered from transcript",
            None if m.final_answer_present == Some(false) => "no final answer (died)",
            None if m.final_answer_present == Some(true) => "completed (stream/-o truncated)",
            None => "suspicious",
        };
        let sig = m
            .signal
            .as_deref()
            .map(|s| format!(" signal={s}"))
            .unwrap_or_default();
        let ver = m
            .codex_version
            .as_deref()
            .map(|v| format!("  [{v}]"))
            .unwrap_or_default();
        let sid = m.session_id.as_deref().unwrap_or("no-session");
        println!(
            "{}  {} {sid}  exit={exit}{sig}  {verdict}{ver}",
            m.timestamp, m.provider
        );
        println!("       {}", inc.dir.display());
    }
    Ok(())
}

/// `review sessions <id>` - surface one session's artifacts: its digest, the
/// on-disk codex rollout transcript (the authoritative record), and the final
/// response. This is the on-demand form of the post-mortem that a suspicious
/// run prints inline - reachable for any past session by ID.
///
/// The session names its own project, so the caller's directory plays no part
/// and a checkout that has since been deleted still shows. `-C` is an
/// assertion that the session belongs to the selected project.
pub(super) fn run_session_show(session_id: &str, selector: Option<&str>) -> Result<()> {
    let mut records: Vec<sessions::SessionRecord> = sessions::read_all()
        .into_iter()
        .filter(|r| r.session_id == session_id)
        .collect();
    if records.is_empty() {
        bail!(
            "no sidecar record for session {session_id}\n  \
             (run `review sessions` to list known sessions)"
        );
    }
    records.sort_by_key(|r| r.epoch_secs);
    let latest = records.last().expect("non-empty");
    if let Some(sel) = selector {
        let selected = selected_root(sel)?;
        if !project::same_project(&latest.project, &selected) {
            bail!(
                "session {session_id} belongs to {}, not {} (-C {sel})",
                latest.project,
                selected.display()
            );
        }
    }

    let now = provider::now_epoch_secs();
    // A bare `?` reads poorly in prose; the listing keeps it as its column.
    let age = sessions::Age::at_epoch(latest.epoch_secs, now);
    let last = match age.elapsed_secs() {
        Some(_) => age.to_string(),
        None => "at an unknown time".to_string(),
    };

    println!("session: {session_id}");
    println!(
        "provider: {} / archetype: {} ({}) / {} touch(es), last {last}",
        latest.provider,
        latest.archetype,
        latest.kind,
        records.len()
    );
    println!("project: {}", latest.project);
    if let Some(ref model) = latest.model {
        println!("model: {model}");
    }
    if let Some(ref effort) = latest.effort {
        println!("effort: {effort}");
    }
    if let Some(ref sandbox) = latest.sandbox {
        println!("sandbox: {sandbox}");
    }
    for root in &latest.writable_roots {
        println!("writable root: {root}");
    }

    if let Some(ref d) = latest.digest {
        print_digest_summary(d);
    }

    // The recorded home keeps custom CODEX_HOME sessions visible. Older rows
    // have no path and use the current process environment/default.
    // Only codex keeps a rollout `review` reads. A recorded name that no
    // longer parses (a removed provider) is shown as recorded, with none.
    let has_rollout = match latest.provider.parse::<provider::Provider>() {
        Ok(provider::Provider::Codex) => true,
        Ok(provider::Provider::Claude | provider::Provider::Grok) | Err(_) => false,
    };
    let transcript = if has_rollout {
        let home = transcript::codex_home(
            latest.codex_home.as_deref(),
            None,
            std::path::Path::new(&latest.project),
        );
        let lookup = match home.as_deref() {
            Some(home) => transcript::summarize_session_in_home_lookup(session_id, home, None),
            None => transcript::Lookup::UnknownHome,
        };
        match lookup {
            transcript::Lookup::Found(summary) => Some(summary),
            transcript::Lookup::Absent => {
                println!("transcript: (not found under the resolved CODEX_HOME)");
                None
            }
            transcript::Lookup::Unreadable(error) => {
                println!("transcript: (unreadable: {error})");
                None
            }
            transcript::Lookup::UnknownHome => {
                println!("transcript: (Codex home could not be resolved)");
                None
            }
        }
    } else {
        None
    };
    if let Some(ref t) = transcript {
        println!("transcript: {}", t.path);
        println!(
            "  task_complete={} stream_error={}",
            t.task_complete, t.stream_error
        );
        if let Some(ref last) = t.last_event {
            println!("  last_event: {last}");
        }
        if let Some(ref tool) = t.last_in_flight_tool {
            println!("  last_in_flight_tool: {tool}");
        }
    }

    // Prefer the transcript's authoritative final_answer when it exists: it
    // recovers the real report even for rows recorded before runtime recovery
    // landed (whose sidecar `response` may be a truncated interim note).
    let recovered = transcript.as_ref().and_then(|t| t.final_answer.as_deref());
    // A codex rollout that reached task_complete but carries no final_answer
    // produced no conclusion - the turn died mid-work (task_complete fires on
    // aborted turns too). Flag it even for old rows that predate digest
    // persistence and so have no stored exit code.
    if transcript
        .as_ref()
        .is_some_and(|t| t.final_answer.is_none() && t.task_complete)
    {
        println!(
            "note: rollout has no final answer - the turn produced no conclusion (likely died mid-work)"
        );
    }
    println!("--- response ---");
    match (recovered, &latest.response, &latest.error) {
        (Some(fa), sidecar, _) => {
            if sidecar.as_deref() != Some(fa) {
                println!("(recovered final answer from transcript)");
            }
            println!("{fa}");
        }
        (None, Some(r), _) => println!("{r}"),
        (None, None, Some(e)) => println!("(no response) error: {e}"),
        (None, None, None) => println!("(no response recorded)"),
    }
    Ok(())
}

/// Print a persisted `DigestSummary` (from the sidecar) in the same shape as the
/// inline post-mortem, so `review sessions <id>` reads like a live run.
fn print_digest_summary(d: &provider::DigestSummary) {
    match d.exit_code {
        Some(code) => println!("exit: {code}"),
        None => println!("exit: -"),
    }
    if let Some(ref sig) = d.signal {
        println!("signal: {sig}");
    }
    println!("captured: {}", d.captured);
    if let Some(ref msg) = d.turn_error {
        println!("turn failed: {msg}");
    }
    if let Some(ref msg) = d.review_error {
        println!("review failed: {msg}");
    }
    // Like the live digest, this status note can appear alongside the
    // end-of-turn note below.
    if d.captured && d.exit_code.is_none() && d.signal.is_none() {
        println!("note: answer captured; process exit status could not be confirmed");
    }
    if let Some(line) = digest_summary_note(d) {
        println!("{line}");
    }
    if let Some(false) = d.task_complete {
        println!("task_complete: false");
    }
    if let Some(true) = d.stream_error {
        println!("stream_error: true");
    }
    println!("turns: {}", d.turns);
    println!(
        "usage: input={} cached={} output={} reasoning={}",
        d.input_tokens, d.cached_input_tokens, d.output_tokens, d.reasoning_output_tokens
    );
    if let Some(ref path) = d.incident_path {
        println!("incident: {path}");
    }
}

/// The one line `print_digest_summary` adds to say how the run ended, if any.
/// Pure so each case is tested.
fn digest_summary_note(d: &provider::DigestSummary) -> Option<String> {
    if d.recovered_from_transcript {
        Some("recovered: final answer restored from transcript".to_string())
    } else if d.interrupted {
        // The recorded reason, not an assumed one: a signalled `review` ends its
        // turns the same way `review interrupt` does, and saying the operator
        // ran the verb would misdescribe how the turn ended.
        let how = d
            .terminated_by_review
            .as_deref()
            .unwrap_or("operator interrupt (`review interrupt`)");
        Some(format!("note: interrupted - {how}"))
    } else if d.turn_error.is_some() && !d.captured {
        // Explained by `turn failed:` - don't also guess "died mid-turn" at
        // it. Gated on the missing answer: rows written before post-answer exit
        // problems moved out of `turn_error` record a grok or claude answer
        // followed by a non-zero exit as captured *with* a `turn_error`, and
        // "no conclusion" would contradict the answer printed below. The live
        // `print_digest` has the same gate.
        Some("note: no conclusion was produced".to_string())
    } else if !d.captured && (d.exit_code != Some(0) || d.signal.is_some()) {
        // No real final answer obtained and the process failed: a mid-turn
        // death. task_complete alone doesn't refute this - it fires on aborts.
        Some("note: no final answer captured - run likely died mid-turn".to_string())
    } else {
        None
    }
}

/// `review sessions` - aggregate sidecar records by session_id and print
/// recent sessions with their age, provider, archetype, touch count, and the
/// prompt that opened them. Filtered to the current project unless `--all`.
pub(super) fn run_sessions(all: bool, limit: usize, selector: Option<&str>) -> Result<()> {
    let project_filter: Option<std::path::PathBuf> = if all {
        None
    } else {
        // Only which project we are in matters here, so no project config is
        // parsed: a broken one must not stop the operator listing sessions.
        // (A registered `-C` name does read the global file, to find it.)
        let sel = project::select(selector)?;
        Some(match config::project_root(&sel)? {
            Some(root) => root,
            None => sel.dir().to_path_buf(),
        })
    };
    let in_filter = |p: &str| {
        project_filter
            .as_ref()
            .is_none_or(|root| project::same_project(p, root))
    };

    // Runs that are happening *right now*. The sidecar is only written when a
    // run returns, so without this a session with a turn in flight is
    // indistinguishable from an idle one - it shows its previous response and a
    // touch count that has not moved. During the codex hang that motivated the
    // watchdog, that ambiguity is what made a wedged 10-hour run look like
    // nothing was happening at all.
    let mut live = inflight::read_live(None);
    live.retain(|m| in_filter(&m.project));
    let now = provider::now_epoch_secs();
    if !live.is_empty() {
        live.sort_by_key(|m| m.started_epoch);
        for m in &live {
            let since = sessions::format_age(now.saturating_sub(m.started_epoch));
            if m.cleanup_pending {
                println!(
                    "[cleanup pending] {} / its review is gone; processes still being torn down",
                    m.provider
                );
            } else {
                println!("[in flight] {} / turn in flight since {since}", m.provider);
            }
            println!("       session: {}", m.session_id);
            if all {
                println!("       project: {}", m.project);
            }
            println!();
        }
    }

    // Messages queued behind the global lock: no marker yet, so without this
    // they appear nowhere until they launch. A queued message only holds a
    // session id; its project comes from the session's own record.
    for sid in inflight::pending_launches(None) {
        if live.iter().any(|m| m.session_id == sid) {
            continue;
        }
        let project = sessions::latest_for_session(&sid).map(|r| r.project);
        if project_filter.is_some() && !project.as_deref().is_some_and(in_filter) {
            continue;
        }
        println!("[queued] message waiting to launch");
        println!("       session: {sid}");
        if all && let Some(p) = project {
            println!("       project: {p}");
        }
        println!();
    }

    let mut records = sessions::read_all();
    records.retain(|r| in_filter(&r.project));

    if records.is_empty() {
        if live.is_empty() {
            if project_filter.is_some() {
                eprintln!("no sessions recorded for this project (try --all)");
            } else {
                eprintln!("no sessions recorded");
            }
        }
        return Ok(());
    }

    // Group by session_id.
    let mut groups: std::collections::HashMap<String, Vec<sessions::SessionRecord>> =
        std::collections::HashMap::new();
    for rec in records {
        groups.entry(rec.session_id.clone()).or_default().push(rec);
    }

    struct Row {
        latest_secs: Option<u64>,
        opener: sessions::SessionRecord,
        latest: sessions::SessionRecord,
        touches: usize,
    }

    let mut rows: Vec<Row> = groups
        .into_values()
        .map(|mut entries| {
            entries.sort_by_key(|r| r.epoch_secs);
            let touches = entries.len();
            // Chronologically first entry is the opener (the fresh run that
            // created the session); a session touch in pathological cases where
            // the creation row is missing.
            let opener = entries[0].clone();
            let latest = entries.last().cloned().expect("non-empty group");
            Row {
                latest_secs: latest.epoch_secs,
                opener,
                latest,
                touches,
            }
        })
        .collect();

    rows.sort_by_key(|r| std::cmp::Reverse(r.latest_secs));
    rows.truncate(limit);

    for row in &rows {
        let age = sessions::Age::at_epoch(row.latest_secs, now);
        let touches_label = if row.touches == 1 { "touch" } else { "touches" };
        println!(
            "[{age}] {} / {} ({}) / {} {touches_label}",
            row.opener.provider, row.opener.archetype, row.opener.kind, row.touches
        );
        println!("       session: {}", row.latest.session_id);
        let opened = first_line_truncated(&row.opener.operator_prompt, 80);
        println!("       opened:  {opened}");
        if all {
            println!("       project: {}", row.opener.project);
        }
        println!();
    }

    Ok(())
}

fn first_line_truncated(s: &str, max_chars: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    let count = line.chars().count();
    if count <= max_chars {
        return line.to_string();
    }
    let mut out: String = line.chars().take(max_chars).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(v: serde_json::Value) -> provider::DigestSummary {
        serde_json::from_value(v).expect("summary parses")
    }

    #[test]
    fn an_answer_with_a_non_zero_exit_is_not_called_inconclusive() {
        // What grok and claude record for an answer from a process that then
        // exited non-zero: captured, with the observed exit code and no
        // provider-stated `turn_error`.
        let d = summary(serde_json::json!({
            "exit_code": 1, "captured": true, "turns": 1,
            "input_tokens": 0, "cached_input_tokens": 0, "output_tokens": 0,
            "reasoning_output_tokens": 0,
        }));
        assert_eq!(digest_summary_note(&d), None);
        // Rows recorded before that change carry a `turn_error` instead, and
        // still must not be called inconclusive.
        let legacy = summary(serde_json::json!({
            "exit_code": 1, "captured": true, "turns": 1,
            "input_tokens": 0, "cached_input_tokens": 0, "output_tokens": 0,
            "reasoning_output_tokens": 0,
            "turn_error": "claude exited non-zero after producing an answer",
        }));
        assert_eq!(digest_summary_note(&legacy), None);
    }

    #[test]
    fn a_stated_failure_without_an_answer_is_called_inconclusive() {
        let d = summary(serde_json::json!({
            "exit_code": 1, "captured": false, "turns": 1,
            "input_tokens": 0, "cached_input_tokens": 0, "output_tokens": 0,
            "reasoning_output_tokens": 0,
            "turn_error": "claude reported an error (API status 404)",
        }));
        assert_eq!(
            digest_summary_note(&d).as_deref(),
            Some("note: no conclusion was produced")
        );
    }

    #[test]
    fn an_interrupt_note_names_how_the_run_was_stopped() {
        let d = summary(serde_json::json!({
            "exit_code": 0, "captured": false, "turns": 1,
            "input_tokens": 0, "cached_input_tokens": 0, "output_tokens": 0,
            "reasoning_output_tokens": 0, "interrupted": true,
            "terminated_by_review": "review was signalled to stop",
        }));
        assert_eq!(
            digest_summary_note(&d).as_deref(),
            Some("note: interrupted - review was signalled to stop")
        );
    }
}
