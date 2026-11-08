# Consolidations

Questions the design hunt over `review` found answered in more than one place
(scopes: providers, lifecycle, config, orchestration, forensics, grok-man).
One entry per question, with every site that answers it and where the hunters
propose its single owner should live. Unverified.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.

The per-provider questions below are owned by `provider::Provider`
(src/provider/kind.rs) and the exhaustive matches beside it, described in
`notes/structural.md`. Each entry here keeps only its residue.

## CON-006 - Did codex's evidence hold a final answer?

Reported by: providers, orchestration, forensics.

The run-level question is settled: every runner builds one
`provider::Outcome` and every decision reads `has_answer`/`failed`. What
remains is codex's evidence readers, which still answer "is there a final
answer" each in their own way over the rollout and stream:
- `run_codex_json`'s stream parser: the last `item.completed` agent_message,
  used as fallback text, unphased (it can be commentary the user then sees as
  the answer under a death note).
- `transcript::parse`: the `final_answer` of the last turn (phase-tagged).
- `watchdog::has_final_answer`: any phase-tagged answer since the baseline.
- `incident::scan_transcript`: the last `task_complete.last_agent_message` is
  non-null.
- `run_session_show`: `task_complete && final_answer.is_none()` means "likely
  died mid-work".

The Stranded-kill and `scan_transcript` defects in `notes/bugs.md` are two of
their disagreements. Owner proposed: one rollout fold whose result feeds the
codex runner's `Answer` and every reader above.

## CON-007 - How did the run end, in words? (codex residue)

Reported by: providers, forensics, orchestration.

The digest's outcome lines have one renderer, `Outcome::lines`, used live and
by `review sessions <ID>`. Still separate:
- `run_codex_json`'s fallback text ("codex ended the turn", "interrupted by the
  operator", "codex produced no final message", "codex died without a final
  answer"), and claude's "(interrupted by the operator before a final
  answer)", which restate the outcome as response text.
- `run_session_show`'s "rollout has no final answer" note and its re-render of
  the transcript block `print_digest` renders.

`run_incidents`' verdict is deliberately separate: it reads bundle metadata,
whose `final_answer_present` is a tri-state the outcome cannot express (the
reasoning is at the site). Owner proposed: the placeholder text derived from
the outcome in one place, so maki and opencode do not write a third and
fourth copy.

## CON-009 - Was this run interrupted, and what does an interrupted run say?

Reported by: providers, lifecycle.

Sources of the fact:
- The `.interrupt` request file, written by `interrupt_cmd` before signalling
  and consumed by each runner.
- `stopped_by_signal` in each runner.
- The keeper's private `interrupted` flag, which is never reported.
- `interrupt_cmd`'s row scan for `digest.interrupted`.
- Stale-request cleanup in two places: `read_live` drops orphaned requests,
  and each runner drops a leftover request at resume start
  (`take_interrupt_request` before launch, claude and codex separately).
- `take_interrupt_request` after the wait, keyed on `marker_sid` in claude and
  on `watched_session_id` in codex; grok does neither (harmless while a grok
  request cannot be made, but a step a new runner will forget).

What an interrupted run says: the answer-wins rule and the two termination
strings now live once, in `Outcome::ended`. The "(interrupted by the operator
before a final answer)" placeholder text is still written by claude and codex
separately (see CON-007).

Owner proposed: the keeper reports whether it delivered a SIGINT
(`Event::Interrupted { by: Control | Signal }` or a field on `Done`), making
"interrupted" one fact from one source; the request file then only tells the
owner that an external SIGINT was operator intent, and even that can go if
`interrupt_cmd` writes `i` down a path the owner reads. The interrupt text
moves into the shared lifecycle driver.

## CON-011 - When does the in-flight marker go up, and when is the pid offered?

Reported by: providers, lifecycle.

| Provider | Marker goes up | Pid offered |
|---|---|---|
| claude | always before spawn | after the `init` event |
| codex | before spawn on resume; in a task once `thread.started` arrives on a fresh run | after the first stdout byte |
| grok | before spawn on resume; after the whole first turn on a fresh run | never |

The real rule, per the providers hunter, is "before spawn when the id is
known, otherwise when it is learned; offer the pid once the provider has shown
its handler is installed". A fresh grok run is invisible to `review sessions`
for its entire first turn, which may be the longest. Both runners keep the
marker alive by moving the guard into a task parked on `future::pending()`,
then `abort()` plus `await` to drop it at the right time, with ordering
comments in both. Owner proposed: one lifecycle function parameterised by the
session-id policy and a readiness signal from the stream decoder; the
lifecycle hunter's `Run` handle owns the marker and takes `set_session(id)` and
`signal_ready()` as method calls, removing the parked task.

## CON-012 - How is a run stopped when `review` is signalled?

Reported by: lifecycle, providers.

The claude and codex wait loops each contain the same arm:

```rust
Ok(_) = stop_rx.wait_for(|stop| *stop), if !stopped_by_signal => {
    stopped_by_signal = true;
    if *first_seen.borrow() { contained.interrupt() } else { contained.terminate() }
}
```

Each also has its own first-output watch channel (`first_tx` in claude,
`first_output_tx` in codex), its own parked marker task, and its own
`take_interrupt_request(..) || stopped_by_signal`. This is the part maki and
opencode would copy. Owner proposed: `Contained`, told `signal_ready()` once
(which also writes the marker's readiness), exposing `stop_gracefully()` that
interrupts or terminates by readiness; the supervisor reaches every contained
run through the registry rather than a per-runner `select!` arm.

## CON-013 - Is shutdown under way?

Reported by: lifecycle, providers.

Sites:
- `provider::STOP` (a watch channel), read through `stop_requested()` and
  through `runtime.stop.borrow()` in each runner before launch.
- `contained::Registry::closed`, set by `terminate_all`.
- `contained::spawn`, which checks both.
- The runners' own pre-launch `if *runtime.stop.borrow() { bail!("not
  launched: ...") }`, repeating `spawn`'s check with the message text copied.
  `run_grok_turn` does not have it (see the grok stop-channel entry in
  `notes/bugs.md`).

The order of operations keeps them consistent today: `STOP` first, `closed`
after the graceful phase. Owner proposed: one `Shutdown` state
(`Running | Draining | Closed`) owned by the lifecycle module.

## CON-014 - Is this recorded process still the one we recorded?

Reported by: lifecycle.

Sites:
- `inflight::identity_alive(pid, Option<start>)`: pidfd, then stat, then pidfd;
  falls back to `pid_alive` when the start is unknown.
- `inflight::pid_alive`: `kill(0)`, used directly by `interrupt_cmd`.
- `interrupt_cmd::interrupt`'s own keeper identity check: pidfd,
  `getpgid == pid`, `ppid == marker.pid`, and `keeper_start` if known, then
  pidfd again.
- `keeper::admit`: its own parent/child proof.
- `tests/keeper.rs::alive`: another.

They already disagree: the owner-liveness hang in `notes/bugs.md`. Owner
proposed: `ProcessIdentity { pid: Pid, start: StartTime }` with
`fn pin(&self) -> Option<PinnedProcess>`, where `PinnedProcess` holds the pidfd
and is minted only after the start time is re-verified, and signalling goes
only through it, so a bare-pid `kill` cannot be written. The marker's three
`(u32, Option<u64>)` pairs become `owner: ProcessIdentity` and
`keeper: Option<ProcessIdentity>`. `Pid(NonZeroU32)` removes the "pid 0 would
signal our own group" defensive comment in `interrupt_cmd`. These primitives
live in `keeper.rs` today, which `inflight` and `interrupt_cmd` import them
from; the lifecycle hunter calls that an odd edge.

## CON-015 - What did the teardown establish?

Reported by: lifecycle.

Sites:
- `Keeper::outcome`: `clean = empty`,
  `incomplete = !empty && (!complete || survivors.is_empty())`.
- `Contained::finish`: re-derives `survivors` (empty if `outcome.clean`) and
  `incomplete: !outcome.clean && outcome.incomplete`, and cross-checks the
  survivor count against the `Survivor` events.
- `Keeper::log_orphaned`: recomputes the survivor list from `members`, not
  from what `outcome` decided.
- `Leftovers::is_clean` and `is_empty`.
- `report_lines`: `n.max(1)` for incomplete ("at least 1 process" with zero
  named survivors), plus a separate branch for unknown.
- `Leftovers::merge`: keeps the first `unknown` reason and drops the second
  without a trace, so two unknowns (a probe and a wake) report as one.

`Outcome` and `Leftovers` are bags of booleans and counts in which most
combinations are meaningless (`clean: true, survivors: 3`; `incomplete: true`
with `clean: true`), and each consumer defends against them its own way. Owner
proposed: one type owned by `keeper` and used unchanged on the wire, in the
rows and in the ledger:

```rust
enum Teardown {
    Clean { killed: Killed },
    Survived { killed: Killed, named: NonEmptyOr<Vec<Proc>>, more_unnamed: bool },
    Unknown { why: UnknownReason, known_survivors: Vec<Proc>, killed: Killed },
}
```

with `Killed { named: Vec<Proc>, dropped: usize }` carrying the `KILLED_MAX`
cap, `UnknownReason` an enum (`KeeperDied(status)`, `ReportCutOff`,
`Malformed`, `Abandoned`, `ProbeTimedOut`, ...), and `merge` a lattice join
(`Unknown > Survived > Clean`). `Done` carries the final verdict and `finish`
only checks it against the survivor stream.

## CON-016 - How long can a keeper take?

Reported by: lifecycle.

Sites:
- `Keeper::teardown`: the actual schedule (term grace, a kill phase with a
  leaves-first half, two settle beats) plus `finish`'s `flush_ms`.
- `Settings::terminate_bound`: `terminate_grace + kill_phase + flush + 4 beats`.
- `Contained::flush_bound`: `flush + 4 beats`.
- `install_signal_supervisor`, which waits `Settings::default().terminate_bound()`
  while keepers run with the injectable `runtime.teardown`.
- `version_probe`, which bounds the whole probe by `settings.startup`, a value
  that means "time to exec" to the keeper.

The bounds are hand-added copies of the keeper's schedule. If the keeper gains
a phase, the supervisor exits while keepers are still tearing down and reports
them as still in progress. Owner proposed: a `TeardownSchedule` value in
`keeper` that the keeper executes and that exposes `worst_case(Start)`
(`Leftover` or `Terminate`); `Settings` holds it instead of loose durations,
and the supervisor reads the live settings from the runtime.

## CON-017 - Which marker is this run's?

Reported by: lifecycle.

`still_ours` compares `run_id` read from disk, before each rewrite and before
removal, with an acknowledged check-then-act race. Two runs of one session
share a path because the path is keyed by session alone; the argument that
makes the race unreachable is the launch lock, a cross-module invariant. Owner
proposed: key the marker path by run (`<session>.<run_id>.json`, or a
directory per session with one file per run), so "ours" holds by
construction; `read_live` groups by session, and `still_ours` and the race go.

## CON-021 - What is a reserved name?

Reported by: config, grok-man.

Sites: `config::RESERVED_NAMES`; the `cli` test that every subcommand is in
it; the `man` test that the `docs/config.md` table equals it. They agree, held
by pairwise tests (the grok-man hunter reads the `man` one as a checked
rendering of one owner). The config hunter proposes deriving the list from
clap's `Command` at startup (`Cli::command().get_subcommands()` names and
aliases, plus `all` and `help`) and dropping the hand list, so the tests become
unnecessary rather than guarding a copy.

## CON-022 - What does `bare` mean?

Reported by: config, orchestration.

Sites:
- `config::BARE` (the parse rule forbidding a non-empty `bare`) and the
  reserved-word exception.
- `app::BARE` (the label for no archetype) and `resolve_archetype_arg` (the
  positional `bare` meaning none).
- The `-a all` expansion.
- `prompt::assemble` (an empty or whitespace prime sends stdin verbatim).

They already disagree: `-a all` includes `bare`, and `-a bare` depends on the
config entry while the positional does not (both in `notes/bugs.md`). Owner
proposed: the archetype resolver in config, returning
`Archetype { label, prime: Option<Prime> }`.

## CON-026 - What is a project root's canonical identity?

Reported by: config.

`config::project_root_of` canonicalizes with a silent fallback to the raw
parent; `project::canonical` does the same; `project::same_project` compares.
`run_session_resume` compares two `PathBuf`s with `!=`, correct only because
both came from `project_root_of`. Owner proposed: a `ProjectRoot` newtype
mintable only by those constructors (see `notes/structural.md`).


## CON-029 - Is there a record for this session, and which project is it in?

Reported by: orchestration.

Sites:
- `resume_record` and `message_target`, each emitting the identical "no record
  of session ... on this host" text.
- `run_session_show`, re-implementing the lookup (`read_all` + filter + sort)
  instead of `latest_for_session`, and re-implementing `message_target`'s `-C`
  assertion (`selected_root` + `same_project`) with different wording.
- `run_sessions`, with its own project filtering.
- `interrupt_cmd::interrupt`, taking `sessions::read_all` as a function and
  doing its own.

The re-validation in `run_session_resume` after `run_message` is a documented
deliberate re-check. Owner proposed: a `sessions::Session` query object
(`Session::find(id) -> Result<Session, NotFound>` with `latest()`, `rows()`,
`project()`, `assert_project(selector)`).

## CON-030 - What state is a session in?

Reported by: orchestration, lifecycle.

Answered independently by `run_sessions` (marker means `[in flight]` or
`[cleanup pending]`, launch lock means `[queued]`, else rows), by `run_message`
(marker, `cleanup_pending`, `interruptible`), by `interrupt_cmd` (marker, pid
wait, queued) and implicitly by `resume_record` (a row exists). The precedence
(marker beats row, queued only without a marker) is control flow in each.
`run_message` pre-checks `cleanup_pending` and `interruptible`, and
`interrupt()` re-checks both with different wording, so an agent sees two
refusals for one condition depending on the verb. `Marker.cleanup_pending` is
a `#[serde(skip)]` bool set by the reader, mixing a stored record with a
computed verdict, and launch locks are a separate `pending_launches` list
joined by string. Owner proposed: one resolver in a session module combining
`inflight` and `sessions`, returning
`SessionState { Queued, InFlight(Marker), CleanupPending, Idle(Record), Unknown }`
(the lifecycle hunter's version: `read_live` returns `Live { marker, state:
RunState }` with `RunState = Running { signal_ready } | CleanupPending { keeper }`
plus `Queued`). This is also the natural home for the record lookup above.

## CON-031 - Where does `review`'s data live?

Reported by: orchestration, forensics.

`sessions::sessions_path` (`XDG_DATA_HOME` else `HOME/.local/share`, then
`review/`), `provider::orphan_log_path` (same logic with a `data_root`
override), `inflight` and `incident` (their own, with `data_root`), and
`audit::audit_dir` (no override). Because only some honour the override, the
sidecar cannot be redirected in tests and nothing in `tests/` exercises
`sessions::record` round-trips with the runners. (The stub runner tests were
checked and cannot reach the audit log, because only the app layer writes it;
an app-layer test that does not isolate `XDG_DATA_HOME` or `HOME` could.) Owner proposed: one `DataDir` resolved once at startup and
passed in, which also answers the private-log routing.

## CON-032 - How is a run recorded?

Reported by: orchestration.

`record_run` (fan-out, auto-resume, panic) and the inline block in
`run_session_resume` both build audit and sidecar rows, and already differ:
- resume writes `env_keys: Vec::new()`; the fan-out's auto-resume, which uses
  the profile's env, writes its keys; a manual resume of that session then
  records none, so the chain loses them;
- resume writes the sidecar only when `output.is_ok()`, the fan-out whenever a
  session id exists;
- resume passes the CLI session id to the audit row, the fan-out
  `result.session_id` (`None` becomes `""`);
- audit rows stamp `chrono_now()` at write time, sidecar rows
  `completed_epoch`, so the two logs disagree on when a run ended.

Owner proposed: one `record(RunContext, &ProviderResult, Kind)`.

## CON-033 - Which commands are project-scoped?

Reported by: orchestration.

The refusal `match` at the top of `app::run`, the `AFTER_HELP` paragraph, and
the handling inside each verb (`message_target`, `run_session_show`,
`run_sessions`). Owner proposed: `cli::Command::project_scope() -> Scope { None,
Asserted, Selected }`, driving both the refusal and the verbs.

## CON-035 - How is the codex rollout format read?

Reported by: forensics.

Four hand-rolled `serde_json::Value` line loops read it, each with its own
`get("payload").get("type")` matching: `transcript::parse`,
`watchdog::has_final_answer`, `watchdog::last_event` and
`incident::scan_transcript`. The 0.157.0 `message`/`output_text` change was
fixed in only one place (`final_answer_text`) and nearly missed the watchdog.
Related decisions made twice:
- "What is the tool in flight?": `ends_with("_call")` /
  `ends_with("_call_output")` in `parse`; nothing in the watchdog, whose
  `last_event` reports `custom_tool_call` but cannot say whether it is open.
- `last_event` formatted as `"type/ptype"` by two independent `format!` sites
  (`transcript::parse`, `watchdog::last_event`).
- Fixtures: the same JSON shapes (`FINAL_ANSWER`, `TASK_STARTED`, the 0.157.0
  assistant message) hand-typed separately in `transcript.rs`, `watchdog.rs`,
  `incident.rs` tests and `tests/provider_runs.rs`.

The watchdog depends on `transcript` for half its job (`final_answer_text`,
`slice_from_offset`, `find_transcript_path`) and deliberately avoids `parse`
with a paragraph explaining why; `scan_from_baseline` is a one-line wrapper
restating that dependency. Owner proposed: a `codex::rollout::Event` enum
parsed once (`TaskStarted`, `TaskComplete { last_agent_message }`,
`TurnAborted`, `UserMessage`, `Reasoning`, `Message { role, phase, text }`,
`AgentMessage { phase, text }`, `ToolCall { call_id, name, args }`,
`ToolOutput { call_id }`, `StreamError`, `TokenCount { rate_limits }`,
`Other { top, ptype }`), each reader a fold over it, `transcript` and
`watchdog` siblings depending on a `rollout.rs`, and a test-only fixtures
module with builders per codex version.

## CON-036 - Which rollout bytes belong to this run?

Reported by: forensics.

`slice_from_offset` is shared and correct. What to do when the baseline is
unknown is decided twice by different mechanisms: the watchdog pends forever
(`None => pending()`), and recovery uses `Some(baseline.unwrap_or(u64::MAX))`,
turning "untrustworthy" into "past EOF" because `None` would have meant
"everything". The same `Option<u64>` means opposite things at two adjacent
call sites, held apart by a comment. `scan_transcript` and the transcript tail
skip scoping entirely. Owner proposed:
`enum Scope { WholeFile, From(RolloutOffset), Untrusted }` passed to every
rollout reader, with `slice` total over it.

## CON-040 - What does grok think of its trust store?

Reported by: grok-man.

`check_trust` refuses a symlinked store and errors on a malformed one;
`untrusted_hint` reads through a symlink and treats malformed as "no hint".
The post-run "did the grant land" check in `invoke` is a third caller, calling
`check_trust` and matching `Ok(Trusted)`, so a store that became Declined
between launch and the re-check prints "did not durably trust" instead of the
truth. Owner proposed: one `fn load_store(&self) -> StoreState` (`Missing`,
`Unloadable(reason)`, `Loaded(TrustDocument)`) that all three map from.

## CON-041 - Is this grok sandbox string one of `review`'s profiles?

Reported by: grok-man.

Decided by prefix in `grok_write_launch`, by construction in `profile_name`,
and by content match in `merge_profile` and `prepare_grok_write`
(`profile_name(roots) != *name`). The prefix check is the weakest and the one
that routes. Owner proposed: a `ProfileName` newtype minted only by
`RootSet::profile_name()` or by parsing a recorded name with the prefix (see
`notes/structural.md`).

## CON-042 - What is a grok write run's canonical root set?

Reported by: grok-man.

`canonical_roots` is called in three places (`invoke`, `profile_name`,
`merge_profile`); its comment says the name and the record both use it "so a
resume can check one against the other", a property held only because every
site remembers. `ensure_profile(name, roots)` takes a name alongside the roots
it should be derived from, so a caller can pass a name for a different set and
`merge_profile` creates it. Owner proposed: a `RootSet` type (sorted and
deduped by construction), making `profile_name(&RootSet)` total.

