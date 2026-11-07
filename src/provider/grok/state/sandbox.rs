use std::collections::HashMap;
use std::path::Path;

use super::{Grok, has_symlink_component};

impl Grok {
    /// Make sure profile `name` exists in `<home>/sandbox.toml` granting exactly
    /// `roots`, returning whether this call created it. Locked because a
    /// fan-out launches several grok runs at once and two creating the same
    /// profile unlocked would race; renamed into place so a grok starting
    /// concurrently never reads a half-written file.
    pub(in crate::provider::grok) fn ensure_profile(
        &self,
        name: &str,
        roots: &[String],
    ) -> anyhow::Result<bool> {
        let mut created = false;
        locked_update(&self.home, "sandbox.toml", |text| {
            let (new_text, c) = merge_profile(text, name, roots)?;
            created = c;
            Ok(new_text)
        })?;
        Ok(created)
    }
}

/// Locked read-modify-write of `<grok_home>/<name>`: an exclusive `flock` on
/// `<name>.lock` for the whole cycle, then a unique 0600 temp file renamed over
/// the original. `update` returns the new body, or `None` to leave the file
/// untouched.
///
/// The discipline is copied from grok's writer for `trusted_folders.toml`
/// (`trust.rs` `record_decision_strict`/`persist_doc`). Grok has no writer for
/// `sandbox.toml` at all, so for that file the lock serializes `review`
/// processes against each other - a fan-out launching several grok runs at
/// once - not against grok. The rename is what protects grok: a grok starting
/// mid-write reads either the old file or the new one.
///
/// A path through a symlink is refused: grok refuses to load its
/// trust-boundary files (`sandbox.toml` among them) through one
/// (`resolve_trust_boundary_sources`), so a profile written there could never
/// be used, and following the link would drop lock and temp files into
/// wherever it points.
fn locked_update(
    grok_home: &Path,
    name: &str,
    update: impl FnOnce(&str) -> anyhow::Result<Option<String>>,
) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    let path = grok_home.join(name);
    if has_symlink_component(&path) {
        anyhow::bail!(
            "{} is reached through a symlink - grok refuses to load it, so review will not write it",
            path.display()
        );
    }
    std::fs::create_dir_all(grok_home)?;
    let dir = grok_home.to_path_buf();
    let file_name = name.to_string();

    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.join(format!("{file_name}.lock")))?;
    // SAFETY: a valid fd owned by `lock`, which outlives the lock; the kernel
    // releases the flock when the fd closes.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        anyhow::bail!(
            "cannot lock {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }

    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let Some(new_text) = update(&text).map_err(|e| anyhow::anyhow!("{e} ({})", path.display()))?
    else {
        return Ok(());
    };
    let tmp = dir.join(format!(
        "{file_name}.review-{}",
        crate::config::generate_uuid()
    ));
    let mut f = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(new_text.as_bytes())?;
    f.sync_all()?;
    drop(f);
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// Prefix of the grok sandbox profiles `review` creates for write runs.
///
/// Grok's built-in `workspace` profile grants writes to the workspace, the grok
/// home and the temp dirs only, and grok offers no CLI flag or environment
/// variable for more: the one surface that can add a writable path is a named
/// profile in `~/.grok/sandbox.toml` (or a project `.grok/sandbox.toml`, which
/// would put a host-specific generated file into every repo). So the roots
/// `writable_roots` derives - the brokkr lock, the cargo home, a shared target -
/// reach grok through a profile `review` creates in the operator's global
/// sandbox file. Without it every grok build died at `lock: failed to open lock
/// file` before compiling, and fixers worked around it by moving `HOME` under
/// `target/`, which silently bypassed the host's build-contention lock.
///
/// **One profile per distinct root set, named for its content**
/// (`profile_name`), never one shared profile. A shared profile has to be
/// union-only (rewriting it to one run's roots would strip a concurrent run's
/// between the write and that grok starting), and union-only means a root one
/// project's `.review.toml` adds becomes writable to every later grok write run
/// on the host - broader than codex, where each run gets exactly its own roots.
/// Content-addressed profiles give grok the codex property: a run gets exactly
/// its set, a profile is created once and never modified, and concurrent runs
/// with different sets touch different entries.
pub(in crate::provider::grok) const PROFILE_PREFIX: &str = "review-ws-";

const PROFILE_BANNER: &str = "\n# Created by `review` for grok write runs whose writable roots are exactly\n\
# the list below (derived per host - brokkr lock, cargo home, shared target -\n\
# plus any profile extras). The name is a hash of the list: never edit it;\n\
# delete the profile and review recreates it. Other profiles are left alone.\n";

/// The root set in canonical form: sorted, deduplicated. The profile name and
/// the recorded roots both use it, so a resume can check one against the other.
pub(in crate::provider) fn canonical_roots(roots: &[String]) -> Vec<String> {
    let mut v = roots.to_vec();
    v.sort();
    v.dedup();
    v
}

/// The profile name for a root set: `PROFILE_PREFIX` + 16 hex digits of
/// FNV-1a over the canonical set. FNV rather than `DefaultHasher`, whose output
/// is not stable across Rust releases - a name that changed under a toolchain
/// bump would orphan every recorded session's profile. Not a security hash and
/// not meant as one: the name only has to be stable and collision-unlikely
/// across one host's handful of sets, and `merge_profile` refuses a same-named
/// profile whose contents differ, so a collision fails loudly.
pub(in crate::provider::grok) fn profile_name(roots: &[String]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for root in canonical_roots(roots) {
        for b in root.bytes().chain(std::iter::once(0)) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{PROFILE_PREFIX}{h:016x}")
}

/// Mirror of grok's `ProfileConfig` (`xai-grok-sandbox/src/profiles.rs`).
#[derive(serde::Deserialize)]
#[allow(dead_code)] // fields exist to be type-checked, as grok checks them
struct ProfileConfig {
    #[serde(default)]
    extends: Option<String>,
    #[serde(default)]
    restrict_network: Option<bool>,
    #[serde(default)]
    read_only: Vec<String>,
    #[serde(default)]
    read_write: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
}

/// Mirror of grok's `SandboxConfig`.
#[derive(serde::Deserialize)]
struct SandboxConfig {
    #[serde(default)]
    profiles: HashMap<String, ProfileConfig>,
}

/// Ensure profile `name` (`extends = "workspace"`, `read_write` = `roots`)
/// exists in a `sandbox.toml` body. Returns the new body when it had to be
/// created, and whether it was.
///
/// Create-only: an existing profile of that name is checked, never modified.
/// Its roots must be exactly `roots` and its base `workspace` (or absent, which
/// grok resolves from `workspace` - `profiles.rs` `resolve`, the `Custom` arm);
/// anything else means someone edited a content-addressed profile, or two sets
/// collided, and silently "fixing" it would rewrite the operator's config under
/// a name that no longer describes it. The result is also refused if grok
/// itself could not deserialize the whole file - see the module docs.
pub(in crate::provider::grok) fn merge_profile(
    text: &str,
    name: &str,
    roots: &[String],
) -> anyhow::Result<(Option<String>, bool)> {
    let roots = canonical_roots(roots);
    let existing: SandboxConfig = if text.trim().is_empty() {
        SandboxConfig {
            profiles: HashMap::new(),
        }
    } else {
        toml::from_str(text).map_err(|e| {
            anyhow::anyhow!(
                "grok could not load sandbox.toml (it would ignore the whole file): {e}"
            )
        })?
    };
    if let Some(p) = existing.profiles.get(name) {
        let base_ok = matches!(p.extends.as_deref(), None | Some("workspace"));
        if !base_ok || canonical_roots(&p.read_write) != roots {
            anyhow::bail!(
                "grok sandbox profile `{name}` does not match the roots it is named for - \
                 it was edited; delete it and review will recreate it"
            );
        }
        return Ok((None, false));
    }

    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| anyhow::anyhow!("cannot parse grok sandbox.toml: {e}"))?;
    let profiles_item = doc.entry("profiles").or_insert_with(|| {
        let mut t = toml_edit::Table::new();
        t.set_implicit(true);
        toml_edit::Item::Table(t)
    });
    let inline = profiles_item.is_inline_table();
    let profiles = profiles_item
        .as_table_like_mut()
        .ok_or_else(|| anyhow::anyhow!("grok sandbox.toml: `profiles` is not a table"))?;
    let mut table = toml_edit::Table::new();
    table.insert("extends", toml_edit::value("workspace"));
    let list: toml_edit::Array = roots.iter().map(String::as_str).collect();
    table.insert("read_write", toml_edit::value(list));
    // A standard table gets the banner; `profiles = { ... }` written inline can
    // only take an inline entry.
    let item = if inline {
        toml_edit::value(table.into_inline_table())
    } else {
        table.decor_mut().set_prefix(PROFILE_BANNER);
        toml_edit::Item::Table(table)
    };
    profiles.insert(name, item);
    let body = doc.to_string();
    // The gate that matters: grok deserializes the whole file and drops all of
    // it on any error, taking every profile with it.
    let _: SandboxConfig = toml::from_str(&body).map_err(|e| {
        anyhow::anyhow!("grok could not load sandbox.toml after adding `{name}`: {e}")
    })?;
    Ok((Some(body), true))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{grok_at, scratch};
    use super::*;

    fn roots(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn profile_names_are_stable_and_content_addressed() {
        let a = roots(&["/home/u/.brokkr", "/home/u/.cargo"]);
        // Order and duplicates do not change the set, so not the name.
        let a_shuffled = roots(&["/home/u/.cargo", "/home/u/.brokkr", "/home/u/.cargo"]);
        assert_eq!(profile_name(&a), profile_name(&a_shuffled));
        assert!(profile_name(&a).starts_with(PROFILE_PREFIX));
        // A different set is a different profile - no widening across sets.
        let b = roots(&["/home/u/.brokkr", "/home/u/.cargo", "/srv/fixtures"]);
        assert_ne!(profile_name(&a), profile_name(&b));
        // Pinned: the name must survive toolchain upgrades, or every recorded
        // session's profile would be orphaned. FNV-1a, not `DefaultHasher`.
        assert_eq!(profile_name(&[]), "review-ws-cbf29ce484222325");
    }

    #[test]
    fn profile_is_created_once_with_exact_roots_and_never_modified() {
        let set = roots(&["/home/u/.cargo", "/home/u/.brokkr"]);
        let name = profile_name(&set);
        let (text, created) = merge_profile("", &name, &set).expect("empty file");
        let text = text.expect("created");
        assert!(created);
        assert!(text.contains("Created by `review`"), "{text}");
        let parsed: toml::Table = text.parse().expect("valid toml");
        let p = &parsed["profiles"][name.as_str()];
        assert_eq!(p["extends"].as_str(), Some("workspace"));
        assert_eq!(
            p["read_write"].as_array().map(Vec::len),
            Some(2),
            "exactly this set"
        );

        // Already there with the same set: no write.
        assert_eq!(
            merge_profile(&text, &name, &set).expect("again"),
            (None, false)
        );

        // A second set gets its own profile; the first is untouched.
        let other = roots(&["/srv/fixtures"]);
        let (grown, _) = merge_profile(&text, &profile_name(&other), &other).expect("second");
        let grown: toml::Table = grown.expect("changed").parse().expect("valid");
        assert_eq!(
            grown["profiles"][name.as_str()]["read_write"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn edited_profiles_and_unloadable_files_are_refused() {
        let theirs =
            "# my comment\n[profiles.devbox2]\nextends = \"devbox\"\nread_write = [\"/x\"]\n";
        let set = roots(&["/a"]);
        let name = profile_name(&set);
        let (text, _) = merge_profile(theirs, &name, &set).expect("merge");
        assert!(
            text.expect("changed")
                .starts_with("# my comment\n[profiles.devbox2]"),
            "operator content survives"
        );

        // A content-addressed profile whose contents no longer match its name.
        let edited = format!("[profiles.{name}]\nextends = \"workspace\"\nread_write = [\"/b\"]\n");
        let err = merge_profile(&edited, &name, &set).expect_err("refused");
        assert!(err.to_string().contains("was edited"), "{err}");
        let rebased = format!("[profiles.{name}]\nextends = \"strict\"\nread_write = [\"/a\"]\n");
        assert!(merge_profile(&rebased, &name, &set).is_err());

        // Someone else's profile grok cannot deserialize: grok would drop the
        // whole file, ours included, so writing ours is pointless.
        let broken = "[profiles.theirs]\nread_write = \"/x\"\n";
        let err = merge_profile(broken, &name, &set).expect_err("refused");
        assert!(err.to_string().contains("ignore the whole file"), "{err}");
    }

    #[test]
    fn inline_layouts_and_a_missing_extends_are_accepted() {
        let set = roots(&["/a"]);
        let name = profile_name(&set);
        // `profiles = { ... }` inline: the new entry must be inline too.
        let inline = "profiles = { theirs = { extends = \"strict\" } }\n";
        let (text, created) = merge_profile(inline, &name, &set).expect("inline");
        assert!(created);
        let parsed: toml::Table = text.expect("changed").parse().expect("valid");
        assert_eq!(
            parsed["profiles"][name.as_str()]["extends"].as_str(),
            Some("workspace")
        );
        // Grok resolves a custom profile with no `extends` from `workspace`,
        // so one without it still matches.
        let bare = format!("[profiles.{name}]\nread_write = [\"/a\"]\n");
        assert_eq!(
            merge_profile(&bare, &name, &set).expect("no extends"),
            (None, false)
        );
    }

    #[test]
    fn ensure_profile_writes_0600_and_refuses_a_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let home = scratch();
        let grok = grok_at(&home);
        let set = roots(&["/a"]);
        let name = profile_name(&set);
        assert!(grok.ensure_profile(&name, &set).expect("write"));
        let mode = std::fs::metadata(home.join("sandbox.toml"))
            .expect("written")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(!grok.ensure_profile(&name, &set).expect("noop"));

        // Grok refuses to load a symlinked trust-boundary file, so a profile
        // written through one could never be used.
        let real = home.join("dotfiles");
        std::fs::create_dir_all(&real).expect("dotfiles dir");
        std::fs::rename(home.join("sandbox.toml"), real.join("sandbox.toml")).expect("move");
        std::os::unix::fs::symlink(real.join("sandbox.toml"), home.join("sandbox.toml"))
            .expect("link");
        let other = roots(&["/b"]);
        let err = grok
            .ensure_profile(&profile_name(&other), &other)
            .expect_err("refused");
        assert!(err.to_string().contains("symlink"), "{err}");
        let _ = std::fs::remove_dir_all(&home);
    }
}
