# Teardown: nothing a run started outlives it

Provider-agnostic: every provider process `review` starts - codex, claude and
grok runs, auto-resumes, grok wake turns, `review message`, and the incident
bundle's version probe - runs under a **keeper** (`src/keeper/mod.rs`), launched
through `contained::spawn` (`src/contained.rs`). There is no other spawn site
for a provider.

## Why

A codex workspace-write run launched two Python codemods from a shell inside
codex's sandbox. Both hung. codex finished, `review` exited 0 with
`captured: true`, and the scripts kept running flat out for about five and a
half hours, reparented to the user's systemd, with write access to the project
the whole time. `review`'s exit, which orchestration procedures treat as a true
signal that the run is over, was false.

`review` already spawned codex and claude in a process group of their own and
killed the group on the watchdog and signal paths. That could not have helped,
for two reasons:

- **Descendants leave the group.** In the checked-in codex source
  (`research/codex`): `linux-sandbox/src/bwrap.rs` passes `--new-session` to
  bwrap; `linux-sandbox/src/linux_run_main.rs` forks bwrap's child with
  `setpgid(0, 0)`, and its `PR_SET_PDEATHSIG` ties that child to
  `codex-linux-sandbox`, not to codex, so nothing ties `codex-linux-sandbox`
  to codex; `utils/pty` `setsid()`s every unified-exec PTY. An agent's shell
  is therefore in a session and group of its own, and when codex exits it is
  orphaned to init.
- **A normal exit did no teardown at all.** The group was only killed when
  `review` itself ended the run.

## Rejected designs

**`review` as one child subreaper for all its runs.** Orphans would reparent
to `review` instead of init, but they could no longer be attributed: with
several runs in one `review` (a fan-out, an auto-resume beside other runs), a
worker that a still-running codex backgrounded and whose
shell then exited is an adopted orphan exactly like a finished run's leftover.
Sweeping orphans when a run ends kills live work belonging to another run.

**A cgroup per run.** `cgroup.kill` is the stronger primitive, but it needs a
writable, delegated cgroup. On the operator's hosts `review` runs from a logind
session scope (`/user.slice/user-<uid>.slice/session-<n>.scope`), which is
root-owned and not delegated, so a cgroup backend would never execute where it
matters; a systemd user scope needs a reachable user manager. An unexercised
second backend is not containment.

**A pre-main keeper hook** (an `.init_array` constructor dispatching before
`main`, so even the unit-test binary could act as keeper). Rust documents `std`
before `main` as unsupported, the default panic hook can block on a full stderr
before `catch_unwind` runs, and other constructors can create threads first. The
crate was split into a library and a thin binary instead, so the runner tests
are integration tests using `CARGO_BIN_EXE_review` as their keeper (a bin
crate's unit tests are not given that path - checked).

## The keeper

One process per invocation, between `review` and the provider: `review __keep
<spec-fd>`, dispatched by `main` before any tokio runtime or signal
supervisor exists. The spec is read from a sealed anonymous memory file (16 MiB
internal transport limit), then its descriptor is closed before provider creation.
Only its descriptor number is in argv; environment values are never put there.
It is synchronous and single-threaded, and owns every wait
status in its tree (one `waitpid(-1)` loop), so nothing competes with it.

- **Subreaper.** `PR_SET_CHILD_SUBREAPER`: whatever a provider descendant does
  to its session, group or ancestry, it stays a descendant of the keeper, and
  the keeper belongs to exactly one run.
- **Proof of emptiness is `ECHILD`.** As the nearest living ancestor of every
  process in its tree, the keeper has a child for as long as any of them
  lives. `waitpid` returning `ECHILD` after reaping is the only accepted proof
  that nothing is left. Discovery (walking `/proc/<pid>/task/<tid>/children`,
  falling back to a `/proc` scan without `CONFIG_PROC_CHILDREN`) only finds
  signal targets: the kernel documents that walk as unreliable while siblings
  exit, and a leader that called `pthread_exit` has no readable task list, so
  an empty walk proves nothing and a failed one is never terminal.
- **Identity through pidfds.** Signals go only through pidfds. A direct child is
  admitted by pid (only the keeper can reap it). A process found under an
  admitted parent is admitted when, after its parent was read from
  `/proc/<pid>/stat`, both its pidfd and the parent's still show them alive: a
  process alive at the end of the check held its pid throughout, so the parent
  read named the admitted parent. Admitting by a parent's pid number alone
  admits strangers once that number is reused. pidfd liveness is also right
  for a zombie leader whose other threads still run, where `/proc` state is not.
- **Escalation.** SIGTERM and SIGCONT (a stopped process cannot act on SIGTERM)
  to every member as it is found, for a grace; then SIGKILL in rounds with a
  full beat between them (a SIGCHLD does not cut the pause short), leaves first
  for the first half of the kill phase and every known member after that; at
  the deadline, everything known, then two more beats of finding and killing
  what appears. SIGTERM reaches a supervisor and its child together, which is
  fine for codex's: `codex-linux-sandbox` forwards a SIGTERM to its bwrap child
  and still waits for it before cleaning up (`linux_run_main.rs`). Leaves first
  in the kill rounds covers a supervisor that the SIGTERM grace did not end, so
  it can reap a killed child and clean up: `codex-linux-sandbox` removes the empty
  placeholder files and directories it created in the workspace as mount
  targets only after its bwrap child exits, and killing it first leaves them in
  the tree. It is only half the phase because a parent that forks a new child
  whenever its last one dies (a shell loop around `sleep`) is never a leaf:
  leaves-first alone killed its children round after round, and a child it
  forked just before its own final kill was left alive, adopted, unseen. One
  absolute deadline covers both phases, and discovery stops at it (reserving
  time to signal what it found), so a wide tree cannot stretch the teardown.
- **Startup.** `review` spawns the keeper with SIGINT, SIGTERM, SIGCHLD and
  SIGPIPE blocked (the mask survives exec), and the keeper reads them from a
  signalfd, so no signal can take its default action in a startup window. The
  provider is forked by the keeper itself with the exec status on a
  close-on-exec pipe in the same poll set, and a startup deadline fails a
  provider that never execs, so a stuck exec cannot hold the global launch lock.

## Protocol

The shared `keeper::Launcher` prepares the signal mask, process group and
inherited descriptors for both production and protocol tests. Provider program,
argv, cwd and environment preserve Unix bytes. Parent-side environment intent is
explicit: inherit with ordered sets/removals, or exact entries. It is resolved
once before spawning; the wire contains only the exact environment. The keeper
keeps its own environment and sends the provider's through execve. PWD is forced
to the byte-preserving cwd. PATH lookup uses that exact environment and child cwd;
empty and relative components, including literal ~, resolve there. An absent PATH
uses /bin:/usr/bin, while an empty PATH searches only the child cwd. Inputs with
NULs or invalid environment keys are refused before spawn.

Two pipes beyond stdio, numbered at least 3 so a spawn's stdio setup cannot
overwrite them, their numbers in the spec, private to the keeper once it starts
(the provider inherits neither):

- **control**, `review` to keeper: `g` launch, `i` interrupt, `t` terminate.
  EOF means `review` is gone. Nothing launches before `g`, so `review` records
  the keeper's identity in the in-flight marker first.
- **events**, keeper to `review`, one JSON line each: `spawned`,
  `exec_failed`, `exited` (the provider itself has been reaped), one
  `survivor` per process still alive, one `killed` per ended process named in
  accounting, and compact `done` with the verdict and both event counts. The keeper
  queues events and writes on writability, so backpressure never blocks a
  teardown; only a closed reader means `review` is gone. `review` reads them
  concurrently from launch.

An interrupt is forwarded as SIGINT through the provider's pidfd while it runs;
after a successful delivery later requests are ignored, and none is forwarded
after the provider exits. Done reports successful delivery, not proof that the
provider handled it. A damaged or missing Done leaves delivery unknown. Caller
stop intent and interrupt-request intent remain separate and are preserved even
when delivery cannot be confirmed.
`review interrupt` signals the keeper (the owning `review`'s child, leader of
its own group, as the marker records it), which forwards.

## What a run waits for

`exited` and the keeper's own exit are separate moments, and the difference is
load-bearing. The codex watchdog judges whether *codex* refuses to exit; it
waits on `exited`, then the runner waits out the teardown. Waiting on the keeper
alone made a leftover that needed the SIGKILL rounds look like a stranded codex,
so a clean run was killed by the watchdog and filed as an incident. The
in-flight marker stays until the keeper is reaped, then remains handoff pending
through recording, so a `review message`
arriving during the teardown finds the session in flight instead of launching
beside its leftovers.

The keeper constructs one typed Teardown verdict: Clean, Survived (named
processes and whether others may remain), or Unknown (all reasons and known
survivors). Only ECHILD proves Clean. Without that proof, discovery cannot rule
out unseen members even when every read succeeded; surviving reports therefore
conservatively set the incomplete flag. Orphan logging uses this same immutable
verdict rather than discovering survivors again.

A run's verdict is complete only with a valid final `done`, matching counts of
`survivor` and `killed` events, unique final survivor PIDs, and the keeper reaped
with a normal exit. The process-event streams carry identities; Done carries
counts rather than an unbounded duplicate list. Killed records cover different
times, so PID reuse within them, or between killed and survivor records, is valid.
Each physical event is bounded; the total number of survivors is uncapped.
Duplicate terminal events, events after Done, reader failures, truncated lines and contradictory verdicts make the
outcome unknown. Cancellation aborts the owned reader and preserves every
already collected process record. Anything less - a
dead keeper, truncated or malformed events, a run abandoned before its teardown
was confirmed (`Contained` dropped unfinished, which also tells the keeper to
terminate) - is an unknown outcome, never a clean one.

## Recording and reporting

The persisted leftovers shape stays flat for existing audit and sidecar readers;
conversion from the live verdict is explicit. Joining independent invocations
preserves every unknown reason in the existing unknown string and every survivor
record; a reused PID across invocations is not evidence of a duplicate process.
The historical killed field means observed ended during cleanup, not proof of
signal causality.

Every invocation's non-clean verdict goes into its command's ledger
(`supervisor::Lifecycle::unresolved`), whichever result is reported: an auto-resume, a grok
wake or another provider's answer cannot hide a teardown that failed. The run's
audit and sidecar rows carry `leftovers`: what was killed (capped, with a
dropped count), what survived, whether enumeration was incomplete, or why the
outcome is unknown. Cleanly killed leftovers are recorded and never printed:
nobody can act on them, and nothing explains why an agent left them. Survivors
and unknown outcomes are printed, every survivor named, before the `runtime:`
line, and the command exits 3, which outranks the 1 of a run with no answer
(`app::exit_status`). A codex death with a non-clean teardown is not
auto-resumed.

`app::run` creates one `supervisor::Lifecycle`, shared through ProviderRuntime
by fan-out attempts, automatic retries, Grok wakes, messages and incident version
probes. It owns stop notification, the initialized keeper registry/admission gate,
the recording obligations and the ledger. A fresh default runtime creates an
independent domain for tests; cloning a runtime preserves its owner. Contained
handles retain that owner through finish and abandonment. Completion publishes
non-clean evidence before removing its exact control-pipe registration. Report
formatting lives in `report.rs`; the bounded version probe lives beside the
incident writer in `incident/probe.rs`.

The command installs one signal supervisor capturing that lifecycle. Its first
signal closes admission and publishes stop under the same mutex used for spawn
and recording-guard registration. Existing interruptible turns retain recording
guards through their sidecar writes; guards cannot register after stop. The count
then determines whether to wait gracefully. Escalation separately requests termination of every
live keeper, waits up to the maximum bound registered with the currently live
keepers, prints the ledger and names any keeper
still tearing down. Process signal registration remains process-wide; lifecycle
state has no process-wide mutable fallback.

Admission closure, graceful notification and completed teardown are distinct.
The registry's closed latch is the authority for keeper admission; stop watch
reads are notification snapshots, never permission to launch. The shared run's
early check avoids remaining preparation after an observed stop; Grok's wake
check instead preserves its answer while failing unread background results.
The app uses the notification to print a continuation hint after recording.
Forced termination can close admission without publishing graceful notification;
the signal supervisor deliberately requests graceful stop before escalating.
Escalation requests termination and does not prove completion. No phase enum is
needed by these callers: each keeper's completion and ledger supply that evidence.
The gate serializes keeper admission, not provider execution. An admitted keeper
may still receive `go` after stop; it retains its owner and is handled by the
run's stop arm or supervisor escalation.

TeardownSchedule is serialized in the spec and owns execution deadlines (term,
leaves-first, kill, two settle beats) and parent waiting allowances (flush plus
two transport beats). A terminate during cleanup does not restart its deadlines;
its waiting allowance covers either possible grace. These are cooperative
schedules and enforced parent wait limits, not guarantees against blocking
syscalls. The version probe has a separate whole-operation budget, followed by
the admitted teardown allowance; startup still means time to exec.

Shared pidfd and `/proc` identity primitives live in `process.rs`. The keeper
retains tree-membership policy and discovery; the interrupt verb retains its
keeper-ownership checks. ProcessIdentity separates Alive/Gone/Unknown observation
from pinned signalling authority; unknown failures preserve markers, and signalling
requires a recorded start plus a pinned owner/keeper ownership check. Keeper
admission still proves tree membership independently; a marker is not that proof.
`inflight.rs` remains the marker/request/launch-lock store:
its lock protocol and the host-wide launch lock are unchanged.

## Limits

- **Linux 5.3 or later**, for `pidfd_open` and `pidfd_send_signal`. A keeper
  that cannot open a pidfd for the provider it just forked kills it and fails
  the launch rather than run it unsupervised.
- **A `/proc` read can block** on a process stuck in uninterruptible sleep
  holding its memory lock (a hung FUSE or NFS mount); the deadlines are checked
  between reads, so such a read can hold the keeper past them.
- **Daemons an agent starts are its leftovers.** A build cache server or a
  build tool's daemon the run spawned is ended with the run, and the next run
  that wants one starts it again.
- **Owner death.** If `review` is SIGKILLed or crashes, each keeper sees its
  control pipe close and tears its run down anyway, and appends its outcome to
  `teardowns.jsonl` (`teardowns-private.jsonl` for a private project) beside the
  sidecar logs, best-effort. Nothing reads that log back, and an outcome that
  reached the events pipe just before `review` died is in neither place. While
  such a keeper is still tearing down, the session's marker reads as cleanup
  pending and `review message` and `review interrupt` refuse it; once the keeper
  is gone that protection ends.
- **Processes that are not descendants** - started through `systemd-run`, D-Bus
  activation, or anything else that asks another process to spawn them - are
  outside every keeper's tree.
- **What SIGKILL cannot end** (a task in uninterruptible sleep, a process the
  keeper may not signal) is reported as surviving.

## Tests

`tests/provider_runs.rs` drives `invoke` and the complete adapters against stub providers with the
built binary as keeper: a SIGTERM-ignoring script in its own session (the
ticket's case), a double-forked daemon, a pipe-holding leftover, a keeper that
dies (unknown outcome), a provider that cannot exec. `tests/keeper.rs` drives a
keeper over its protocol for owner death (tears down, writes the orphan log) and
for an owner gone before `g` (launches nothing). Both use the production
Launcher. Protocol regressions also cover byte preservation, exact environments,
large spec transport, child PATH/cwd resolution and successful interrupt delivery.
Pure policy tests exercise synthetic tree targeting, kernel-emptiness verdicts,
independent-invocation joins and schedule bounds; containment tests exercise
terminal-report validation and historical flat rows. Membership admission itself
requires live kernel identity checks, covered through real-process containment
rather than simulated pidfds.

Lifecycle unit tests cover independent runtime domains, clone sharing, stop and
recording registration ordering, late stop subscribers, exact registration
ownership, poisoned-state evidence preservation and admission refusal. Existing
runner stubs exercise graceful stop with real containment using the same lifecycle
as production. Run::wait retains its stop arm in both provider and keeper phases.

### Review data location

`data::DataDir` snapshots the parent process's XDG data base once, with HOME's
`.local/share` fallback. It derives the review sessions, audit, incidents,
inflight and teardown locations; injected domains have the same layout.
Unavailable storage remains unavailable, without retrying ambient resolution.
`LogScope` binds an ensured audit identity to its public/private log family.
Runtime retains that family and its DataDir; the orphan destination is derived
and owned before keeper spawn, rather than configured independently of markers.
The keeper still receives the owned destination in Spec. Provider homes remain
provider-owned. The host launch lock remains at temp_dir()/review.lock and is
outside DataDir. Inflight's existing launch-lock protocol is unchanged.

## Session handoff ownership

Each invocation reserves its namespace before spawn, including fresh Codex
before its session ID is learned. Reservation failures in resolved storage refuse
launch. Markers are atomically published at <session>.<run_id>.json with one
owner identity, one keeper identity and separate signal readiness. The legacy
child_pid field is decoded only as readiness when it equals the keeper; an old
marker lacks the new protocol capability and cannot be interrupted by this build.

Lease closure consumes exact <session>.<run_id>.interrupt requests and changes
markers to HandoffPending without unlinking them. The invocation reservation
registers only paths it exclusively created, and ProviderResult retains it through
recovery and recording. After atomic receipt publication the app removes those
pending markers. Final reservation drop cleans owned paths on other paths. Raw
stream IDs remain evidence; only checked SessionId values reach marker builders.

An automatic recovery explicitly names its predecessor. The state resolver can
select the successor from exactly two matching owner/session/project/provider
observations when the predecessor is pending; two Running markers or unrelated
observations are ambiguous. This never redirects a captured receipt waiter.

Receipts contain no prompt or answer. They identify the requested invocation,
its interruption outcome, the final session and final sidecar append success.
All recording attempts precede every receipt. Publication is atomic and refuses
replacement; reads are bounded and check schema and run identity. Owner death
requires a final receipt read before an incomplete-handoff refusal. Completed
receipts age out after a day. Abandoned exact requests/reservations may remain as
namespace tombstones; observation never deletes a request because a non-atomic
directory scan missed its marker. Legacy session-wide request consumption remains
a weaker compatibility facade, separate from production exact targeting.

Audit and session JSONL appends share an exclusive flock on the log inode.
The lock covers tail inspection, newline repair and the entire append, including
short writes. An incomplete tail is isolated by a newline without deleting any
old byte. Shared reader locks snapshot bytes; session decoding splits physical
lines before UTF-8/JSON parsing so a partial multibyte character damages only
its own fragment. A successful sidecar append therefore supplies a complete,
readable row among cooperating writers. Old builds do not acquire these locks;
no cross-version concurrent-writer atomicity or fsync durability is claimed.
These log locks are independent of the frozen launch-lock policy.
