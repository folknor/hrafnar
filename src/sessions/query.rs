//! Explicit snapshots, not an immutable process cache: messages deliberately
//! refresh after another owner records a turn. Epoch ties retain append order;
//! epochs across public/private files are historical clocks, not a total order.
use super::SessionRow;
use anyhow::{Result, bail};

pub struct Snapshot {
    pub rows: Vec<SessionRow>,
}
impl Snapshot {
    pub fn read() -> Self {
        Self {
            rows: super::read_all(),
        }
    }
    pub fn empty() -> Self {
        Self { rows: Vec::new() }
    }
    pub fn find(&self, id: &str) -> Option<Session<'_>> {
        Session::new(self.rows.iter().filter(|r| r.session_id == id).collect())
    }
    pub fn sessions(&self) -> Vec<Session<'_>> {
        let mut groups: std::collections::BTreeMap<&str, Vec<&SessionRow>> =
            std::collections::BTreeMap::new();
        for row in &self.rows {
            groups.entry(&row.session_id).or_default().push(row);
        }
        groups.into_values().filter_map(Session::new).collect()
    }
    pub fn retain_project(&mut self, root: &std::path::Path) {
        self.rows
            .retain(|r| crate::project::same_project(&r.project, root));
    }
    pub fn latest(&self, id: &str) -> Option<&SessionRow> {
        self.find(id).map(|s| s.latest())
    }
    pub fn require(&self, id: &str) -> Result<Session<'_>> {
        self.find(id).ok_or_else(|| not_found(id))
    }
}
pub struct Session<'a> {
    pub rows: Vec<&'a SessionRow>,
}
impl<'a> Session<'a> {
    fn new(mut rows: Vec<&'a SessionRow>) -> Option<Self> {
        rows.sort_by_key(|r| r.epoch_secs);
        (!rows.is_empty()).then_some(Self { rows })
    }
    pub fn opener(&self) -> &'a SessionRow {
        self.rows[0]
    }
    pub fn latest(&self) -> &'a SessionRow {
        self.rows.last().expect("nonempty session query")
    }
}
pub fn not_found(id: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "no record of session {id} on this host\n  `review sessions` lists the sessions this host's review runs created."
    )
}
pub fn assert_project(
    id: &str,
    recorded: &str,
    selected: &std::path::Path,
    selector: &str,
) -> Result<()> {
    if !crate::project::same_project(recorded, selected) {
        bail!(
            "session {id} belongs to {recorded}, not {} (-C {selector})",
            selected.display()
        );
    }
    Ok(())
}
pub struct Observation {
    pub live: Vec<crate::inflight::Live>,
    pub queued: Vec<String>,
}
pub enum State<'a> {
    InFlight(&'a crate::inflight::Live),
    CleanupPending(&'a crate::inflight::Live),
    HandoffPending(&'a crate::inflight::Live),
    /// Directory scanning is not atomic. Multiple observed live markers are
    /// ambiguous evidence, not proof that two harnesses actually overlapped.
    Ambiguous,
    Queued(Option<&'a SessionRow>),
    Idle(&'a SessionRow),
    Unknown,
}
impl Observation {
    pub fn capture(data_root: Option<&std::path::Path>) -> Self {
        Self {
            live: crate::inflight::read_live(data_root),
            queued: crate::inflight::pending_launches(data_root),
        }
    }
    pub fn state<'a>(&'a self, id: &str, snapshot: &'a Snapshot) -> State<'a> {
        let matches: Vec<_> = self.live.iter().filter(|m| m.session_id == id).collect();
        let selected = match matches.as_slice() {
            [] => None,
            [only] => Some(*only),
            [a, b] if follows(a, b) => Some(*b),
            [a, b] if follows(b, a) => Some(*a),
            _ => return State::Ambiguous,
        };
        if let Some(m) = selected {
            return if m.cleanup_pending() {
                State::CleanupPending(m)
            } else if m.phase == crate::inflight::Phase::HandoffPending {
                State::HandoffPending(m)
            } else {
                State::InFlight(m)
            };
        }
        let row = snapshot.latest(id);
        if self.queued.iter().any(|s| s == id) {
            State::Queued(row)
        } else {
            row.map_or(State::Unknown, State::Idle)
        }
    }
}

// Exactly one automatic recovery is supported. This is target selection, not
// outcome merging: a waiter already holding the predecessor stays on it. The
// successor is selected only from exactly two markers that match here; any
// other set of live markers for a session is ambiguous.
fn follows(previous: &crate::inflight::Live, next: &crate::inflight::Live) -> bool {
    previous.phase == crate::inflight::Phase::HandoffPending
        && !previous.cleanup_pending()
        && !next.cleanup_pending()
        && previous.predecessor.is_none()
        && previous.run_id != next.run_id
        && crate::sessions::handoff::RunId::try_from(previous.run_id.clone()).is_ok()
        && crate::sessions::handoff::RunId::try_from(next.run_id.clone()).is_ok()
        && next
            .predecessor
            .as_ref()
            .is_some_and(|id| id.as_str() == previous.run_id)
        && previous.owner == next.owner
        && previous.session_id == next.session_id
        && previous.harness == next.harness
        && previous.project == next.project
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(epoch: Option<u64>, prompt: &str) -> SessionRow {
        serde_json::from_value(serde_json::json!({
            "timestamp": "t", "epoch_secs": epoch, "project": "/p", "hostname": "h",
            "audit_id": "a", "provider": "codex", "archetype": "bare", "session_id": "s",
            "operator_prompt": prompt, "assembled_prompt": prompt, "review_version": "0",
        }))
        .expect("row")
    }
    #[test]
    fn only_explicit_recovery_can_supersede_a_pending_invocation() {
        let first: crate::inflight::Marker = serde_json::from_value(serde_json::json!({
            "session_id": "s", "provider": "codex", "project": "/p", "started_epoch": 1,
            "pid": std::process::id(), "run_id": "first", "protocol": 1, "phase": "handoff_pending",
        }))
        .expect("marker");
        let first = crate::inflight::Live {
            marker: first,
            state: crate::inflight::RunState::Running,
        };
        let mut second = first.clone();
        second.marker.run_id = "second".into();
        second.marker.predecessor = Some("first".to_owned().try_into().expect("run id"));
        let snapshot = Snapshot::empty();
        for phase in [
            crate::inflight::Phase::Running,
            crate::inflight::Phase::HandoffPending,
        ] {
            second.marker.phase = phase;
            for live in [
                vec![first.clone(), second.clone()],
                vec![second.clone(), first.clone()],
            ] {
                let observation = Observation {
                    live,
                    queued: Vec::new(),
                };
                match observation.state("s", &snapshot) {
                    State::InFlight(m) | State::HandoffPending(m) => assert_eq!(m.run_id, "second"),
                    _ => panic!("explicit recovery should be selected"),
                }
            }
        }
        let rejects = |a: crate::inflight::Live, b: crate::inflight::Live| {
            assert!(matches!(
                Observation {
                    live: vec![a, b],
                    queued: Vec::new()
                }
                .state("s", &snapshot),
                State::Ambiguous
            ));
        };
        let mut changed = second.clone();
        changed.marker.predecessor = None;
        rejects(first.clone(), changed);
        let mut changed = first.clone();
        changed.marker.phase = crate::inflight::Phase::Running;
        rejects(changed, second.clone());
        let mut changed = second.clone();
        changed.marker.project = "/other".into();
        rejects(first.clone(), changed);
        let mut changed = second.clone();
        changed.marker.harness = "claude".into();
        rejects(first.clone(), changed);
        let mut changed = second.clone();
        changed.marker.owner = crate::process::ProcessIdentity::recorded(1, None).expect("owner");
        rejects(first.clone(), changed);
        let mut changed = first.clone();
        changed.marker.predecessor = Some("second".to_owned().try_into().expect("id"));
        rejects(changed, second.clone());
        let mut changed = second.clone();
        changed.marker.predecessor = Some("second".to_owned().try_into().expect("id"));
        rejects(first.clone(), changed);
        let mut changed = first.clone();
        changed.state = crate::inflight::RunState::CleanupPending;
        rejects(changed, second.clone());
        assert!(matches!(
            Observation {
                live: vec![first.clone(), second, first],
                queued: Vec::new()
            }
            .state("s", &snapshot),
            State::Ambiguous
        ));
    }

    #[test]
    fn queries_share_stable_epoch_order_and_state_precedence() {
        let snapshot = Snapshot {
            rows: vec![
                row(Some(7), "first"),
                row(None, "old"),
                row(Some(7), "last"),
            ],
        };
        assert_eq!(
            snapshot
                .require("s")
                .expect("session")
                .opener()
                .operator_prompt,
            "old"
        );
        assert_eq!(
            snapshot.latest("s").expect("latest").operator_prompt,
            "last"
        );
        assert_eq!(snapshot.sessions()[0].latest().operator_prompt, "last");
        assert!(snapshot.require("missing").is_err());
        let marker: crate::inflight::Marker = serde_json::from_value(serde_json::json!({
            "session_id": "s", "provider": "codex", "project": "/p", "started_epoch": 1,
            "pid": std::process::id(), "run_id": "run", "protocol": 1,
        }))
        .expect("marker");
        let live = crate::inflight::Live {
            marker,
            state: crate::inflight::RunState::Running,
        };
        let mut observation = Observation {
            live: vec![],
            queued: vec![],
        };
        assert!(matches!(observation.state("s", &snapshot), State::Idle(_)));
        assert!(matches!(
            observation.state("unknown", &snapshot),
            State::Unknown
        ));
        observation.queued.push("s".into());
        assert!(matches!(
            observation.state("s", &snapshot),
            State::Queued(Some(_))
        ));
        observation.live.push(live.clone());
        assert!(matches!(
            observation.state("s", &snapshot),
            State::InFlight(_)
        ));
        observation.live[0].state = crate::inflight::RunState::CleanupPending;
        assert!(matches!(
            observation.state("s", &snapshot),
            State::CleanupPending(_)
        ));
        observation.live.push(live);
        assert!(matches!(
            observation.state("s", &snapshot),
            State::Ambiguous
        ));
    }
}
