# Why a run failed

This page explains how to tell why a run produced no usable answer, and what
to do next. It starts with the exit status and the digest, then gives one
section per kind of failure, each naming the cause and the fix.

## Exit status and the digest

A failed run exits `1`, or `3` when something it started was left running
(see "Processes left running" below). `hrafnar man run` lists every exit
status.

Most failures still print a session ID and a digest, because the session
exists and you can inspect it or message it. Read the digest top down:

- **`turn failed: reason`**: the harness stated why it ended the turn. See
  "A stated failure".
- **`hrafnar failed: reason`**: `hrafnar` could not verify the result. See "A
  result hrafnar could not verify". It fails the run even when an answer was
  captured, and `hrafnar` never retries it.
- **`terminated by hrafnar: reason`**: `hrafnar` ended the process, for an
  interrupt, a signal or the codex stall watchdog. This isn't always a
  failure: the watchdog also ends a codex process that wrote its answer and
  then hung, and that answer is kept.
- **`recovered`**: codex never reported its answer but wrote one to its
  transcript, and `hrafnar` restored it. This is a success even though
  `captured` reads `false`.
- **`captured: false` with none of the above**: the run died mid-turn
  without saying why. See "A death".
- **`captured: true`**: there is an answer. A non-zero process exit after it
  is reported but doesn't fail the run. When the exit status is unknown and
  no signal was seen, a note says the answer was captured but the exit status
  could not be confirmed.

`hrafnar sessions ID` shows the same digest later (see
`hrafnar man sessions`).

## A stated failure

A stated failure means the harness refused or could not run the request: the
content was flagged, a rate limit or quota ran out, authentication failed, or
the model name is unknown. The reason comes first in the digest, because the
next move follows from it.

`hrafnar` never retries a stated failure, since resending the same request
gets the same refusal. Fix the cause (model name, credentials, quota or
prompt) and start a fresh run. If the cause was transient, message the
session instead.

A maki turn counts as answered only when maki says it finished the turn. A
maki turn that stopped at its token or turn limit is a stated failure with
its partial text kept, even though maki doesn't flag it as an error.

## A death

A death is a turn cut off with no final answer and no reason given. Only
codex runs get a second chance, in two steps:

1. If codex wrote a final answer to its transcript but exited before
   reporting it, `hrafnar` restores the answer and the digest says
   `recovered`.
2. If there was no answer, `hrafnar` files an incident bundle (see
   `hrafnar man sessions`) and resumes the session once with a nudge to
   finish (see `hrafnar man run`).

`hrafnar` doesn't resume a codex death that was interrupted, or one that left
processes running.

Claude, grok and maki classify their own turns, so `hrafnar` never resumes
them. A turn of theirs that ends without an answer is reported with the
reason the harness gave. A claude or maki run killed or crashed after it
started is a death, and keeps its session ID.

A run that never started has no session. Maki saves a session only once it
holds a message, so a maki killed while still connecting to its model vendor
can leave an ID that maki then reports as not found.

## A stall

A stall is a codex run whose transcript stays silent, with no final answer,
for `stall_timeout_secs` (set in `[_defaults]`; `0` turns the check off).
`hrafnar` kills it as stalled. Codex normally writes to its transcript every
few minutes, even while it waits on a long command, so `hrafnar` treats a
long silence as a wedge. That rule is empirical: a change in how often codex
writes could make it misfire.

The check applies only when `hrafnar` can find and read the run's
transcript, and knew at launch where this run's part of it begins. Without
both, nothing bounds a hung codex run.

A stalled run's digest says `terminated by hrafnar`, with how long the
transcript was quiet and its last event. `hrafnar` files an incident bundle
and treats the stall as a death, so it resumes the session once as "A
death" describes.

## An interrupt

An interrupt is a turn ended by `hrafnar interrupt`, by `hrafnar message` on a
running turn, or by a signal to `hrafnar`. The digest is flagged interrupted
and the run exits `1`. `hrafnar` never resumes an interrupted run or files an
incident for it. If the harness finished before the interrupt arrived, the
answer stands and the run succeeds.

## A result hrafnar could not verify

A `hrafnar failed:` line means `hrafnar` needed a fact to accept the result
and couldn't get it. The cases are:

- a claude or maki result object that lacks the fields `hrafnar` uses to tell
  an answer from an error
- a grok turn that ended with no stop reason
- grok background work `hrafnar` could not check, a wake turn that could not
  launch, or the wake limit (see "A finished run with no report")

Each of these fails the run. Rerun it, or message the session to finish.

## A finished run with no report

Grok can end its turn while one of its commands is still running in the
background, and then never see the result. `hrafnar` waits for that work and
resumes the session with the shell results it finds, printing each wake. It
repeats until a turn ends with nothing unread.

`hrafnar` can't read a finished grok subagent's result, so that subagent's
wake only tells the model to fetch it. A clean wake turn doesn't prove the
model read it. One gap is known: a background subagent that finishes while
the turn is still running can go undetected.

The run fails with `hrafnar failed:` when either of these happens:

- grok still has unread results after ten wakes
- grok's own record of the session can't be read or parsed

`hrafnar` doesn't wake grok after a stated failure.

Claude and grok runs keep shell commands in the foreground for the same
reason, so the model waits for them, with a 20 minute ceiling per command.
`hrafnar` sets both limits through environment variables, so a profile's
`env` can change them. A model can still give a command a shorter timeout.

## Processes left running

When a run ends, however it ended, `hrafnar` ends whatever the run's agent
started that is still running: background commands, daemons, and shells in
sessions of their own. That includes daemons the next run starts again when
it needs them, such as a build cache server. `hrafnar` records this in the
session record (`leftovers`) and doesn't print it.

What `hrafnar` could not end is printed above the `runtime:` line, and the
command exits `3`. The lines are:

- **`left running: N processes ... started survived teardown`**, then one
  line per process with its pid and command line. `at least N` means more
  may remain that could not be identified.
- **`teardown outcome unknown: ... (keeper pid N) - reason`**: `hrafnar`
  could not confirm the teardown, so processes the run started may still be
  running. Any it knew to have survived are listed under it.

Either way, the run's processes may still be changing the tree. Check them
before you start more work there.

When `hrafnar` is signalled, it prints the same report to stderr before it
exits. When `hrafnar` is killed outright, what its run started is still torn
down, but nothing is printed. The outcome is appended, best effort, to
`teardowns.jsonl` (or `teardowns-private.jsonl`) beside the session logs.

## Before anything launched

These errors fail the run before any session exists, and nothing is sent:

- no prompt on stdin
- a config file that doesn't parse
- an unknown archetype
- no profile given and no `[_defaults].profile` set, or an unknown profile
- a profile with no entry for the harness `--harness` names
- a chosen harness that isn't installed on this host
- a project that `-C` can't resolve

The error says which one, and what to change.

## A run that cannot write

A run that should have edited files but couldn't is a sandbox question: see
`hrafnar man sandbox`. A grok write run cancelled at its first edit, with a
hint about folder trust in the reason, ran in a folder grok doesn't trust.
