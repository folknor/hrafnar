//! How a run that reached a turn ended: one value, built by the runner that
//! observed it, read by every decision and rendering that follows.
//!
//! The facts are a product, not a sum. An answer, a reason the harness
//! stated, a failure `review` diagnosed itself and `review` ending the process
//! are independent observations, and every combination of them occurs: codex
//! can record a stated stream error and still capture an answer, a watchdog
//! kill can follow a recovered answer, grok can state a failure that `review`
//! then also fails to verify. A sum type forced a precedence onto those and
//! lost one fact each time, so the policy questions (did it answer, did it
//! fail, may it be retried, was it interrupted) are methods over the facts
//! instead. Process exit status is not here: it is forensics, on `Digest`.
//!
//! A turn that was never established (spawn failure, unknown model, no result
//! and no sign the session began) has no outcome at all: it is the `Err` of
//! `HarnessResult::output`, never an outcome with nothing set. A turn that
//! ran and produced nothing is the opposite: an `Ok` turn whose outcome says
//! so. Collapsing it into `Err` would drop the session id (the cut-off turn is
//! the one most worth resuming) and leave `review message`'s cold-cache clock
//! stale after a turn that warmed the cache.

use super::{Digest, DigestSummary};

/// Where the run's answer came from, if it has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    None,
    /// Reported by the harness itself: codex's `-o` file, or a grok or claude
    /// result object that is not an error.
    Captured,
    /// Not reported, but restored from codex's rollout transcript.
    Recovered,
}

/// Whether `review` ended the harness process rather than letting it exit.
/// Interrupted versus killed is policy (an answer wins over an interrupt);
/// the `Termination` is the cause, and is never used to infer which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndedBy {
    Natural,
    /// An interrupt request (`review interrupt`, `review message`, or a signal
    /// to `review`) ended a turn that had no answer. The cause says which.
    Interrupted(Termination),
    /// `review` killed the process for its own reason (the codex watchdog).
    Killed(Termination),
}

/// Why `review` ended a harness process. Persisted as its `Display` text in
/// `terminated_by_review` (the row schema is a flat string, and stays one), so
/// the text of each cause is fixed: rows and greps depend on it exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Termination {
    /// `review` itself was signalled to stop.
    Signal,
    /// `review interrupt` or `review message`.
    OperatorInterrupt,
    Watchdog(WatchdogTermination),
    /// A persisted reason that is not one of the canonical texts, kept
    /// verbatim: re-rendering history must not rewrite what it said.
    Recorded(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogTermination {
    /// The answer was on disk and the process would not exit.
    Stranded,
    /// No answer, and observed progress stayed unchanged this long.
    Stalled { quiet_secs: u64 },
}

const SIGNAL_TEXT: &str = "review was signalled to stop";
const OPERATOR_TEXT: &str = "operator interrupt (`review interrupt`)";
const STRANDED_TEXT: &str = "watchdog: stranded completion";

impl std::fmt::Display for Termination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Signal => f.write_str(SIGNAL_TEXT),
            Self::OperatorInterrupt => f.write_str(OPERATOR_TEXT),
            Self::Watchdog(WatchdogTermination::Stranded) => f.write_str(STRANDED_TEXT),
            // Named a stall *timeout* on purpose: it is one, and saying so
            // keeps a future codex cadence change diagnosable.
            Self::Watchdog(WatchdogTermination::Stalled { quiet_secs }) => {
                write!(f, "watchdog: stall timeout ({quiet_secs}s silent)")
            }
            Self::Recorded(text) => f.write_str(text),
        }
    }
}

impl Termination {
    /// Decode a persisted reason. Only an exact canonical text maps to a
    /// cause; anything else is kept as `Recorded`, so a historical string
    /// that merely resembles a cause is never re-rendered differently.
    pub fn parse(text: &str) -> Self {
        let stalled = text
            .strip_prefix("watchdog: stall timeout (")
            .and_then(|rest| rest.strip_suffix("s silent)"))
            .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|digits| digits.parse::<u64>().ok())
            .map(|quiet_secs| Self::Watchdog(WatchdogTermination::Stalled { quiet_secs }));
        match text {
            SIGNAL_TEXT => Self::Signal,
            OPERATOR_TEXT => Self::OperatorInterrupt,
            STRANDED_TEXT => Self::Watchdog(WatchdogTermination::Stranded),
            _ => match stalled {
                // Exactness: the parsed form must render back to the text.
                Some(t) if t.to_string() == text => t,
                _ => Self::Recorded(text.to_string()),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub answer: Answer,
    /// The harness's own account of why the turn ended (a refusal, a rate
    /// limit, an API error). Kept even beside an answer: it is evidence, and
    /// the retry policy filters it (`stated_failure`), not the record.
    pub stated: Option<String>,
    /// A failure `review` diagnosed itself: a result it could not classify,
    /// background results it could not verify. Fails the run even beside an
    /// answer.
    pub unverified: Option<String>,
    pub ended_by: EndedBy,
    /// The harness rejected the run's credentials. Claude only, read from
    /// its stream; never persisted, so a summary decodes it as `None`.
    pub auth: Option<AuthFailure>,
}

/// A claude run that failed to authenticate, and whether resending its prompt
/// under a new session is known to be safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFailure {
    /// The whole stream was read, and the main agent's credential error came
    /// before any model output or tool activity: nothing ran but startup.
    BeforeWork,
    /// Work may have happened first, or part of the stream was not observed.
    /// Reported, never replayed.
    Unknown,
}

impl Outcome {
    pub fn answered(answer: Answer) -> Self {
        Self {
            answer,
            stated: None,
            unverified: None,
            ended_by: EndedBy::Natural,
            auth: None,
        }
    }

    pub fn auth(mut self, auth: Option<AuthFailure>) -> Self {
        self.auth = auth;
        self
    }

    pub fn no_answer() -> Self {
        Self::answered(Answer::None)
    }

    pub fn stated(mut self, reason: String) -> Self {
        self.stated = Some(reason);
        self
    }

    pub fn unverified(mut self, reason: String) -> Self {
        self.unverified = Some(reason);
        self
    }

    /// Settle how `review` ended the run, once for every runner. An answer
    /// wins over an interrupt request: the harness can finish between the
    /// request and the signal, and calling that interrupted would report a
    /// completed run as cut short. A kill `review` made for its own reason is
    /// kept as the stated termination either way, so an interrupt landing on a
    /// watchdog kill does not erase what the watchdog saw.
    pub fn ended(
        mut self,
        interrupt_requested: bool,
        stopped_by_signal: bool,
        killed: Option<Termination>,
    ) -> Self {
        self.ended_by = if interrupt_requested && !self.has_answer() {
            EndedBy::Interrupted(killed.unwrap_or(if stopped_by_signal {
                Termination::Signal
            } else {
                Termination::OperatorInterrupt
            }))
        } else {
            match killed {
                Some(why) => EndedBy::Killed(why),
                None => EndedBy::Natural,
            }
        };
        self
    }

    pub fn has_answer(&self) -> bool {
        self.answer != Answer::None
    }

    /// The exit-status verdict: no answer, or an answer `review` could not
    /// vouch for. A non-zero exit after an answer does not fail a run.
    pub fn failed(&self) -> bool {
        self.unverified.is_some() || !self.has_answer()
    }

    /// A stated reason for producing nothing - the one death no retry or wake
    /// may touch, because resending buys the same refusal. A reason stated
    /// beside an answer does not veto anything.
    pub fn stated_failure(&self) -> Option<&str> {
        self.stated.as_deref().filter(|_| !self.has_answer())
    }

    /// No answer and an explanation for it, stated or diagnosed: nothing a
    /// further turn on the same request would fix.
    pub fn explained_failure(&self) -> bool {
        !self.has_answer() && (self.stated.is_some() || self.unverified.is_some())
    }

    pub fn interrupted(&self) -> bool {
        matches!(self.ended_by, EndedBy::Interrupted(_))
    }

    /// Why `review` ended the process, if it did.
    pub fn termination(&self) -> Option<&Termination> {
        match &self.ended_by {
            EndedBy::Natural => None,
            EndedBy::Interrupted(why) | EndedBy::Killed(why) => Some(why),
        }
    }

    /// Nothing about the run is worth a digest: an answer, no diagnosis, no
    /// intervention. Process status is judged separately, by whoever holds it.
    pub fn is_clean(&self) -> bool {
        self.answer == Answer::Captured
            && self.stated.is_none()
            && self.unverified.is_none()
            && self.ended_by == EndedBy::Natural
    }

    /// Read a persisted summary back into facts. Not an exact reconstruction:
    /// rows are decoded field by field, and the policy methods then apply to
    /// what was decoded. Normalization, stated so it is not mistaken for
    /// fidelity:
    /// - rows written before post-answer exit problems left `turn_error`
    ///   record an answer *with* a stated reason; decoded as such, and
    ///   `stated_failure` ignores it beside the answer;
    /// - `recovered_from_transcript` wins over `captured` (the writer never set
    ///   both);
    /// - an interrupted row with no recorded reason is an operator interrupt;
    ///   a reason without the flag is a kill.
    pub fn interpret_summary(d: &DigestSummary) -> Self {
        let answer = if d.recovered_from_transcript {
            Answer::Recovered
        } else if d.captured {
            Answer::Captured
        } else {
            Answer::None
        };
        let ended_by = match (d.interrupted, d.terminated_by_review.clone()) {
            (true, why) => EndedBy::Interrupted(
                why.as_deref()
                    .map_or(Termination::OperatorInterrupt, Termination::parse),
            ),
            (false, Some(why)) => EndedBy::Killed(Termination::parse(&why)),
            (false, None) => EndedBy::Natural,
        };
        Self {
            answer,
            stated: d.turn_error.clone(),
            unverified: d.review_error.clone(),
            ended_by,
            auth: None,
        }
    }

    /// Text when a turn returned no reportable text. Evidence classification is
    /// already complete; process exit alone does not decide whether a turn died.
    pub fn placeholder(&self, harness: super::Harness) -> String {
        if let Some(reason) = &self.stated {
            format!("({harness} ended the turn: {reason})")
        } else if self.interrupted() {
            "(interrupted by the operator before a final answer)".into()
        } else if self.termination().is_some() {
            format!("({harness} was terminated without a final answer)")
        } else if let Some(reason) = &self.unverified {
            format!("({harness} result could not be verified: {reason})")
        } else {
            format!("({harness} produced no final answer)")
        }
    }

    /// The outcome lines of a digest, live or persisted, in reading order: what
    /// was stated, what `review` diagnosed, the status note, who ended the run,
    /// and how the missing answer (if any) should be read. The one renderer, so
    /// `review sessions` says what the run printed.
    pub fn lines(&self, f: &StatusFacts<'_>) -> Vec<String> {
        let mut out = vec![format!("captured: {}", self.answer == Answer::Captured)];
        // Ahead of everything else: the harness said why the turn ended,
        // which reframes the exit code, and the next move follows from it.
        if let Some(msg) = &self.stated {
            out.push(format!("turn failed: {msg}"));
        }
        if let Some(msg) = &self.unverified {
            out.push(format!("review failed: {msg}"));
        }
        if self.has_answer() && f.exit_code.is_none() && !f.signaled {
            out.push("note: answer captured; process exit status could not be confirmed".into());
        }
        // Before the notes below: it reframes the exit code and signal, which
        // otherwise read as the harness dying on its own.
        if let Some(why) = self.termination() {
            out.push(format!("terminated by review: {why}"));
            if let Some(secs) = f.quiet_secs {
                out.push(format!("  rollout silent for: {secs}s"));
            }
            if let Some(ev) = f.last_event {
                out.push(format!("  last rollout event: {ev}"));
            }
        }
        match self.answer {
            Answer::Recovered => out.push(
                "recovered: final answer restored from transcript (stream/-o truncated)".into(),
            ),
            Answer::Captured => {}
            // `task_complete` is not a success signal - it fires on aborted
            // turns too - so the exit status sets the tone, and a kill by us
            // is never called a death.
            Answer::None => out.push(
                if self.stated.is_some() || self.unverified.is_some() {
                    // "Not an answer" rather than "interim": for claude the
                    // text is the error message itself.
                    "note: no conclusion was produced; the text below is not an answer"
                } else if self.interrupted() {
                    "note: interrupted before a final answer; the text below is whatever \
                     it had produced by then"
                } else if self.termination().is_some() {
                    "note: review terminated the run; the text below is whatever it \
                     had produced by then"
                } else if f.exit_code != Some(0) || f.signaled {
                    "note: run died without a final answer; the text below is the last \
                     interim note, not a conclusion"
                } else if f.task_complete == Some(true) {
                    "note: turn ended without a final answer (text below is interim)"
                } else {
                    "note: no final answer was produced"
                }
                .into(),
            ),
        }
        out
    }
}

/// The forensic facts the outcome lines are read against.
pub struct StatusFacts<'a> {
    pub exit_code: Option<i32>,
    /// A signal was observed (live) or recorded (a row, whatever its text).
    pub signaled: bool,
    pub quiet_secs: Option<u64>,
    pub last_event: Option<&'a dyn std::fmt::Display>,
    pub task_complete: Option<bool>,
}

/// What a run that reached a turn returns: its text and how it ended.
#[derive(Debug)]
pub struct Turn {
    pub text: String,
    pub outcome: Outcome,
}

impl Turn {
    /// The persisted summary of this turn. A clean answer with no forensic
    /// digest records none, as it always has; every other outcome is
    /// recorded, with or without forensics, so an adapter that builds no
    /// digest cannot drop its own failure from the logs.
    pub fn summary(&self, digest: Option<&Digest>) -> Option<DigestSummary> {
        match digest {
            Some(d) => Some(d.summary(&self.outcome)),
            None if self.outcome.is_clean() => None,
            None => Some(Digest::default().summary(&self.outcome)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_wins_over_an_interrupt_but_keeps_a_kill() {
        let stranded = Termination::Watchdog(WatchdogTermination::Stranded);
        let o = Outcome::answered(Answer::Recovered).ended(true, false, Some(stranded.clone()));
        assert!(!o.interrupted());
        assert_eq!(o.ended_by, EndedBy::Killed(stranded.clone()));
        let o = Outcome::no_answer().ended(true, false, Some(stranded.clone()));
        assert_eq!(o.ended_by, EndedBy::Interrupted(stranded));
        let o = Outcome::no_answer().ended(true, true, None);
        assert_eq!(o.termination(), Some(&Termination::Signal));
    }

    #[test]
    fn terminations_render_and_parse_their_canonical_texts_exactly() {
        for t in [
            Termination::Signal,
            Termination::OperatorInterrupt,
            Termination::Watchdog(WatchdogTermination::Stranded),
            Termination::Watchdog(WatchdogTermination::Stalled { quiet_secs: 181 }),
        ] {
            assert_eq!(Termination::parse(&t.to_string()), t);
        }
        assert_eq!(
            Termination::Watchdog(WatchdogTermination::Stalled { quiet_secs: 5 }).to_string(),
            "watchdog: stall timeout (5s silent)"
        );
        // Near misses stay verbatim rather than being normalised.
        for text in [
            "watchdog: stall timeout (05s silent)",
            "watchdog: stall timeout (+5s silent)",
            "watchdog",
            "watchdog: stranded completion ",
        ] {
            assert_eq!(Termination::parse(text), Termination::Recorded(text.into()));
            assert_eq!(Termination::parse(text).to_string(), text);
        }
    }

    #[test]
    fn a_recorded_signal_text_counts_as_observed_status_whatever_it_says() {
        let row: DigestSummary = serde_json::from_value(serde_json::json!({
            "exit_code": null, "signal": "SIGWEIRD", "captured": true, "turns": 1,
            "input_tokens": 0, "cached_input_tokens": 0,
            "output_tokens": 0, "reasoning_output_tokens": 0
        }))
        .expect("row");
        let lines = Outcome::interpret_summary(&row).lines(&row.status_facts());
        assert!(
            !lines.iter().any(|l| l.contains("could not be confirmed")),
            "{lines:?}"
        );
    }

    #[test]
    fn independent_facts_survive_together() {
        let o = Outcome::no_answer()
            .stated("cancelled".into())
            .unverified("cannot read log".into());
        assert_eq!(o.stated_failure(), Some("cancelled"));
        assert!(o.failed() && o.explained_failure());
        let answered = Outcome::answered(Answer::Captured).stated("stream error".into());
        assert_eq!(answered.stated_failure(), None);
        assert!(!answered.failed());
        assert!(!answered.is_clean());
    }

    #[test]
    fn a_clean_answer_without_forensics_records_nothing() {
        let turn = Turn {
            text: String::new(),
            outcome: Outcome::answered(Answer::Captured),
        };
        assert!(turn.summary(None).is_none());
        let failed = Turn {
            text: String::new(),
            outcome: Outcome::no_answer().stated("refused".into()),
        };
        let s = failed.summary(None).expect("a failure is always recorded");
        assert_eq!(s.turn_error.as_deref(), Some("refused"));
        assert!(!s.captured);
    }

    #[test]
    fn summaries_round_trip_their_facts() {
        for o in [
            Outcome::answered(Answer::Recovered).ended(
                false,
                false,
                Some(Termination::Watchdog(WatchdogTermination::Stalled {
                    quiet_secs: 600,
                })),
            ),
            Outcome::no_answer().ended(true, false, Some(Termination::Recorded("watchdog".into()))),
            Outcome::no_answer().ended(true, true, None),
            Outcome::no_answer()
                .stated("x".into())
                .unverified("y".into()),
            Outcome::answered(Answer::Captured).unverified("z".into()),
        ] {
            let s = Digest::default().summary(&o);
            assert_eq!(Outcome::interpret_summary(&s), o);
        }
    }
}
