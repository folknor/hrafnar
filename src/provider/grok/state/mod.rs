//! `review`'s dealings with grok's own state directory (`$GROK_HOME`, else
//! `~/.grok`): what a `workspace-write` run needs set up there, and the event
//! log that explains a run grok reports only as `stopReason: cancelled`.
//!
//! **Folder trust** (`trusted_folders.toml`). In a folder grok does not trust,
//! every file edit raises a permission prompt, and under
//! `--permission-mode dontAsk` that prompt resolves `cancelled` - which ends the
//! whole turn rather than handing a denial back to the model. Observed on grok
//! 1.0.40: six write runs in an untrusted checkout each diagnosed their task and
//! were cancelled at their first edit. A write run therefore launches with
//! grok's own (hidden) `--trust` flag, which records the grant under grok's own
//! key and lock - `review` does not write this file. What `review` does here is
//! read it: to refuse a folder the operator explicitly declined (grok's grant
//! would overwrite that), to refuse a store grok cannot parse (grok then trusts
//! nothing, fail-closed), and to explain a cancelled turn.
//!
//! **Writable roots** (`sandbox.toml`). Grok's `workspace` profile writes only
//! to the workspace, the grok home and temp; the brokkr lock and the cargo
//! home are outside it, so every build died before compiling. The only surface
//! grok offers for more is a named profile, so `review` creates one per
//! distinct root set, named for its content (`profile_name`, `ensure_profile`)
//! and never modified after. That write copies grok's own discipline for its
//! trust store (`xai-grok-workspace/src/trust.rs`): an exclusive `flock` on
//! `<file>.lock` across the read-modify-write, `toml_edit` so other content
//! survives, a 0600 temp file renamed over the original.
//!
//! **Why a turn was cancelled** (`sessions/<encoded cwd>/<id>/events.jsonl`):
//! `cancellation_category` and the tool whose permission was refused. The
//! result object carries neither.
//!
//! The schema mirrors (`TrustDocument` in `trust`, `SandboxConfig` in `sandbox`)
//! copy grok's serde definitions field for field, because grok deserializes each
//! file **whole**: one malformed entry anywhere and grok drops the entire file.
//! For the trust store that means nothing is trusted; for the sandbox file it
//! means the global definition of `review`'s profile vanishes and a checked-out
//! repo's `.grok/sandbox.toml` may define the name instead. Validating only our
//! own table would miss both.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod background;
mod events;
mod sandbox;
mod trust;

pub(super) use background::{Unread, WakeCheck, wake_prompt};
pub(super) use events::{CancelCause, cancel_cause, session_events};
pub(in crate::provider) use sandbox::{ProfileName, RootSet};
pub(super) use trust::{Trust, trust_failures};

/// Grok's environment as the *child* will see it. A profile `env` redirects
/// only the child, so reading `review`'s own environment would set up - and
/// judge trust against - the wrong home: the `CODEX_HOME`/`data_root` trap.
#[derive(Debug, Clone)]
pub(super) struct Grok {
    /// `GROK_HOME` (profile env, then process), else `<HOME>/.grok` - grok's own
    /// resolution order (`xai-dirs`).
    pub(super) home: PathBuf,
    /// The user's home directory the child sees, canonical; grok never trusts it
    /// (`is_unsafe_trust_root`), and falls back from a git root equal to it.
    user_home: Option<PathBuf>,
}

impl Grok {
    /// Resolve from a profile `env` and the process environment. The grok home
    /// must be absolute: grok's trust store refuses a relative one and would
    /// trust nothing. (A symlink anywhere on the path to its files is refused
    /// where they are used - `check_trust`, `ensure_profile` - because grok
    /// refuses to load its trust-boundary files through one.)
    pub(super) fn resolve(env: Option<&BTreeMap<String, String>>) -> anyhow::Result<Self> {
        let var = |name: &str| {
            env.and_then(|e| e.get(name))
                .map(PathBuf::from)
                .or_else(|| std::env::var_os(name).map(PathBuf::from))
                .filter(|p| !p.as_os_str().is_empty())
        };
        let user_home = var("HOME");
        let home = match var("GROK_HOME") {
            Some(p) => p,
            None => user_home.as_ref().map(|h| h.join(".grok")).ok_or_else(|| {
                anyhow::anyhow!("cannot locate grok's home (HOME and GROK_HOME unset)")
            })?,
        };
        if !home.is_absolute() {
            anyhow::bail!(
                "GROK_HOME {} is relative - grok refuses a relative home",
                home.display()
            );
        }
        let user_home = user_home.map(|h| std::fs::canonicalize(&h).unwrap_or(h));
        Ok(Self { home, user_home })
    }
}

/// `path` or any of its ancestors is a symlink. Grok's own test
/// (`path_has_symlink_component`) for refusing trust-boundary files.
fn has_symlink_component(path: &Path) -> bool {
    path.ancestors()
        .any(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()))
}

#[cfg(test)]
mod test_support {
    use super::*;

    /// Canonicalized, because these tests are about symlinks and the only ones
    /// in play must be the ones a test plants. `target` is itself a symlink on
    /// hosts that keep the build cache on another drive, and the trust-boundary
    /// check refuses a path with *any* symlinked ancestor - so an unresolved
    /// scratch path made every store "reached through a symlink" before a test
    /// had planted one, failing the suite on those hosts only.
    pub(super) fn scratch() -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(crate::ids::generate_uuid());
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        std::fs::canonicalize(&dir).expect("canonicalize scratch dir")
    }

    /// A `Grok` rooted at a scratch home, with a user home that no test path
    /// lives under.
    pub(super) fn grok_at(home: &Path) -> Grok {
        Grok {
            home: home.to_path_buf(),
            user_home: Some(PathBuf::from("/home/nobody-here")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grok_env_is_the_childs_and_the_home_must_be_absolute() {
        let env = BTreeMap::from([
            ("GROK_HOME".to_string(), "/srv/grok".to_string()),
            ("HOME".to_string(), "/srv/someone".to_string()),
        ]);
        let grok = Grok::resolve(Some(&env)).expect("profile env");
        assert_eq!(grok.home, PathBuf::from("/srv/grok"));
        assert_eq!(grok.user_home, Some(PathBuf::from("/srv/someone")));
        let relative = BTreeMap::from([("GROK_HOME".to_string(), "grok".to_string())]);
        assert!(Grok::resolve(Some(&relative)).is_err());
    }
}
