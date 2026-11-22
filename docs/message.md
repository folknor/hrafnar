# Steering a session

This page covers how you send a follow-up to a session, interrupt its turn,
and continue it later with `review message` and `review interrupt`. The
commands come first, then what a message inherits, which sessions accept
one, and the exit status.

## Sending a message

You pipe the message into `review message` with the session ID. The ID is
the one printed as `session: ID` above a run's response:

```sh
echo "Also check the error paths." | review message 1234abcd
```

What happens depends on the state of the session:

- **Idle session**: the message continues it as a new turn.
- **Codex, claude or maki turn in flight**: `review` interrupts that turn
  first, which cuts it off and loses whatever it hadn't finished. The message
  then continues the session. If the turn finishes before the interrupt takes
  effect, its answer stands and the message continues after it.
- **Grok turn in flight**: refused, because grok has no way to stop a turn
  from outside. Send the message once the turn ends.

A session ID you message must be 1 to 128 ASCII letters, digits, `-` or `_`.
Recorded sessions with other IDs stay readable in `review sessions` but can't
be messaged.

`review message --dry-run` checks the session and project, prints the message
and sends nothing.

## Interrupting without a message

`review interrupt ID` ends a codex, claude or maki turn without sending
anything. It waits for the run to record its session, then prints the
`review message` command that continues it. A run that is already recording
its result gets no signal; `review interrupt` waits for that handoff instead.
The interrupted run exits 1, like any run that produced no answer.

If the run in flight was started by a `review` build that can't hand off its
result, both commands refuse before signalling it. Wait for that run to
finish.

## Never kill review to stop an agent

Interrupt a turn with `review message` or `review interrupt`, never by
stopping or killing the `review` process. A signal to the process works in
stages:

1. The first SIGINT or SIGTERM ends its codex, claude and maki turns the way
   `review interrupt` does, and records them within a bounded wait.
2. A second signal tears the runs down at once, which can lose their records.
3. A third exits without waiting for the teardown to report.

SIGKILL loses the record, though what the runs started is still torn down.
Grok turns aren't ended gracefully at any stage. A session without a record
can't be continued at all.

## What a message inherits

`review message` sends stdin as-is: no archetype prime and no profile, so it
takes no `-a`, `-p` or `--harness`. The session keeps the settings recorded
for it, and `review` announces each one as it starts:

- the harness
- the sandbox level, for example
  `sandbox: workspace-write (inherited from the session record)`
- the model and effort
- the writable roots, as `review man sandbox` describes
- for codex, the `CODEX_HOME` the session was stored under
- for claude, the `claude_token_command` credential, when the session was
  created with one

A maki session records no effort. Maki resumes at the thinking level its own
config sets, else the one it last used anywhere on the host, not necessarily
the one the session ran at.

The session record has env names but no values, and no codex `config`
overrides. So a follow-up runs without the profile's `env` and `config`
entries, apart from the `CODEX_HOME` above. A session that relied on one, such
as a custom endpoint, `GROK_HOME` or a model provider override, loses it.

## Where a message runs

A message runs in the project the session was started in, whatever your
current directory. With `-C`, the selected project must be that one. Before it
interrupts anything, `review` checks that:

- the project directory still exists
- its `.review.toml` parses
- its audit id is the one the session was recorded under

If any check fails, nothing is interrupted and nothing is sent.

## Which sessions can be messaged

You can message a session this host's `review` recorded, or a fresh run on
this host still in flight that hasn't recorded yet. The fresh run writes its
record when it returns, and the message waits for that record after the
interrupt. `review` reads the harness from the record. It refuses an ID with
neither a record nor a run in flight; `review sessions` lists what this host
knows.

A session whose earlier `review` died while its processes were still being
torn down shows as `[cleanup pending]` in `review sessions`. `review` refuses
messages and interrupts to it until the teardown ends.

## Cold sessions

A message pays off only while the harness's prompt cache is warm, so `review`
refuses a session idle too long:

| Harness | Refused when last touched more than |
|---|---|
| codex | 27 minutes ago |
| claude | 55 minutes ago |
| grok | 55 minutes ago |
| maki | 55 minutes ago |

These are refusal thresholds, not a promise that the cache is warm before
them. Every turn that ran resets the session's clock, whether or not it
produced an answer, because reprocessing the session warms the cache either
way. A record with no usable timestamp is never refused as cold.

When `review` refuses a cold session, start a fresh run. Restate in its
instructions what the new session needs to know: the question, what has been
established, and what is left.

## Several messages at once

`review` serialises messages to one session up to their launch. A second
message waits until the first has launched, then finds the first one's turn
in flight:

- on codex, claude or maki it interrupts that turn
- on grok it is refused
- if the first turn has already finished, it continues the session

While a message waits behind the host-wide launch lock, `review sessions`
shows it as `[queued]`. A message that arrives just after the first launched,
before the harness has reported its process, waits briefly rather than
failing.

## Exit status

`review message` exits with one of these codes:

- **0**: the turn produced an answer, or the command was a `--dry-run`
- **1**: the turn produced no answer, including an interrupted turn, or
  `review` failed to verify it
- **3**: something the turn started was left running; this outranks 1

`review` never resumes an interrupted run on its own; `review man failures`
covers automatic retries.
