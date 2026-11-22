# Steering a session

`echo "<message>" | review message <ID>` delivers stdin to a session, whatever
it is doing. The ID is the one printed as `session: <ID>` above a run's
response. IDs used to launch a turn must be 1-128 ASCII letters, digits,
dashes or underscores. Historical rows remain readable even if their IDs
cannot be launched.

- **Idle session**: the message continues it, as a new turn.
- **Codex, claude or maki turn in flight**: that turn is interrupted first - cut off,
  and whatever it had not finished is lost - and the message then continues the
  session. If the turn finishes before the interrupt lands, its answer stands
  and the message continues after it.
- **Grok turn in flight**: refused. Grok has no way to stop a turn from outside;
  send the message once the turn ends.

`review interrupt <ID>` ends a codex, claude or maki turn without a message, waits
for its run to record the session, and prints the `review message` command that
continues it. A run already finishing waits for its exact handoff, without
sending another signal. Active runs from an older review without handoff
support are refused before signalling; wait for them to finish. Old recorded
sessions remain readable and can still be continued.

**Never stop or kill the `review` process to interrupt an agent.** A first
SIGINT or SIGTERM makes `review` try to end its codex, claude and maki turns
gracefully and record them, within a bounded wait. A second signal, SIGKILL, or
a harness that does not stop in time loses that record, and an unrecorded
session cannot be continued at all. Grok turns are not ended gracefully.

## What a message sends

Stdin as-is: no archetype prime, no profile. The session keeps the harness,
sandbox level, model and effort recorded for it, and each is announced
(`sandbox: workspace-write (inherited from the session record)`). There is no
`-a` or `-p`. A maki session records no effort, and maki itself
resumes at the thinking level its own config sets, else the one it last used
anywhere on the host, not the one the session ran at.

It does **not** keep the profile's `env` or codex `config` overrides: the
session record holds env names only and no overrides, so there is nothing to
restore them from. A session that relied on one - a custom endpoint,
`CODEX_HOME`, `GROK_HOME`, a model-provider override - runs its follow-up
without it. Writable roots are carried over as described in
`review man sandbox`.

The message runs in the project the session was started in, whatever the
current directory. With `-C`, the selected project must be that one. Before
interrupting anything, `review` checks that the project directory still
exists, that its `.review.toml` parses, and that its audit id is the one the
session was recorded under; if any check fails, nothing is interrupted.

## Which sessions can be messaged

One this host's `review` recorded, or a fresh run of this host's still in
flight that has not recorded yet (its record is written when it returns, and
the message waits for it after interrupting). The harness is read from the
record. An ID with neither is refused; `review sessions` lists what this host
knows.

A message pays off only while the harness's prompt cache is warm, so a session
idle too long is refused:

| Harness | Refused when last touched more than |
|---|---|
| codex | 27 minutes ago |
| claude | 55 minutes ago |
| grok | 55 minutes ago |
| maki | 55 minutes ago |

These are refusal thresholds, not a promise that the cache is warm before
them. A session's clock is reset by every turn that ran, whether or not the
turn produced an answer, since reprocessing the session warms the cache either
way. A record with no usable timestamp is not refused.

When a session is refused as cold, start a fresh run and restate in its
instructions what the new session needs to know: the question, what has been
established, what is left.

## Several messages at once

Messages to one session are serialised up to their launch. A second message
waits until the first has launched, then finds its turn in flight: on codex,
claude or maki it interrupts it, on grok it is refused, and if the first turn has
already finished it simply continues the session. While the first message is
still waiting to launch (behind the host-wide launch lock), `review sessions`
shows it as `[queued]`. A message arriving just after the first has launched,
before the harness has reported its process, waits briefly for it rather than
failing.

## Exit status

`1` if the turn produced no answer, including an interrupted one; `3` if
anything it started was left running, which outranks `1`; `0` otherwise, and
for `review message --dry-run`, which sends nothing. An interrupted run is never auto-resumed (see
`review man failures`).

A session whose earlier `review` died while that run's processes were still
being torn down shows as `[cleanup pending]` in `review sessions`, and a message
or interrupt is refused until the teardown ends.
