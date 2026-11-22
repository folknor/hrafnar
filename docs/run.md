# Starting a fresh run

This page covers a run: `hrafnar` with no subcommand, which sends your
instructions to one new session on one harness. It explains the input, the
archetype and profile flags, how concurrent runs are spaced, the dry run,
the output, auto-resume and the exit status. `hrafnar --help` has the short
form of each rule.

```sh
echo "instructions" | hrafnar -a ARCHETYPE -p PROFILE --harness HARNESS
```

Every run starts a fresh session, and nothing from an earlier run is
carried over. If the new session needs context, state it in the
instructions. To continue an existing session while its cache is warm, use
`hrafnar message ID` instead (see `hrafnar man message`).

## Instructions on stdin

Your instructions come on stdin, and a run needs them. The limit is 20000
bytes; a longer input is refused before anything launches. A pipe and a
heredoc are the same to `hrafnar`:

```sh
hrafnar -a security <<'EOF'
Audit the session handling in src/auth.
EOF
```

Quote the heredoc delimiter (`'EOF'`), so the shell leaves `$`, backticks
and backslashes in the text alone. Bare `hrafnar`, with no arguments and
nothing on stdin, prints its help and exits 2. With any argument given,
empty stdin is an error instead, exiting 1.

## Priming with an archetype

An archetype is a named prompt from the config that primes the session. With
`-a NAME`, `hrafnar` sends the archetype's prompt, a blank line, then your
stdin. The flag takes these forms:

- **No `-a`**: stdin is sent unchanged, and the run is recorded under the
  archetype name `bare`.
- **`-a bare`**: the same unprimed run, whether or not the config defines
  `bare`.
- **`-a security`**: the `security` archetype's prompt, then stdin.

An archetype whose prompt is empty or whitespace sends stdin unchanged, with
no blank line in front. `-a` takes one name: a comma-separated list is
refused, because one run is one session. An unknown name is an error that
lists the configured archetypes. A positional archetype (`hrafnar security`)
is a hidden alias for `-a security` and prints a warning; a positional
`bare` means no archetype.

## Choosing the profile and harness

The profile decides the model and settings, and it holds one entry per
harness (see `hrafnar man profiles` for its fields). The run takes `-p NAME`,
else `[_defaults].profile` from the config. With neither, or with a profile
no config file defines, the run errors before launching.

The run then picks one of the profile's harness entries: `--harness NAME`
when given, otherwise the order `hrafnar man profiles` sets out. A chosen
harness that isn't installed fails the run before anything launches.
`hrafnar list` marks the harness a plain `-p NAME` would pick on this host.

`--provider` is refused, with an error naming `-p` and `--harness`.

## Running several sessions at once

One run is one session. For several archetypes or profiles, start several
`hrafnar` calls side by side, each with the same stdin.

Runs on one host share a launch lock, so calls you start together reach
their harnesses 30s apart. Each run holds the lock for 30s after it
launches, or until it ends if that comes sooner. A long run delays no other
`hrafnar` call by more than that spacing.

## Previewing with a dry run

`--dry-run` prints the assembled prompt on stdout and launches nothing. It
still reads stdin and resolves the archetype, profile and harness, so it
fails on the same configuration errors a real run would. On stderr it
names:

- the config files searched
- this host's name
- the chosen harness and its profile table

A chosen harness that isn't installed gets a warning, not an error. A dry
run exits 0.

## Reading the output

A run prints these, in order:

1. A `--- HARNESS ---` header.
2. `session: ID`, when the harness created a session.
3. A digest, for codex and grok runs and for any run that didn't end
   cleanly. `hrafnar --help` lists its fields.
4. The response.
5. For an interrupted run, the `hrafnar message` command that continues it.
6. Anything the run left running (see `hrafnar man failures`).
7. A `runtime:` line.

The runtime is wall clock from the moment `hrafnar` started, so it includes
the wait for stdin and for the launch lock. A run that fails before its
harness produces a turn prints the header and the error on stderr instead.

## Auto-resume after a codex death

A codex run that dies mid-turn without a final answer is resumed once,
straight away, in the same session with the same profile. The resume
prompt is a short nudge to finish.

`hrafnar` then reports one attempt, chosen like this:

- **The resume answered**: its answer is reported, prefixed with
  `(auto-resumed after the initial run died without a final answer)`.
- **The resume was interrupted**: the resume is reported, so its continue
  command is shown.
- **Otherwise**: the original death is reported.

A run is never auto-resumed when it was interrupted, when its harness stated
why it failed, or when its teardown left processes running.
`hrafnar man failures` explains each of those outcomes.

## Exit status

The exit status is the run's contract with the script or agent that started
it:

- **0**: the run produced an answer, and nothing it started is still
  running. A dry run also exits 0.
- **1**: the run produced no answer, or `hrafnar` could not verify the
  answer. This covers a run that died, stalled, stated a failure or was
  interrupted, and any error before launch.
- **2**: invalid arguments, or bare `hrafnar` with nothing on stdin.
- **3**: something the run started was left running, or its teardown could
  not be confirmed. This outranks 1, because those processes may still be
  changing the tree, so check them before retrying.
- **128 plus the signal number**: `hrafnar` was stopped by a signal.

A failed run still prints its session ID when the harness created one, so
you can inspect it with `hrafnar sessions ID` or continue it with
`hrafnar message ID`.

## Long runs

A run can take hours. Start `hrafnar` with the largest timeout your harness
allows for background commands. When a harness stops a command at its
timeout, it signals `hrafnar`, which ends the turn and records the session
so you can continue it. `hrafnar man message` covers what a signal does and
how to steer a running agent without stopping `hrafnar`.
