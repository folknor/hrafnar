//! The command line. `review` with no subcommand is the run itself (stdin to
//! the harnesses), with the archetype as `-a`, the profile as `-p` and the
//! global `-C` selecting the project. The verbs are `message`, `interrupt`,
//! `list`, `sessions`, `incidents`, `init` and `man`.
//!
//! Hidden migration aliases warn when used: the old positional archetype
//! (`review security`, with `review bare` meaning none), `review resume <ID>`
//! and `--session <ID>`, both now `review message <ID>`, and `review config`,
//! now `review list --all`. A retired `--provider` is refused by the run. Run flags beside a
//! subcommand are refused by hand (`Cli::parse_checked`), because clap would
//! accept them in front of one and silently drop them. Which commands accept
//! `-C` is one exhaustive policy here (`Cli::project_scope`).
//!
//! The help text is what orchestrating agents read in place of the docs, so it
//! states the rules they would otherwise trip on: profile precedence and the
//! sandbox default, the digest fields, exit status, and resume's limits. The
//! harness lists and stale cutoffs in it are filled in from
//! `harness::Harness` at runtime, so they cannot drift. Change the behaviour,
//! change the help.

use clap::{CommandFactory, Parser, Subcommand};

const AFTER_HELP_TEMPLATE: &str = "\
Instructions come on stdin. With no archetype they are sent unchanged; with
-a, the archetype's priming prompt is prepended. `-a bare` also sends stdin
unchanged, whether or not `bare` is in the config. Every run starts one fresh
session on one harness, and the session ID is printed above the response so
you can follow up with `review message <ID>` while the cache is warm. For
several opinions, launch several `review` calls; launches on one host are
spaced apart automatically to avoid rate limits.

Steering a running agent: `echo \"<new info>\" | review message <ID>`. It ends
the {INTERRUPTIBLE} turn in flight and continues the session with your message. Do not
stop or kill the `review` process to interrupt an agent - use `review message`,
or `review interrupt <ID>` to stop without a message.

Runs can take hours. A harness that stops background commands after a timeout
(Claude Code: 30 minutes unless the command passes a larger `timeout`) signals
`review`, which interrupts the turn exactly as above, so launch `review` with
the largest timeout the harness allows.

Settings resolve from the command line, then the project's .review.toml, then
the global config ($XDG_CONFIG_HOME/review/config.toml, else
~/.config/review/config.toml). `review list` shows the archetypes and profiles
you can use; `review list --all` the whole result and where each value came
from.

Another project (-C): `review -C <name>` acts on a project registered on this
host in the global config, under [_projects.<host>] as `name = \"path\"`; the
path must hold the project's .review.toml itself. A name matches ignoring case,
and `owner/name` keys also match by the part after the slash. A path selector
is spelled as one (`-C ../bz`, `-C ~/src/bz`) and is searched upward like the
current directory. There is no fallback between the two. The current directory
never changes; -C picks the project. `review message` and `review sessions <ID>`
take the project from the session itself, and -C there must agree with it.
{PROJECT_REFUSALS} refuse -C.

Profiles (-p): a profile is a [profiles.<name>] table naming the harness it
runs on ({HARNESSES}) and that harness's model, effort, sandbox and env. Without
-p the run uses [_defaults].profile. A harness that is not installed fails the
run before anything launches. A profile resolves to its first definition,
looking in the project file before the global one, and that definition wins
whole. Fields are never merged, so a project profile that leaves out `model`
does not inherit the global profile's `model`, and a project profile may not
name a different harness than the global one of the same name. `sandbox` defaults to read-only, so a profile has to
ask for workspace-write before a codex or grok run may modify files. Claude
and maki profiles must omit `sandbox`: what a claude run can touch is decided
by Claude Code's own permission settings, and a maki run's by maki's
permissions.toml - which by default lets maki edit the project. Maki profiles
must also omit `effort`, which maki ignores. A codex workspace-write run is by
default granted the project's own .git directory, so it can git commit (a
codex config override of sandbox_workspace_write.writable_roots replaces it);
any run that changes git hooks or config is warned about at exit, by path
(`review man sandbox`). A claude run started from inside a Claude Code session
may fail to authenticate, since Claude Code does not pass CLAUDE_CODE_OAUTH_TOKEN
on; a global [_defaults].claude_token_command fixes that (`review man config`).

Output: a `--- <harness> ---` header, then `session: <ID>`, then the
response. Codex and grok runs also print a digest between the session
line and the response:
  exit / signal     the harness process's exit status
  captured          true if a final answer was captured; false means the text
                    below is an interim note, not a conclusion
  turn failed       the reason the harness gave for ending the turn
  review failed     review could not verify the run (grok background work);
                    the run fails even if an answer was captured
  terminated by review
                    the stall watchdog killed a codex run that went silent
  recovered         the final answer was restored from codex's transcript
  turns / usage     turn count and token usage (input, cached, output, reasoning)
  transcript        codex's rollout file and its last events
  incident          a forensic bundle for the run (see `review incidents`)
The output ends with a `runtime:` line. The exit status is 1 when the run
produced no answer, including runs that died, stalled or were interrupted, or
an answer that came with `review failed`.

When a run ends, whatever its agent left running (background commands, daemons,
shells in sessions of their own) is ended before `review` reports. That is
recorded and never printed. Anything that could not be ended is printed above
the `runtime:` line as `left running:` with its pids and command lines, or
`teardown outcome unknown:`, and the exit status is 3 whatever the answers were:
those processes may still be changing the tree, so check them before starting
more work there.

Examples:
  echo \"what does foo() do?\" | review -p deep          Plain prompt, 'deep' profile
  echo \"audit the auth flow\" | review -a security      With an archetype
  echo \"ask claude\" | review -p opus                   A profile on another harness
  echo \"check bz\" | review -C bz                       Another registered project
  echo \"check\" | review -a bugs --dry-run              Preview the prompt
  echo \"follow up\" | review message <ID>               Message a session, running or not
  review interrupt <ID>                                Stop a {INTERRUPTIBLE} run mid-turn
  review list                                          Archetypes and profiles

More depth on any of this: `review man` lists the bundled topics.";

/// Help templates name harnesses through markers, filled from
/// `harness::Harness` by `render`: `{INTERRUPTIBLE}` (`claude, codex or maki`),
/// `{UNINTERRUPTIBLE}` (`grok`), `{HARNESSES}` (`claude, codex, grok, maki`) and
/// `{CUTOFFS}` (each harness's stale cutoff). Hand-written lists here drifted
/// from the code that decides them, and a new harness would have been missing
/// from every one.
fn render(template: &str) -> String {
    use crate::harness::Harness;
    template
        .replace("{INTERRUPTIBLE}", &Harness::interruptible_names())
        .replace("{UNINTERRUPTIBLE}", &Harness::uninterruptible_names())
        .replace("{HARNESSES}", &Harness::supported())
        .replace("{CUTOFFS}", &Harness::stale_cutoff_prose())
        .replace("{PROJECT_REFUSALS}", &project_refusals())
}

/// A rendered template, for clap attributes, which take a `&'static str`.
macro_rules! rendered {
    ($name:ident = $template:expr) => {
        static $name: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| render($template));
    };
}

rendered!(AFTER_HELP = AFTER_HELP_TEMPLATE);
rendered!(MESSAGE_AFTER_HELP = MESSAGE_AFTER_HELP_TEMPLATE);
rendered!(INTERRUPT_AFTER_HELP = INTERRUPT_AFTER_HELP_TEMPLATE);
rendered!(
    MESSAGE_ABOUT =
        "Send stdin to a session, interrupting its {INTERRUPTIBLE} turn if one is running"
);
rendered!(
    INTERRUPT_ABOUT =
        "Interrupt a {INTERRUPTIBLE} run in flight, then print how to resume its session"
);

const MESSAGE_AFTER_HELP_TEMPLATE: &str = "\
If a {INTERRUPTIBLE} turn is in flight, it is interrupted first (as `review
interrupt` does) - the turn is cut off and whatever it had not finished is lost -
and the message then continues the session. A {UNINTERRUPTIBLE} turn in flight cannot be
interrupted; the message is refused until it ends. A run already recording
waits for its handoff. Active runs from older builds without handoff support
must finish before continuation.

The session must come from a run on this host: its harness is read from the
record that run left (`review sessions` lists them), and an ID with no record
is refused. The run continues in the project the session was started in,
whatever the current directory; -C only asserts it. There is no -a or -p. Stdin is sent as-is, with no
archetype prime, and the session keeps the sandbox, model and effort it was
started with.

Continuing only pays off while the model host's prompt cache is warm, so a session
last touched more than {CUTOFFS}
is refused. When that happens, start a fresh run and restate the context the
new session needs.

The exit status is 1 if the turn produced no answer (or `review failed` to
verify it), and 3 if anything it
started was left running (which outranks 1). A session whose earlier `review` is gone while that run's
processes are still being torn down is refused until they are.";

const INTERRUPT_AFTER_HELP_TEMPLATE: &str = "\
Works on {INTERRUPTIBLE} runs, not {UNINTERRUPTIBLE}. This ends the turn, waits for the run
to record its session, and prints the `review message` command that continues
it. A run already recording waits for its exact handoff without signalling.
Active runs from older builds without handoff support must finish before
continuation. The interrupted run exits 1, like any run that produced no answer. To stop a
turn and send new information in one step, use `review message` instead.

Signalling the `review` process (Ctrl-C, SIGTERM, a stopped background task)
ends its {INTERRUPTIBLE} turns the same way and records them before exiting; a
second signal skips that and tears the runs down at once, which can lose their
records, and a third exits without waiting for the teardown to report. SIGKILL
loses the session, though what the runs started is still torn down.";

const LIST_AFTER_HELP: &str = "\
This is the authoritative view of what a run in this directory can use. It
merges the project's .review.toml with the global config, so reading
.review.toml alone gives an incomplete and often wrong answer: profiles and
archetypes may live in the global file.

By default it lists the archetypes, and the profiles whose harness is
installed on this host with the default marked; profiles hidden for a missing
harness are named. --all adds everything: the files consulted, harness
availability, each value's source and any definition it shadows, and the
remaining settings. Env vars are listed by name only, never by value. Outside
a project it shows the global layer alone.";

#[derive(Parser)]
#[command(
    name = "review",
    about = "Send a prompt to a fresh AI session on one of several harnesses",
    override_usage = "echo <instructions> | review [OPTIONS]\n       review <COMMAND>",
    after_help = AFTER_HELP.as_str(),
    // Without this a subcommand name after a flag (`review -C bz config`) is
    // taken as the legacy positional archetype.
    subcommand_precedence_over_arg = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Act on another project: a name registered under [_projects.<host>] in
    /// the global config, or a path spelled as one (/..., ./..., ../..., ~/...)
    #[arg(short = 'C', long = "project", global = true, value_name = "PROJECT")]
    pub project: Option<String>,

    #[command(flatten)]
    pub run: RunArgs,
}

#[derive(clap::Args)]
#[group(multiple = true)]
pub struct RunArgs {
    /// Archetype to prime with. "bare" selects an unprimed run even when absent
    /// from config. Omit to send stdin unchanged.
    #[arg(short = 'a', long, value_name = "NAME")]
    pub archetype: Option<String>,

    /// Profile to run: [profiles.<name>], naming the harness and its
    /// model/effort/sandbox/env. Overrides [_defaults].profile. The first
    /// definition found wins whole; sandbox defaults to read-only
    #[arg(short = 'p', long, value_name = "NAME")]
    pub profile: Option<String>,

    /// Retired: the harness is part of the profile. Still accepted so the
    /// run can refuse it with the replacement rather than clap's diagnostic.
    #[arg(long = "provider", hide = true, value_name = "NAME")]
    pub retired_provider: Option<String>,

    /// Print the assembled prompt instead of sending it
    #[arg(long)]
    pub dry_run: bool,

    /// Migration: the archetype used to be positional (`review security`). Still
    /// accepted, with a warning, so orchestration procedures written against
    /// the old form keep working until they are updated; `bare` means none.
    #[arg(hide = true, value_name = "ARCHETYPE")]
    pub legacy_archetype: Option<String>,

    /// Migration: `--session <ID>` is the old spelling of `review resume <ID>`.
    #[arg(long, hide = true, value_name = "ID")]
    pub session: Option<String>,
}

// Keep field access convenient while Clap owns the run-argument membership.
impl std::ops::Deref for Cli {
    type Target = RunArgs;
    fn deref(&self) -> &RunArgs {
        &self.run
    }
}

impl Cli {
    /// Parse the real command line, exiting with clap's diagnostic on error.
    pub fn parse_checked() -> Self {
        match Self::try_parse_checked(std::env::args_os()) {
            Ok(cli) => cli,
            Err(e) => e.exit(),
        }
    }

    /// Parse, then refuse run flags beside a subcommand.
    ///
    /// The run flags belong to the run, and clap would otherwise accept them in
    /// front of a subcommand and drop them: `review --dry-run resume <ID>`
    /// parsed, lost the `--dry-run`, and sent the turn for real. clap's own
    /// `args_conflicts_with_subcommands` did this until `-C` arrived, but it
    /// makes no exception for a global flag - `review -C bz config` became a run
    /// primed with an archetype named `config` - and `conflicts_with` cannot name
    /// a subcommand. So it is checked here, on explicit presence, so that a
    /// defaulted run argument never looks supplied.
    pub fn try_parse_checked<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        use clap::FromArgMatches;
        let mut cmd = Self::command();
        let matches = cmd.try_get_matches_from_mut(args)?;
        if let Some((sub, _)) = matches.subcommand() {
            let run = cmd
                .get_groups()
                .find(|g| g.get_id() == "RunArgs")
                .expect("flattened run arguments have a group");
            let ids: Vec<_> = run.get_args().cloned().collect();
            for id in ids {
                let id = id.as_str();
                if matches.value_source(id) == Some(clap::parser::ValueSource::CommandLine) {
                    let arg = cmd
                        .get_arguments()
                        .find(|a| a.get_id() == id)
                        .expect("group member is an argument");
                    let shown = if let Some(short) = arg.get_short() {
                        format!("-{short}")
                    } else if let Some(long) = arg.get_long() {
                        format!("--{long}")
                    } else {
                        "a positional archetype".to_string()
                    };
                    return Err(cmd.error(
                        clap::error::ErrorKind::ArgumentConflict,
                        format!("{shown} belongs to a run and cannot be used with `review {sub}`"),
                    ));
                }
            }
        }
        Self::from_arg_matches(&matches).map_err(|e| e.format(&mut cmd))
    }

    pub fn print_help() {
        let mut cmd = Self::command();
        let _ = cmd.print_help();
    }
}

#[derive(Subcommand)]
pub enum Command {
    #[command(about = MESSAGE_ABOUT.as_str(), after_help = MESSAGE_AFTER_HELP.as_str())]
    Message {
        /// Session ID, as printed above the earlier run's response
        #[arg(value_name = "ID")]
        id: String,

        /// Print what would be sent instead of sending it (interrupts nothing)
        #[arg(long)]
        dry_run: bool,
    },

    /// Migration: the old name of `message`, for an idle session. Still
    /// accepted, with a warning.
    #[command(hide = true)]
    Resume {
        /// Session ID, as printed above the earlier run's response
        #[arg(value_name = "ID")]
        id: String,

        /// Print what would be sent instead of sending it
        #[arg(long)]
        dry_run: bool,
    },

    #[command(about = INTERRUPT_ABOUT.as_str(), after_help = INTERRUPT_AFTER_HELP.as_str())]
    Interrupt {
        /// Session ID of the run, as listed by `review sessions`
        #[arg(value_name = "ID")]
        id: String,
    },

    /// List the archetypes and runnable profiles; --all for the whole config
    #[command(visible_alias = "ls", after_help = LIST_AFTER_HELP)]
    List {
        /// List the projects registered for -C on this host instead
        #[arg(long, conflicts_with = "all")]
        projects: bool,

        /// Show the whole resolved configuration, every value with its source
        #[arg(long)]
        all: bool,
    },

    /// Migration: the old name of `list --all`. Still accepted, with a warning.
    #[command(hide = true)]
    Config,

    /// List recent sessions, or show one session's artifacts by ID
    Sessions {
        /// Session ID to show artifacts for (transcript, digest, response).
        /// Omit to list recent sessions.
        #[arg(value_name = "ID")]
        id: Option<String>,

        /// List sessions across all projects, not just the current one
        #[arg(long, conflicts_with = "id")]
        all: bool,

        /// Maximum number of sessions to list (most recent first)
        #[arg(long, default_value = "20")]
        limit: usize,
    },

    /// List recent forensic bundles written for suspicious codex runs
    Incidents {
        /// Maximum number of incidents to list (most recent first)
        #[arg(long, default_value = "20")]
        limit: usize,
    },

    /// Create a starter .review.toml in the current directory
    Init,

    /// Print a bundled topic with more depth than this help; no topic lists them
    Man {
        /// Topic name, as listed by `review man`
        #[arg(value_name = "TOPIC")]
        topic: Option<String>,
    },
}

/// Selection chooses launch policy; assertion checks a recorded session's
/// project. Neither is meaningful for a host-wide command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectScope {
    Selected,
    Asserted,
    Unscoped(&'static str),
}

impl Command {
    pub(crate) fn project_scope(&self) -> ProjectScope {
        match self {
            Self::Message { .. } | Self::Resume { .. } => ProjectScope::Asserted,
            Self::Sessions { id: Some(_), .. } => ProjectScope::Asserted,
            Self::Sessions {
                id: None,
                all: true,
                ..
            } => ProjectScope::Unscoped("review sessions --all"),
            Self::Sessions {
                id: None,
                all: false,
                ..
            }
            | Self::Init
            | Self::List { .. }
            | Self::Config => ProjectScope::Selected,
            Self::Interrupt { .. } => ProjectScope::Unscoped("review interrupt"),
            Self::Incidents { .. } => ProjectScope::Unscoped("review incidents"),
            Self::Man { .. } => ProjectScope::Unscoped("review man"),
        }
    }
}

impl Cli {
    pub(crate) fn project_scope(&self) -> ProjectScope {
        match &self.command {
            Some(command) => command.project_scope(),
            None if self.session.is_some() => ProjectScope::Asserted,
            None => ProjectScope::Selected,
        }
    }
}

// These are presentation representatives, not an independent refusal list.
// Scope owns whether each representation refuses -C and the diagnostic name.
fn project_refusals() -> String {
    [
        Command::Interrupt { id: String::new() },
        Command::Incidents { limit: 20 },
        Command::Man { topic: None },
        Command::Sessions {
            id: None,
            all: true,
            limit: 20,
        },
    ]
    .iter()
    .filter_map(|command| match command.project_scope() {
        ProjectScope::Unscoped(name) => Some(format!("`{name}`")),
        ProjectScope::Selected | ProjectScope::Asserted => None,
    })
    .collect::<Vec<_>>()
    .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_checked(std::iter::once("review").chain(args.iter().copied()))
    }

    fn parsed(args: &[&str]) -> Cli {
        match parse(args) {
            Ok(cli) => cli,
            Err(e) => panic!("{args:?} should parse: {e}"),
        }
    }

    #[test]
    fn the_command_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn help_names_harnesses_from_the_table() {
        // Every marker is filled: an unknown one would print literally.
        for help in [
            &*AFTER_HELP,
            &*MESSAGE_AFTER_HELP,
            &*INTERRUPT_AFTER_HELP,
            &*MESSAGE_ABOUT,
            &*INTERRUPT_ABOUT,
        ] {
            assert!(!help.contains("{"), "unfilled marker in {help:?}");
        }
        let message = MESSAGE_AFTER_HELP.replace('\n', " ");
        for p in crate::harness::Harness::all() {
            let mins = p.stale_cutoff().as_secs() / 60;
            assert!(
                message.contains(&format!("{mins} minutes ago (")),
                "message help should state {p}'s cutoff"
            );
        }
        assert!(
            message.contains(&crate::harness::Harness::stale_cutoff_prose()),
            "{message}"
        );
    }

    #[test]
    fn a_run_takes_its_archetype_and_profile_as_flags() {
        let cli = parsed(&["-a", "security", "-p", "deep", "--dry-run"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.archetype.as_deref(), Some("security"));
        assert_eq!(cli.profile.as_deref(), Some("deep"));
        assert!(cli.dry_run);
        assert!(cli.legacy_archetype.is_none());
    }

    #[test]
    fn a_plain_run_needs_no_arguments_at_all() {
        let cli = parsed(&[]);
        assert!(cli.command.is_none());
        assert!(cli.archetype.is_none());
        assert!(cli.legacy_archetype.is_none());
    }

    #[test]
    fn message_takes_its_own_dry_run() {
        match parsed(&["message", "abc", "--dry-run"]).command {
            Some(Command::Message { id, dry_run }) => {
                assert_eq!(id, "abc");
                assert!(dry_run);
            }
            _ => panic!("expected message"),
        }
    }

    #[test]
    fn resume_takes_its_own_dry_run() {
        match parsed(&["resume", "abc", "--dry-run"]).command {
            Some(Command::Resume { id, dry_run }) => {
                assert_eq!(id, "abc");
                assert!(dry_run);
            }
            _ => panic!("expected resume"),
        }
    }

    #[test]
    fn run_flags_in_front_of_a_subcommand_are_refused() {
        // Accepting them meant dropping them: `--dry-run resume <ID>` parsed,
        // the subcommand never saw the flag, and a real turn was sent.
        for args in [
            &["--dry-run", "resume", "abc"][..],
            &["-p", "deep", "resume", "abc"],
            &["-a", "bugs", "resume", "abc"],
            &["--provider", "claude", "resume", "abc"],
            &["--provider", "codex", "message", "abc"],
            &["-p", "deep", "config"],
            &["--session", "abc", "sessions"],
        ] {
            assert!(parse(args).is_err(), "{args:?} should be refused");
        }
    }

    #[test]
    fn retired_run_flags() {
        // --provider still parses, so the run can refuse it by name.
        assert_eq!(
            parsed(&["--provider", "codex"]).retired_provider.as_deref(),
            Some("codex")
        );
        assert!(parse(&["--stagger", "0"]).is_err(), "retired with fan-out");
    }

    #[test]
    fn list_takes_its_view_and_an_alias() {
        for name in ["list", "ls"] {
            assert!(matches!(
                parsed(&[name]).command,
                Some(Command::List {
                    projects: false,
                    all: false
                })
            ));
        }
        assert!(matches!(
            parsed(&["list", "--all"]).command,
            Some(Command::List { all: true, .. })
        ));
        assert!(matches!(
            parsed(&["ls", "--projects"]).command,
            Some(Command::List { projects: true, .. })
        ));
        assert!(parse(&["list", "--all", "--projects"]).is_err());
        // The old name still parses, as `list --all`.
        assert!(matches!(parsed(&["config"]).command, Some(Command::Config)));
    }

    #[test]
    fn a_subcommand_keeps_its_own_dry_run() {
        assert!(parse(&["message", "abc", "--dry-run"]).is_ok());
    }

    #[test]
    fn project_goes_before_or_after_a_subcommand() {
        for args in [&["-C", "bz", "list"][..], &["list", "-C", "bz"]] {
            let cli = parsed(args);
            assert!(
                matches!(cli.command, Some(Command::List { .. })),
                "{args:?}"
            );
            assert_eq!(cli.project.as_deref(), Some("bz"), "{args:?}");
        }
        let cli = parsed(&["--project", "bz", "-a", "bugs"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.project.as_deref(), Some("bz"));
        assert!(cli.legacy_archetype.is_none());
    }

    #[test]
    fn the_old_positional_archetype_still_parses() {
        let cli = parsed(&["security", "--profile", "deep"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.legacy_archetype.as_deref(), Some("security"));
        assert_eq!(cli.profile.as_deref(), Some("deep"));
    }

    #[test]
    fn the_old_session_flag_still_parses() {
        let cli = parsed(&["bugs", "--session", "abc", "--provider", "codex"]);
        assert_eq!(cli.session.as_deref(), Some("abc"));
    }

    #[test]
    fn interrupt_takes_a_session_id() {
        match parsed(&["interrupt", "abc"]).command {
            Some(Command::Interrupt { id }) => assert_eq!(id, "abc"),
            _ => panic!("expected interrupt"),
        }
        assert!(parse(&["interrupt"]).is_err(), "the id is required");
    }

    #[test]
    fn project_scope_follows_the_effective_invocation_and_help() {
        let cases: &[(&[&str], ProjectScope)] = &[
            (&[], ProjectScope::Selected),
            (&["config"], ProjectScope::Selected),
            (&["init"], ProjectScope::Selected),
            (&["sessions"], ProjectScope::Selected),
            (&["sessions", "s"], ProjectScope::Asserted),
            (&["message", "s"], ProjectScope::Asserted),
            (&["resume", "s"], ProjectScope::Asserted),
            (&["--session", "s"], ProjectScope::Asserted),
            (
                &["sessions", "--all"],
                ProjectScope::Unscoped("review sessions --all"),
            ),
            (
                &["interrupt", "s"],
                ProjectScope::Unscoped("review interrupt"),
            ),
            (&["incidents"], ProjectScope::Unscoped("review incidents")),
            (&["man"], ProjectScope::Unscoped("review man")),
        ];
        for (args, expected) in cases {
            assert_eq!(parsed(args).project_scope(), *expected, "{args:?}");
            if let ProjectScope::Unscoped(name) = expected {
                assert!(AFTER_HELP.contains(&format!("`{name}`")), "{name}");
            }
        }
        // Exercise every actual command too, so a newly unscoped verb cannot
        // be omitted from the help representatives unnoticed.
        let mut command = Cli::command();
        command.build();
        for sub in command
            .get_subcommands()
            .filter(|sub| sub.get_name() != "help")
        {
            let mut args = vec![sub.get_name()];
            args.extend(
                sub.get_arguments()
                    .filter(|arg| arg.is_positional() && arg.is_required_set())
                    .map(|_| "s"),
            );
            if let ProjectScope::Unscoped(name) = parsed(&args).project_scope() {
                assert!(AFTER_HELP.contains(&format!("`{name}`")), "{name}");
            }
        }
    }

    #[test]
    fn man_takes_one_topic_at_most() {
        assert!(parse(&["man"]).is_ok());
        assert!(parse(&["man", "sandbox"]).is_ok());
        assert!(parse(&["man", "sandbox", "inheritance"]).is_err());
    }

    #[test]
    fn a_subcommand_name_is_the_subcommand() {
        assert!(matches!(parsed(&["config"]).command, Some(Command::Config)));
        assert!(matches!(
            parsed(&["sessions", "--all"]).command,
            Some(Command::Sessions { all: true, .. })
        ));
    }
}
