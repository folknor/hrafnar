# Sessions and incidents

This page covers what `review` records about each run, where it keeps those
records, and the two commands that read them: `review sessions` lists and
shows sessions, and `review incidents` lists the forensic bundles filed for
suspicious codex runs. The fields of a stored row are listed under "The
session log".

## Where records are stored

Everything `review` records lives in one data directory on this host:
`$XDG_DATA_HOME/review`, or `~/.local/share/review` when `XDG_DATA_HOME` is
unset. `review` resolves that directory once, from its own environment, when
the command starts. A profile's `env` changes the harness's environment, not
this one, so it can't move `review`'s storage. The directory holds:

- **`sessions.jsonl`**: the session log, one row per recorded invocation
- **`sessions-private.jsonl`**: the session log for a project with
  `[_audit] private = true` (see `review man config`)
- **`audit/`**: one `audit.jsonl` per project, with each invocation's
  full prompt and response; `audit-private/` for private projects
- **`incidents/`**: the forensic bundles `review incidents` lists
- **`inflight/`**: markers for runs happening right now
- **`teardowns.jsonl`**: the teardown log, `teardowns-private.jsonl` for
  private projects

Records stay on this host. Harness homes, such as a codex `CODEX_HOME`, are
the harness's own storage and are not part of this directory.

## What a run records

`review` appends a row to the session log when an invocation ends with a
session ID. That row is what `review message` reads to find the session's
harness, project and settings. Each row has:

- **`kind`**: `run` for a fresh session, `session` for a resume, automatic
  resumes included
- **The two prompts**: `operator_prompt` is what you asked, and
  `assembled_prompt` is exactly what that invocation received
- **The timestamp**: when the invocation completed, not when it started

Some invocations go to the audit log only and never to the session log:

- a resume whose launch failed, so it didn't refresh the cache clock
- a claude run refused credentials before doing any work
- a claude attempt abandoned for its token retry (see `review man config`)

An automatic recovery records two rows, the dead run and its resume, in the
order they were invoked. The resume's `assembled_prompt` is the recovery
nudge, and its `operator_prompt` is still your original prompt. Both rows are
written after the recovery ends, whichever answer is displayed.

## Listing sessions

`review sessions` lists the current project's sessions, most recently touched
first. `--limit` caps the list (default 20), `--all` lists every project's,
and `-C` lists another project's. Each entry shows:

- the age of its last row: `just now`, `5m ago`, `2h10m ago`, `3d ago`, or
  `?` when the row has no usable timestamp
- the harness, archetype and `kind` of the row that opened it
- how many rows it has, its touch count
- the session ID
- the first line of the prompt that opened it, cut at 80 characters
- with `--all`, the project

## Live entries in the listing

The listing prints runs in progress before the recorded sessions, because a
row is written only when a run returns. Without these entries, a running turn
would look like an idle session showing its previous answer. The entries are:

- **`[in flight] HARNESS / turn in flight since AGE`**: a turn is running now
- **`[recording] HARNESS / waiting for the run handoff`**: the harness has
  been torn down and `review` is still recording the result
- **`[cleanup pending] HARNESS / ...`**: the `review` process that owned the
  run is gone, but its processes are still being torn down
- **`[queued] message waiting to launch`**: a `review message` holds the
  session but hasn't launched yet, usually because it is waiting for the
  host-wide launch lock
- **`[ambiguous] session: ID / ...`**: more than one live run was observed
  for the session; retry shortly rather than picking one

How each state affects a new message is in `review man message`.

## Showing one session

`review sessions ID` shows the last row of a session, from whichever project
started it. A project whose checkout was deleted still shows. With `-C`, the
command refuses a session that doesn't belong to the selected project. The
output has:

- the harness, archetype, `kind`, touch count and age
- the project, model, effort, sandbox and each writable root, when recorded
- the run digest, when one was recorded
- the response, or the error when there was no response

For a codex session, `review sessions ID` also reads the session's transcript
from the codex home saved with the row. A row with no saved `codex_home` uses
the current environment's `CODEX_HOME`, or codex's default. The transcript
section shows its path, whether the turn completed, the last event, and any
tool call still in flight. When the transcript can't be read, the section
says whether it was not found, unreadable, or the codex home couldn't be
resolved.

The transcript's final answer, when it has one, is shown in place of the
recorded response. It is marked `(recovered final answer from transcript)`
when the two differ. A completed transcript with no final answer gets a note
that the turn produced no conclusion, and the recorded response follows it.

## Listing incidents

`review incidents` lists the forensic bundles filed for suspicious codex
runs, such as deaths, stalls and stated failures, newest first (`--limit`,
default 20). Interrupted runs never get a bundle. Each entry shows the time,
harness, session, exit status and signal, a verdict, the codex version, and
the bundle directory. The verdicts are:

- **`turn failed: REASON`**: codex stated why the turn failed, so it is not a
  hang
- **`recovered from transcript`**: the answer was restored from the
  transcript
- **`no final answer (died)`**: the run died mid-turn
- **`completed (stream/-o truncated)`**: codex finished, but its output was
  cut off
- **`suspicious`**: none of the above could be established

A bundle has codex's stderr, the raw event stream, the digest and the
transcript excerpt, so you can diagnose the run without reproducing it. A run
reports its bundle path only when every file was written, and otherwise names
the files that failed. `review incidents` warns about any bundle whose
metadata it can't read, can't parse, or whose schema version it doesn't
support, and lists the rest.

## The session log

The session log has one JSON object per line, so `jq` answers what the
listing doesn't. Concurrent writers take turns appending. A line left
incomplete by a failed write is closed off before the next row, and readers
skip any malformed line with a warning while keeping the rows around it.

The fields of a row are:

- **`session_id`**, **`kind`**, **`timestamp`**, **`epoch_secs`**: the
  session, the row kind, and the completion time as text and as seconds
- **`provider`**: the harness
- **`project`**, **`hostname`**, **`audit_id`**: where the run happened
- **`archetype`**, **`model`**, **`effort`**: what was requested
- **`profile_origin`**: the config layer and table the profile came from,
  absent for a manual message
- **`env_keys`**: the names of the profile's env variables, never values
- **`sandbox`**: the sandbox level, absent for claude and maki
- **`provider_sandbox`**: grok's own permission selector, grok rows only
- **`writable_roots`**: roots granted beyond the harness's defaults
- **`codex_home`**: the codex home the run used, codex rows only
- **`operator_prompt`**, **`assembled_prompt`**: the two prompts
- **`response`**, **`error`**: the answer, or why there was none
- **`digest`**: the flat run summary, absent for a clean answer with nothing
  else to record
- **`served_model`**, **`cost_usd`**, **`grok_trust`**, **`leftovers`**:
  what the harness reported serving, and what the run left running
- **`credential`**: `token_command` when claude authenticated with the
  global `claude_token_command`
- **`review_version`**: the `review` version that wrote the row

Some rows on disk lack the optional fields; a missing field means it wasn't
recorded. A message inherits only the requested settings from a row: the
served model, cost, trust and leftovers are evidence, never settings.

These queries select runs that captured no answer, and runs at the
`workspace-write` sandbox level, from the default data directory:

```sh
jq 'select(.digest.captured == false)' ~/.local/share/review/sessions.jsonl
jq 'select(.sandbox == "workspace-write")' ~/.local/share/review/sessions.jsonl
```
