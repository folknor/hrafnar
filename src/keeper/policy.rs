//! Pure teardown pacing, targeting and evidence. Discovery finds targets;
//! only waitpid ECHILD establishes emptiness.
use super::Proc;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub enum Start {
    Leftover,
    Terminate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TeardownSchedule {
    pub leftover_grace: Duration,
    pub terminate_grace: Duration,
    pub kill_phase: Duration,
    pub beat: Duration,
    pub flush: Duration,
}

impl Default for TeardownSchedule {
    fn default() -> Self {
        Self {
            leftover_grace: crate::timings::LEFTOVER_GRACE,
            terminate_grace: crate::timings::SIGKILL_ESCALATION,
            kill_phase: crate::timings::KILL_PHASE,
            beat: crate::timings::TEARDOWN_BEAT,
            flush: crate::timings::KEEPER_FLUSH,
        }
    }
}

pub(super) struct Deadlines {
    pub term: Instant,
    pub leaves: Instant,
    pub kill: Instant,
    pub settle: Instant,
}

impl TeardownSchedule {
    pub(super) fn validate(&self) -> std::io::Result<()> {
        let now = Instant::now();
        if now.checked_add(self.worst_case(Start::Terminate)).is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "teardown schedule overflows clock",
            ));
        }
        Ok(())
    }

    fn grace(&self, start: Start) -> Duration {
        match start {
            Start::Leftover => self.leftover_grace,
            // A terminate received during leftover cleanup does not restart it.
            Start::Terminate => self.terminate_grace,
        }
    }

    pub(super) fn deadlines(&self, now: Instant, start: Start) -> Deadlines {
        let term = now + self.grace(start);
        let kill = term + self.kill_phase;
        Deadlines {
            term,
            leaves: term + self.kill_phase / 2,
            kill,
            settle: kill + self.settle_time(),
        }
    }

    fn settle_time(&self) -> Duration {
        self.beat.saturating_mul(2)
    }

    /// Enforced parent waiting allowance, not a guarantee against blocked /proc
    /// reads or arbitrary reporting work. Two beats tolerate transport scheduling.
    pub fn flush_bound(&self) -> Duration {
        self.flush.saturating_add(self.beat.saturating_mul(2))
    }

    pub fn worst_case(&self, start: Start) -> Duration {
        let grace = match start {
            Start::Leftover => self.leftover_grace,
            // Terminate can arrive during an already-running leftover grace.
            Start::Terminate => self.terminate_grace.max(self.leftover_grace),
        };
        grace
            .saturating_add(self.kill_phase)
            .saturating_add(self.settle_time())
            .saturating_add(self.flush_bound())
    }
}

/// Historical `killed` means observed ended during cleanup, not causal proof
/// that a signal killed it. Keep that accounting independent of the verdict.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Teardown<Processes = Vec<Proc>> {
    Clean {
        killed: Processes,
        dropped: usize,
    },
    Survived {
        killed: Processes,
        dropped: usize,
        named: Processes,
        more_unnamed: bool,
    },
    Unknown {
        killed: Processes,
        dropped: usize,
        named: Processes,
        reasons: Vec<String>,
    },
}

impl<P: Default> Default for Teardown<P> {
    fn default() -> Self {
        Self::Unknown {
            killed: P::default(),
            dropped: 0,
            named: P::default(),
            reasons: vec!["the teardown was not observed".into()],
        }
    }
}

impl<P> Teardown<P> {
    /// One verdict shape, with lists live and counts on the wire. Process events
    /// are the wire's identity authority; only the payload representation changes.
    pub fn map_processes<Q>(
        self,
        killed_map: impl FnOnce(P) -> Q,
        named_map: impl FnOnce(P) -> Q,
    ) -> Teardown<Q> {
        match self {
            Self::Clean { killed, dropped } => Teardown::Clean {
                killed: killed_map(killed),
                dropped,
            },
            Self::Survived {
                killed,
                dropped,
                named,
                more_unnamed,
            } => Teardown::Survived {
                killed: killed_map(killed),
                dropped,
                named: named_map(named),
                more_unnamed,
            },
            Self::Unknown {
                killed,
                dropped,
                named,
                reasons,
            } => Teardown::Unknown {
                killed: killed_map(killed),
                dropped,
                named: named_map(named),
                reasons,
            },
        }
    }
}

impl Teardown<usize> {
    pub fn process_counts(&self) -> (usize, usize, usize) {
        match self {
            Self::Clean { killed, dropped } => (*killed, 0, *dropped),
            Self::Survived {
                killed,
                dropped,
                named,
                ..
            }
            | Self::Unknown {
                killed,
                dropped,
                named,
                ..
            } => (*killed, *named, *dropped),
        }
    }
}

impl Teardown {
    pub fn observed(empty: bool, killed: Vec<Proc>, dropped: usize, named: Vec<Proc>) -> Self {
        if empty && !named.is_empty() {
            Self::Unknown {
                killed,
                dropped,
                named,
                reasons: vec!["contradictory empty-tree and survivor evidence".into()],
            }
        } else if empty {
            Self::Clean { killed, dropped }
        } else {
            // Even a successful discovery walk cannot prove exhaustive membership:
            // children can fork or disappear between reads. ECHILD alone is proof.
            Self::Survived {
                killed,
                dropped,
                named,
                more_unnamed: true,
            }
        }
    }

    pub fn valid(&self) -> bool {
        let named = self.named();
        if named.iter().any(|p| p.pid == 0) {
            return false;
        }
        match self {
            Self::Clean { .. } => true,
            Self::Survived { more_unnamed, .. } => !named.is_empty() || *more_unnamed,
            Self::Unknown { reasons, .. } => !reasons.is_empty(),
        }
    }

    pub fn named(&self) -> &[Proc] {
        match self {
            Self::Clean { .. } => &[],
            Self::Survived { named, .. } | Self::Unknown { named, .. } => named,
        }
    }

    pub fn accounting(&self) -> (&[Proc], usize) {
        match self {
            Self::Clean { killed, dropped }
            | Self::Survived {
                killed, dropped, ..
            }
            | Self::Unknown {
                killed, dropped, ..
            } => (killed, *dropped),
        }
    }

    /// Join independent invocations (wakes, probes). Repeated PID numbers may
    /// be reuse; retain both records. Uniqueness is checked only on one wire report.
    pub fn merge(&mut self, other: Self) {
        let take = |value: Self| match value {
            Self::Clean { killed, dropped } => {
                (killed, dropped, Vec::new(), false, Vec::new(), false)
            }
            Self::Survived {
                killed,
                dropped,
                named,
                more_unnamed,
            } => (killed, dropped, named, more_unnamed, Vec::new(), false),
            Self::Unknown {
                killed,
                dropped,
                named,
                reasons,
            } => (killed, dropped, named, false, reasons, true),
        };
        let (mut killed, dropped, mut named, more, mut reasons, unknown) =
            take(std::mem::take(self));
        let (k, d, n, m, r, u) = take(other);
        killed.extend(k);
        named.extend(n);
        reasons.extend(r);
        *self = if unknown || u {
            Self::Unknown {
                killed,
                dropped: dropped.saturating_add(d),
                named,
                reasons,
            }
        } else if !named.is_empty() || more || m {
            Self::Survived {
                killed,
                dropped: dropped.saturating_add(d),
                named,
                more_unnamed: more || m,
            }
        } else {
            Self::Clean {
                killed,
                dropped: dropped.saturating_add(d),
            }
        };
    }
}

/// Snapshot edges are only between admitted identities. The caller owns that
/// proof; a pid number alone must never be admitted by this policy.
pub(super) fn kill_targets(edges: &[(u32, u32)], leaves_first: bool) -> Vec<u32> {
    let all = || edges.iter().map(|(pid, _)| *pid).collect();
    if !leaves_first {
        return all();
    }
    let parents: std::collections::BTreeSet<_> = edges.iter().map(|(_, p)| *p).collect();
    let leaves: Vec<_> = edges
        .iter()
        .filter(|(pid, _)| !parents.contains(pid))
        .map(|(pid, _)| *pid)
        .collect();
    if leaves.is_empty() { all() } else { leaves }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn synthetic_tree_targets_leaves_then_every_member() {
        let tree = [(1, 0), (2, 1), (3, 2), (4, 1)];
        assert_eq!(kill_targets(&tree, true), vec![3, 4]);
        assert_eq!(kill_targets(&tree, false), vec![1, 2, 3, 4]);
        assert_eq!(kill_targets(&[(1, 2), (2, 1)], true), vec![1, 2]);
    }
    #[test]
    fn only_kernel_emptiness_is_clean() {
        assert!(matches!(
            Teardown::observed(true, vec![], 0, vec![]),
            Teardown::Clean { .. }
        ));
        assert!(matches!(
            Teardown::observed(false, vec![], 0, vec![]),
            Teardown::Survived {
                more_unnamed: true,
                ..
            }
        ));
    }
    #[test]
    fn independent_invocations_keep_reused_pids_and_all_unknown_reasons() {
        let proc_ = Proc {
            pid: 7,
            cmdline: "worker".into(),
            truncated: false,
        };
        let mut verdict = Teardown::Unknown {
            killed: vec![],
            dropped: 0,
            named: vec![proc_.clone()],
            reasons: vec!["probe".into()],
        };
        verdict.merge(Teardown::Unknown {
            killed: vec![],
            dropped: 0,
            named: vec![proc_],
            reasons: vec!["wake".into()],
        });
        assert!(verdict.valid());
        assert_eq!(verdict.named().len(), 2);
        assert!(
            matches!(verdict, Teardown::Unknown { reasons, .. } if reasons == ["probe", "wake"])
        );
    }

    #[test]
    fn schedule_execution_fits_its_waiting_allowance() {
        let schedule = TeardownSchedule {
            leftover_grace: Duration::from_secs(20),
            ..TeardownSchedule::default()
        };
        let now = Instant::now();
        for start in [Start::Leftover, Start::Terminate] {
            let d = schedule.deadlines(now, start);
            assert!(d.term <= d.leaves && d.leaves <= d.kill && d.kill <= d.settle);
            assert!(
                d.settle.duration_since(now) + schedule.flush_bound() <= schedule.worst_case(start)
            );
        }
    }
}
