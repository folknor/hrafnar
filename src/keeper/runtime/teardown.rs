//! Tree membership, reaping rounds, and bounded teardown.

use std::time::{Duration, Instant};

use super::super::process::{children_of, read_cmdline};
use super::super::protocol::{Event, KILLED_MAX, Outcome, Proc};
use super::{Keeper, Member};
use crate::process::{pidfd_exited, pidfd_open, pidfd_signal, stat_ppid_start};

impl Keeper {
    /// Walk the tree from the keeper down, admitting what can be proven ours.
    /// Returns whether every process visited had readable children. Stops at
    /// `deadline`, incomplete, so a large or growing tree cannot hold a round
    /// past the teardown's bound.
    fn discover(&mut self, deadline: Instant) -> bool {
        // Members that died since the last look are recorded, not dropped:
        // a process that dies between two rounds was still ended by us.
        self.account_dead();
        self.refresh_cmdlines(deadline);
        let mut complete = true;
        let mut stack = vec![self.me];
        let mut seen = std::collections::BTreeSet::new();
        while let Some(parent) = stack.pop() {
            if Instant::now() >= deadline {
                return false;
            }
            if !seen.insert(parent) {
                continue;
            }
            let children = match children_of(parent, deadline) {
                Ok(c) => c,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            for child in children {
                if Instant::now() >= deadline {
                    return false;
                }
                if !self.members.contains_key(&child) && !self.admit(child, parent) {
                    continue;
                }
                stack.push(child);
            }
        }
        complete
    }

    fn admit(&mut self, child: u32, parent: u32) -> bool {
        let Ok(pidfd) = pidfd_open(child) else {
            return false;
        };
        let Some((ppid, _)) = stat_ppid_start(child) else {
            return false;
        };
        if ppid != parent || pidfd_exited(&pidfd) {
            return false;
        }
        // The keeper is alive by definition; any other parent must still be
        // the admitted process, alive after the read.
        if parent != self.me
            && !self
                .members
                .get(&parent)
                .is_some_and(|p| !pidfd_exited(&p.pidfd))
        {
            return false;
        }
        let (proc_, _) = read_cmdline(child);
        self.members.insert(
            child,
            Member {
                pidfd,
                proc_,
                termed: false,
            },
        );
        true
    }

    /// Re-read every member's command line, keeping a new one only if it is
    /// a real command line and the process was still alive after the read (so
    /// the read was this process's).
    fn refresh_cmdlines(&mut self, deadline: Instant) {
        for (pid, m) in &mut self.members {
            if Instant::now() >= deadline {
                return;
            }
            let (fresh, fallback) = read_cmdline(*pid);
            if (!fallback || m.proc_.cmdline.starts_with('[')) && !pidfd_exited(&m.pidfd) {
                m.proc_ = fresh;
            }
        }
    }

    fn leader_pid(&self) -> Option<u32> {
        self.leader.as_ref().map(|(p, _)| *p)
    }

    /// Note members that have died since the last look as killed.
    fn account_dead(&mut self) {
        let leader = self.leader_pid();
        let dead: Vec<u32> = self
            .members
            .iter()
            .filter(|(_, m)| pidfd_exited(&m.pidfd))
            .map(|(pid, _)| *pid)
            .collect();
        for pid in dead {
            if let Some(m) = self.members.remove(&pid)
                && Some(pid) != leader
            {
                if self.killed.len() < KILLED_MAX {
                    self.killed.push(m.proc_);
                } else {
                    self.killed_dropped += 1;
                }
            }
        }
    }

    /// Wait for the next event, at most one beat (and never past `until`),
    /// servicing signals, control and events. Used while SIGTERMing, where
    /// waking early only means finding the newcomers sooner.
    fn beat(&mut self, until: Instant) {
        let left = until.saturating_duration_since(Instant::now());
        let beat = Duration::from_millis(self.spec.beat_ms).min(left);
        if !beat.is_zero() {
            self.service(Some(beat));
        }
    }

    /// Pause a whole beat (never past `until`), servicing and reaping all the
    /// while. Between kill rounds: the SIGCHLD from a killed leaf must not cut
    /// short the moment its parent gets to reap it and clean up. Returns true
    /// once the tree is empty.
    fn full_beat(&mut self, until: Instant) -> bool {
        let end = (Instant::now() + Duration::from_millis(self.spec.beat_ms)).min(until);
        loop {
            if self.reap() {
                return true;
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            self.service(Some(left));
        }
    }

    pub(super) fn teardown(&mut self, term_grace: Duration) -> Outcome {
        let term_until = Instant::now() + term_grace;
        let final_at = term_until + Duration::from_millis(self.spec.kill_phase_ms);
        let mut complete;
        // SIGTERM (and SIGCONT, so a stopped process can act on it) to every
        // member as it is found, until the grace runs out.
        loop {
            if self.reap() {
                self.account_dead();
                return self.outcome(true, true);
            }
            // Whether this walk was complete matters only for the final
            // verdict, which the kill rounds below decide.
            let _ = self.discover(term_until);
            for m in self.members.values_mut().filter(|m| !m.termed) {
                let _ = pidfd_signal(&m.pidfd, libc::SIGTERM);
                let _ = pidfd_signal(&m.pidfd, libc::SIGCONT);
                m.termed = true;
            }
            self.account_dead();
            if Instant::now() >= term_until {
                break;
            }
            self.beat(term_until);
        }
        // SIGKILL leaves first for the first half of the kill phase, a full
        // beat between rounds, so a supervising ancestor (codex-linux-sandbox
        // removing the placeholder files it created in the workspace) can
        // reap and clean up before its turn. Only for half: a parent that
        // forks a new child each time its last one dies is never a leaf, and
        // leaves-first alone would kill its children forever. From then on,
        // every known member each round.
        let beat = Duration::from_millis(self.spec.beat_ms);
        let leaves_until = term_until + Duration::from_millis(self.spec.kill_phase_ms) / 2;
        loop {
            if self.reap() {
                self.account_dead();
                return self.outcome(true, true);
            }
            // Discovery stops short of the deadline, so what it found can
            // still be killed in the time left.
            complete = self.discover(final_at.checked_sub(beat * 2).unwrap_or(final_at));
            self.account_dead();
            let now = Instant::now();
            if now >= final_at {
                break;
            }
            let mut targets: Vec<u32> = if now >= leaves_until {
                Vec::new()
            } else {
                let parents: std::collections::BTreeSet<u32> = self
                    .members
                    .keys()
                    .filter_map(|pid| stat_ppid_start(*pid).map(|(ppid, _)| ppid))
                    .collect();
                self.members
                    .keys()
                    .copied()
                    .filter(|pid| !parents.contains(pid))
                    .collect()
            };
            if targets.is_empty() {
                targets = self.members.keys().copied().collect();
            }
            for pid in targets {
                if let Some(m) = self.members.get(&pid) {
                    let _ = pidfd_signal(&m.pidfd, libc::SIGKILL);
                }
            }
            if self.full_beat(final_at) {
                self.account_dead();
                return self.outcome(true, true);
            }
        }
        // At the deadline: SIGKILL everything known, then keep finding and
        // killing what appears (a child forked just before its parent died
        // is now the keeper's) for two more beats, so nothing killed in the
        // last round, or orphaned by it, is reported as surviving.
        let settle_end = final_at + beat * 2;
        loop {
            for m in self.members.values() {
                let _ = pidfd_signal(&m.pidfd, libc::SIGKILL);
            }
            if self.full_beat(settle_end) {
                self.account_dead();
                return self.outcome(true, true);
            }
            if Instant::now() >= settle_end {
                break;
            }
            complete = self.discover(settle_end);
        }
        let empty = self.reap();
        self.account_dead();
        self.outcome(empty, complete)
    }

    fn outcome(&mut self, empty: bool, complete: bool) -> Outcome {
        let survivors: Vec<Proc> = if empty {
            Vec::new()
        } else {
            self.members.values().map(|m| m.proc_.clone()).collect()
        };
        for s in &survivors {
            self.queue(&Event::Survivor(s.clone()));
        }
        Outcome {
            status: self.leader_status,
            clean: empty,
            killed: std::mem::take(&mut self.killed),
            killed_dropped: self.killed_dropped,
            survivors: survivors.len(),
            incomplete: !empty && (!complete || survivors.is_empty()),
        }
    }
}
