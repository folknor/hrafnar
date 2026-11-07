//! The `review` command itself: argument handling, the fan-out and the verbs.
//! `main.rs` only dispatches between this and the keeper.

use anyhow::{Result, bail};

use crate::cli::{self, Cli};
use crate::{
    config, config_cmd, config_write, contained, input, interrupt_cmd, lock, man, project, prompt,
    provider,
};

mod history;
mod launch;
mod message;

use history::{run_incidents, run_session_show, run_sessions};
use launch::{Launch, RecordCtx, TaskOutcome, run_launch};
use message::run_message;

/// The archetype label recorded for a run launched without one: stdin is sent
/// unchanged, with no priming prompt.
const BARE: &str = "bare";

/// Format the wall-clock runtime printed when `review` returns: sub-minute runs
/// get a decimal second (short runs differ by fractions), longer ones roll up to
/// m/s then h/m/s. `sessions::format_age` is deliberately not reused - it floors
/// anything under a minute to "now", which is most of a resume's runtime.
fn format_runtime(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        return format!("{:.1}s", d.as_secs_f64());
    }
    let (mins, rem_s) = (secs / 60, secs % 60);
    if mins < 60 {
        return format!("{mins}m{rem_s:02}s");
    }
    format!("{}h{:02}m{rem_s:02}s", mins / 60, mins % 60)
}

/// The exit-status verdict for one result: no answer, or an answer `review`
/// could not verify (`Digest::review_error`). Distinct from
/// `died_without_answer`, which decides retries: an unverified answer is a
/// failure, but resending the request would not verify it.
fn run_failed(r: &provider::ProviderResult) -> bool {
    r.output.is_err() || r.digest.as_ref().is_some_and(provider::Digest::failed)
}

/// The archetype selection: `-a`, or the positional form it replaced.
///
/// The archetype used to be the positional argument, which put archetypes and
/// subcommands in one namespace - every new subcommand silently shadowed any
/// project's archetype of that name. As a flag it cannot collide. The positional
/// form is still accepted, with a warning, so orchestration procedures written
/// against it keep working until they are updated; a positional `bare` (the old
/// way to say "no archetype") means none.
fn resolve_archetype_arg(flag: Option<&str>, legacy: Option<&str>) -> Result<Option<String>> {
    match (flag, legacy) {
        (Some(_), Some(positional)) => {
            bail!("archetype given twice: -a and a positional '{positional}'\n  Use -a only.")
        }
        (Some(a), None) => Ok(Some(a.to_string())),
        (None, Some("bare")) => {
            eprintln!("warning: `review bare` is now plain `review` - no -a means no archetype");
            Ok(None)
        }
        // The name is not echoed back: this runs before the config is loaded,
        // so a typo would be recommended verbatim (`use review -a secruity`).
        // The unknown-archetype error that follows lists what is configured.
        (None, Some(positional)) => {
            eprintln!("warning: the archetype is now a flag - use `review -a <name>`");
            Ok(Some(positional.to_string()))
        }
        (None, None) => Ok(None),
    }
}

pub async fn run() -> Result<()> {
    // Total wall clock, started before anything else runs. This deliberately
    // includes the stdin read and the global-lock wait: it is what the operator
    // waited, not just what the providers spent.
    let started = std::time::Instant::now();
    let cli = Cli::parse_checked();

    // Forward SIGINT/SIGTERM to any running codex process groups before exiting.
    // codex runs in its own process group (so we can kill it and its children as
    // a unit), which means it no longer receives a terminal Ctrl-C implicitly -
    // without this, killing `review` would leave codex running detached.
    // Installed once, process-wide, and before any provider can be spawned; see
    // `provider::install_signal_supervisor` for why it cannot be per-run.
    provider::install_signal_supervisor();

    // Refused before the registry is read, so `interrupt -C typo <ID>` reports
    // the misuse rather than a lookup failure. These commands are host-wide or
    // session-addressed with no project to assert, so accepting -C would
    // suggest a scope restriction that does not exist.
    if let Some(ref sel) = cli.project {
        let refused = match &cli.command {
            Some(cli::Command::Interrupt { .. }) => Some("review interrupt"),
            Some(cli::Command::Incidents { .. }) => Some("review incidents"),
            Some(cli::Command::Man { .. }) => Some("review man"),
            Some(cli::Command::Sessions {
                id: None,
                all: true,
                ..
            }) => Some("review sessions --all"),
            _ => None,
        };
        if let Some(what) = refused {
            bail!("-C {sel} does not apply to `{what}`, which is not scoped to a project");
        }
    }
    let selector = cli.project.as_deref();

    // Right after the arguments are validated and before anything reads the
    // environment - project, config, stdin, providers - so a broken config or a
    // vanished checkout cannot stop an agent reading how to repair it.
    if let Some(cli::Command::Man { topic }) = &cli.command {
        std::process::exit(man::run(topic.as_deref()));
    }

    if matches!(cli.command, Some(cli::Command::Init)) {
        return config::init(&project::select(selector)?);
    }

    if matches!(cli.command, Some(cli::Command::Config)) {
        return config_cmd::run(&project::select(selector)?);
    }

    if let Some(cli::Command::Message { id, dry_run }) = &cli.command {
        return run_message(id, *dry_run, selector, started).await;
    }

    // Migration: `review resume` is `review message`, and runs as it - so it
    // interrupts a turn in flight and takes the session's launch lock, rather
    // than launching a second turn beside a running one.
    if let Some(cli::Command::Resume { id, dry_run }) = &cli.command {
        eprintln!("warning: `review resume <ID>` is now `review message <ID>`");
        return run_message(id, *dry_run, selector, started).await;
    }

    if let Some(cli::Command::Interrupt { id }) = &cli.command {
        return interrupt_cmd::run(id).await;
    }

    if let Some(cli::Command::Sessions { id, all, limit }) = &cli.command {
        return match id {
            Some(sid) => run_session_show(sid, selector),
            None => run_sessions(*all, *limit, selector),
        };
    }

    if let Some(cli::Command::Incidents { limit }) = &cli.command {
        return run_incidents(*limit);
    }

    // Migration: `--session <ID>` is the old spelling of `review resume <ID>`.
    // Everything that rode along with it is ignored rather than refused, so a
    // procedure written against the old form keeps working until it is updated.
    if let Some(ref session_id) = cli.session {
        eprintln!("warning: `--session <ID>` is now `review message <ID>`");
        if cli.profile.is_some() || cli.provider.is_some() {
            eprintln!(
                "warning: --profile/--provider are ignored on a resume - the session \
                 keeps its own provider and settings"
            );
        }
        return run_message(session_id, cli.dry_run, selector, started).await;
    }

    let archetype_arg =
        resolve_archetype_arg(cli.archetype.as_deref(), cli.legacy_archetype.as_deref())?;
    let archetype_arg = archetype_arg.as_deref();

    // A bare `review` - no arguments at all and nothing piped in - gets the help
    // text. Read before the config so this works outside a project too. With any
    // flag given the operator meant to run something, so a missing stdin falls
    // through to `read_stdin`'s error, which says what is missing, rather than
    // answering with help that silently ignores the flags.
    // `Cli` loses whether its defaulted `--stagger` was explicitly supplied,
    // so argv presence preserves the rule that any argument requests a run.
    let early_stdin = if archetype_arg.is_none() && std::env::args_os().len() == 1 {
        match input::read_stdin_optional()? {
            Some(s) => Some(s),
            None => {
                Cli::print_help();
                std::process::exit(2);
            }
        }
    } else {
        None
    };

    let (mut cfg, project_root) = config::load(&project::select(selector)?)?;
    let stdin_instructions = match early_stdin {
        Some(s) => s,
        None => input::read_stdin()?,
    };

    if cli.dry_run {
        eprintln!("config: {}", cfg.searched());
        eprintln!("hostname: {}", cfg.hostname);
    }

    // Resolve archetype(s) - supports "all", groups, comma-separated, or single
    // names - into (label, prime) pairs. `bare` is a synthetic no-prime selector;
    // "all" includes configured archetypes except the special `bare` entry.
    let mut archetypes_to_run: Vec<(String, String)> = Vec::new();
    match archetype_arg {
        None => archetypes_to_run.push((BARE.to_string(), String::new())),
        Some(arg) => {
            for name in arg.split(',') {
                if name == BARE {
                    // The legacy positional form has always meant this without
                    // consulting config; keep the explicit flag equivalent.
                    archetypes_to_run.push((BARE.to_string(), String::new()));
                } else if name == "all" {
                    archetypes_to_run.extend(
                        cfg.archetypes
                            .iter()
                            .filter(|(n, _)| n.as_str() != BARE)
                            .map(|(n, p)| (n.clone(), p.value.clone())),
                    );
                } else if let Some(group) = cfg.groups.get(name) {
                    for member in &group.value {
                        // Resolution checked every member exists.
                        let prime = cfg.archetype(member).unwrap_or_default();
                        archetypes_to_run.push((member.clone(), prime.to_string()));
                    }
                } else if let Some(prime) = cfg.archetype(name) {
                    archetypes_to_run.push((name.to_string(), prime.to_string()));
                } else {
                    let mut available: Vec<&str> =
                        cfg.archetypes.keys().map(String::as_str).collect();
                    available.extend(cfg.groups.keys().map(String::as_str));
                    bail!(
                        "'{name}' not found in {}\n  \
                         configured: {}",
                        cfg.searched(),
                        if available.is_empty() {
                            "(none)".to_string()
                        } else {
                            available.join(", ")
                        }
                    );
                }
            }
        }
    }

    // Deduplicate (e.g. "all" + a specific archetype, or overlapping groups)
    let mut seen = std::collections::HashSet::new();
    archetypes_to_run.retain(|(name, _)| seen.insert(name.clone()));

    if archetypes_to_run.is_empty() {
        bail!(
            "no priming archetypes configured in {}\n  \
             Use -a bare or omit -a to send stdin unchanged, or add one under [archetypes].",
            cfg.searched()
        );
    }

    // Provider list: --provider wins, otherwise [_defaults].providers from the
    // first layer that sets it.
    let providers_to_run: Vec<String> = match cli.provider.as_ref().filter(|v| !v.is_empty()) {
        Some(v) => v.clone(),
        None => match cfg.default_providers() {
            Some(p) if !p.is_empty() => p.to_vec(),
            _ => bail!(
                "no providers to run: pass --provider <name> or set [_defaults].providers \
                 (looked in {})",
                cfg.searched()
            ),
        },
    };

    for prov in &providers_to_run {
        if !config::KNOWN_PROVIDERS.contains(&prov.as_str()) {
            bail!(
                "unknown provider '{prov}'\n  supported: {}",
                config::KNOWN_PROVIDERS.join(", ")
            );
        }
    }

    // If a profile was requested, every launched provider must have it in some
    // layer. Validate up front so we fail before spawning.
    if let Some(ref profile) = cli.profile {
        for prov in &providers_to_run {
            if cfg.resolve_profile(prov, profile).is_none() {
                bail!(
                    "profile '{profile}' not defined for provider '{prov}'\n  \
                     looked for [{prov}.{profile}] and [{}.{prov}.{profile}] in {}",
                    config::toml_key(&cfg.hostname),
                    cfg.searched()
                );
            }
        }
    }

    let missing: Vec<&str> = providers_to_run
        .iter()
        .map(String::as_str)
        .filter(|p| !provider::is_available(p))
        .collect();

    // Dry run: print what would be sent and exit
    if cli.dry_run {
        if !missing.is_empty() {
            eprintln!(
                "warning: not installed on this host: {} - a real run would fail",
                missing.join(", ")
            );
        }
        for (arch_name, prime) in &archetypes_to_run {
            let prompt = prompt::assemble(prime, &stdin_instructions);
            if archetypes_to_run.len() > 1 {
                println!("=== {arch_name} ===\n");
            }
            println!("{prompt}");
            if archetypes_to_run.len() > 1 {
                println!();
            }
        }
        return Ok(());
    }

    // Every provider must be installed here, checked before anything launches.
    // With host-scoped config gone, one config reaches every machine, including
    // ones missing a harness; skipping it would quietly run a narrower fan-out
    // than was asked for, so it fails instead. A dry run launches nothing, so it
    // only warns.
    if !missing.is_empty() {
        bail!(
            "not installed on this host (not found on PATH): {}\n  \
             Nothing was launched. Install it, or choose providers with --provider.",
            missing.join(", ")
        );
    }

    // Global lock
    let lock_path = std::env::temp_dir().join("review.lock");
    let lock_file = lock::open_lock_file(&lock_path)?;
    lock::acquire_blocking(&lock_file)?;

    // Re-read under the lock: cfg was loaded before waiting, so its audit id
    // may be stale if another first run persisted one while this run queued.
    let local_config_path = cfg
        .files
        .local
        .as_deref()
        .expect("config::load always resolves a local config path");
    let audit_id = config_write::ensure_audit_id(local_config_path)?;
    cfg.audit.id = Some(audit_id.clone());

    // Codex run settings from project config: today just the stall timeout,
    // which is tunable (and disableable) because it rests on an empirical codex
    // property rather than a documented contract.
    let provider_runtime =
        provider::ProviderRuntime::from_config(cfg.stall_timeout_secs(), cfg.audit.private);

    // Spawn all providers with staggered launches to avoid rate limits
    let stagger = std::time::Duration::from_secs(cli.stagger);
    struct PendingResult {
        provider: String,
        ctx: std::sync::Arc<RecordCtx>,
        handle: tokio::task::JoinHandle<TaskOutcome>,
    }
    let mut pending: Vec<PendingResult> = Vec::new();
    let mut launch_count = 0u32;

    for (arch_name, prime) in &archetypes_to_run {
        let assembled = prompt::assemble(prime, &stdin_instructions);

        for prov_name in &providers_to_run {
            // Profile overrides (validated above to exist when --profile is set).
            let profile = cli
                .profile
                .as_ref()
                .and_then(|name| cfg.resolve_profile(prov_name, name));
            let model = profile.and_then(|p| p.model.clone());
            let effort = profile.and_then(|p| p.effort.clone());
            let sandbox = profile.and_then(|p| p.sandbox.clone());
            let env = profile.and_then(|p| p.env.clone());
            let config = profile.map(|p| p.config.clone()).unwrap_or_default();
            let profile_roots = profile
                .map(|p| p.writable_roots.clone())
                .unwrap_or_default();
            let env_keys: Vec<String> = env
                .as_ref()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();

            let launch = Launch {
                provider: prov_name.clone(),
                model,
                effort,
                sandbox,
                writable_roots: profile_roots,
                env,
                config,
                prompt: assembled.clone(),
                root: project_root.clone(),
                runtime: provider_runtime.clone(),
                delay: stagger * launch_count,
            };
            let ctx = std::sync::Arc::new(RecordCtx {
                project_root: project_root.clone(),
                private: cfg.audit.private,
                audit_id: audit_id.clone(),
                archetype: arch_name.clone(),
                env_keys,
                operator_prompt: stdin_instructions.clone(),
                prompt: assembled.clone(),
            });
            pending.push(PendingResult {
                provider: prov_name.clone(),
                ctx: std::sync::Arc::clone(&ctx),
                handle: tokio::spawn(run_launch(launch, ctx)),
            });
            launch_count += 1;
        }
    }

    // Hold the lock until every staggered launch has fired, plus one interval,
    // so a queued invocation can't start launching concurrently with our later
    // tasks. Task i launches at `stagger * i` (last at `stagger*(launch_count-1)`),
    // so we wait `stagger * launch_count` to cover the last launch and a trailing
    // gap before the next invocation's first launch.
    if launch_count > 0 && !stagger.is_zero() {
        tokio::time::sleep(stagger * launch_count).await;
    }
    drop(lock_file);

    // Collect results
    let mut results: Vec<(String, provider::ProviderResult)> = Vec::new();
    for p in pending {
        // A finished task has already recorded its runs; only a panicked one
        // (which never got the chance) is recorded here.
        let result = match p.handle.await {
            Ok(outcome) => outcome.result,
            Err(err) => {
                let result = provider::ProviderResult {
                    provider: p.provider.clone(),
                    output: Err(anyhow::anyhow!("task panicked: {err}")),
                    session_id: None,
                    digest: None,
                    completed_epoch: provider::now_epoch_secs(),
                    // A panicked task never reached a provider, so it launched
                    // with nothing; recording a sandbox here would attribute
                    // permissions to a run that never happened.
                    sandbox: None,
                    writable_roots: Vec::new(),
                    model: None,
                    effort: None,
                    served: provider::Served::default(),
                    grok_trust: None,
                    // Whatever it had launched is in the ledger, which the
                    // exit status reads.
                    leftovers: contained::Leftovers::default(),
                };
                p.ctx.record(&result);
                result
            }
        };

        results.push((p.ctx.archetype.clone(), result));
    }

    // Print results
    let multi = archetypes_to_run.len() > 1;
    let mut current_arch = "";
    for (arch_name, result) in &results {
        if multi && arch_name.as_str() != current_arch {
            if !current_arch.is_empty() {
                println!();
            }
            println!("=== {arch_name} ===\n");
            current_arch = arch_name;
        }
        provider::print_result(result);
    }
    let left_running = print_left_running();

    println!("\nruntime: {}", format_runtime(started.elapsed()));

    // A codex run that died without a final answer returns Ok (so its session
    // id + digest survive), but it is a failure for exit-code purposes - a dead
    // review that exits 0 lies to scripts and CI. Count it as failed.
    //
    // Leftovers are a separate verdict across every invocation, and outrank
    // it: one provider's answer cannot excuse another's processes still
    // running.
    let all_failed = results.iter().all(|(_, r)| run_failed(r));
    if let Some(code) = exit_status(all_failed, left_running) {
        std::process::exit(code);
    }

    Ok(())
}

/// Exit status for a finished run or message: `3` when anything it started was
/// left running or its teardown could not be confirmed, `1` when no answer was
/// produced, else none (exit 0). Leftovers win: a failed run is retried or
/// resumed, but with its processes possibly still changing the tree, the
/// first thing to do is check them, and `3` says so whatever the answers were.
fn exit_status(no_answer: bool, left_running: bool) -> Option<i32> {
    if left_running {
        Some(EXIT_LEFT_RUNNING)
    } else if no_answer {
        Some(1)
    } else {
        None
    }
}

/// See `exit_status`.
const EXIT_LEFT_RUNNING: i32 = 3;

/// Print what is still running, or may be, from every invocation this
/// `review` made (see `contained`), whichever results were reported. Returns
/// whether there was anything: exit 0 promises nothing the run started is
/// alive.
fn print_left_running() -> bool {
    let lines = contained::report_lines(&contained::unresolved());
    if lines.is_empty() {
        return false;
    }
    println!();
    for line in &lines {
        println!("{line}");
    }
    true
}

/// The project root `-C <sel>` names: a registered root as it stands, a path
/// walked up to its `.review.toml`.
fn selected_root(sel: &str) -> Result<std::path::PathBuf> {
    match project::select(Some(sel))? {
        config::Selection::Exact(dir) => Ok(dir),
        // No ReviewConfig exists on this path, so there is no `files.local`
        // value from which to take the filename for this diagnostic.
        discover => config::project_root(&discover)?.ok_or_else(|| {
            anyhow::anyhow!(
                "-C {sel}: no .review.toml at or above {}",
                discover.dir().display()
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_archetype_flag_is_taken_as_given() {
        let got = resolve_archetype_arg(Some("security,bugs"), None).expect("ok");
        assert_eq!(got.as_deref(), Some("security,bugs"));
        assert_eq!(resolve_archetype_arg(None, None).expect("ok"), None);
    }

    #[test]
    fn the_old_positional_archetype_still_works() {
        // Orchestration procedures written against `review security` keep
        // working until they are updated; they get a warning, not a failure.
        let got = resolve_archetype_arg(None, Some("security")).expect("ok");
        assert_eq!(got.as_deref(), Some("security"));
    }

    #[test]
    fn a_positional_bare_means_no_archetype() {
        // `review bare` was the old way to say "no priming".
        assert_eq!(resolve_archetype_arg(None, Some("bare")).expect("ok"), None);
    }

    #[test]
    fn an_archetype_given_both_ways_is_refused() {
        let err = resolve_archetype_arg(Some("a"), Some("b"))
            .expect_err("ambiguous")
            .to_string();
        assert!(err.contains("twice"), "{err}");
    }

    #[test]
    fn leftovers_outrank_a_missing_answer_in_the_exit_status() {
        assert_eq!(exit_status(false, false), None);
        assert_eq!(exit_status(true, false), Some(1));
        assert_eq!(exit_status(false, true), Some(3));
        assert_eq!(exit_status(true, true), Some(3));
    }
}
