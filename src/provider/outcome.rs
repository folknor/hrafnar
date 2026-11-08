//! How a run that reached a turn ended: one value, built by the runner that
//! observed it, read by every decision and rendering that follows.
//!
//! The facts are a product, not a sum. An answer, a reason the provider
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
//! `ProviderResult::output`, never an outcome with nothing set. A turn that
//! ran and produced nothing is the opposite: an `Ok` turn whose outcome says
//! so. Collapsing it into `Err` would drop the session id (the cut-off turn is
//! the one most worth resuming) and leave `review message`'s cold-cache clock
//! stale after a turn that warmed the cache.

use super::{Digest, DigestSummary};

/// Where the run's answer came from, if it has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    None,
    /// Reported by the provider itself: codex's `-o` file, or a grok or claude
    /// result object that is not an error.
    Captured,
    /// Not reported, but restored from codex's rollout transcript.
    Recovered,
}

/// Whether `review` ended the provider process rather than letting it exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndedBy {
    Natural,
    /// An interrupt request (`review interrupt`, `review message`, or a signal
    /// to `review`) ended a turn that had no answer. The reason says which.
    Interrupted(String),
    /// `review` killed the process for its own reason (the codex watchdog).
    Killed(String),
}

/// Why `review` ended a run whose interrupt came from `review interrupt` or
/// `review message`.
pub const OPERATOR_INTERRUPT: &str = "operator interrupt (`review interrupt`)";
/// Why `review` ended a run when `review` itself was signalled.
pub const SIGNAL_STOP: &str = "review was signalled to stop";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub answer: Answer,
    /// The provider's own account of why the turn ended (a refusal, a rate
    /// limit, an API error). Kept even beside an answer: it is evidence, and
    /// the retry policy filters it (`stated_failure`), not the record.
    pub stated: Option<String>,
    /// A failure `review` diagnosed itself: a result it could not classify,
    /// background results it could not verify. Fails the run even beside an
    /// answer.
    pub unverified: Option<String>,
    pub ended_by: EndedBy,
}

impl Outcome {
    pub fn answered(answer: Answer) -> Self {
        Self {
            answer,
            stated: None,
            unverified: None,
            ended_by: EndedBy::Natural,
        }
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
    /// wins over an interrupt request: the provider can finish between the
    /// request and the signal, and calling that interrupted would report a
    /// completed run as cut short. A kill `review` made for its own reason is
    /// kept as the stated termination either way, so an interrupt landing on a
    /// watchdog kill does not erase what the watchdog saw.
    pub fn ended(
        mut self,
        interrupt_requested: bool,
        stopped_by_signal: bool,
        killed: Option<String>,
    ) -> Self {
        self.ended_by = if interrupt_requested && !self.has_answer() {
            EndedBy::Interrupted(killed.unwrap_or_else(|| {
                if stopped_by_signal {
                    SIGNAL_STOP
                } else {
                    OPERATOR_INTERRUPT
                }
                .to_string()
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
    pub fn termination(&self) -> Option<&str> {
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
            (true, why) => EndedBy::Interrupted(why.unwrap_or_else(|| OPERATOR_INTERRUPT.into())),
            (false, Some(why)) => EndedBy::Killed(why),
            (false, None) => EndedBy::Natural,
        };
        Self {
            answer,
            stated: d.turn_error.clone(),
            unverified: d.review_error.clone(),
            ended_by,
        }
    }

    /// The outcome lines of a digest, live or persisted, in reading order: what
    /// was stated, what `review` diagnosed, the status note, who ended the run,
    /// and how the missing answer (if any) should be read. The one renderer, so
    /// `review sessions` says what the run printed.
    pub fn lines(&self, f: &StatusFacts<'_>) -> Vec<String> {
        let mut out = vec![format!("captured: {}", self.answer == Answer::Captured)];
        // Ahead of everything else: the provider said why the turn ended,
        // which reframes the exit code, and the next move follows from it.
        if let Some(msg) = &self.stated {
            out.push(format!("turn failed: {msg}"));
        }
        if let Some(msg) = &self.unverified {
            out.push(format!("review failed: {msg}"));
        }
        if self.has_answer() && f.exit_code.is_none() && f.signal.is_none() {
            out.push("note: answer captured; process exit status could not be confirmed".into());
        }
        // Before the notes below: it reframes the exit code and signal, which
        // otherwise read as the provider dying on its own.
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
                } else if f.exit_code != Some(0) || f.signal.is_some() {
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
    pub signal: Option<&'a str>,
    pub quiet_secs: Option<u64>,
    pub last_event: Option<&'a str>,
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
        let o = Outcome::answered(Answer::Recovered).ended(true, false, Some("watchdog".into()));
        assert!(!o.interrupted());
        assert_eq!(o.ended_by, EndedBy::Killed("watchdog".into()));
        let o = Outcome::no_answer().ended(true, false, Some("watchdog".into()));
        assert_eq!(o.ended_by, EndedBy::Interrupted("watchdog".into()));
        let o = Outcome::no_answer().ended(true, true, None);
        assert_eq!(o.termination(), Some(SIGNAL_STOP));
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
            Outcome::answered(Answer::Recovered).ended(false, false, Some("watchdog".into())),
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
