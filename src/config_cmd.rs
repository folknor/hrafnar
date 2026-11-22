//! `review config`: the effective configuration and where each value came from.
//!
//! Orchestrators used to parse `.review.toml` directly to learn which
//! archetypes, providers and profiles exist. With the global layer that file no
//! longer tells the whole story - a profile may come from
//! `~/.config/review/config.toml`, and a stale local one may be hiding it - so
//! this command is the single source of truth. It renders what the resolver
//! produced, i.e. exactly what a run would use; it never re-parses TOML, because
//! a second reader is a second opinion that can disagree with the one that
//! actually launches providers.
//!
//! Plain text only, for people and agents alike. There is deliberately no JSON
//! mode: agents read text as reliably as people do, and in practice they switch
//! a JSON default off when given one.
//!
//! Rendering is pure (`render_text` takes the resolved config and an
//! availability map), so it is testable without `PATH` or files; `run` only
//! loads, probes and prints.

use crate::config::{self, Profile, ProfileDef, ProfileEntry, Provider, ReviewConfig};
use anyhow::Result;
use std::fmt::Write as _;

/// Whether a provider's binary is on `PATH` on this host. A function rather
/// than a map, so no provider can be missing from it and read as absent.
pub type Availability<'a> = &'a dyn Fn(Provider) -> bool;

/// How much of an archetype's first prompt line the human view shows.
const PRIME_PREVIEW_CHARS: usize = 72;

/// Works outside a project too: with no `.review.toml` it shows the global
/// layer and provider availability, which is what an orchestrator needs before
/// it has picked a project.
pub fn run(sel: &config::Selection) -> Result<()> {
    let cfg = config::load_optional(sel)?;
    print!("{}", render_text(&cfg, &crate::provider::is_available));
    Ok(())
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
    writeln!(out, "providers:")?;
    // The default provider is the default profile's, when it resolves.
    let default = cfg
        .default_profile()
        .and_then(|name| cfg.resolve_profile(name))
        .map(|def| def.provider);
    let pw = Provider::all()
        .iter()
        .map(|p| p.as_str().len())
        .max()
        .unwrap_or(0);
    let mut missing_default = None;
    for &provider in Provider::all() {
        let is_default = default == Some(provider);
        let ok = avail(provider);
        if is_default && !ok {
            missing_default = Some(provider);
        }
        let provider = provider.as_str();
        let status = if ok { "installed" } else { "NOT INSTALLED" };
        let mark = if is_default { "  default" } else { "" };
        let line = format!("  {provider:<pw$}  {status:<13}{mark}");
        writeln!(out, "{}", line.trim_end())?;
    }
    if let Some(provider) = missing_default {
        writeln!(
            out,
            "  warning: default provider '{provider}' is not installed on this host - \
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
    if cfg.profiles.is_empty() {
        writeln!(out, "  (none)")?;
    }
    for (name, entry) in &cfg.profiles {
        write_profile(out, name, entry)?;
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

    // Printed on every call because everyone runs `review config` before using
    // the tool, and an orchestrating agent told to "interrupt the agent" will
    // otherwise stop its own background task - killing `review`, not the turn.
    writeln!(out)?;
    writeln!(out, "{}", steering())?;
    Ok(())
}

/// The interruptible providers are named from `Provider::interruptible`, so a
/// new one appears here when its runner can be interrupted, and not before.
fn steering() -> String {
    format!(
        "steering a running agent:
  echo \"<new info>\" | review message <ID>   ends a {} turn in flight, then sends the message
  review interrupt <ID>                     ends the turn without a message
  Do not stop or kill the `review` process to interrupt an agent.",
        Provider::interruptible_names()
    )
}

fn write_profile(out: &mut String, name: &str, entry: &ProfileEntry) -> std::fmt::Result {
    let def = &entry.effective;
    let provider = def.provider;
    let p = &def.profile;
    writeln!(out, "  {name}  {provider}  ({})", def_source(def))?;
    // Each line of a multi-line comment keeps the block's indent, so it cannot
    // be mistaken for the next profile.
    if let Some(ref c) = p.comment {
        for line in c.lines().map(str::trim).filter(|l| !l.is_empty()) {
            writeln!(out, "    # {line}")?;
        }
    }
    writeln!(out, "    {}", settings(provider, p))?;
    if !p.writable_roots.is_empty() {
        writeln!(out, "    writable_roots: {}", p.writable_roots.join(", "))?;
    }
    let keys = env_keys(p);
    if !keys.is_empty() {
        writeln!(out, "    env: {}", keys.join(", "))?;
    }
    for c in &p.config {
        writeln!(out, "    config: {c}")?;
    }
    for s in &entry.shadowed {
        writeln!(
            out,
            "    shadows {}: {}",
            def_source(s),
            settings(provider, &s.profile)
        )?;
    }
    Ok(())
}

/// `local [profiles.deep]`.
fn def_source(def: &ProfileDef) -> String {
    source(def.layer.as_str(), &def.table)
}

fn source(layer: &str, table: &str) -> String {
    format!("{layer} {table}")
}

fn settings(provider: Provider, p: &Profile) -> String {
    format!(
        "model {}, effort {}, sandbox {}",
        p.model.as_deref().unwrap_or("unset"),
        p.effort.as_deref().unwrap_or("unset"),
        sandbox_text(
            provider,
            p.sandbox.as_ref().map(crate::config::SandboxLevel::as_str)
        )
    )
}

/// The human view says what an unset sandbox *means* for the provider, because
/// "unset" alone invites the wrong guess: codex and grok runners default to
/// read-only, while claude has no filesystem sandbox on this axis at all.
///
/// Both answers come from `provider::Permissions::profile`, the function that
/// decides what a run records, so the advertised default is the recorded one.
fn sandbox_text(provider: Provider, sandbox: Option<&str>) -> String {
    match (
        crate::provider::Permissions::profile(provider, None).selector(),
        sandbox,
    ) {
        (None, Some(s)) => format!("{s} (ignored by {provider})"),
        (None, None) => format!("n/a ({provider} ignores sandbox)"),
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

[profiles.deep]
provider = \"codex\"
model = \"stale-model\"
effort = \"low\"
env = { SECRET_TOKEN = \"hunter2\" }
";

    const GLOBAL: &str = "\
[archetypes]
bugs = \"You hunt for edge cases.\"
security = \"global security prime\"

[_defaults]
profile = \"opus\"
stall_timeout_secs = 600

[profiles.deep]
provider = \"codex\"
comment = \"global deep comment\"
model = \"fresh-model\"
effort = \"high\"
sandbox = \"workspace-write\"
writable_roots = [\"/srv/data\"]
config = ['model_provider=\"x\"']

[profiles.opus]
provider = \"claude\"
comment = \"\"\"
Heavy reasoning.
  Use for design questions.
\"\"\"
model = \"opus\"
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

    fn avail(codex: bool) -> impl Fn(Provider) -> bool {
        move |p| match p {
            Provider::Claude => true,
            Provider::Codex => codex,
            Provider::Grok | Provider::Maki => false,
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
    fn a_stale_local_profile_is_shown_next_to_what_it_shadows() {
        let t = text(true);
        assert!(t.contains("deep  codex  (local [profiles.deep])"), "{t}");
        assert!(t.contains("model stale-model, effort low"), "{t}");
        assert!(
            t.contains(
                "shadows global [profiles.deep]: model fresh-model, effort high, \
                 sandbox workspace-write"
            ),
            "{t}"
        );
        // The winner replaces the shadowed profile whole: none of its roots or
        // overrides leak into the effective view.
        assert!(!t.contains("/srv/data"), "{t}");
        assert!(t.contains("opus  claude  (global [profiles.opus])"), "{t}");
    }

    #[test]
    fn a_profile_comment_is_shown_under_its_name() {
        let t = text(true);
        assert!(
            t.contains(
                "opus  claude  (global [profiles.opus])\n    # Heavy reasoning.\n    \
                 # Use for design questions.\n    model opus"
            ),
            "{t}"
        );
        // A shadowed definition's comment describes a profile that will not
        // run, so it stays out of the effective view.
        assert!(!t.contains("global deep comment"), "{t}");
    }

    #[test]
    fn env_names_appear_but_values_never_do() {
        let t = text(true);
        assert!(t.contains("env: SECRET_TOKEN"), "{t}");
        assert!(!t.contains("hunter2"), "{t}");
    }

    #[test]
    fn availability_is_reported_and_a_missing_default_stands_out() {
        let c = cfg();
        let text = render_text(&c, &avail(false));
        assert!(text.contains("claude  installed"), "{text}");
        assert!(text.contains("grok    NOT INSTALLED"), "{text}");
        assert!(text.contains("NOT INSTALLED  default"), "{text}");
        assert!(
            text.contains("default provider 'codex' is not installed"),
            "{text}"
        );

        // Installed default: no warning. grok is not installed but is not a
        // default, so it is listed without alarm.
        let text = render_text(&c, &avail(true));
        assert!(!text.contains("warning"), "{text}");
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
        assert!(
            text.contains("shadows global [profiles.deep]: model fresh-model"),
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
        // profiles and provider availability are the answer it needs.
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
        assert!(
            text.contains("deep  codex  (global [profiles.deep])"),
            "{text}"
        );
        assert!(text.contains("model fresh-model"), "{text}");
        // The global default profile's provider is the default.
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
