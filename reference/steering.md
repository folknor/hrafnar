# How message and interrupt coordinate on a session

This page explains how `review message` and `review interrupt` act on one
session without two turns running beside each other. The answer spans
`app::message`, `interrupt_cmd`, `inflight`, `sessions::query`,
`sessions::handoff` and `app::launch`, so it lives here; each piece's own rule
is on its code.

## The files a session has on disk

A session's state is spread over five stores, each written by a different
party:

- **Session row**: the sidecar log (`sessions`), appended by the run task as
  soon as a run ends and before its result is printed. It names the session's
  project, audit id, harness and inherited settings, and its epoch drives the
  stale-cache gate.
- **In-flight marker**: one file per invocation (`inflight::Guard`), written
  before the harness spawns whenever the session id is known. It names the
  owning `review`, the keeper pid once there is one, the run id, and its
  phase: `Running`, then `HandoffPending` once the harness has exited and the
  run is recovering and recording.
- **Readiness**: the marker's `signal_ready` flag, set once the harness has
  printed output. Before that, codex's node wrapper can die on `SIGINT`, so
  nothing signals it.
- **Interrupt request**: an empty file named for the session and the exact
  run id, written by the interrupter before the signal. The owning run
  consumes it once the harness has been reaped, which is how it tells an
  interrupt from a mid-turn death: the exit status is the same.
- **Handoff receipt**: `<run id>.result` (`sessions::handoff`), published by
  the owning run after every recovery and recording attempt. It says whether
  the turn was interrupted, which session it ended on, and whether the row
  was written.

The **launch lock** is a sixth file, `<session>.launch` beside the marker,
held by `review message` and never by a fresh run. `review sessions` shows a
held one as `[queued]`.

## What a message does

`review message <ID>` runs these steps in order:

1. Reads stdin, so a missing message never costs the turn in flight.
2. Takes the session's launch lock, waiting while another message holds it.
3. Captures the live markers and the rows, and classifies the session:
   in flight, handoff pending, cleanup pending, queued, idle or unknown.
4. Validates the target project from the row, or the marker of a fresh run
   that has no row yet, before signalling anything.
5. If a marker is live, interrupts that exact run id (see the next section)
   and rereads the rows, then validates the project again and requires it to
   match step 4.
6. Applies the stale-cache gate and inherits the settings from the latest
   row.
7. Takes the host launch lock and invokes the harness. Both locks are dropped
   the moment the harness spawns, when its marker already exists.

Because of step 7, a second message that waited in step 2 sees the first
message's marker in step 3 and interrupts it. A grok turn in flight is
refused in step 5, because grok has no interrupt lever. A marker whose owner
is gone while its keeper still tears down refuses the message, because a new
turn would run beside the old processes.

## What an interrupt does

`interrupt_cmd::interrupt_observed` acts on one observed run id, never on
whatever is in flight later:

1. If the marker is `HandoffPending`, the harness has already exited, so it
   only waits for the receipt.
2. Otherwise it waits up to `timings::INTERRUPT_PID_WAIT` for `signal_ready`.
   A second message meets this window every time, because the launch lock
   hands over at spawn, a second or two before the harness prints output. If
   the receipt appears meanwhile, the run ended and there is nothing to
   interrupt.
3. It reobserves the marker, pins the keeper by pidfd and checks it belongs
   to the marker's owner. A changed run id or owner means wait for the
   original run's receipt instead.
4. It writes the interrupt request, then sends `SIGINT` to the keeper, which
   forwards one `SIGINT` to the harness.
5. It waits for the receipt. If the owner dies without one, it reads once more
   before reporting the handoff missing, because publication can race the
   liveness check.

Printing the resume command any earlier than step 5 would hand you a command
that fails, since the row it needs is not written yet.

## Recovery and the receipt

A codex run that dies mid-turn is resumed once automatically under the same
owner. The replacement's marker names the first run as its `predecessor`, and
`sessions::query` resolves the two markers to the replacement. An interrupter
already holding the first run id keeps waiting on that run's receipt, which
the owner publishes for the abandoned attempt before the replacement launches.
Any other pair of live markers for one session reads as ambiguous, and both
verbs ask you to retry.

An interrupted run counts as failed for the exit status, is flagged
`interrupted`, and is never auto-resumed or bundled as an incident. A request
only counts when the run has no answer, because the harness can finish
between the request and the signal.
