# Configuration

Settings resolve from the command line, then the project's `.review.toml`,
then the operator's global config: `$XDG_CONFIG_HOME/review/config.toml`, else
`~/.config/review/config.toml`. Both files share one format. Nothing is built
in.

**`review config` is the authoritative view.** It runs the same resolver a run
uses and shows every effective value with the file and table it came from, and
what each one shadows. Reading `.review.toml` alone gives an incomplete and
often wrong answer, because profiles and archetypes may live in the global
file. Outside a project it shows the global layer alone.

## Finding the project file

`review` walks up from the current directory to the first `.review.toml`,
stopping at the git root. A run requires one, because its `[_audit]` block
identifies the project; a file holding only `[_audit]` is a complete config.
`review init` creates a starter file in the current directory (or in the
selected one with `-C`), refusing if one already exists there or above it.
`-C` picks another project (see `review man projects`).

A config that exists but does not parse is always an error, never treated as
absent.

## Sections

```toml
[archetypes]
security = "You are a security expert. Read the codebase."
bugs = "You hunt for edge cases and correctness bugs."

[_defaults]
profile = "deep"
# Seconds of silence after which a codex run counts as stalled. 0 disables;
# omitted uses the built-in default. Codex only.
stall_timeout_secs = 900
# Global file only. Prints a Claude OAuth token on stdout; run only when a
# claude run fails to authenticate. See "Claude credentials" below.
claude_token_command = "bw get notes claude-oauth-token"

# Profiles: see `review man profiles`.
[profiles.deep]
provider = "codex"
model = "gpt-5.6-terra"
effort = "high"
sandbox = "read-only"

# Project file only.
[_audit]
id = "0123456789abcdef0123456789abcdef"
private = false

# Global file only: see `review man projects`.
[_projects.myhostname]
bz = "~/Programs/bz"
```

- `[archetypes]`: name to priming prompt. The prompt owns the session's role
  and whether it should modify files; `review` adds nothing of its own.
- `[profiles.<name>]`: a provider and its launch settings (see
  `review man profiles`).
- `[_defaults]`: `profile` (the one used without `-p`), `stall_timeout_secs`,
  and, in the global file only, `claude_token_command`.
- `[_audit]`: `id` names the project's audit trail. `review init` creates a
  32-character hex id; a missing or empty id gets one before the first launch.
  Existing nonempty ids remain opaque strings, unchanged.
  The id is saved before launch, and the run fails if it cannot be saved.
  `private = true` files the project's sessions in a separate private log.
- `[_projects.<host>]`: the `-C` registry.
- Any other top-level table is an error. Retired forms say what replaced them:
  `[<provider>.<name>]` and legacy `[<host>.<provider>.<name>]` profile tables
  move to `[profiles.<name>]` with a `provider` key; `[_defaults].providers`
  and `[_defaults].provider` become `[_defaults].profile`; `[_groups]` goes,
  since a run takes one archetype and several opinions are several `review`
  calls.

## How the two files combine

- **Archetypes** are a union; for a name both define, the project wins.
- **`profile` and `stall_timeout_secs`** come from the first layer that sets
  them, each on its own.
- **Profiles** resolve to their first definition and win whole, never merged
  field by field, and a project profile may not change a global profile's
  provider (see `review man profiles`).
- **`[_audit]`** is the project's alone and is an error in the global file.
  **`[_projects]`** and **`claude_token_command`** are the global file's alone
  and are errors in a project: a command named by a project file would run
  code from any checkout.

## Claude credentials

A claude logged in through `CLAUDE_CODE_OAUTH_TOKEN` exported from a login
shell works from a terminal, but Claude Code does not pass that variable to the
commands it runs. A `review` started from inside a Claude Code session then
launches a claude without it, which falls back to its own saved login and, if
that has expired, fails after about 30 seconds with "OAuth session expired and
could not be refreshed". `review` says so when it happens.

`claude_token_command` is a shell command whose stdout is a token. It runs
through `/bin/sh -c`, from `$HOME`, with stdin closed and at most 60 seconds to
finish, at most once per `review` process. A fresh run tries claude's own login
first, so one synced global file serves hosts that log claude in other ways.
It runs:

- after a fresh claude run fails to authenticate, if nothing but startup ran:
  the model produced nothing and no tool ran anywhere in the attempt, the whole
  output was read, the run ended on that failure, processes it started were
  all torn down, and it had no token of its own (inherited or set in the
  profile's `env`). The retry takes the host's launch lock like any launch.
  The run is then launched once more, as a new session, with the token as
  `CLAUDE_CODE_OAUTH_TOKEN`. The failed attempt is kept in the audit log but
  not in `review sessions`. Startup hooks run again for the second launch;
- before `review message` or `review resume` of a session whose first run
  authenticated with it: that session goes on with the same credential source,
  without trying claude's own login first. A `CLAUDE_CODE_OAUTH_TOKEN` already
  in `review`'s own environment is used instead, since claude would read that
  one anyway. If the command is no longer
  configured, or gives no token, the message is refused before anything is
  sent. A message that fails to authenticate is never resent, since its prompt
  is already in the session.

A failed attempt stops being a target for `review interrupt` once the retry is
decided; stopping `review` itself still prevents the retry. The token is passed
only to that claude process. `review` removes it from everything it reads back
from that run - claude's output, and the command lines of processes left at
teardown - before printing or recording any of it; the session records only
that the command was used, and only when the token was accepted. The keeper
that tears the run down scrubs it from leftover command lines too, including
in the log it writes if `review` itself has died. The command's own stderr goes
to your terminal unfiltered.

## Reserved words

An archetype may not use the names below. Reserved command names come
from the CLI itself, including hidden migration commands and aliases; this table
is checked against that vocabulary.

| Reserved word | Why |
|---|---|
| `message` | a subcommand |
| `resume` | a subcommand (hidden alias of `message`) |
| `interrupt` | a subcommand |
| `config` | a subcommand |
| `sessions` | a subcommand |
| `incidents` | a subcommand |
| `init` | a subcommand |
| `man` | a subcommand |
| `help` | clap's generated subcommand |

Subcommand names are reserved because the legacy positional archetype form
(`review sessions`) would run the subcommand instead, silently.

`bare` is a special selector: `-a bare` and the legacy `review bare` mean "no
archetype", whether or not `[archetypes].bare` exists. Existing `bare = ""`
definitions remain valid, but a non-empty prompt is refused because the
selector cannot prime.

Config discovery propagates filesystem inspection errors; it never treats them
as a missing file and falls back to another project or the global layer. A
dangling .review.toml symlink is selected and fails to read. A dangling .git
entry still stops upward discovery. Init refuses any existing config entry.
