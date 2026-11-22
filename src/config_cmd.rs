//! `review list`: what a run here can use. By default only the archetypes and
//! the profiles with an installed harness; `--projects` the `-C` registry;
//! `--all` the effective configuration and where each value came from.
//!
//! The default view is terse because whoever picks a role needs only what is
//! available; the full resolved view (file paths, sources, env names, every
//! default) buries that answer.
//!
//! `.review.toml` alone does not say which archetypes, harnesses and profiles
//! exist: a profile may come from
//! `~/.config/review/config.toml`, and which harness it runs on depends on
//! `[_defaults].prefer` and what this host has installed - so
//! this command is the single source of truth. It renders what the resolver
//! produced, i.e. exactly what a run would use; it never re-parses TOML, because
//! a second reader is a second opinion that can disagree with the one that
//! actually launches harnesses.
//!
//! Plain text only, for people and agents alike. There is deliberately no JSON
//! mode: agents read text as reliably as people do, and in practice they switch
//! a JSON default off when given one.
//!
//! Rendering is pure (`render_text` takes the resolved config and an
//! availability map), so it is testable without `PATH` or files; `run` only
//! loads, probes and prints.

use crate::config::{self, Harness, Profile, ProfileDef, ReviewConfig};
use anyhow::Result;
use std::fmt::Write as _;

/// Whether a harness's binary is on `PATH` on this host. A function rather
/// than a map, so no harness can be missing from it and read as absent.
pub type Availability<'a> = &'a dyn Fn(Harness) -> bool;

/// How much of an archetype's first prompt line the human view shows.
const PRIME_PREVIEW_CHARS: usize = 72;

/// Which view `review list` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// What can be run here: archetypes and runnable profiles.
    Terse,
    /// The `-C` registry.
    Projects,
    /// The whole resolved configuration, every value with its source.
    All,
}

/// Works outside a project too: with no `.review.toml` it shows the global
/// layer, which is what an orchestrator needs before it has picked a project.
pub fn run(sel: &config::Selection, view: View) -> Result<()> {
    let cfg = config::load_optional(sel)?;
    let avail = &crate::harness::is_available;
    print!(
        "{}",
        match view {
            View::Terse => render_list(&cfg, avail),
            View::Projects => render_projects(&cfg),
            View::All => render_text(&cfg, avail),
        }
    );
    Ok(())
}

/// The default view: what a run here can be primed with and run as. Each
/// profile shows its installed harnesses, the one a plain `-p <name>` picks
/// marked `*`, and the first line of its comment; settings are left to
/// `--all`. Profiles with no installed harness are hidden but named, so one an
/// agent was told to use does not just vanish.
pub fn render_list(cfg: &ReviewConfig, avail: Availability) -> String {
    let mut out = String::new();
    let _ = write_list(&mut out, cfg, avail);
    out
}

fn write_list(out: &mut String, cfg: &ReviewConfig, avail: Availability) -> std::fmt::Result {
    writeln!(out, "archetypes:")?;
    if cfg.archetypes.is_empty() {
        writeln!(out, "  (none)")?;
    }
    let aw = cfg.archetypes.keys().map(String::len).max().unwrap_or(0);
    for (name, s) in &cfg.archetypes {
        writeln!(out, "  {name:<aw$}  {}", prime_preview(&s.value))?;
    }

    writeln!(out)?;
    writeln!(out, "profiles:")?;
    let default = cfg.default_profile();
    // Installed entries, in `prefer` order and then `Harness` order, so the
    // line reads in the order a run would try them.
    let prefer = cfg.prefer();
    let installed = |def: &ProfileDef| -> Vec<Harness> {
        let mut hs: Vec<Harness> = def.harnesses().filter(|h| avail(*h)).collect();
        hs.sort_by_key(|h| prefer.iter().position(|p| p == h).unwrap_or(usize::MAX));
        hs
    };
    let (shown, hidden): (Vec<_>, Vec<_>) = cfg
        .profiles
        .iter()
        .partition(|&(_, def)| !installed(def).is_empty());
    if shown.is_empty() {
        writeln!(out, "  (none)")?;
    }
    // Harness column first, so the default marks line up.
    let rows: Vec<(&String, &ProfileDef, String)> = shown
        .iter()
        .map(|&(name, def)| {
            let pick = plain_pick(cfg, name, def, avail);
            let harnesses: Vec<String> = installed(def)
                .into_iter()
                .map(|h| {
                    if pick == Some(h) {
                        format!("{h}*")
                    } else {
                        h.to_string()
                    }
                })
                .collect();
            (name, def, harnesses.join(", "))
        })
        .collect();
    let pw = rows.iter().map(|(n, _, _)| n.len()).max().unwrap_or(0);
    let hw = rows.iter().map(|(_, _, h)| h.len()).max().unwrap_or(0);
    for (name, def, harnesses) in &rows {
        let line = if default == Some(name.as_str()) {
            format!("  {name:<pw$}  {harnesses:<hw$}   (default)")
        } else {
            format!("  {name:<pw$}  {harnesses}")
        };
        writeln!(out, "{line}")?;
        if let Some(line) = first_line(def.comment.as_deref()) {
            writeln!(out, "  {:pw$}  # {line}", "")?;
        }
    }
    if !hidden.is_empty() {
        let names: Vec<String> = hidden
            .iter()
            .map(|(n, def)| format!("{n} ({} not installed)", harness_names(def.harnesses())))
            .collect();
        writeln!(out, "  hidden: {}", names.join(", "))?;
    }
    match default {
        None => writeln!(out, "  no default profile: a run needs -p")?,
        Some(name) => match cfg.resolve_profile(name) {
            None => writeln!(
                out,
                "  warning: default profile '{name}' is not defined - a run without -p will fail"
            )?,
            Some(def) if installed(def).is_empty() => writeln!(
                out,
                "  warning: default profile '{name}' needs {}, none of which is installed - \
                 a run without -p will fail",
                harness_names(def.harnesses())
            )?,
            Some(def) if plain_pick(cfg, name, def, avail).is_none() => writeln!(
                out,
                "  warning: default profile '{name}' has several installed harnesses and \
                 [_defaults].prefer picks none - a run without -p needs --harness"
            )?,
            Some(_) => {}
        },
    }

    writeln!(out)?;
    writeln!(
        out,
        "steer a running agent: echo \"<new info>\" | review message <ID>; \
         stop one: review interrupt <ID>. Never kill `review` itself."
    )?;
    writeln!(out, "Everything, with sources: review list --all")?;
    Ok(())
}

/// The installed harness a plain `-p <name>` (no `--harness`) would run on,
/// from the same function the run uses; `None` when that choice is ambiguous
/// or nothing it could pick is installed.
fn plain_pick(
    cfg: &ReviewConfig,
    name: &str,
    def: &ProfileDef,
    avail: Availability,
) -> Option<Harness> {
    config::choose_harness(name, def, None, cfg.prefer(), avail)
        .ok()
        .filter(|h| avail(*h))
}

/// The first non-blank line of a comment.
fn first_line(comment: Option<&str>) -> Option<&str> {
    comment.and_then(|c| c.lines().map(str::trim).find(|l| !l.is_empty()))
}

/// `codex, claude`.
fn harness_names(harnesses: impl Iterator<Item = Harness>) -> String {
    harnesses
        .map(Harness::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// `review list --projects`: this host's `-C` registry.
pub fn render_projects(cfg: &ReviewConfig) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "projects (for -C, global [_projects.{}]):",
        config::toml_key(&cfg.hostname)
    );
    if cfg.projects.is_empty() {
        let _ = writeln!(out, "  (none)");
    }
    let nw = cfg.projects.keys().map(String::len).max().unwrap_or(0);
    for (name, path) in &cfg.projects {
        let _ = writeln!(out, "  {name:<nw$}  {path}");
    }
    out
}

/// Env var *names* only - values can carry secrets.
fn env_keys(p: &Profile) -> Vec<&str> {
    p.env
        .as_ref()
        .map(|e| e.keys().map(String::as_str).collect())
        .unwrap_or_default()
}

/// The terminal view: compact, no colours, one block per section.
pub fn render_text(cfg: &ReviewConfig, avail: Availability) -> String {
    let mut out = String::new();
    // Writing into a `String` cannot fail; the `fmt::Result` plumbing exists
    // only so the body can use `?` instead of discarding each `writeln!`.
    let _ = write_text(&mut out, cfg, avail);
    out
}

fn write_text(out: &mut String, cfg: &ReviewConfig, avail: Availability) -> std::fmt::Result {
    writeln!(out, "files:")?;
    match cfg.files().local {
        Some(ref l) => writeln!(out, "  local:   {}", l.display())?,
        None => writeln!(out, "  local:   none (not in a project; global layer only)")?,
    }
    match cfg.files().global {
        Some(ref g) if cfg.files().global_loaded => writeln!(out, "  global:  {}", g.display())?,
        Some(ref g) => writeln!(out, "  global:  {} (absent)", g.display())?,
        None => writeln!(
            out,
            "  global:  none (neither XDG_CONFIG_HOME nor HOME is usable)"
        )?,
    }
    writeln!(out, "host:      {}", cfg.hostname)?;
    match cfg.project_root() {
        Some(root) => writeln!(out, "project:   {}", root.as_path().display())?,
        None => writeln!(out, "project:   none")?,
    }

    writeln!(out)?;
    writeln!(
        out,
        "projects (for -C, global [_projects.{}]):",
        config::toml_key(&cfg.hostname)
    )?;
    if cfg.projects.is_empty() {
        writeln!(out, "  (none)")?;
    }
    let nw = cfg.projects.keys().map(String::len).max().unwrap_or(0);
    for (name, path) in &cfg.projects {
        writeln!(out, "  {name:<nw$}  {path}")?;
    }

    writeln!(out)?;
    writeln!(out, "harnesses:")?;
    // The default harness is the one a run with neither -p nor --harness
    // would launch on, when that choice resolves.
    let default = cfg.default_profile().and_then(|name| {
        cfg.resolve_profile(name)
            .and_then(|def| config::choose_harness(name, def, None, cfg.prefer(), avail).ok())
    });
    let pw = Harness::all()
        .iter()
        .map(|p| p.as_str().len())
        .max()
        .unwrap_or(0);
    let mut missing_default = None;
    for &harness in Harness::all() {
        let is_default = default == Some(harness);
        let ok = avail(harness);
        if is_default && !ok {
            missing_default = Some(harness);
        }
        let harness = harness.as_str();
        let status = if ok { "installed" } else { "NOT INSTALLED" };
        let mark = if is_default { "  default" } else { "" };
        let line = format!("  {harness:<pw$}  {status:<13}{mark}");
        writeln!(out, "{}", line.trim_end())?;
    }
    if let Some(harness) = missing_default {
        writeln!(
            out,
            "  warning: default harness '{harness}' is not installed on this host - \
             a run without -p will fail"
        )?;
    }

    writeln!(out)?;
    writeln!(out, "archetypes:")?;
    if cfg.archetypes.is_empty() {
        writeln!(out, "  (none)")?;
    }
    let aw = cfg.archetypes.keys().map(String::len).max().unwrap_or(0);
    for (name, s) in &cfg.archetypes {
        writeln!(
            out,
            "  {name:<aw$}  {:<6}  {}",
            s.layer.as_str(),
            prime_preview(&s.value)
        )?;
    }

    writeln!(out)?;
    writeln!(out, "profiles:")?;
    match cfg.profile {
        Some(ref s) => {
            let state = if cfg.resolve_profile(&s.value).is_some() {
                ""
            } else {
                ", NOT DEFINED"
            };
            writeln!(
                out,
                "  default: {}  ({} {}{state})",
                s.value,
                s.layer.as_str(),
                s.table
            )?;
        }
        None => writeln!(out, "  default: none set (a run needs -p)")?,
    }
    match cfg.prefer {
        Some(ref s) => writeln!(
            out,
            "  prefer: {}  ({})",
            if s.value.is_empty() {
                "(empty)".to_string()
            } else {
                harness_names(s.value.iter().copied())
            },
            source(s.layer.as_str(), &s.table)
        )?,
        None => writeln!(
            out,
            "  prefer: none set (a profile with several installed harnesses needs --harness)"
        )?,
    }
    if cfg.profiles.is_empty() {
        writeln!(out, "  (none)")?;
    }
    for (name, def) in &cfg.profiles {
        write_profile(out, name, def, avail)?;
    }

    writeln!(out)?;
    match cfg.stall_timeout_secs {
        Some(ref s) => writeln!(
            out,
            "stall_timeout_secs: {}  ({})",
            s.value,
            source(s.layer.as_str(), &s.table)
        )?,
        None => writeln!(out, "stall_timeout_secs: unset (built-in default)")?,
    }
    match cfg.claude_token_command {
        Some(ref command) => writeln!(
            out,
            "claude_token_command: {command}  (global [_defaults]; run after a claude run fails to authenticate, and before continuing a session that used it)"
        )?,
        None => writeln!(out, "claude_token_command: unset")?,
    }

    // Printed on every call because everyone runs `review list` before using
    // the tool, and an orchestrating agent told to "interrupt the agent" will
    // otherwise stop its own background task - killing `review`, not the turn.
    writeln!(out)?;
    writeln!(out, "{}", steering())?;
    Ok(())
}

/// The interruptible harnesses are named from `Harness::interruptible`, so a
/// new one appears here when its runner can be interrupted, and not before.
fn steering() -> String {
    format!(
        "steering a running agent:
  echo \"<new info>\" | review message <ID>   ends a {} turn in flight, then sends the message
  review interrupt <ID>                     ends the turn without a message
  Do not stop or kill the `review` process to interrupt an agent.",
        Harness::interruptible_names()
    )
}

/// One profile: its source, comment, then one block per harness entry.
fn write_profile(
    out: &mut String,
    name: &str,
    def: &ProfileDef,
    avail: Availability,
) -> std::fmt::Result {
    writeln!(
        out,
        "  {name}  ({})",
        source(def.layer.as_str(), &def.table)
    )?;
    // Each line of a multi-line comment keeps the block's indent, so it cannot
    // be mistaken for the next profile.
    if let Some(ref c) = def.comment {
        for line in c.lines().map(str::trim).filter(|l| !l.is_empty()) {
            writeln!(out, "    # {line}")?;
        }
    }
    let hw = def.harnesses().map(|h| h.as_str().len()).max().unwrap_or(0);
    for (&harness, p) in &def.entries {
        let missing = if avail(harness) {
            ""
        } else {
            "  (not installed)"
        };
        writeln!(
            out,
            "    {:<hw$}  {}{missing}",
            harness.as_str(),
            settings(harness, p)
        )?;
        // Details sit under the settings, past the harness column.
        let pad = " ".repeat(4 + hw + 2);
        if !p.writable_roots.is_empty() {
            writeln!(out, "{pad}writable_roots: {}", p.writable_roots.join(", "))?;
        }
        let keys = env_keys(p);
        if !keys.is_empty() {
            writeln!(out, "{pad}env: {}", keys.join(", "))?;
        }
        for c in &p.config {
            writeln!(out, "{pad}config: {c}")?;
        }
    }
    Ok(())
}

fn source(layer: &str, table: &str) -> String {
    format!("{layer} {table}")
}

fn settings(harness: Harness, p: &Profile) -> String {
    format!(
        "model {}, effort {}, sandbox {}",
        p.model.as_deref().unwrap_or("unset"),
        p.effort.as_deref().unwrap_or("unset"),
        sandbox_text(
            harness,
            p.sandbox.as_ref().map(crate::config::SandboxLevel::as_str)
        )
    )
}

/// The human view says what an unset sandbox *means* for the harness, because
/// "unset" alone invites the wrong guess: codex and grok runners default to
/// read-only, while claude has no filesystem sandbox on this axis at all.
///
/// Both answers come from `harness::Permissions::profile`, the function that
/// decides what a run records, so the advertised default is the recorded one.
fn sandbox_text(harness: Harness, sandbox: Option<&str>) -> String {
    match (
        crate::harness::Permissions::profile(harness, None).selector(),
        sandbox,
    ) {
        (None, Some(s)) => format!("{s} (ignored by {harness})"),
        (None, None) => format!("n/a ({harness} ignores sandbox)"),
        (Some(_), Some(s)) => s.to_string(),
        (Some(default), None) => format!("unset (runner default: {default})"),
    }
}

/// First non-blank line of a prime, truncated; `...` marks anything cut,
/// including further lines.
fn prime_preview(prime: &str) -> String {
    let mut lines = prime.lines().map(str::trim).filter(|l| !l.is_empty());
    let Some(first) = lines.next() else {
        return "(empty)".to_string();
    };
    let mut shown: String = first.chars().take(PRIME_PREVIEW_CHARS).collect();
    if lines.next().is_some() || first.chars().count() > PRIME_PREVIEW_CHARS {
        shown.push_str("...");
    }
    shown
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::{Layer, parse_file, resolve};

    const LOCAL: &str = "\
[archetypes]
security = \"You are a security expert.\\nRead the codebase.\"
goal = \"\"

[_defaults]
profile = \"deep\"

[profiles.local-tier]
comment = \"Project-only tier.\"
grok = {}
";

    const GLOBAL: &str = "\
[archetypes]
bugs = \"You hunt for edge cases.\"
security = \"global security prime\"

[_defaults]
profile = \"opus\"
stall_timeout_secs = 600
prefer = [\"codex\", \"claude\"]

[profiles.deep]
comment = \"Deep work.\"
claude = { model = \"opus-deep\" }

[profiles.deep.codex]
model = \"fresh-model\"
effort = \"high\"
sandbox = \"workspace-write\"
writable_roots = [\"/srv/data\"]
config = ['model_provider=\"x\"']
env = { SECRET_TOKEN = \"hunter2\" }

[profiles.opus]
comment = \"\"\"
Heavy reasoning.
  Use for design questions.
\"\"\"
claude = { model = \"opus\" }

[profiles.qwen]
maki = { model = \"openrouter/qwen\" }
";

    fn cfg() -> ReviewConfig {
        let local = parse_file(LOCAL, "local", Layer::Local).unwrap();
        let global = parse_file(GLOBAL, "global", Layer::Global).unwrap();
        resolve(
            local,
            Some(global),
            "host",
            config::ConfigFiles {
                local: Some("/proj/.review.toml".into()),
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn avail(codex: bool) -> impl Fn(Harness) -> bool {
        move |p| match p {
            Harness::Claude => true,
            Harness::Codex => codex,
            Harness::Grok | Harness::Maki => false,
        }
    }

    fn text(avail_codex: bool) -> String {
        render_text(&cfg(), &avail(avail_codex))
    }

    #[test]
    fn each_value_shows_the_layer_it_came_from() {
        let t = text(true);
        assert!(t.contains("project:   /proj"), "{t}");
        assert!(t.contains("host:      host"), "{t}");
        // The local archetype wins over the global one of the same name.
        assert!(t.contains("security  local"), "{t}");
        assert!(t.contains("bugs      global"), "{t}");
        assert!(!t.contains("global security prime"), "{t}");
        assert!(t.contains("default: deep  (local [_defaults])"), "{t}");
        assert!(
            t.contains("stall_timeout_secs: 600  (global [_defaults])"),
            "{t}"
        );
    }

    #[test]
    fn every_harness_entry_is_shown_with_its_settings() {
        let t = text(true);
        assert!(
            t.contains(
                "  deep  (global [profiles.deep])\n    # Deep work.\n    \
                 claude  model opus-deep, effort unset, sandbox n/a (claude ignores sandbox)\n    \
                 codex   model fresh-model, effort high, sandbox workspace-write\n            \
                 writable_roots: /srv/data\n            env: SECRET_TOKEN\n            \
                 config: model_provider=\"x\"\n"
            ),
            "{t}"
        );
        assert!(
            t.contains("  local-tier  (local [profiles.local-tier])"),
            "{t}"
        );
        assert!(
            t.contains(
                "    grok  model unset, effort unset, sandbox unset (runner default: read-only)  \
                 (not installed)"
            ),
            "{t}"
        );
        assert!(
            t.contains("  prefer: codex, claude  (global [_defaults])"),
            "{t}"
        );
        assert!(!t.contains("shadows"), "{t}");
    }

    #[test]
    fn a_profile_comment_is_shown_under_its_name() {
        let t = text(true);
        assert!(
            t.contains(
                "opus  (global [profiles.opus])\n    # Heavy reasoning.\n    \
                 # Use for design questions.\n    claude  model opus"
            ),
            "{t}"
        );
    }

    #[test]
    fn an_unset_prefer_says_what_that_means() {
        let c = resolve(
            parse_file(LOCAL, "local", Layer::Local).unwrap(),
            None,
            "host",
            config::ConfigFiles::default(),
        )
        .unwrap();
        let t = render_text(&c, &avail(true));
        assert!(t.contains("prefer: none set"), "{t}");
    }

    #[test]
    fn env_names_appear_but_values_never_do() {
        let t = text(true);
        assert!(t.contains("env: SECRET_TOKEN"), "{t}");
        assert!(!t.contains("hunter2"), "{t}");
    }

    #[test]
    fn the_default_harness_follows_prefer_and_availability() {
        let c = cfg();
        // codex is preferred and installed: it is the default.
        let text = render_text(&c, &avail(true));
        assert!(text.contains("codex   installed      default"), "{text}");
        assert!(text.contains("grok    NOT INSTALLED"), "{text}");
        assert!(!text.contains("warning"), "{text}");
        // Without codex, prefer falls through to claude.
        let text = render_text(&c, &avail(false));
        assert!(text.contains("claude  installed      default"), "{text}");
        assert!(text.contains("codex   NOT INSTALLED\n"), "{text}");
        assert!(!text.contains("warning"), "{text}");
    }

    #[test]
    fn a_default_profile_whose_only_harness_is_missing_stands_out() {
        let local = parse_file(
            "[_defaults]\nprofile = \"q\"\n[profiles.q.maki]\n",
            "l",
            Layer::Local,
        )
        .unwrap();
        let c = resolve(local, None, "host", config::ConfigFiles::default()).unwrap();
        let text = render_text(&c, &avail(true));
        assert!(text.contains("NOT INSTALLED  default"), "{text}");
        assert!(
            text.contains("default harness 'maki' is not installed"),
            "{text}"
        );
    }

    #[test]
    fn text_view_previews_primes_and_explains_unset_sandbox() {
        let text = render_text(&cfg(), &avail(true));
        // Multi-line prime: first line, marked as cut.
        assert!(text.contains("You are a security expert...."), "{text}");
        assert!(!text.contains("Read the codebase"), "{text}");
        assert!(text.contains("(empty)"), "{text}");
        assert!(
            text.contains("sandbox unset (runner default: read-only)"),
            "{text}"
        );
        assert!(text.contains("n/a (claude ignores sandbox)"), "{text}");
        assert!(
            text.contains("stall_timeout_secs: 600  (global [_defaults])"),
            "{text}"
        );
    }

    #[test]
    fn outside_a_project_the_global_layer_is_still_shown() {
        // An orchestrator asks before it has picked a project; the global
        // profiles and harness availability are the answer it needs.
        let global = parse_file(GLOBAL, "global", Layer::Global).unwrap();
        let c = resolve(
            config::ConfigFile::default(),
            Some(global),
            "host",
            config::ConfigFiles::default(),
        )
        .unwrap();
        let text = render_text(&c, &avail(true));
        assert!(text.contains("not in a project"), "{text}");
        assert!(text.contains("project:   none"), "{text}");
        assert!(text.contains("deep  (global [profiles.deep])"), "{text}");
        assert!(text.contains("model fresh-model"), "{text}");
        assert!(!text.contains("local-tier"), "{text}");
        // The global default profile's harness is the default.
        assert!(
            text.contains("default: opus  (global [_defaults])"),
            "{text}"
        );
        assert!(text.contains("claude  installed      default"), "{text}");
    }

    #[test]
    fn a_default_profile_no_layer_defines_stands_out() {
        let local = parse_file("[_defaults]\nprofile = \"ghost\"\n", "l", Layer::Local).unwrap();
        let c = resolve(local, None, "host", config::ConfigFiles::default()).unwrap();
        let text = render_text(&c, &avail(true));
        assert!(
            text.contains("default: ghost  (local [_defaults], NOT DEFINED)"),
            "{text}"
        );
    }

    #[test]
    fn the_list_shows_installed_harnesses_with_the_plain_pick_marked() {
        let t = render_list(&cfg(), &avail(true));
        // In prefer order; codex is what `-p deep` alone launches.
        assert!(
            t.contains(
                "profiles:\n  deep  codex*, claude   (default)\n        # Deep work.\n  \
                 opus  claude*\n        # Heavy reasoning.\n"
            ),
            "{t}"
        );
        assert!(
            t.contains("  hidden: local-tier (grok not installed), qwen (maki not installed)"),
            "{t}"
        );
        assert!(t.contains("security  You are a security expert...."), "{t}");
        // None of the provenance or settings the full view carries.
        for absent in [
            "[profiles.",
            "fresh-model",
            "SECRET_TOKEN",
            "stall_timeout",
            "warning",
        ] {
            assert!(!t.contains(absent), "{absent}: {t}");
        }
        assert!(t.contains("review message <ID>"), "{t}");
        assert!(t.contains("review list --all"), "{t}");
    }

    #[test]
    fn the_list_drops_uninstalled_harnesses_from_a_profile() {
        let t = render_list(&cfg(), &avail(false));
        assert!(t.contains("  deep  claude*   (default)"), "{t}");
        assert!(!t.contains("codex"), "{t}");
    }

    #[test]
    fn an_ambiguous_choice_marks_nothing_and_a_default_warns() {
        let local = parse_file(
            "[_defaults]\nprofile = \"r\"\n[profiles.r]\nclaude = {}\ncodex = {}\n",
            "l",
            Layer::Local,
        )
        .unwrap();
        let c = resolve(local, None, "host", config::ConfigFiles::default()).unwrap();
        let t = render_list(&c, &avail(true));
        assert!(t.contains("  r  claude, codex   (default)"), "{t}");
        assert!(!t.contains('*'), "{t}");
        assert!(
            t.contains("default profile 'r' has several installed harnesses"),
            "{t}"
        );
        assert!(t.contains("--harness"), "{t}");
    }

    #[test]
    fn the_list_warns_when_the_default_cannot_run() {
        let local = parse_file(
            "[_defaults]\nprofile = \"q\"\n[profiles.q.maki]\n",
            "l",
            Layer::Local,
        )
        .unwrap();
        let c = resolve(local, None, "host", config::ConfigFiles::default()).unwrap();
        let t = render_list(&c, &avail(true));
        assert!(t.contains("hidden: q (maki not installed)"), "{t}");
        assert!(
            t.contains("default profile 'q' needs maki, none of which is installed"),
            "{t}"
        );
        let local = parse_file("[_defaults]\nprofile = \"ghost\"\n", "l", Layer::Local).unwrap();
        let c = resolve(local, None, "host", config::ConfigFiles::default()).unwrap();
        let t = render_list(&c, &avail(true));
        assert!(t.contains("default profile 'ghost' is not defined"), "{t}");
        assert!(t.contains("  (none)"), "{t}");
    }

    #[test]
    fn the_projects_view_is_the_registry_alone() {
        let global =
            parse_file("[_projects.host]\nbz = \"~/bz\"\n", "global", Layer::Global).unwrap();
        let c = resolve(
            config::ConfigFile::default(),
            Some(global),
            "host",
            config::ConfigFiles::default(),
        )
        .unwrap();
        assert_eq!(
            render_projects(&c),
            "projects (for -C, global [_projects.host]):\n  bz  ~/bz\n"
        );
    }

    #[test]
    fn steering_advice_is_always_shown() {
        let t = text(true);
        assert!(t.contains("review message <ID>"), "{t}");
        assert!(
            t.contains("Do not stop or kill the `review` process"),
            "{t}"
        );
    }

    #[test]
    fn prime_preview_truncates_long_lines() {
        let long = "x".repeat(PRIME_PREVIEW_CHARS + 10);
        let shown = prime_preview(&long);
        assert_eq!(shown.chars().count(), PRIME_PREVIEW_CHARS + 3);
        assert!(shown.ends_with("..."));
        assert_eq!(prime_preview("  \n \n"), "(empty)");
        assert_eq!(prime_preview("short"), "short");
    }
}
