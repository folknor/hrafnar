//! Printing a result: the digest above the message, and the command to
//! continue the session. Live and persisted summaries share this rendering;
//! provider-specific evidence keeps its adapter's renderer.

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
    print_status(
        d.exit.code(),
        d.exit.signal_name().as_deref(),
        outcome,
        &d.status_facts(),
    );
    print_usage(d.turns, &d.usage);
    if let Some(evidence) = &d.provider {
        match evidence {
            ProviderForensics::Codex(c) => c.print(),
        }
    }
    if let Some(ref path) = d.incident_path {
        println!("incident: {}", path.display());
    }
}

/// Print a persisted `DigestSummary` (from the sidecar) in the same shape as the
/// inline post-mortem, so `review sessions <id>` reads like a live run.
pub(crate) fn print_digest_summary(d: &super::DigestSummary) {
    print_status(
        d.exit_code,
        d.signal.as_deref(),
        &Outcome::interpret_summary(d),
        &d.status_facts(),
    );
    if let Some(false) = d.task_complete {
        println!("task_complete: false");
    }
    if let Some(true) = d.stream_error {
        println!("stream_error: true");
    }
    print_usage(
        d.turns,
        &Usage {
            total_input: d.input_tokens,
            cached_input: d.cached_input_tokens,
            output: d.output_tokens,
            reasoning_output: d.reasoning_output_tokens,
        },
    );
    if let Some(ref path) = d.incident_path {
        println!("incident: {path}");
    }
}

fn print_status(
    exit_code: Option<i32>,
    signal: Option<&str>,
    outcome: &Outcome,
    facts: &super::outcome::StatusFacts<'_>,
) {
    match exit_code {
        Some(code) => println!("exit: {code}"),
        None => println!("exit: -"),
    }
    if let Some(signal) = signal {
        println!("signal: {signal}");
    }
    for line in outcome.lines(facts) {
        println!("{line}");
    }
}

fn print_usage(turns: u32, usage: &Usage) {
    println!("turns: {turns}");
    println!(
        "usage: input={} cached={} output={} reasoning={}",
        usage.total_input, usage.cached_input, usage.output, usage.reasoning_output
    );
}
