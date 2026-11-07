use super::*;

pub fn print_result(result: &ProviderResult) {
    match &result.output {
        Ok(text) => {
            println!("--- {} ---", result.provider);
            if let Some(ref sid) = result.session_id {
                println!("session: {sid}");
            }
            if let Some(ref d) = result.digest {
                print_digest(d);
            }
            println!("{text}");
            if let (Some(sid), Some(true)) = (
                result.session_id.as_deref(),
                result.digest.as_ref().map(|d| d.interrupted),
            ) {
                println!("\n{}", resume_hint(&result.provider, sid));
            }
        }
        Err(err) => {
            eprintln!("--- {} ---", result.provider);
            eprintln!("error: {err}");
        }
    }
}

/// How to continue an interrupted session, and by when - shared by the run's
/// own output and `review interrupt`, so both say the same thing.
pub fn resume_hint(provider: &str, session_id: &str) -> String {
    let cutoff = crate::timings::stale_session(provider).as_secs() / 60;
    format!(
        "interrupted - continue within {cutoff}m with:\n  \
         echo \"<message>\" | review message {session_id}"
    )
}

fn print_digest(d: &Digest) {
    match d.exit_code {
        Some(code) => println!("exit: {code}"),
        None => println!("exit: -"),
    }
    if let Some(ref sig) = d.signal {
        println!("signal: {sig}");
    }
    println!("captured: {}", d.captured);
    // Ahead of everything else: codex stated why the turn ended, which reframes
    // the exit code and makes the notes below unnecessary. Most often an upstream
    // refusal, and the operator's next move (rephrase, get authorised, wait out a
    // limit) follows from the message, not from the exit status.
    if let Some(ref msg) = d.turn_error {
        println!("turn failed: {msg}");
    }
    if let Some(ref msg) = d.review_error {
        println!("review failed: {msg}");
    }
    if d.captured && d.exit_code.is_none() && d.signal.is_none() {
        println!("note: answer captured; process exit status could not be confirmed");
    }
    // Print this before the notes below: it reframes the exit code/signal, which
    // otherwise read as codex dying on its own when in fact we killed it.
    if let Some(ref why) = d.terminated_by_review {
        println!("terminated by review: {why}");
        if let Some(secs) = d.quiet_secs {
            println!("  rollout silent for: {secs}s");
        }
        if let Some(ref ev) = d.last_rollout_event {
            println!("  last rollout event: {ev}");
        }
    }
    if d.recovered_from_transcript {
        println!("recovered: final answer restored from transcript (stream/-o truncated)");
    } else if !d.captured {
        // No -o and nothing to recover: no final answer was produced. Note that
        // task_complete is NOT a success signal - it fires on aborted turns too
        // (task_complete.last_agent_message=null), so the exit status sets tone.
        // A kill by us is excluded: "died" would be a lie about our own doing.
        if d.turn_error.is_some() || d.review_error.is_some() {
            // Already explained above. Saying "died without a final answer"
            // underneath a diagnosis is the bug this branch avoids: it reframes
            // an explained no-answer as a mystery.
            // "Not an answer" rather than "interim": for codex and grok the text
            // is interim commentary, but for claude it is the error message
            // itself.
            println!("note: no conclusion was produced; the text below is not an answer");
        } else if d.terminated_by_review.is_some() {
            println!(
                "note: review terminated the run; the text below is whatever it \
                 had produced by then"
            );
        } else if d.exit_code != Some(0) || d.signal.is_some() {
            println!(
                "note: run died without a final answer; the text below is the last \
                 interim note, not a conclusion"
            );
        } else if d.transcript.as_ref().is_some_and(|t| t.task_complete) {
            println!("note: turn ended without a final answer (text below is interim)");
        }
    }
    println!("turns: {}", d.turns);
    let u = &d.usage;
    println!(
        "usage: input={} cached={} output={} reasoning={}",
        u.input_tokens, u.cached_input_tokens, u.output_tokens, u.reasoning_output_tokens
    );
    if !d.log_lines.is_empty() {
        println!("--- codex log lines ({}) ---", d.log_lines.len());
        for line in &d.log_lines {
            println!("{line}");
        }
        println!("--- end log lines ---");
    }
    if let Some(ref t) = d.transcript {
        println!("transcript: {}", t.path);
        println!(
            "  task_complete={} stream_error={}",
            t.task_complete, t.stream_error
        );
        if let Some(ref last) = t.last_event {
            println!("  last_event: {last}");
        }
        if let Some((ref name, ref args)) = t.last_in_flight_tool {
            println!("  last_in_flight_tool: {name} {}", truncate(args, 200));
        }
    }
    if let Some(ref path) = d.incident_path {
        println!("incident: {path}");
    }
}
