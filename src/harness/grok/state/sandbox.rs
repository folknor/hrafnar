use std::collections::HashMap;

use super::{Grok, has_symlink_component};
use crate::fsutil::locked_update;

impl Grok {
    /// Make sure the profile named by `roots` exists in `<home>/sandbox.toml` granting exactly
    /// `roots`, returning whether this call created it. Locked because
    /// concurrent `hrafnar` calls launch several grok runs at once and two creating the same
    /// profile unlocked would race (grok never writes `sandbox.toml`, so the
    /// lock serializes `hrafnar` against itself); renamed into place so a grok starting
    /// concurrently never reads a half-written file.
    pub(in crate::harness::grok) fn ensure_profile(&self, roots: &RootSet) -> anyhow::Result<bool> {
        let path = self.home.join("sandbox.toml");
        if has_symlink_component(&path) {
            anyhow::bail!(
                "{} is reached through a symlink - grok refuses to load it, so hrafnar will not write it",
                path.display()
            );
        }
        let mut created = false;
        locked_update(&self.home, "sandbox.toml", |text| {
            let (new_text, c) = merge_profile(text, roots)?;
            created = c;
            Ok(new_text)
        })?;
        Ok(created)
    }
}

/// Prefix of the grok sandbox profiles `hrafnar` creates for write runs.
///
/// Grok's built-in `workspace` profile grants writes to the workspace, the grok
/// home and the temp dirs only, and grok offers no CLI flag or environment
/// variable for more: the one surface that can add a writable path is a named
/// profile in `~/.grok/sandbox.toml` (or a project `.grok/sandbox.toml`, which
/// would put a host-specific generated file into every repo). So the roots
/// `writable_roots` derives - the brokkr lock, the cargo home, a shared target -
/// reach grok through a profile `hrafnar` creates in the operator's global
/// sandbox file. Without it a grok build fails at `lock: failed to open lock
/// file` before compiling, and a model working around that by moving `HOME`
/// under `target/` silently bypasses the host's build-contention lock.
///
/// **One profile per distinct root set, named for its content**
/// (`profile_name`), never one shared profile. A shared profile has to be
/// union-only (rewriting it to one run's roots would strip a concurrent run's
/// between the write and that grok starting), and union-only means a root one
/// project's `.hrafnar.toml` adds becomes writable to every later grok write run
/// on the host - broader than codex, where each run gets exactly its own roots.
/// Content-addressed profiles give grok the codex property: a run gets exactly
/// its set, a profile is created once and never modified, and concurrent runs
/// with different sets touch different entries. Nothing needs pruning; the
/// cost is one entry per distinct set in the operator's `sandbox.toml`.
pub(in crate::harness::grok) const PROFILE_PREFIX: &str = "hrafnar-ws-";

const PROFILE_BANNER: &str = "\n# Created by `hrafnar` for grok write runs whose writable roots are exactly\n\
# the list below (derived per host - brokkr lock, cargo home, shared target -\n\
# plus any profile extras). The name is a hash of the list: never edit it;\n\
# delete the profile and hrafnar recreates it. Other profiles are left alone.\n";

/// A lexical set of already selected paths, not filesystem canonicalization
/// or proof of a grant: no path expansion, resolution or host derivation.
/// The byte ordering and hash are fixed, because recorded sessions name their
/// profile by them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::harness) struct RootSet(Vec<String>);

impl RootSet {
    pub(in crate::harness) fn new(roots: &[String]) -> Self {
        let mut paths = roots.to_vec();
        paths.sort();
        paths.dedup();
        Self(paths)
    }

    pub(in crate::harness) fn paths(&self) -> &[String] {
        &self.0
    }

    /// `hrafnar-ws-` plus 16 lowercase hex digits of FNV-1a, with a NUL after
    /// each path. Never DefaultHasher, which is not stable across Rust
    /// releases: recorded profiles must survive compiler changes. A collision is
    /// refused by the create-only writer's content check.
    pub(in crate::harness) fn profile_name(&self) -> ProfileName {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for root in &self.0 {
            for b in root.bytes().chain(std::iter::once(0)) {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        }
        ProfileName(format!("{PROFILE_PREFIX}{h:016x}"))
    }
}

/// Only hrafnar-generated profile syntax; other names starting `hrafnar-ws-`
/// stay native grok profiles. Parsing a recorded name does not prove matching
/// roots; preparation checks that before the writer runs. Fresh custom
/// selectors remain opaque native names, even with this syntax.
///
/// A resume under a generated name uses the roots recorded with the session,
/// not re-derived ones, because the name pins one set and another host would
/// derive a different one. Missing recorded roots mean the empty set, never
/// the current host's roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::harness) struct ProfileName(String);

impl ProfileName {
    pub(in crate::harness) fn from_record(name: &str) -> Option<Self> {
        let suffix = name.strip_prefix(PROFILE_PREFIX)?;
        (suffix.len() == 16
            && suffix
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        .then(|| Self(name.to_string()))
    }
    pub(in crate::harness) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProfileName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
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

/// Ensure the profile named by `roots` (`extends = "workspace"`, `read_write` = `roots`)
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
pub(in crate::harness::grok) fn merge_profile(
    text: &str,
    roots: &RootSet,
) -> anyhow::Result<(Option<String>, bool)> {
    let name = roots.profile_name();
    let name = name.as_str();
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
        // The generated name binds the read_write set and workspace base,
        // not the entire effective policy. Other valid fields remain accepted.
        let base_ok = matches!(p.extends.as_deref(), None | Some("workspace"));
        if !base_ok || RootSet::new(&p.read_write) != *roots {
            anyhow::bail!(
                "grok sandbox profile `{name}` does not match the roots it is named for - \
                 it was edited; delete it and hrafnar will recreate it"
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
    let list: toml_edit::Array = roots.paths().iter().map(String::as_str).collect();
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

    fn roots(v: &[&str]) -> RootSet {
        RootSet::new(&v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>())
    }

    fn profile_name(roots: &RootSet) -> String {
        roots.profile_name().to_string()
    }

    #[test]
    fn profile_names_are_stable_and_content_addressed() {
        let a = roots(&["/home/u/.brokkr", "/home/u/.cargo"]);
        // Order and duplicates do not change the set, so not the name.
        let a_shuffled = roots(&["/home/u/.cargo", "/home/u/.brokkr", "/home/u/.cargo"]);
        assert_eq!(profile_name(&a), profile_name(&a_shuffled));
        assert_eq!(profile_name(&a), "hrafnar-ws-a4924c479efe6e4c");
        // A different set is a different profile - no widening across sets.
        let b = roots(&["/home/u/.brokkr", "/home/u/.cargo", "/srv/fixtures"]);
        assert_ne!(profile_name(&a), profile_name(&b));
        // Pinned: the name must survive toolchain upgrades, or every recorded
        // session's profile would be orphaned. FNV-1a, not `DefaultHasher`.
        assert_eq!(profile_name(&roots(&[])), "hrafnar-ws-cbf29ce484222325");
    }

    #[test]
    fn generated_name_parsing_preserves_only_the_historical_exact_shape() {
        let name = "hrafnar-ws-cbf29ce484222325";
        assert_eq!(
            ProfileName::from_record(name).expect("historical").as_str(),
            name
        );
        for native in [
            "hrafnar-ws-",
            "hrafnar-ws-mine",
            "hrafnar-ws-CBF29CE484222325",
            "hrafnar-ws-cbf29ce4842223250",
            "workspace",
        ] {
            assert!(ProfileName::from_record(native).is_none(), "{native}");
        }
    }

    #[test]
    fn profile_is_created_once_with_exact_roots_and_never_modified() {
        let set = roots(&["/home/u/.cargo", "/home/u/.brokkr"]);
        let name = profile_name(&set);
        let (text, created) = merge_profile("", &set).expect("empty file");
        let text = text.expect("created");
        assert!(created);
        assert!(text.contains("Created by `hrafnar`"), "{text}");
        let parsed: toml::Table = text.parse().expect("valid toml");
        let p = &parsed["profiles"][name.as_str()];
        assert_eq!(p["extends"].as_str(), Some("workspace"));
        assert_eq!(
            p["read_write"].as_array().map(Vec::len),
            Some(2),
            "exactly this set"
        );

        // Already there with the same set: no write.
        assert_eq!(merge_profile(&text, &set).expect("again"), (None, false));

        // A second set gets its own profile; the first is untouched.
        let other = roots(&["/srv/fixtures"]);
        let (grown, _) = merge_profile(&text, &other).expect("second");
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
        let (text, _) = merge_profile(theirs, &set).expect("merge");
        assert!(
            text.expect("changed")
                .starts_with("# my comment\n[profiles.devbox2]"),
            "operator content survives"
        );

        // A content-addressed profile whose contents no longer match its name.
        let edited = format!("[profiles.{name}]\nextends = \"workspace\"\nread_write = [\"/b\"]\n");
        let err = merge_profile(&edited, &set).expect_err("refused");
        assert!(err.to_string().contains("was edited"), "{err}");
        let rebased = format!("[profiles.{name}]\nextends = \"strict\"\nread_write = [\"/a\"]\n");
        assert!(merge_profile(&rebased, &set).is_err());

        // Someone else's profile grok cannot deserialize: grok would drop the
        // whole file, ours included, so writing ours is pointless.
        let broken = "[profiles.theirs]\nread_write = \"/x\"\n";
        let err = merge_profile(broken, &set).expect_err("refused");
        assert!(err.to_string().contains("ignore the whole file"), "{err}");
    }

    #[test]
    fn inline_layouts_and_a_missing_extends_are_accepted() {
        let set = roots(&["/a"]);
        let name = profile_name(&set);
        // `profiles = { ... }` inline: the new entry must be inline too.
        let inline = "profiles = { theirs = { extends = \"strict\" } }\n";
        let (text, created) = merge_profile(inline, &set).expect("inline");
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
            merge_profile(&bare, &set).expect("no extends"),
            (None, false)
        );
    }

    #[test]
    fn ensure_profile_writes_0600_and_refuses_a_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let home = scratch();
        let grok = grok_at(&home);
        let set = roots(&["/a"]);
        assert!(grok.ensure_profile(&set).expect("write"));
        let mode = std::fs::metadata(home.join("sandbox.toml"))
            .expect("written")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(!grok.ensure_profile(&set).expect("noop"));

        // Grok refuses to load a symlinked trust-boundary file, so a profile
        // written through one could never be used.
        let real = home.join("dotfiles");
        std::fs::create_dir_all(&real).expect("dotfiles dir");
        std::fs::rename(home.join("sandbox.toml"), real.join("sandbox.toml")).expect("move");
        std::os::unix::fs::symlink(real.join("sandbox.toml"), home.join("sandbox.toml"))
            .expect("link");
        let other = roots(&["/b"]);
        let err = grok.ensure_profile(&other).expect_err("refused");
        assert!(err.to_string().contains("symlink"), "{err}");
        let _ = std::fs::remove_dir_all(&home);
    }
}
