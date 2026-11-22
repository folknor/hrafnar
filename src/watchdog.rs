//! Progress watchdog: source-owned revisions and answer evidence, shared patience.
//! The Codex source uses append-only rollout length and a run-scoped phase
//! answer. Its measured silence policy lives on `src/harness/codex/watch.rs`.
//! Other adapters must supply their own measured policy;
//! generic timings disable stall detection by default.

use std::time::{Duration, Instant};

/// Polling and patience settings, injectable so tests can drive the whole
/// watchdog in milliseconds instead of minutes.
#[derive(Clone, Copy)]
pub struct Timings {
    /// How often to stat/read the rollout. Cheap (a stat, plus a read only when
    /// the file grew), and slow enough to be invisible next to a long turn.
    pub poll_interval: Duration,
    /// How long observed progress must stay unchanged, *after* a final
    /// answer exists, before we call it stranded. The cost of firing early is
    /// killing a run that was about to continue; the cost of firing late is a
    /// few idle minutes.
    pub quiet_grace: Duration,
    /// How long observed progress must stay unchanged with *no* answer before
    /// we call the run stalled. `None` disables the stall branch entirely.
    ///
    /// This one is a timeout, and is named as such deliberately - see the module
    /// docs for why the honest name matters.
    pub stall_grace: Option<Duration>,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            poll_interval: crate::timings::POLL_INTERVAL,
            quiet_grace: crate::timings::QUIET_GRACE,
            stall_grace: None,
        }
    }
}

/// Which of the two failure shapes the poller detected.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The source holds an answer and the process has gone quiet.
    Stranded,
    /// No answer is available and progress exceeded its measured silence budget.
    Stalled,
}

/// Verdict handed back to the runner when the watchdog fires.
pub struct Verdict<E = String> {
    pub kind: Kind,
    /// Exact answer that licensed a stranded kill, scoped to this run.
    pub final_answer: Option<String>,
    /// Operator-facing explanation, printed before we kill the child.
    pub reason: String,
    /// How long the rollout had been unchanged when we decided. Recorded on the
    /// digest and in the incident bundle: if the stall branch ever misfires on a
    /// healthy run, this is the number that shows how close the call was.
    pub quiet_secs: u64,
    /// `type/payload.type` of the last event in the rollout - what codex was
    /// doing when it went quiet.
    pub last_event: Option<E>,
}

impl<E> Verdict<E> {
    /// The cause recorded for the kill. `reason` is the longer operator-facing
    /// explanation of the same verdict; this is its fixed, persisted name.
    pub fn termination(&self) -> crate::harness::Termination {
        use crate::harness::{Termination, WatchdogTermination};
        Termination::Watchdog(match self.kind {
            Kind::Stranded => WatchdogTermination::Stranded,
            Kind::Stalled => WatchdogTermination::Stalled {
                quiet_secs: self.quiet_secs,
            },
        })
    }
}

/// Answer and diagnostic evidence from one successful source read. None from
/// Source::evidence means unreadable, not a successful read with no answer.
pub(crate) struct Evidence<A, E = String> {
    pub answer: Option<A>,
    pub last_event: Option<E>,
}

/// A revision changes only when observed progress changes. Sources own that
/// definition; the monitor owns elapsed patience, never harness decoding.
pub(crate) trait Source {
    type Answer;
    type Event: std::fmt::Display;
    async fn revision(&mut self) -> Option<u64>;
    async fn evidence(&mut self) -> Option<Evidence<Self::Answer, Self::Event>>;
}

pub(crate) struct Detection<A, E> {
    pub kind: Kind,
    pub evidence: Evidence<A, E>,
    pub quiet_secs: u64,
}

pub(crate) async fn monitor<S: Source>(
    source: &mut S,
    timings: Timings,
) -> Detection<S::Answer, S::Event> {
    let mut last_revision = 0;
    let mut last_change = Instant::now();
    loop {
        tokio::time::sleep(timings.poll_interval).await;
        let Some(revision) = source.revision().await else {
            continue;
        };
        if revision != last_revision {
            last_revision = revision;
            last_change = Instant::now();
            continue;
        }
        let quiet = last_change.elapsed();
        let stall_due = timings.stall_grace.is_some_and(|grace| quiet >= grace);
        if quiet < timings.quiet_grace && !stall_due {
            continue;
        }
        let Some(evidence) = source.evidence().await else {
            continue;
        };
        let kind = if evidence.answer.is_some() && quiet >= timings.quiet_grace {
            Kind::Stranded
        } else if evidence.answer.is_none() && stall_due {
            Kind::Stalled
        } else {
            continue;
        };
        return Detection {
            kind,
            evidence,
            quiet_secs: quiet.as_secs(),
        };
    }
}

// Preserve the established caller seam; Codex owns rollout IO and verdict prose.
pub use crate::harness::codex::watch::wait_for_stranded_completion;

#[cfg(test)]
mod tests {
    use super::*;

    struct Probe {
        revisions: std::collections::VecDeque<Option<u64>>,
        answers: std::collections::VecDeque<Option<Evidence<String>>>,
        evidence_reads: usize,
    }
    impl Source for Probe {
        type Answer = String;
        type Event = String;
        async fn revision(&mut self) -> Option<u64> {
            self.revisions.pop_front().unwrap_or(Some(4))
        }
        async fn evidence(&mut self) -> Option<Evidence<Self::Answer, Self::Event>> {
            self.evidence_reads += 1;
            self.answers
                .pop_front()
                .expect("finite observation fixture")
        }
    }

    #[tokio::test]
    async fn monitor_skips_unavailable_evidence_and_preserves_licensed_answer() {
        let mut probe = Probe {
            revisions: [None, Some(4), Some(3), Some(4)].into(),
            answers: [
                None,
                Some(Evidence {
                    answer: Some("licensed".into()),
                    last_event: Some("final".into()),
                }),
            ]
            .into(),
            evidence_reads: 0,
        };
        let timings = Timings {
            poll_interval: Duration::from_millis(1),
            quiet_grace: Duration::ZERO,
            stall_grace: Some(Duration::ZERO),
        };
        let detected = tokio::time::timeout(Duration::from_secs(1), monitor(&mut probe, timings))
            .await
            .expect("monitor resolves");
        assert!(detected.kind == Kind::Stranded);
        assert_eq!(detected.evidence.answer.as_deref(), Some("licensed"));
        assert_eq!(probe.evidence_reads, 2);
    }

    #[tokio::test]
    async fn monitor_stalls_only_after_a_successful_unanswered_read() {
        assert!(Timings::default().stall_grace.is_none());
        let mut probe = Probe {
            revisions: [Some(4)].into(),
            answers: [Some(Evidence {
                answer: None,
                last_event: None,
            })]
            .into(),
            evidence_reads: 0,
        };
        let timings = Timings {
            poll_interval: Duration::from_millis(1),
            quiet_grace: Duration::ZERO,
            stall_grace: Some(Duration::ZERO),
        };
        let detected = tokio::time::timeout(Duration::from_secs(1), monitor(&mut probe, timings))
            .await
            .expect("monitor resolves");
        assert!(detected.kind == Kind::Stalled);
        assert_eq!(probe.evidence_reads, 1);
    }
}
