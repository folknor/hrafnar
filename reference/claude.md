# Claude

What Claude Code was observed to do, which the rules in
`src/harness/claude_compat.rs` and `claude_stream.rs` rest on, and the
token retry that spans several modules. maki shares the runner; its own
evidence is in [maki.md](maki.md).

## Turns classify themselves

Every run's stream ends in one result object, and its `is_error` says
whether `result` is an answer. `subtype` is no guide: on Claude Code 2.1.284
an unknown `--model` came back `subtype: "success"`, `is_error: true`,
`terminal_reason: "api_error"`, `api_error_status: 404`, exit 1. That failed
launch still had a session: the result named its id and the transcript was
on disk.

The `init` event arrives within half a second of launch, and claude writes
the session's transcript to `~/.claude/projects/` by then. That is why a run
that dies after `init` keeps its session id.

## Shell commands stay in the foreground

`--print` kills a background Bash command when the model ends its turn.
Probed on Claude Code 2.1.284 (`scripts/claude_background_probe.py`): asked
to run a 60-second command in the background and report its output, the
model said it would report when the command finished and ended its turn.
About five seconds later claude marked the command `killed` and exited 0,
with `is_error: false` and `terminal_reason: "completed"`.

`CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1` removes the `run_in_background`
parameter from the Bash tool, and under it the same prompt ran in the
foreground and reported. Bash's default timeout of 2 minutes then applies.
Under `dontAsk`, a permission allowlist naming commands (`Bash(python3 *)`)
denied every call that passed its own `timeout` (the probe's model tried
200s), so the default is the limit that binds. With both timeout variables
raised to 20 minutes, a 150-second command ran to completion.

## Refused calls

Unverified: no captured stream in this repo shows a refusal. `denials_in`
assumes the `result` object carries a `permission_denials` array of
`{tool_name, tool_use_id, tool_input}`, the Agent SDK's
`SDKPermissionDenial`; maki's SDK mode writes the same key in imitation
(`research/maki/src/sdk_mode.rs`), which corroborates the key, not the
entry shape. What the refused call's own `tool_result` text says isn't
known either, so it isn't read. A probe of a `dontAsk` run that is refused
a command would settle both; until one has, a claude run whose result lacks
the array reports no denials rather than failing.

## Stopping a run

A claude turn ends cleanly on `SIGINT`. Probed on Claude Code 2.1.284 with a
command running mid-turn, `SIGINT` to claude ended the turn at once, killed
the command, exited 0 and printed a result: `is_error: true`,
`subtype: "error_during_execution"`, `terminal_reason: "aborted_tools"` and
the session id. The session resumed with the model seeing its command as
rejected. `SIGTERM` also left a resumable session but printed nothing
(exit 143), which is why the stop path sends `SIGINT`. A `SIGINT` before
claude installs its handler kills it with nothing printed, so the keeper's
pid is offered for signalling only after `init`.

Claude has no session lock of its own: two turns on one idle session both
run. The per-session launch lock in `inflight` is the only thing that
serialises `hrafnar message` calls to a claude session.

Verified end to end through `hrafnar`:

- a `SIGTERM` to `hrafnar` mid-command ended the turn in 0.2s, recorded it,
  printed the `hrafnar message` command and left nothing running
- `hrafnar interrupt` from a second process did the same
- `hrafnar message` interrupted the turn and resumed it with the new message,
  which the model answered

## Credentials from a Claude Code session

A claude launched from inside a Claude Code session can lack the operator's
credential. Claude Code doesn't pass `CLAUDE_CODE_OAUTH_TOKEN` to the shells
it runs, so a claude started from one falls back to its saved login in
`~/.claude/.credentials.json`. With that file stale (`expiresAt: 0`, refresh
token expired), every such run and a plain `claude -p` failed after 30.0s
with "OAuth session expired and could not be refreshed" (bz hrafnar#10).
Blanking the Claude Code session variables (`CLAUDECODE`,
`CLAUDE_CODE_CHILD_SESSION`, the messaging socket and token) changed
nothing; the missing token was the whole cause.

On claude 2.1.330 the failed run printed, in order:

1. `system` hook and `init` events
2. one main-agent `assistant` event (`parent_tool_use_id: null`) with
   top-level `error: "authentication_failed"`
3. a `result` with `is_error: true`, `terminal_reason: "api_error"` and no
   API status

`ClaudeMetrics` classifies that as `AuthFailure::BeforeWork` only from a
fully observed stream with no work in it; anything less is `Unknown`, and
is never replayed, because a resend could repeat work.

### How the token command is used

`[_defaults].claude_token_command` is used on two paths, and the rule for
each is spread over `app::launch`, the session log and the adapter:

- **A fresh run** (`app::launch::claude_auth_retry`) retries on
  `BeforeWork` when the command is configured, the child's effective
  environment has no token, teardown was clean and no stop was requested.
  The first attempt is settled first: audited, kept out of the sidecar
  because it advanced no session, and given its own handoff naming itself,
  so waiters on its id don't hang behind an unrelated launch. A handoff that
  can't be published stops the retry. The prompt is then relaunched as a new
  session, not resent into the old one, because the failed attempt already
  wrote the prompt to its transcript. Its `run` row records `credential`
  only when the token was accepted.
- **A message or resume** never replays. A session whose `run` row records
  `credential: "token_command"` continues on that credential: the command
  runs before the launch lock, and if it is unconfigured or fails the
  message is refused before anything is sent, writing no sidecar row. The
  row is found by kind and bound to harness, project and session id, not by
  timestamp order, which clock skew and the log merge can reorder. A
  host-wide "needs the token" marker isn't used, because one invocation's
  failure would choose the credential for unrelated later ones, go stale
  once native login is fixed, and race between concurrent runs.

The token is redacted by name in incident bundles like any `*TOKEN*`
variable. The keeper redacts it from every process record it emits, using
the child's own secret-named environment values, so its orphan log is
covered when `hrafnar` has died. A command line cut part-way through a token
has the trailing fragment redacted when it is at least 8 characters.
