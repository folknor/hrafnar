//! The `review` command itself: argument handling, the fan-out and the verbs.
//! `main.rs` only dispatches between this and the keeper.

use anyhow::{Result, bail};

use crate::cli::{self, Cli};
use crate::exit::CommandExit;
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

/// Format the wall-clock runtime printed when `review` returns: sub-minute runs
/// get a decimal second (short runs differ by fractions), longer ones roll up to
/// m/s then h/m/s. `sessions::format_age` is deliberately not reused because it
/// shows whole seconds below a minute and omits seconds at longer durations.
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

/// The exit-status verdict for one result: no turn, no answer, or an answer
/// `review` could not verify (`Outcome::failed`). Retries are decided
/// separately: an unverified answer is a failure, but resending the request
/// would not verify it.
fn run_failed(r: &provider::ProviderResult) -> bool {
    !r.output.as_ref().is_ok_and(|turn| !turn.outcome.failed())
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

pub async fn run() -> Result<CommandExit> {
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
    // `supervisor::install_signal_supervisor` for why it cannot be per-run.
    let lifecycle = crate::supervisor::Lifecycle::default();
    crate::supervisor::install_signal_supervisor(lifecycle.clone());

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

    match &cli.command {
        Some(cli::Command::Man { topic }) => return Ok(man::run(topic.as_deref())),
        Some(cli::Command::Init) => {
            config_write::init(&project::select(selector)?)?;
            return Ok(CommandExit::Success);
        }
        Some(cli::Command::Config) => {
            config_cmd::run(&project::select(selector)?)?;
            return Ok(CommandExit::Success);
        }
        Some(cli::Command::Message { id, dry_run }) => {
            return run_message(id, *dry_run, selector, started, &lifecycle).await;
        }
        Some(cli::Command::Resume { id, dry_run }) => {
            eprintln!("warning: `review resume <ID>` is now `review message <ID>`");
            return run_message(id, *dry_run, selector, started, &lifecycle).await;
        }
        Some(cli::Command::Interrupt { id }) => {
            interrupt_cmd::run(id).await?;
            return Ok(CommandExit::Success);
        }
        Some(cli::Command::Sessions { id, all, limit }) => {
            match id {
                Some(id) => run_session_show(id, selector)?,
                None => run_sessions(*all, *limit, selector)?,
            }
            return Ok(CommandExit::Success);
        }
        Some(cli::Command::Incidents { limit }) => {
            run_incidents(*limit)?;
            return Ok(CommandExit::Success);
        }
        None => {}
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
        return run_message(session_id, cli.dry_run, selector, started, &lifecycle).await;
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
                return Ok(CommandExit::Usage);
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

    let archetypes_to_run = cfg.select_archetypes(archetype_arg)?;

    // Provider list: --provider wins, otherwise [_defaults].providers from the
    // first layer that sets it.
    // Both are typed where they were parsed (clap, the config files), so there
    // is no unknown name left to reject here.
    let providers_to_run: Vec<provider::Provider> =
        match cli.provider.as_ref().filter(|v| !v.is_empty()) {
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

    // If a profile was requested, every launched provider must have it in some
    // layer. Validate up front so we fail before spawning.
    if let Some(ref profile) = cli.profile {
        for prov in &providers_to_run {
            cfg.require_profile(*prov, profile)?;
        }
    }

    let missing: Vec<&str> = providers_to_run
        .iter()
        .filter(|p| !provider::is_available(**p))
        .map(|p| p.as_str())
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
        return Ok(CommandExit::Success);
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

    // Fan-out orchestration stays here: the operator-owned host launch lock
    // belongs at this site, and scheduling/collection form its one command path.
    // Provider execution and recording live in launch; views live in history.
    // Global lock
    let lock_path = std::env::temp_dir().join("review.lock");
    let lock_file = lock::open_lock_file(&lock_path)?;
    lock::acquire_blocking(&lock_file)?;

    // Refresh the cached audit id from the project file. ensure_audit_id locks
    // and rereads that file, so concurrent first runs share the persisted id.
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
    let provider_runtime = provider::ProviderRuntime::from_config(
        lifecycle.clone(),
        cfg.stall_timeout(),
        cfg.audit.private,
    );

    // Spawn all providers with staggered launches to avoid rate limits
    let stagger = std::time::Duration::from_secs(cli.stagger);
    struct PendingResult {
        provider: provider::Provider,
        ctx: std::sync::Arc<RecordCtx>,
        prompt: String,
        handle: tokio::task::JoinHandle<TaskOutcome>,
    }
    struct ArchetypeRun<T> {
        name: String,
        launches: Vec<T>,
    }
    let mut pending = Vec::new();
    let mut launch_count = 0u32;

    for (arch_name, prime) in &archetypes_to_run {
        let assembled = prompt::assemble(prime, &stdin_instructions);
        let mut group = ArchetypeRun {
            name: arch_name.clone(),
            launches: Vec::new(),
        };

        for &prov_name in &providers_to_run {
            // Profile overrides (validated above to exist when --profile is set).
            let definition = cli
                .profile
                .as_ref()
                .and_then(|name| cfg.profiles.get(&prov_name)?.get(name))
                .map(|entry| &entry.effective);
            let settings = provider::RunSettings::from_profile(prov_name, definition);
            let env_keys = settings.env_keys();
            let profile_origin = settings.origin().cloned();
            let launch = Launch {
                spec: provider::LaunchSpec {
                    provider: prov_name,
                    start: provider::SessionStart::Fresh { id: None },
                    settings,
                    prompt: assembled.clone(),
                    root: project_root.clone(),
                },
                runtime: provider_runtime.clone(),
                delay: stagger * launch_count,
            };
            let ctx = std::sync::Arc::new(RecordCtx {
                project_root: project_root.clone(),
                private: cfg.audit.private,
                audit_id: audit_id.clone(),
                archetype: arch_name.clone(),
                env_keys,
                profile_origin,
                operator_prompt: stdin_instructions.clone(),
            });
            group.launches.push(PendingResult {
                provider: prov_name,
                prompt: assembled.clone(),
                ctx: std::sync::Arc::clone(&ctx),
                handle: tokio::spawn(run_launch(launch, ctx)),
            });
            launch_count += 1;
        }
        pending.push(group);
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
    let mut results = Vec::new();
    for group in pending {
        let mut completed = ArchetypeRun {
            name: group.name,
            launches: Vec::new(),
        };
        for p in group.launches {
            // A finished task has already recorded its runs; only a panicked one
            // (which never got the chance) is recorded here.
            let result = match p.handle.await {
                Ok(outcome) => outcome.into_result(),
                Err(err) => {
                    let result = provider::ProviderResult {
                        provider: p.provider,
                        output: Err(anyhow::anyhow!("task panicked: {err}")),
                        session_id: None,
                        codex_home: None,
                        digest: None,
                        completed_epoch: provider::now_epoch_secs(),
                        // A task can panic after spawning. Its effective launch
                        // facts are unavailable here; do not replace them with
                        // requested permissions and claim they were granted.
                        permissions: None,
                        writable_roots: Vec::new(),
                        model: None,
                        effort: None,
                        served: provider::Served::default(),
                        grok_trust: None,
                        // Whatever it had launched is in the ledger, which the
                        // exit status reads.
                        leftovers: contained::Leftovers::default(),
                    };
                    p.ctx.record(&result, &p.prompt);
                    result
                }
            };

            completed.launches.push(result);
        }
        results.push(completed);
    }

    // Print results
    let multi = archetypes_to_run.len() > 1;
    for (index, group) in results.iter().enumerate() {
        if multi {
            if index > 0 {
                println!();
            }
            println!("=== {} ===\n", group.name);
        }
        for result in &group.launches {
            provider::print_result(result);
        }
    }
    let left_running = print_left_running(&lifecycle);

    println!("\nruntime: {}", format_runtime(started.elapsed()));

    // A codex run that died without a final answer returns Ok (so its session
    // id + digest survive), but it is a failure for exit-code purposes - a dead
    // review that exits 0 lies to scripts and CI. Count it as failed.
    //
    // Leftovers are a separate verdict across every invocation, and outrank
    // it: one provider's answer cannot excuse another's processes still
    // running.
    let all_failed = results.iter().flat_map(|g| &g.launches).all(run_failed);
    Ok(CommandExit::of_runs(all_failed, left_running))
}

/// Print what is still running, or may be, from every invocation this
/// `review` made (see `contained`), whichever results were reported. Returns
/// whether there was anything: exit 0 promises nothing the run started is
/// alive.
fn print_left_running(lifecycle: &crate::supervisor::Lifecycle) -> bool {
    let lines = crate::report::report_lines(&lifecycle.unresolved());
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
}
