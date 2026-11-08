# Sessions and incidents

`review` records every run that produced a session ID, on this host only, in
`sessions.jsonl` under `$XDG_DATA_HOME/review`, else `~/.local/share/review`
(`sessions-private.jsonl` for a project with `[_audit] private = true`). That record is what `review message` reads to
find a session's provider, project and settings.

## Listing sessions

`review sessions` lists the current project's sessions, most recently touched
first (`--limit`, default 20; `--all` for every project). `-C` lists another
project's. Each entry shows its age, provider, archetype, how many times it was
touched, and the first line of the prompt that opened it.
Ages use `just now`, `<age> ago`, or `?` when the timestamp is unknown.

Two kinds of entry come before the list:

- `[in flight] <provider> / turn in flight since <age>`: a run is happening
  right now. Its record is only written when it returns, so without this a
  running turn would look like an idle session showing its previous answer.
- `[queued] message waiting to launch`: a `review message` that holds the
  session's launch lock but has not launched yet, usually because it is waiting
  behind the host-wide launch lock. A further message waiting behind that one
  adds no entry of its own (see `review man message`).

A run whose `review` process has died is not shown in flight.

## One session

`review sessions <ID>` shows a session's last record wherever it was started:
provider, archetype, project, model, effort, sandbox and writable roots, the
digest, and the response. A deleted project still shows. With `-C`, the
session must belong to the selected project.
Fresh runs are recorded as `run`; resumes, including automatic resumes, are
recorded as `session`. Each row's assembled prompt is what that invocation
received. An automatic recovery row records the recovery nudge and retains the
original operator prompt separately. Recovery pairs are appended in invocation
order after recovery ends, whichever answer is displayed.

For codex it also reads the session's transcript under the resolved
`CODEX_HOME` recorded with the session: its path, whether the turn completed,
the last event and any tool call still in flight. When the transcript holds a
final answer, that answer is shown in place of the recorded response, marked
`(recovered final answer from transcript)` when the two differ. A completed
transcript with no final answer is flagged as having produced no conclusion;
the recorded response is still shown below it. Older rows without a saved
home use the current process environment and default, so a profile-only home
from an older row cannot be reconstructed. A transcript lookup says whether
it was absent, unreadable, or the home could not be resolved.

## Incidents

`review incidents` lists the forensic bundles filed for suspicious codex runs,
newest first (`--limit`): deaths, stalls and stated failures. Each line shows
the time, session, exit status and signal, a verdict, and the bundle directory.

The verdicts:

- `turn failed: <reason>` - codex stated why; not a hang.
- `recovered from transcript` - the answer was restored.
- `no final answer (died)` - a genuine mid-turn death.
- `completed (stream/-o truncated)` - codex finished but its output was cut.
- `suspicious` - none of the above could be established.

A bundle holds codex's stderr, the raw event stream, the digest and the
transcript excerpt, so the run can be diagnosed without reproducing it. A
bundle path is reported only when every artifact was written; partial writes
are reported with their failed paths. `review incidents` lists readable
bundles and warns about metadata it could not read or parse. Interrupted runs
are never bundled. Metadata uses schema 1; older schema-1 bundles remain
readable, and unsupported versions or invalid metadata produce a warning.

## Querying further

The session log is one JSON object per line. Fields include `session_id`,
`provider`, `project`, `archetype`, `model`, `effort`, `sandbox`,
`writable_roots`, `profile_origin` (selected profile layer and table), `env_keys` (env var names only), `codex_home` (the resolved
Codex rollout directory), the prompts, the response or error, and a flat
`digest`, so `jq` answers what the listing does not (with the default data
directory):

```
jq 'select(.digest.captured == false)' ~/.local/share/review/sessions.jsonl
jq 'select(.sandbox == "workspace-write")' ~/.local/share/review/sessions.jsonl
```
