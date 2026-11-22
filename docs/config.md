# Configuration files

This page covers the two config files `review` reads, how `review` finds
them, what each section holds and how the files combine. The per-section
reference is under "Sections", and the errors a config can produce are in
the last table. Profile fields have their own topic, `review man profiles`.

Settings resolve from the command line first, then the project's
`.review.toml`, then your global config. The global config is
`$XDG_CONFIG_HOME/review/config.toml`, or `~/.config/review/config.toml` when
that variable is unset or relative. Both files share one format, and nothing
is built in: a setting no layer gives has no value.

## Seeing the effective config

`review list` shows what a run in the current directory can use. It merges
both files with the same rules a run uses, so it is the authoritative answer;
reading `.review.toml` alone misses profiles and archetypes defined in the
global file.

The command has three views:

- **`review list`**: the archetypes, and the profiles with at least one
  harness installed on this host. Each profile lists its installed harnesses
  and marks with `*` the one a plain `-p NAME` picks (no mark when the choice
  is ambiguous). The default profile is marked, and profiles with no
  installed harness are named.
- **`review list --all`**: every effective value with the file and table it
  came from, the files consulted, harness availability, and every harness
  entry's settings. Environment variables appear by name only.
- **`review list --projects`**: the `-C` registry for this host (see
  `review man projects`).

`review ls` is the same command. Outside a project, `review list` shows the
global layer alone.

## Finding the project file

`review` walks up from the current directory to the first `.review.toml`,
stopping at the directory that holds `.git`. A run needs that file, because
its `[_audit]` block identifies the project. A file holding only `[_audit]` is
a complete config. `-C` picks another project instead (see
`review man projects`).

`review init` creates a starter `.review.toml` with a fresh audit id in the
current directory, or in the directory `-C` selects. It refuses when a
`.review.toml` already exists there or in a directory above it up to the git
root, because a new file would shadow the existing one.

Discovery never guesses past a problem. A config that exists but doesn't
parse is an error, not an absent file. A directory entry `review` can't
inspect is an error too, not a reason to fall back to another project or the
global layer. A dangling `.review.toml` symlink is selected and fails to read,
and a dangling `.git` entry still stops the walk.

## Sections

This example uses every section. Each file accepts only these five tables:

```toml
[archetypes]
security = "You are a security expert. Read the codebase."

[_defaults]
profile = "deep"
prefer = ["codex", "claude"]
stall_timeout_secs = 900
claude_token_command = "bw get notes claude-oauth-token"

[profiles.deep]
comment = "Deep review."
codex = { model = "gpt-5.6-terra", effort = "high" }
claude = { model = "opus" }

[_audit]
id = "0123456789abcdef0123456789abcdef"
private = false

[_projects.myhostname]
bz = "~/Programs/bz"
```

The sections hold:

- **`[archetypes]`**: a name mapped to a priming prompt, prepended to stdin
  by `-a NAME`. The prompt decides the session's role and whether it may
  modify files; `review` adds nothing of its own.
- **`[profiles.NAME]`**: an optional `comment` and one entry per harness,
  each with that harness's launch settings (see `review man profiles`).
- **`[_defaults]`**: the keys listed under "Defaults".
- **`[_audit]`**: project file only. `id` names the project's audit trail,
  and `private = true` files the project's sessions in a separate private
  log.
- **`[_projects.HOST]`**: global file only. The `-C` registry for the host of
  that name (see `review man projects`).

### Defaults

The `[_defaults]` table takes four keys:

- **`profile`**: the profile a run uses without `-p`.
- **`prefer`**: harnesses in order of preference. A run without `--harness`
  takes the first one listed that the profile defines and this host has
  installed (see `review man profiles`).
- **`stall_timeout_secs`**: seconds of silence after which a codex run counts
  as stalled (see `review man failures`). `0` disables the check; omitting it
  keeps the built-in default. Only codex runs use it.
- **`claude_token_command`**: global file only. A shell command that prints
  a Claude OAuth token, described under "Claude credentials".

### Audit id

`review init` writes a 32-character hex `id`. When `id` is missing or empty,
`review` generates one and saves it to `.review.toml` before the first launch;
the run fails if the file can't be written. Any other nonempty `id` is kept
as written and treated as an opaque string.

## How the project and global files combine

Each section combines by its own rule:

- **Archetypes** are a union. For a name both files define, the project's
  prompt wins.
- **`profile`, `prefer` and `stall_timeout_secs`** each come whole from the
  first layer that sets them. A project's `prefer` replaces the global list;
  it doesn't extend it.
- **Profiles** are add only: a project may define new ones but not redefine
  a global one (see `review man profiles`).
- **`[_audit]`** belongs to the project file alone.
- **`[_projects]` and `claude_token_command`** belong to the global file
  alone. A command named in a project file would run code from any checkout.

## Claude credentials

Claude Code doesn't pass `CLAUDE_CODE_OAUTH_TOKEN` to the commands it runs.
If you export that token from a login shell, a `review` started inside a
Claude Code session launches claude without it. That claude falls back to its
own saved login and, if the login has expired, fails after about 30s with
"OAuth session expired and could not be refreshed". `review` says when this
happens.

`claude_token_command` fixes this. `review` runs it through `/bin/sh -c`,
from `$HOME`, with stdin closed and 60s to finish, at most once per `review`
process, and reads the token from its stdout. A fresh run tries claude's own
login first, so one synced global file serves hosts that log claude in other
ways.

`review` runs the command in two cases:

- **A fresh claude run fails to authenticate** before doing anything else:
  the model produced nothing, no tool ran, the whole output was read, the run
  ended on that failure, every process it started was torn down, and it had
  no token of its own (inherited, or set in the profile's `env`). `review`
  then launches the run once more as a new session, with the token as
  `CLAUDE_CODE_OAUTH_TOKEN`. The retry takes the host's launch lock like any
  launch, and startup hooks run again. The failed attempt stays in the audit
  log but not in `review sessions`.
- **`review message` continues a session whose first run used the token**:
  the command runs up front, without trying claude's own login. A
  `CLAUDE_CODE_OAUTH_TOKEN` in `review`'s own environment is used instead,
  since claude reads that one anyway. If the command isn't configured or
  gives no token, `review` refuses the message before sending anything. A
  message that fails to authenticate is never resent, because its prompt is
  already in the session.

Once `review` decides to retry, `review interrupt` no longer targets the
failed attempt; stopping `review` itself still prevents the retry.

`review` passes the token only to that claude process. It removes the token
from everything it reads back from the run before printing or recording it:
claude's output, and the command lines of processes left at teardown. The
teardown keeper scrubs those command lines too, including in the log it
writes if `review` itself has died. The session records only that the
command was used, and only when claude accepted the token. The command's own
stderr goes to your terminal unfiltered.

## Names an archetype can't use

An archetype can't take the name of a subcommand, because the positional
form `review NAME` runs the subcommand instead, silently. The reserved names
come from the command line itself, hidden commands and aliases included:

| Reserved word | Why |
|---|---|
| `message` | a subcommand |
| `resume` | a subcommand (hidden alias of `message`) |
| `interrupt` | a subcommand |
| `list` | a subcommand |
| `ls` | a subcommand (alias of `list`) |
| `config` | a subcommand (hidden alias of `list --all`) |
| `sessions` | a subcommand |
| `incidents` | a subcommand |
| `init` | a subcommand |
| `man` | a subcommand |
| `help` | the generated help subcommand |

`bare` is a selector, not a reserved word: `-a bare` and `review bare` mean no
archetype, whether or not `[archetypes]` defines `bare`. `bare = ""` is
valid; a nonempty prompt under that name is an error, since the selector
never sends it.

## Config errors and the fix

These errors stop every command that loads the config:

| You see | Why | Fix |
|---|---|---|
| `unknown section 'NAME'` | Only the five sections above exist; host tables like `[HOST.HARNESS.PROFILE]` aren't one | Move a host's profiles to `[profiles.NAME.HARNESS]` |
| `[codex.*] ... harness-scoped profile tables` | A profile table is keyed by harness first | Move each `[HARNESS.NAME]` to `[profiles.NAME.HARNESS]` |
| `` `provider` was removed `` | A profile holds one entry per harness, not a `provider` key | Move the settings under `[profiles.NAME.HARNESS]` for the harness `provider` named |
| `[_defaults].provider` or `.providers` | The harness comes from the profile | Set `profile = "NAME"` and order harnesses with `prefer` |
| `[_groups]` | A run takes one archetype | Launch one `review -a NAME` per member |
| `profile 'NAME' is defined in both` | Profiles are add only across files | Rename the project's profile |
| `[_audit] in` the global file | The audit block identifies one project | Move it to that project's `.review.toml` |
| `[_projects]` or `claude_token_command` in a project | Both are global only | Move them to the global config |
| `'NAME' is a reserved name` | The archetype shadows a subcommand | Rename the archetype |
| `archetype 'bare' ... has a prompt` | `bare` never sends a prompt | Use `bare = ""` or rename it |
| `.review.toml already exists` from `review init` | A config is already in force there | Edit the existing file |

`review config` prints `review list --all` with a warning; use
`review list --all`.
