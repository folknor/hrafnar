//! Plain-text teardown reporting, independent of lifecycle bookkeeping.

use crate::supervisor::Unresolved;

/// The closing report: what is still running, or may be. Empty when every
/// teardown was clean. Plain lines: printed to stdout above the `runtime:`
/// line, or to stderr by the signal supervisor on its way out. Survivors and
/// unknown outcomes are printed, every survivor named, because they are what
/// the operator must check before retrying; leftovers the keeper killed
/// cleanly are recorded, never printed.
pub fn report_lines(entries: &[Unresolved]) -> Vec<String> {
    let mut out = Vec::new();
    for e in entries {
        let l = &e.leftovers;
        if let Some(why) = &l.unknown {
            out.push(format!(
                "teardown outcome unknown: {} (keeper pid {}) - {why}; processes it started \
                 may still be running",
                e.label, e.keeper_pid
            ));
            if !l.survivors.is_empty() {
                out.push("  known to have survived:".to_string());
            }
            for p in &l.survivors {
                out.push(format!(
                    "  pid {}  {}{}",
                    p.pid,
                    p.cmdline,
                    if p.truncated { " ..." } else { "" }
                ));
            }
            continue;
        }
        let n = l.survivors.len();
        out.push(format!(
            "left running: {}{} process{} {} started survived teardown",
            if l.incomplete { "at least " } else { "" },
            n.max(1),
            if n.max(1) == 1 { "" } else { "es" },
            e.label
        ));
        for p in &l.survivors {
            out.push(format!(
                "  pid {}  {}{}",
                p.pid,
                p.cmdline,
                if p.truncated { " ..." } else { "" }
            ));
        }
        if l.incomplete {
            out.push("  (more may remain that could not be identified)".to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{contained::Leftovers, keeper::Proc};

    fn proc_(pid: u32, cmdline: &str) -> Proc {
        Proc {
            pid,
            cmdline: cmdline.to_string(),
            truncated: false,
        }
    }

    fn entry(leftovers: Leftovers) -> Unresolved {
        Unresolved {
            label: "the codex run of session s".to_string(),
            keeper_pid: 7,
            leftovers,
        }
    }

    #[test]
    fn cleanly_killed_leftovers_report_nothing() {
        let killed = Leftovers {
            killed: vec![proc_(1, "sleep 600")],
            ..Leftovers::default()
        };
        assert!(killed.is_clean());
        assert!(!killed.is_empty(), "but they are recorded");
    }

    #[test]
    fn survivors_are_named_one_per_line() {
        let lines = report_lines(&[entry(Leftovers {
            survivors: vec![proc_(41233, "python3 a.py"), proc_(41240, "python3 b.py")],
            ..Leftovers::default()
        })]);
        assert_eq!(
            lines,
            vec![
                "left running: 2 processes the codex run of session s started survived teardown",
                "  pid 41233  python3 a.py",
                "  pid 41240  python3 b.py",
            ]
        );
    }

    #[test]
    fn an_incomplete_enumeration_is_a_lower_bound_not_a_count() {
        let lines = report_lines(&[entry(Leftovers {
            incomplete: true,
            ..Leftovers::default()
        })]);
        assert!(
            lines[0].starts_with("left running: at least 1 process "),
            "{lines:?}"
        );
        assert_eq!(lines[1], "  (more may remain that could not be identified)");
    }

    #[test]
    fn an_unknown_outcome_never_reads_as_clean() {
        let unknown = Leftovers {
            unknown: Some("the keeper exited abnormally".to_string()),
            ..Leftovers::default()
        };
        assert!(!unknown.is_clean());
        let lines = report_lines(&[entry(unknown)]);
        assert!(
            lines[0].starts_with("teardown outcome unknown:"),
            "{lines:?}"
        );
    }

    #[test]
    fn an_unknown_outcome_still_names_the_survivors_it_knew() {
        let lines = report_lines(&[entry(Leftovers {
            survivors: vec![proc_(9, "python3 hung.py")],
            unknown: Some("the keeper's report was cut off".to_string()),
            ..Leftovers::default()
        })]);
        assert!(
            lines[0].starts_with("teardown outcome unknown:"),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("pid 9  python3 hung.py")),
            "{lines:?}"
        );
    }
}
