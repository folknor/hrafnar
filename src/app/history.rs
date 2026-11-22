//! Session artifacts, recent sessions, and incident listings.

use anyhow::Result;

use super::selected_root;
use crate::{config, harness, incident, project, sessions};

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
        let verdict = m.verdict();
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
            m.timestamp, m.harness
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
    let snapshot = sessions::query::Snapshot::read();
    let session = snapshot.require(session_id)?;
    let records = &session.rows;
    let latest = session.latest();
    if let Some(sel) = selector {
        let selected = selected_root(sel)?;
        sessions::query::assert_project(session_id, &latest.project, selected.as_path(), sel)?;
    }

    let now = harness::now_epoch_secs();
    // A bare `?` reads poorly in prose; the listing keeps it as its column.
    let age = sessions::Age::at_epoch(latest.epoch_secs, now);
    let last = match age.elapsed_secs() {
        Some(_) => age.to_string(),
        None => "at an unknown time".to_string(),
    };

    println!("session: {session_id}");
    println!(
        "harness: {} / archetype: {} ({}) / {} touch(es), last {last}",
        latest.harness,
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
        harness::print_digest_summary(d);
    }

    let artifacts = harness::session_artifacts(latest);
    artifacts.print();
    let recovered = artifacts.final_answer();
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

/// `review sessions` - aggregate sidecar records by session_id and print
/// recent sessions with their age, harness, archetype, touch count, and the
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
            Some(root) => root.to_path_buf(),
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
    let snapshot = sessions::query::Snapshot::read();
    let observation = sessions::query::Observation::capture(None);
    let ids: std::collections::BTreeSet<_> = observation
        .live
        .iter()
        .map(|m| m.session_id.as_str())
        .collect();
    let mut live = Vec::new();
    for id in ids {
        match observation.state(id, &snapshot) {
            sessions::query::State::InFlight(m)
            | sessions::query::State::CleanupPending(m)
            | sessions::query::State::HandoffPending(m) => {
                if in_filter(&m.project) {
                    live.push(m.clone());
                }
            }
            sessions::query::State::Ambiguous => {
                if observation
                    .live
                    .iter()
                    .any(|m| m.session_id == id && in_filter(&m.project))
                {
                    println!(
                        "[ambiguous] session: {id} / multiple live-run observations; retry shortly"
                    );
                }
            }
            sessions::query::State::Queued(_)
            | sessions::query::State::Idle(_)
            | sessions::query::State::Unknown => {}
        }
    }
    let now = harness::now_epoch_secs();
    if !live.is_empty() {
        live.sort_by_key(|m| m.started_epoch);
        for m in &live {
            let since = sessions::format_age(now.saturating_sub(m.started_epoch));
            if m.cleanup_pending() {
                println!(
                    "[cleanup pending] {} / its review is gone; processes still being torn down",
                    m.harness
                );
            } else if m.phase == crate::inflight::Phase::HandoffPending {
                println!("[recording] {} / waiting for the run handoff", m.harness);
            } else {
                println!("[in flight] {} / turn in flight since {since}", m.harness);
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
    for sid in &observation.queued {
        let sessions::query::State::Queued(row) = observation.state(sid, &snapshot) else {
            continue;
        };
        let project = row.map(|r| r.project.clone());
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

    let mut snapshot = snapshot;
    if let Some(root) = &project_filter {
        snapshot.retain_project(root);
    }

    if snapshot.rows.is_empty() {
        if live.is_empty() {
            if project_filter.is_some() {
                eprintln!("no sessions recorded for this project (try --all)");
            } else {
                eprintln!("no sessions recorded");
            }
        }
        return Ok(());
    }

    struct Row<'a> {
        latest_secs: Option<u64>,
        opener: &'a sessions::SessionRow,
        latest: &'a sessions::SessionRow,
        touches: usize,
    }
    let mut rows: Vec<_> = snapshot
        .sessions()
        .into_iter()
        .map(|session| Row {
            latest_secs: session.latest().epoch_secs,
            opener: session.opener(),
            latest: session.latest(),
            touches: session.rows.len(),
        })
        .collect();

    rows.sort_by_key(|r| std::cmp::Reverse(r.latest_secs));
    rows.truncate(limit);

    for row in &rows {
        let age = sessions::Age::at_epoch(row.latest_secs, now);
        let touches_label = if row.touches == 1 { "touch" } else { "touches" };
        println!(
            "[{age}] {} / {} ({}) / {} {touches_label}",
            row.opener.harness, row.opener.archetype, row.opener.kind, row.touches
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

    fn summary(v: serde_json::Value) -> harness::DigestSummary {
        serde_json::from_value(v).expect("summary parses")
    }

    /// The note lines (everything after the stated and diagnosed reasons)
    /// a persisted summary renders to.
    fn notes(d: &harness::DigestSummary) -> Vec<String> {
        harness::Outcome::interpret_summary(d)
            .lines(&d.status_facts())
            .into_iter()
            .filter(|l| l.starts_with("note:") || l.starts_with("terminated by review:"))
            .collect()
    }

    #[test]
    fn an_answer_with_a_non_zero_exit_is_not_called_inconclusive() {
        // What grok and claude record for an answer from a process that then
        // exited non-zero: captured, with the observed exit code and no
        // harness-stated `turn_error`.
        let d = summary(serde_json::json!({
            "exit_code": 1, "captured": true, "turns": 1,
            "input_tokens": 0, "cached_input_tokens": 0, "output_tokens": 0,
            "reasoning_output_tokens": 0,
        }));
        assert!(notes(&d).is_empty());
        // Rows recorded before that change carry a `turn_error` instead, and
        // still must not be called inconclusive or a failure.
        let legacy = summary(serde_json::json!({
            "exit_code": 1, "captured": true, "turns": 1,
            "input_tokens": 0, "cached_input_tokens": 0, "output_tokens": 0,
            "reasoning_output_tokens": 0,
            "turn_error": "claude exited non-zero after producing an answer",
        }));
        assert!(notes(&legacy).is_empty());
        let outcome = harness::Outcome::interpret_summary(&legacy);
        assert!(!outcome.failed() && outcome.stated_failure().is_none());
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
            notes(&d),
            ["note: no conclusion was produced; the text below is not an answer"]
        );
    }

    #[test]
    fn an_interrupt_names_how_the_run_was_stopped() {
        let d = summary(serde_json::json!({
            "exit_code": 0, "captured": false, "turns": 1,
            "input_tokens": 0, "cached_input_tokens": 0, "output_tokens": 0,
            "reasoning_output_tokens": 0, "interrupted": true,
            "terminated_by_review": "review was signalled to stop",
        }));
        let lines = notes(&d);
        assert_eq!(
            lines[0],
            "terminated by review: review was signalled to stop"
        );
        assert!(lines[1].starts_with("note: interrupted"), "{lines:?}");
    }
}
