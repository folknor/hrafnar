use super::*;

pub fn print_result(result: &ProviderResult) {
    match &result.output {
        Ok(turn) => {
            println!("--- {} ---", result.provider);
            if let Some(ref sid) = result.session_id {
                println!("session: {sid}");
            }
            // A clean answer prints no digest; any other outcome does, with or
            // without forensics behind it.
            match &result.digest {
                Some(d) => print_digest(&turn.outcome, d),
                None if !turn.outcome.is_clean() => {
                    print_digest(&turn.outcome, &Digest::default());
                }
                None => {}
            }
            println!("{}", turn.text);
            if let (Some(sid), true) = (result.session_id.as_deref(), turn.outcome.interrupted()) {
                println!("\n{}", resume_hint(result.provider, sid));
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
pub fn resume_hint(provider: Provider, session_id: &str) -> String {
    let cutoff = provider.stale_cutoff().as_secs() / 60;
    format!(
        "interrupted - continue within {cutoff}m with:\n  \
         echo \"<message>\" | review message {session_id}"
    )
}

fn print_digest(outcome: &Outcome, d: &Digest) {
    match d.exit_code {
        Some(code) => println!("exit: {code}"),
        None => println!("exit: -"),
    }
    if let Some(ref sig) = d.signal {
        println!("signal: {sig}");
    }
    for line in outcome.lines(&d.status_facts()) {
        println!("{line}");
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
        if let Some(ref tool) = t.last_in_flight_tool {
            println!("  last_in_flight_tool: {tool}");
        }
    }
    if let Some(ref path) = d.incident_path {
        println!("incident: {path}");
    }
}
