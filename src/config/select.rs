//! Which of a profile's harness entries a run uses. Pure: availability is a
//! closure, so every branch is testable without `PATH`. The run and
//! `hrafnar list` both call this, so the harness `hrafnar list` marks is the one
//! a plain `-p <name>` launches.

use super::{Harness, ProfileDef};
use anyhow::{Result, bail};

fn names(harnesses: &[Harness]) -> String {
    harnesses
        .iter()
        .map(|h| h.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The harness a run of profile `name` launches on.
///
/// 1. `--harness` (`flag`) when given; the profile must have an entry for it.
///    It may be uninstalled: the caller reports that (a dry run only warns).
/// 2. Otherwise the first harness in `prefer` that the profile defines and
///    this host has installed.
/// 3. Otherwise the profile's one installed entry, if exactly one is.
/// 4. With none installed, a profile with a single entry still resolves to it,
///    so the caller's not-installed check reports the missing harness by name
///    (and a dry run can still preview the prompt). Anything else is an error
///    asking for `--harness` or `[_defaults].prefer`.
pub fn choose_harness(
    name: &str,
    def: &ProfileDef,
    flag: Option<Harness>,
    prefer: &[Harness],
    avail: &dyn Fn(Harness) -> bool,
) -> Result<Harness> {
    let defined: Vec<Harness> = def.harnesses().collect();
    if let Some(harness) = flag {
        if def.entries.contains_key(&harness) {
            return Ok(harness);
        }
        bail!(
            "profile '{name}' has no {harness} entry ({}.{harness}])\n  \
             Its harnesses: {}.",
            def.table.trim_end_matches(']'),
            names(&defined)
        );
    }
    if let Some(&harness) = prefer
        .iter()
        .find(|h| def.entries.contains_key(*h) && avail(**h))
    {
        return Ok(harness);
    }
    let installed: Vec<Harness> = defined.iter().copied().filter(|h| avail(*h)).collect();
    match (installed.as_slice(), defined.as_slice()) {
        ([one], _) => Ok(*one),
        ([], [only]) => Ok(*only),
        ([], _) => bail!(
            "profile '{name}': none of its harnesses ({}) is installed on this host\n  \
             Nothing was launched. Install one, or pick another profile with -p.",
            names(&defined)
        ),
        (several, _) => bail!(
            "profile '{name}' has several installed harnesses ({}) and nothing picks one\n  \
             Pass --harness <name>, or list them in order in [_defaults].prefer.",
            names(several)
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::{Layer, Profile};

    fn def(harnesses: &[Harness]) -> ProfileDef {
        ProfileDef {
            comment: None,
            entries: harnesses.iter().map(|h| (*h, Profile::default())).collect(),
            layer: Layer::Global,
            table: "[profiles.review]".into(),
        }
    }

    fn only(installed: &'static [Harness]) -> impl Fn(Harness) -> bool {
        move |h| installed.contains(&h)
    }

    use Harness::{Claude, Codex, Grok, Maki};

    #[test]
    fn the_flag_wins_even_over_prefer_and_may_be_uninstalled() {
        let d = def(&[Codex, Claude]);
        let got = choose_harness("review", &d, Some(Claude), &[Codex], &only(&[Codex])).unwrap();
        assert_eq!(got, Claude);
    }

    #[test]
    fn a_flag_the_profile_lacks_is_refused_naming_what_it_has() {
        let d = def(&[Codex, Claude]);
        let err = choose_harness("review", &d, Some(Grok), &[], &only(&[Grok]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no grok entry"), "{err}");
        assert!(err.contains("[profiles.review.grok]"), "{err}");
        assert!(err.contains("claude, codex"), "{err}");
    }

    #[test]
    fn prefer_takes_the_first_defined_and_installed() {
        let d = def(&[Codex, Claude, Maki]);
        // grok is preferred but not defined; codex is defined but not installed.
        let got = choose_harness(
            "hrafnar",
            &d,
            None,
            &[Grok, Codex, Maki, Claude],
            &only(&[Grok, Claude, Maki]),
        )
        .unwrap();
        assert_eq!(got, Maki);
    }

    #[test]
    fn without_a_prefer_match_a_single_installed_entry_is_used() {
        let d = def(&[Codex, Claude]);
        for prefer in [&[][..], &[Grok][..], &[Codex][..]] {
            let got = choose_harness("review", &d, None, prefer, &only(&[Claude])).unwrap();
            assert_eq!(got, Claude, "{prefer:?}");
        }
    }

    #[test]
    fn several_installed_without_prefer_is_ambiguous() {
        let d = def(&[Codex, Claude, Grok]);
        let err = choose_harness("review", &d, None, &[Maki], &only(&[Codex, Claude]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("--harness"), "{err}");
        assert!(err.contains("[_defaults].prefer"), "{err}");
        assert!(err.contains("(claude, codex)"), "{err}");
        assert!(!err.contains("grok"), "{err}");
    }

    #[test]
    fn nothing_installed() {
        // One entry: resolved, so the caller names the missing harness.
        let got = choose_harness("q", &def(&[Maki]), None, &[], &only(&[])).unwrap();
        assert_eq!(got, Maki);
        let err = choose_harness("q", &def(&[Maki, Grok]), None, &[], &only(&[]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("none of its harnesses (grok, maki)"), "{err}");
    }
}
