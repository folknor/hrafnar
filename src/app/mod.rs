//! The `review` command itself: argument handling, the run and the verbs.
//! `main.rs` only dispatches between this and the keeper.

use anyhow::{Result, bail};

use crate::cli::{self, Cli};
use crate::exit::CommandExit;
use crate::{
    config, config_cmd, config_write, contained, harness, input, interrupt_cmd, lock, man, project,
    prompt,
};

mod history;
mod launch;
mod message;

use history::{run_incidents, run_session_show, run_sessions};
use launch::{Launch, RecordCtx, run_launch};
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
fn run_failed(r: &harness::HarnessResult) -> bool {
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
    // waited, not just what the harnesses spent.
    let started = std::time::Instant::now();
    let cli = Cli::parse_checked();
    if !matches!(cli.command, Some(cli::Command::Man { .. })) {
        // Resolve once before any command work; no filesystem effects.
        let _ = crate::data::DataDir::process();
    }

    // Forward SIGINT/SIGTERM to any running codex process groups before exiting.
    // codex runs in its own process group (so we can kill it and its children as
    // a unit), which means it no longer receives a terminal Ctrl-C implicitly -
    // without this, killing `review` would leave codex running detached.
    // Installed once, process-wide, and before any harness can be spawned; see
    // `supervisor::install_signal_supervisor` for why it cannot be per-run.
    let lifecycle = crate::supervisor::Lifecycle::default();
    crate::supervisor::install_signal_supervisor(lifecycle.clone());

    // Refused before the registry is read, so `interrupt -C typo <ID>` reports
    // the misuse rather than a lookup failure. These commands are host-wide or
    // session-addressed with no project to assert, so accepting -C would
    // suggest a scope restriction that does not exist.
    if let Some(ref sel) = cli.project
        && let cli::ProjectScope::Unscoped(what) = cli.project_scope()
    {
        bail!("-C {sel} does not apply to `{what}`, which is not scoped to a project");
    }
    // Scope is classified before dispatch: selected verbs resolve config below;
    // asserted verbs pass the selector to their recorded-target checks. Never
    // turn a wrong-scope selector into None (which would select the cwd).
    let selector = cli.project.as_deref();

    match &cli.command {
        Some(cli::Command::Man { topic }) => return Ok(man::run(topic.as_deref())),
        Some(cli::Command::Init) => {
            config_write::init(&project::select(selector)?)?;
            return Ok(CommandExit::Success);
        }
        Some(cli::Command::List { projects, all }) => {
            let view = match (projects, all) {
                (true, _) => config_cmd::View::Projects,
                (false, true) => config_cmd::View::All,
                (false, false) => config_cmd::View::Terse,
            };
            config_cmd::run(&project::select(selector)?, view)?;
            return Ok(CommandExit::Success);
        }
        Some(cli::Command::Config) => {
            eprintln!("warning: `review config` is now `review list --all`");
            config_cmd::run(&project::select(selector)?, config_cmd::View::All)?;
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
        if cli.profile.is_some() || cli.retired_provider.is_some() {
            eprintln!(
                "warning: --profile/--provider are ignored on a resume - the session \
                 keeps its own harness and settings"
            );
        }
        return run_message(session_id, cli.dry_run, selector, started, &lifecycle).await;
    }

    // Retired: the harness is part of the profile. Still parsed, so the
    // refusal can say what replaced it rather than clap's "unexpected argument".
    if let Some(ref p) = cli.retired_provider {
        bail!(
            "--provider was removed: the harness is part of the profile\n  \
             Pick a profile whose harness is {p} with -p <name> (`review list` lists them)."
        );
    }

    let archetype_arg =
        resolve_archetype_arg(cli.archetype.as_deref(), cli.legacy_archetype.as_deref())?;
    let archetype_arg = archetype_arg.as_deref();

    // A bare `review` - no arguments at all and nothing piped in - gets the help
    // text. Read before the config so this works outside a project too. With any
    // flag given the operator meant to run something, so a missing stdin falls
    // through to `read_stdin`'s error, which says what is missing, rather than
    // answering with help that silently ignores the flags. Argv presence, not
    // parsed fields, so any argument at all requests a run.
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

    let mut cfg = config::load(&project::select(selector)?)?;
    let project_root = cfg.require_project_root()?.to_path_buf();
    let stdin_instructions = match early_stdin {
        Some(s) => s,
        None => input::read_stdin()?,
    };

    if cli.dry_run {
        eprintln!("config: {}", cfg.searched());
        eprintln!("hostname: {}", cfg.hostname);
    }

    let (arch_name, prime) = cfg.select_archetype(archetype_arg)?;

    // The profile names the harness and its settings: -p wins, otherwise
    // [_defaults].profile from the first layer that sets it. It must exist
    // before anything is spawned.
    let Some(profile_name) = cli.profile.as_deref().or_else(|| cfg.default_profile()) else {
        bail!(
            "no profile to run: pass -p <name> or set [_defaults].profile \
             (looked in {})",
            cfg.searched()
        );
    };
    let definition = cfg.require_profile(profile_name)?.clone();
    let prov_name = definition.harness;

    let installed = harness::is_available(prov_name);
    let assembled = prompt::assemble(&prime, &stdin_instructions);

    // Dry run: print what would be sent and exit. It launches nothing, so a
    // missing harness only warns.
    if cli.dry_run {
        if !installed {
            eprintln!("warning: {prov_name} is not installed on this host - a real run would fail");
        }
        println!("{assembled}");
        return Ok(CommandExit::Success);
    }

    // One config reaches every machine, including ones missing a harness, so
    // this is checked before anything launches.
    if !installed {
        bail!(
            "{prov_name} is not installed on this host (not found on PATH)\n  \
             Nothing was launched. Install it, or pick a profile for another harness with -p."
        );
    }

    // Harness execution and recording live in launch; views live in history.
    // The host-wide launch lock spaces out `review` calls fired together.
    let lock_path = std::env::temp_dir().join("review.lock");
    let lock_file = lock::open_lock_file(&lock_path)?;
    lock::acquire_blocking(&lock_file)?;

    // Refresh the cached audit id from the project file. ensure_audit_id locks
    // and rereads that file, so concurrent first runs share the persisted id.
    let local_config_path = cfg
        .files()
        .local
        .as_deref()
        .expect("config::load always resolves a local config path");
    let audit_id = config_write::ensure_audit_id(local_config_path)?;
    cfg.audit.id = Some(audit_id.clone());
    let scope = crate::data::LogScope::new(audit_id, cfg.audit.private);
    let data = crate::data::DataDir::process().clone();

    // Codex run settings from project config: today just the stall timeout,
    // which is tunable (and disableable) because it rests on an empirical codex
    // property rather than a documented contract.
    let harness_runtime = harness::HarnessRuntime::from_config(
        lifecycle.clone(),
        cfg.stall_timeout(),
        cfg.claude_token_command.as_deref(),
        data.clone(),
        &scope,
    );

    let settings = harness::RunSettings::from_profile(prov_name, Some(&definition));
    let ctx = std::sync::Arc::new(RecordCtx {
        project_root: project_root.clone(),
        data: data.clone(),
        scope: scope.clone(),
        archetype: arch_name,
        env_keys: settings.env_keys(),
        profile_origin: settings.origin().cloned(),
        operator_prompt: stdin_instructions,
    });
    let launch = Launch {
        spec: harness::LaunchSpec {
            harness: prov_name,
            start: harness::SessionStart::Fresh { id: None },
            settings,
            prompt: assembled.clone(),
            root: project_root,
        },
        runtime: harness_runtime,
    };
    let mut handle = tokio::spawn(run_launch(launch, std::sync::Arc::clone(&ctx)));

    // Hold the lock for the launch spacing, so the next queued `review` on this
    // host launches that much later. A run that is over sooner releases it
    // then: it is no longer talking to its harness, and its result should not
    // wait on the spacing.
    let joined = tokio::select! {
        joined = &mut handle => Some(joined),
        () = tokio::time::sleep(crate::timings::LAUNCH_SPACING) => None,
    };
    drop(lock_file);
    let joined = match joined {
        Some(joined) => joined,
        None => handle.await,
    };

    // A finished task has already recorded its runs; only a panicked one
    // (which never got the chance) is recorded here.
    let result = match joined {
        Ok(outcome) => outcome.into_result(),
        Err(err) => {
            let result = harness::HarnessResult {
                credential: None,
                run_id: None,
                _reservation: None,
                harness: prov_name,
                output: Err(anyhow::anyhow!("task panicked: {err}")),
                session_id: None,
                codex_home: None,
                digest: None,
                completed_epoch: harness::now_epoch_secs(),
                // A task can panic after spawning. Its effective launch facts
                // are unavailable here; do not replace them with requested
                // permissions and claim they were granted.
                permissions: None,
                writable_roots: Vec::new(),
                model: None,
                effort: None,
                served: harness::Served::default(),
                grok_trust: None,
                // Whatever it had launched is in the ledger, which the exit
                // status reads.
                leftovers: contained::Leftovers::default(),
            };
            ctx.record(&result, &assembled);
            result
        }
    };

    harness::print_result(&result);
    let left_running = print_left_running(&lifecycle);

    println!("\nruntime: {}", format_runtime(started.elapsed()));

    // A codex run that died without a final answer returns Ok (so its session
    // id + digest survive), but it is a failure for exit-code purposes - a dead
    // review that exits 0 lies to scripts and CI. Leftovers are a separate
    // verdict that outranks it.
    Ok(CommandExit::of_runs(run_failed(&result), left_running))
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
fn selected_root(sel: &str) -> Result<project::ProjectRoot> {
    match project::select(Some(sel))? {
        config::Selection::Exact(dir) => Ok(project::ProjectRoot::observe(&dir)),
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
        let got = resolve_archetype_arg(Some("security"), None).expect("ok");
        assert_eq!(got.as_deref(), Some("security"));
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
