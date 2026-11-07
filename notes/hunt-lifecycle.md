# Hunt: lifecycle (keeper, contained, inflight, interrupt)

Scope read: `src/keeper.rs`, `src/contained.rs`, `src/inflight.rs`,
`src/interrupt_cmd.rs`, `tests/keeper.rs`, plus the call sites they reach in
`src/provider.rs` (signal supervisor, `Unrecorded`, the claude and codex
runners), `src/app.rs` (`run_message`) and `src/sessions.rs` (`read_all`).

The short version: the keeper itself is the best-built part of the code base
and its core needs no rewrite. The trouble is around it. The run lifecycle
(stop requested, launched, signal-safe, interrupted, provider exited, torn
down, recorded) is a state machine that no type owns. Its pieces are spread
across four process-wide statics in two modules, an on-disk marker whose
fields are reused for meanings their names do not say, a request file used as
a completion signal, and a per-runner copy of the same choreography in each of
the claude and codex runners. Adding maki and opencode, both SIGINT-interruptible
with stream-derived outcomes, means copying that choreography a third and
fourth time and editing at least three hard-coded provider lists.

---

## 1. Bugs and surprises

### 1.1 `interrupt_cmd` decides owner liveness by bare pid, unlike every other reader

`read_live` decides "is the owning `review` alive" with
`identity_alive(marker.pid, marker.pid_start)`, which is pidfd plus start time.
`interrupt_cmd::interrupt` answers the same question twice, in both wait
loops, with `inflight::pid_alive(marker.pid)`, which is `kill(pid, 0)`. If the
owner dies and its pid is reused while the interrupt waits, `pid_alive` stays
true. The first loop (`while interrupt_pending`) then never ends, because
nobody is left to consume the request. The second loop never ends either:
no rows come, and the "owner exited" bail never fires. `review message` calls
the same function, so a message can hang behind it forever. Fix: one liveness
predicate per recorded identity (see 2.1). `pid_alive` should not be public.

### 1.2 `Marker::child_pid` is not the provider's pid; it is the keeper's again

Both runners store `Some(contained.pid())`, the keeper pid, in `child_pid`
(provider.rs, claude runner `let pid = Some(contained.pid())`, codex runner
`let child_pid = Some(keeper_pid)`). `record_keeper` has already stored the
same pid in `keeper`. The field's doc still says "pid of the provider process
`review` spawned - the leader of its process group". So `child_pid` really
means "the run is now safe to signal", carried as a duplicate pid.
`interrupt_cmd` relies on the two being equal without saying so. It checks
the identity of `child_pid` against `marker.keeper_start`, which is only right
because `child_pid == keeper`. If someone "fixes" `child_pid` to hold the real
provider pid (the `Spawned` event carries it, and `go()` returns it unused),
the start-time check breaks silently, and the `ppid == marker.pid` check fails
because the provider's parent is the keeper. Make it
`signal_ready: bool` (or a `Readiness` enum) and keep exactly one keeper
identity.

### 1.3 `contained::spawn` loses `env_clear`, and args are converted lossily

`spawn` rebuilds the keeper's `Command` from `get_envs()`, which reports
explicit sets and removes but not `env_clear()`. A runner that clears the
environment gets a keeper, and so a provider, that inherits all of `review`'s
environment, secrets included. No current runner calls `env_clear`. opencode's
mandatory env isolation is where someone will reach for it, and nothing will
fail when they do. The same function also passes `program`/`args` through
`to_string_lossy`, so a non-UTF-8 path or argument (a workspace path, say)
reaches the provider mangled. Fix: the keeper's `Spec` should carry the
provider's environment explicitly (a full `Vec<(OsString, OsString)>` sent
as bytes, or an `Inherit | Exact(map)` enum) and its argv as bytes. Then the
keeper calls `execve` with exactly what the runner built, instead of the
runner's `Command` being reverse-engineered through the accessors tokio
happens to expose.

### 1.4 opencode's `PWD` requirement will not be met by `current_dir`

`spawn` applies `get_current_dir()` to the keeper, and the provider inherits
the cwd through fork. `PWD` is whatever `review` had. opencode takes its
directory from `PWD`, so every opencode runner must remember to set
`PWD` alongside `current_dir`. That belongs in the spawn primitive: when a cwd
is given, set `PWD` to match, for every provider. It is harmless for the
others and correct for shells.

### 1.5 The supervisor's teardown wait uses default settings, not the run's

`install_signal_supervisor` waits `Settings::default().terminate_bound()`, while
keepers run with `runtime.teardown` (injectable). In tests with fast settings
this only over-waits. But it shows the bound being computed apart from the
settings that actually paced the keepers (see 2.4).

### 1.6 A keeper interrupt outside `Running` is dropped without a word

`Keeper::interrupt` ignores `i`/SIGINT unless `phase == Running`. A SIGINT
during `Starting` (between fork and exec) is lost, and `review interrupt` then
waits for the run to finish on its own and reports "ended before the interrupt
took effect". The readiness gate in the runners makes this unlikely today. But
the keeper never reports whether it forwarded anything (its `interrupted` flag
never leaves the process). So `review` cannot tell "delivered" from "dropped",
and that is the root of 3.2.

### 1.7 Smaller

- `Leftovers::merge` keeps the first `unknown` reason and drops the second
  without a trace. Two unknowns (a probe and a wake) report as one.
- `report_lines` says "at least 1 process" when `incomplete` is set with zero
  named survivors. `n.max(1)` there is a second decision about what `incomplete`
  implies (see 2.3).
- `interrupt_cmd`'s refusal hard-codes the prose "only codex and claude runs
  can be interrupted", a third copy of the interruptible list (see 2.2).
- `version_probe` launches a probe per incident with the run's `Settings`, but
  bounds the whole probe by `settings.startup`, which means "time to exec" to
  the keeper. That reuses a unit for a different meaning.
- `tests/keeper.rs` hand-builds the spawn (pre-exec hook, fd un-cloexec, spec)
  that `contained::spawn` does. `keeper_given` even omits
  `block_handled_signals`, so it tests a keeper started differently from
  production. A `keeper::Launcher` used by both would remove the copy.

---

## 2. Decisions made in more than one place

### 2.1 "Is this recorded process still the one we recorded?"

Sites:
- `inflight::identity_alive(pid, Option<start>)`: pidfd, then stat, then
  pidfd. Falls back to `pid_alive` when the start is unknown.
- `inflight::pid_alive`: `kill(0)`, used directly by `interrupt_cmd` (1.1).
- `interrupt_cmd::interrupt`: its own identity check for the keeper. It uses
  pidfd, `getpgid == pid`, `ppid == marker.pid`, and `keeper_start` if known,
  then pidfd again.
- `keeper::admit`: its own parent/child proof.
- `tests/keeper.rs::alive`: another.

They already disagree (1.1). The single owner should be a type:
`ProcessIdentity { pid: Pid, start: StartTime }` with
`fn pin(&self) -> Option<PinnedProcess>`, where `PinnedProcess` holds the pidfd
and is minted only after the start time has been re-verified. Signalling goes
only through `PinnedProcess`. Then "signal a recorded pid" cannot be written
without the identity check, and a bare-pid `kill` cannot be written at all. The
marker's three `(u32, Option<u64>)` pairs (`pid`/`pid_start`,
`keeper`/`keeper_start`, `child_pid`/none) become `owner: ProcessIdentity`,
`keeper: Option<ProcessIdentity>`. A newtype `Pid(NonZeroU32)` also removes
the "pid 0 would signal our own group" defensive comment in `interrupt_cmd`.

### 2.2 "Can a run of this provider be interrupted and resumed?"

Sites:
- `interrupt_cmd::interruptible`: `matches!(provider, "codex" | "claude")`.
- `provider::Unrecorded::for_provider`: `matches!(provider, "codex" | "claude")`
  with its own doc giving the same reason.
- The prose in `interrupt_cmd`'s refusal.
- Implicitly, every runner that does or does not write a readiness flag.

`interruptible`'s doc claims it is "shared ... so the two cannot disagree". But
`Unrecorded` is a second copy that the comment does not know about. maki and
opencode both belong in the list, and a missed edit to `Unrecorded` means a
SIGTERM ends their runs without waiting for the row, which makes the session
unresumable. The owner should be a `Provider` enum (replacing the
`provider: String` on `Marker`, `SessionRecord`, `KNOWN_PROVIDERS`) with a
capabilities table: `interrupt: Option<InterruptStyle>`, where
`InterruptStyle` covers SIGINT-ends-turn (codex, claude, maki, opencode)
against none (grok). `Marker.provider: String` parsed from disk becomes
`Provider`, with an `Unknown(String)` arm for rows naming removed providers.

### 2.3 "What did the teardown establish?" (clean, survivors, incomplete, unknown)

Sites:
- `Keeper::outcome`: `clean = empty`,
  `incomplete = !empty && (!complete || survivors.is_empty())`.
- `Contained::finish`: re-derives `survivors` (empty if `outcome.clean`),
  `incomplete: !outcome.clean && outcome.incomplete`, and cross-checks the
  survivor count against the `Survivor` events.
- `Keeper::log_orphaned`: recomputes the survivor list from `members` instead
  of from what `outcome` decided.
- `Leftovers::is_clean` / `is_empty`.
- `report_lines`: `n.max(1)` for incomplete, plus a separate branch for unknown.

`Outcome` and `Leftovers` are both bags of booleans and counts in which most
combinations are meaningless (`clean: true, survivors: 3`,
`incomplete: true` with `clean: true`). Each consumer defends against the
nonsense combinations its own way. Replace both with one type, owned by
`keeper` and used unchanged on the wire, in the rows and in the ledger:

```rust
enum Teardown {
    Clean { killed: Killed },
    Survived { killed: Killed, named: NonEmptyOr<Vec<Proc>>, more_unnamed: bool },
    Unknown { why: UnknownReason, known_survivors: Vec<Proc>, killed: Killed },
}
```

`Killed { named: Vec<Proc>, dropped: usize }` carries the `KILLED_MAX` cap.
`UnknownReason` should be an enum (`KeeperDied(status)`, `ReportCutOff`,
`Malformed`, `Abandoned`, `ProbeTimedOut`, ...) rather than prose: rows can
then be grepped by kind, and the exit-status mapping can branch on it.
`merge` becomes a lattice join (`Unknown > Survived > Clean`) defined once.
The survivor event stream can stay for incremental delivery. But `Done` should
carry the final verdict, and `finish` should only check it against the stream,
not re-derive it.

### 2.4 "How long can a keeper take?"

Sites:
- `Keeper::teardown`: the actual schedule (term grace, kill phase with a
  leaves-first half, two settle beats) plus `finish`'s `flush_ms`.
- `Settings::terminate_bound`: `terminate_grace + kill_phase + flush + 4 beats`.
- `Contained::flush_bound`: `flush + 4 beats`.
- The supervisor, which uses `Settings::default()` (1.5).
- `version_probe`, which uses `startup` as a whole-probe bound.

The bounds are hand-added copies of the keeper's schedule. If the keeper
gains a phase, the supervisor exits while keepers are still tearing down, and
reports them as "still in progress". Owner: a `TeardownSchedule` value in
`keeper` that the keeper executes and that exposes `worst_case(Start)`, where
`Start` is `Leftover` or `Terminate`. `Settings` should hold that value and
not loose durations, and the supervisor should read the live settings from
the runtime.

### 2.5 "Is shutdown under way?"

Sites:
- `provider::STOP` (watch channel), read through `stop_requested()` and
  through `runtime.stop.borrow()` in each runner before launch.
- `contained::Registry::closed`, set by `terminate_all`.
- `contained::spawn` checks both.

Today the order of operations keeps them consistent: STOP is set first, and
`closed` follows after the graceful phase. But they are two flags for one
fact, read at different points. The runners' own pre-launch
`if *runtime.stop.borrow() { bail!("not launched: ...") }` repeats the check
`spawn` already makes, with the same message text copied. One `Shutdown` state
(`Running | Draining | Closed`) owned by the lifecycle module would remove the
copy.

### 2.6 "How do we stop this run when `review` is signalled?"

Sites: the claude wait loop and the codex wait loop each contain the same arm:

```rust
Ok(_) = stop_rx.wait_for(|stop| *stop), if !stopped_by_signal => {
    stopped_by_signal = true;
    if *first_seen.borrow() { contained.interrupt() } else { contained.terminate() }
}
```

Each also has its own "first output" watch channel (`first_tx` in claude,
`first_output_tx` in codex), its own marker task parked on
`future::pending()`, and its own
`take_interrupt_request(..) || stopped_by_signal`. For maki and opencode this
is the part that gets copied. Owner: `Contained`. It should be told
`signal_ready()` once (which also writes the marker's readiness, 1.2), and
expose `stop_gracefully()`, which interrupts or terminates by that readiness.
The supervisor's stop should then reach every contained run through the
registry, not through a per-runner `select!` arm.

### 2.7 "Was this run interrupted?"

Sites:
- The `.interrupt` request file, written by `interrupt_cmd` before signalling
  and consumed by each runner.
- `stopped_by_signal` in each runner.
- The keeper's private `interrupted` flag, which is never reported.
- `interrupt_cmd`'s row scan for `digest.interrupted`.
- The cleanup of stale requests, in two places: `read_live` drops orphaned
  requests, and each runner drops a leftover request at resume start
  (`take_interrupt_request` before launch, claude and codex separately).

The keeper is the only party that knows for certain whether it delivered a
SIGINT to the provider while the provider ran. It should report it
(`Event::Interrupted { by: Control | Signal }`, or a field on `Done`). Then
"interrupted" is one fact from one source, and the request file is reduced to
what only it can do: telling the owner that an external SIGINT was operator
intent. Even that can go if `interrupt_cmd` writes `i` down a path the owner
reads (see 3.3).

### 2.8 "Which marker is this run's?"

`still_ours` compares `run_id` read from disk. It is called before each
rewrite and before removal, with an acknowledged check-then-act race. Two
runs of one session share a path because the path is keyed by session alone.
If the marker path were `<session>.<run_id>.json` (or a directory per session
with one file per run), "ours" would hold by construction. `read_live` would
group by session, and the race and `still_ours` would disappear. The
launch-lock argument that makes the race "unreachable" is a cross-module
invariant, not a local one.

---

## 3. Axes that should be types

### 3.1 Session IDs

`session_id: &str` flows everywhere. Its only validation,
`is_safe_filename_component`, runs three separate times in `inflight` (mark,
launch lock path, interrupt path), each quietly degrading differently: a
skipped marker with a warning, `Ok(None)`, and an `InvalidInput` error. A
`SessionId` newtype minted once at the CLI/sidecar boundary (allowlist,
length), with provider-specific shape as an associated check (opencode must
start with `ses`, and maki/claude IDs are UUIDs chosen up front), would make
every path in `inflight` infallible. It would also let opencode's
`ses_`-prefix rule live in one place. The current allowlist does admit
`ses_...`.

### 3.2 Marker state

`Marker.cleanup_pending` is a `#[serde(skip)]` bool set by the reader, which
mixes a stored record with a computed verdict. Callers (`interrupt_cmd`,
`app::run_message`, the sessions listing) each branch on it, and each has its
own refusal. `read_live` should return `Live { marker, state: RunState }`,
with `RunState = Running { signal_ready } | CleanupPending { keeper }` (plus
`Queued` from the launch lock, now a separate `pending_launches` list joined
by string). `interrupt_cmd` and `run_message` then both match one enum. Today
`run_message` pre-checks `cleanup_pending` and `interruptible` and
`interrupt()` re-checks both, with different wording, so an agent sees two
different refusals for one condition depending on the verb.

### 3.3 The interrupt verb's outcome

`interrupt_cmd::Outcome { interrupted: bool, project: String, provider: String }`
is derived by scanning sidecar rows after a count baseline, with a "look one
more time" heuristic for auto-resume pairs. The failure modes are all
`bail!` prose: owner gone, keeper gone, no pid in time, not interruptible,
cleanup pending, queued. `review message` cannot branch on them except by
string. A typed `InterruptError` enum (`NotInFlight`, `Queued`,
`CleanupPending`, `NotInterruptible(Provider)`, `NeverReady`, `KeeperGone`,
`OwnerDiedUnrecorded`) and `Outcome = Interrupted { row } | FinishedFirst { row }`
would let `run_message` decide, for example, to proceed on `KeeperGone`
("finishing on its own") rather than fail.

### 3.4 Keeper protocol bytes

Control is raw bytes `b'g'`, `b'i'`, `b't'`, matched in `read_control` and
sent in four places in `contained`. A `#[repr(u8)] enum Control { Go, Interrupt, Terminate }`
shared by both sides costs nothing and keeps a fourth command from being
mistyped on one side. `Spec`'s six `*_ms: u64` fields are durations flattened
for JSON. Serialize a `TeardownSchedule` (2.4) instead.

### 3.5 Raw wait status

`Outcome.status: Option<i32>` is a raw `waitpid` status crossing a process
boundary, turned back with `ExitStatus::from_raw` in two places in `finish`.
An enum `Exit { Code(i32), Signal(i32) }` on the wire states what it is. For
opencode (exits 130 on SIGINT) and maki (exits 0 on error), runners must not
classify by exit status anyway. Making the status a plainly secondary,
typed field helps keep it that way.

---

## 4. Structure

### 4.1 The lifecycle has no home; give it one

What exists now, by location:
- `keeper.rs`: the subreaper (the child-side process). Cohesive and good.
- `contained.rs`: three unrelated jobs. (a) The parent-side keeper client
  (`spawn`, `Contained`, event reader). (b) The process-wide ledger and its
  human report (`report_lines`, presentation code). (c) The keeper registry and
  admission gate. Plus `version_probe`, an incident feature.
- `provider.rs`: `STOP`, `UNRECORDED`/`Unrecorded`, and
  `install_signal_supervisor`, all lifecycle code living in the provider
  module because that is where codex ran first.
- `inflight.rs`: three on-disk mechanisms that share a directory. (a) Markers.
  (b) Launch locks. (c) Interrupt requests. Plus the process-identity helpers
  (`identity_alive`, `pid_alive`) that the keeper and `interrupt_cmd` also need.
- `interrupt_cmd.rs`: the verb, plus `interruptible` and
  `cleanup_pending_message`, which `app` imports. These are policy owned by a
  CLI verb.
- The runners: the per-run choreography (marker before spawn, keeper recorded,
  launch, readiness, stop arm, request consumption, marker drop ordered before
  `finish`, label set late). The ordering is load-bearing, and the comments say
  so in both runners.

Proposed shape (one directory, `src/lifecycle/`):
- `process.rs`: `Pid`, `ProcessIdentity`, `PinnedProcess`, pidfd and `/proc`
  primitives (moved out of `keeper.rs`, which `inflight` and `interrupt_cmd`
  currently import them from, an odd edge: the subreaper's module is the
  library for "is pid X alive").
- `keeper.rs`: unchanged in substance, using `process.rs`. It owns `Spec`,
  `Control`, `Event`, `Teardown`, `TeardownSchedule`.
- `client.rs`: `spawn`/`Contained`. Extended into a `Run` handle that owns
  the in-flight marker, readiness, the graceful-stop decision, the interrupt
  fact and the label, so a runner's lifecycle shrinks to
  `let run = Run::start(cmd, session, provider).await?; ... run.signal_ready(); ... let end = run.finish().await;`.
  The marker/keeper ordering invariants then live in one function and are
  testable without a provider stub.
- `supervisor.rs`: `Shutdown` state, registry, `Unrecorded`, ledger, the
  signal handler. One module answers "what happens when `review` is told to
  stop".
- `store.rs` (from `inflight`): markers, launch locks, interrupt requests, keyed
  by `SessionId`, returning typed `RunState`.
- `report.rs`: `report_lines`, moved out of the ledger.
- `version_probe` moves to `incident.rs`, which is its only caller's domain.

That is a big move, and it is the right one. Each new provider then
contributes only its argv, env, stream parser and readiness trigger. Today it
would copy about 150 lines of lifecycle ordering per runner.

### 4.2 The sidecar log as an IPC channel

`interrupt_cmd` learns the result of the run it interrupted by polling
`sessions::read_all()`. That reads and parses both whole JSONL logs every 
`INTERRUPT_POLL`, for the life of the wait. The logs only grow, so the cost
grows with history. It also counts rows by position (`skip(rows_before)`),
which assumes no other process appends a row for the same session in between.
The owner already knows the answer when it consumes the request. It could
write a small `<session>.<run_id>.result` beside the marker (row reference,
interrupted or not) for the verb to read. Then the sidecar stays a log and
stops serving as a mailbox.

### 4.3 Parked tasks holding guards

Both runners keep the marker alive by moving the `Guard` into a task that
awaits `future::pending()`, then `abort()` plus `await` to drop it at the
right time. Comments in both runners explain why the await matters. That is
state (the marker) split from the behaviour acting on it (readiness updates)
across a task boundary, only because a fresh codex run learns its session ID
mid-stream. A `Run` handle (4.1) that owns `Option<Guard>` and accepts
`set_session(id)` and `signal_ready()` as plain method calls removes the task,
the abort and the ordering comments.

### 4.4 Process-wide statics and tests

The `LEDGER`, `REGISTRY`, `STOP` and `UNRECORDED` statics make the lifecycle
unobservable per test. Every test in a process shares one ledger, so a test
that leaves an unknown teardown poisons another's exit-status assertion. A
`Lifecycle` value created in `app::run` and passed down (or one per runtime)
would make the ledger and registry assertable in unit tests and remove the
`Option<Registry>` lazy-init dance.

### 4.5 Test layout

`tests/keeper.rs` covers what only a protocol-level test can: owner vanishing,
and terminate before go. That is right. But its spawn helper is a hand copy
of `contained::spawn` (1.7). The admission rules (`admit`, leaves-first
targeting, `outcome`'s verdict) have no unit tests: the only unit tests in
`keeper.rs` check `/proc` parsing. Separating the teardown policy (which
members to signal this round, given a snapshot of the tree, and the verdict
from `empty`/`complete`/members) from the syscalls would make it unit-testable
with synthetic trees. That matters because the verdict logic is exactly what
2.3 says is decided in several places.

---

## 5. What maki and opencode will touch in this scope

- 2.2: three provider lists (`interruptible`, `Unrecorded::for_provider`, the
  refusal prose), plus `"codex"`/`"claude"` literals passed to `inflight::mark`.
- 2.6/4.3: the whole stop/readiness/marker choreography, copied per runner.
  maki needs readiness after `init` (its SIGINT handler is installed by then),
  and opencode after its first event. Both are SIGINT-once, and the keeper's
  forward-once suits both, since maki's second SIGINT kills it.
- 1.3/1.4: opencode's env isolation and `PWD` go through a spawn primitive that
  currently loses `env_clear` and does not set `PWD`.
- 3.1: opencode's `ses` prefix and the caller-chosen IDs. Both new providers
  pick the ID up front, like claude, so their markers can go up before spawn.
  That path exists in the claude runner but not as a reusable piece.
- opencode without `--standalone` attaches to a background service outside
  the keeper's tree. The keeper cannot see that, and its "clean" verdict would
  be true but meaningless for whatever the service does on the run's behalf.
  The runner must make `--standalone` non-optional, and that rule is worth
  encoding in the opencode argv builder with a test, since nothing in the
  lifecycle can detect a violation.
- Exit status: opencode 130 on interrupt and maki 0 on error mean
  `Finished.status` must stay advisory. Typing it (3.5) and keeping outcome
  classification in the stream parsers keeps a new runner from treating
  `status.success()` as the answer.
