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

Most of the per-provider questions below share one proposed owner, a
`Provider` enum with a capability table; that type is described once in
`notes/structural.md`. Each entry here keeps its own sites.

## CON-001 - Is this a known provider?

Reported by: providers, config, orchestration.

Sites:
- `config::KNOWN_PROVIDERS` and its `contains` checks in `parse_file` (three
  times).
- `app::run`'s loop over `--provider` (the fan-out).
- `app::resume_record` (validating a recorded provider).
- `config_cmd::probe_availability` (iterates the list).
- `provider::invoke`'s `other => Err("unknown provider")` arm, reachable only
  because the type allows any string.
- `provider::is_available`, which maps a provider name to a binary name.
- The `man` test.

They agree today; all read `KNOWN_PROVIDERS`. Owner proposed: a `Provider`
enum with `FromStr`/`Deserialize` (and clap `ValueEnum`), parsed once at the
config, CLI and sidecar boundaries, so the parse is the validation and the
dispatch in `invoke` is exhaustive. The config hunter calls the fan-out check a
legitimate re-check of `--provider` input that never went through config, fine
once it is `Provider::from_str`.

The orchestration hunter flags a latent problem: the test
`a_recorded_provider_review_no_longer_supports_is_refused` uses `"opencode"`
as its example of a removed provider. When opencode returns, rows written by
the old opencode integration (if any survive on a host) become resumable by
the new runner with whatever sandbox and model strings the old one recorded.
The retired/new distinction is not representable today; it needs a
`review_version` floor per provider or a distinct spelling for the retired one
(lifecycle and orchestration both suggest an `Unknown(String)` or
`Retired(String)` arm for rows naming removed providers).

## CON-002 - Can this provider's turn be ended with SIGINT and stay resumable?

Reported by: providers, lifecycle, orchestration, config.

Sites:
- `provider::Unrecorded::for_provider`: `matches!(provider, "codex" | "claude")`,
  with its own doc giving the same reason.
- `interrupt_cmd::interruptible`: the same `matches!`. Its doc claims it is
  "shared ... so the two cannot disagree", but `Unrecorded` is a second copy
  the comment does not know about.
- `interrupt_cmd`'s refusal prose: "only codex and claude runs can be
  interrupted".
- `cli.rs` help text (`AFTER_HELP`, `MESSAGE_AFTER_HELP`, `INTERRUPT_AFTER_HELP`,
  the `Command::Message` and `Command::Interrupt` docs): "codex or claude" five
  times.
- `config_cmd`'s STEERING block, which names "codex or claude" in text.
- `install_signal_supervisor`'s messages.
- Implicitly, which runners have a stop arm and set `record_child_pid` (claude
  and codex do; `run_grok_turn` has neither), and which write a readiness flag.
- The `"codex"`/`"claude"` literals passed to `inflight::mark`.

They agree today and nothing ties them together. maki and opencode both answer
yes. The lifecycle hunter notes a missed edit to `Unrecorded` would let a
SIGTERM end their runs without waiting for the row, making the session
unresumable. Owner proposed: a capability, `Provider::interrupt() ->
Option<InterruptStyle>` (SIGINT-ends-turn for codex, claude, maki, opencode;
none for grok), with the help, STEERING and refusal text generated from it or
tested against it.

## CON-003 - Does this provider have a filesystem sandbox, and what is its default level?

Reported by: providers, config, grok-man, orchestration.

Sites:
- `provider::effective_sandbox`: codex and grok get read-only; the rest `None`.
- `config_cmd::sandbox_text`: `"claude"` means ignored; everything else shows
  "unset (runner default: read-only)". The grok-man hunter also points at a
  hard-coded `[("claude", true), ("codex", codex), ("grok", false)]` table in
  `config_cmd.rs` worth checking.
- `config::sandbox_for`: only grok translates; everything else passes through.
- `provider::invoke`: the claude-specific comment, the absent `sandbox`
  argument in the dispatch, and the writable-roots warning.
- `run_grok_turn`'s `(None, true) => read-only` default.
- `run_codex`'s `sandbox.unwrap_or("read-only")` and `codex_sandbox_args`'s
  `!= "read-only"`.
- The `man` sandbox-table test, which hard-codes its columns
  `| codex | grok | claude |` and tests claude by
  `effective_sandbox(...).is_none()`.

The default "read-only" is a literal in at least four places. The sites
already disagree for any provider beyond the three: adding maki or opencode to
`KNOWN_PROVIDERS` compiles and falls into every `_ =>` arm, so `sandbox_text`
prints "runner default: read-only" (false: neither has a filesystem sandbox),
`effective_sandbox` records `None` (right by accident), and `sandbox_for`
passes `read-only` through as if it meant something. The config hunter calls
these three different wrong defaults from one omission. Adding maki to
`KNOWN_PROVIDERS` does not fail the `man` sandbox test, which only checks rows
it finds. Owner proposed: the per-provider capability table, holding the
sandbox model, the default and the native translation, with the `man` test
requiring one column per registered provider. The sandbox type design is in
`notes/structural.md`.

## CON-004 - What is this provider's stale-session cutoff?

Reported by: providers, orchestration, grok-man.

Sites:
- `timings::stale_session`, matching on the provider name with
  `_ => STALE_SESSION`.
- `provider::resume_hint`, which reads it for display.
- `cli::MESSAGE_AFTER_HELP`, which restates 27 and 55 minutes and the grouping
  "(claude, grok)".
- The `cli.rs` test `message_help_states_the_current_stale_cutoffs`, which
  restates the labels `("codex", "(codex)")` and `("claude", "(claude, grok)")`.
- The `man` stale-cutoff test, which iterates `KNOWN_PROVIDERS` (the grok-man
  hunter calls this one a genuine single owner with a checked rendering).

The `cli` test is a pairwise-agreement test: it checks the two numbers and
that grok equals claude, but adding maki with the default cutoff leaves the
help reading "(claude, grok)" and the test passing. maki and opencode would
silently get 55 minutes through the wildcard, which may be right or wrong
since their providers' caches differ, but nobody would have decided it. Owner
proposed: a capability field (`Provider::stale_cutoff()`), with the help line
built from the table at runtime (clap accepts a `String` through
`after_help(..)` if the derive's `&'static str` is replaced by a builder hook
or a `LazyLock`).

## CON-005 - Which providers auto-resume, and on which deaths?

Reported by: orchestration.

`app::after_run` hard-codes `provider_name == "codex"` twice, and
`RESUME_NUDGE` is codex wording in `app`. The comment says only codex
auto-resumes, so only codex has one to withhold. Whether a death class is
resumable rests on codex's mid-turn wedge: provider knowledge living in
orchestration. Owner proposed: a capability on the provider plus a `Died`
outcome variant, so `after_run` needs no provider name.

## CON-006 - Did this run produce an answer?

Reported by: providers, orchestration, forensics.

Sites:
- `run_codex_json`: `captured` means the `-o` file is non-empty.
- `run_codex_json`'s stream parser: the last `item.completed` agent_message,
  used as fallback text, unphased (it can be commentary the user then sees as
  the answer under a death note).
- `transcript::parse`: the `final_answer` of the last turn (phase-tagged).
- `watchdog::has_final_answer`: any phase-tagged answer since the baseline.
- `incident::scan_transcript`: the last `task_complete.last_agent_message` is
  non-null.
- `app::died_without_answer`: digest present, not captured, not recovered;
  `None` digest reads as answered.
- `app::got_final_answer`: digest present and captured or recovered; `None`
  digest reads as not answered. The two are not complements: for a
  digest-less result both are false, and nothing says so.
- The fan-out's `all_failed` (`output.is_err() || died_without_answer`) and
  `run_session_resume`'s exit status, restating the same expression.
- codex's `interrupted = interrupt_requested && !captured && !recovered`.
- The grok wake loop's `turn_error.is_some() && !captured`.
- `app::digest_summary_note`'s `turn_error.is_some() && !d.captured`, whose
  comment says "The live `print_digest` has the same gate".
- `provider::print_digest`'s `!d.captured` branch.
- `app::run_incidents`' verdict (`turn_error`, `recovered_from_transcript`,
  `final_answer_present`).
- `run_session_show`: `task_complete && final_answer.is_none()` means "likely
  died mid-work".

They already disagree. The Stranded-kill and `scan_transcript` defects in
`notes/bugs.md` are two of the disagreements. `got_final_answer` is wrong for
claude and grok, whose clean answers carry no digest; it is harmless only
because only codex reaches `after_auto_resume`. `after_auto_resume` uses
`got_final_answer` while the exit status uses `!died_without_answer`: a maki
result with no digest would be "answered" for the exit code and "unanswered"
for auto-resume. Owner proposed: one outcome value computed once by the runner
(for codex, from one rollout fold plus the `-o` file), persisted in the row,
with `has_answer()` a method on it. The type is in `notes/structural.md`.

## CON-007 - How did the run end, in words?

Reported by: providers, forensics, orchestration.

Five renderings, each checking `turn_error`, `terminated_by_review`,
`interrupted`, `recovered_from_transcript`, `captured`, `exit_code` and
`signal` in its own order:
- `run_codex_json`'s fallback text ("codex ended the turn", "interrupted by the
  operator", "codex produced no final message", "codex died without a final
  answer").
- `provider::print_digest`'s note ladder (live `Digest`).
- `app::digest_summary_note` with `print_digest_summary` (persisted
  `DigestSummary`).
- `app::run_incidents`' verdict ladder.
- `run_session_show`'s "rollout has no final answer" note; `run_session_show`
  also re-renders the transcript block `print_digest` renders.

They already differ: `print_digest` checks `turn_error` before
`terminated_by_review` and has no "interrupted" note, relying on
`terminated_by_review`; `digest_summary_note` checks `interrupted` first and
ignores `terminated_by_review` unless interrupted. A code comment is the only
thing keeping two of them aligned. maki's `is_error: true` with exit 0 would
map onto `captured`/`turn_error`/`exit_code`, which each ladder orders
differently. Owner proposed: compute the outcome once, persist it, and have one
renderer over the persisted form called from both the live and the post-hoc
paths, so `review incidents` and `review sessions` read the verdict instead of
re-inferring it.

## CON-008 - Is this a stated failure that must not be retried?

Reported by: providers, orchestration.

`app::stated_failure` (any `turn_error`) and the grok wake loop's own check
answer it against the same fields with the same meaning. Because `turn_error`
also carries `review`'s own remarks (see the turn-failure entry in
`notes/bugs.md`), both treat `review`'s verification failures as provider
statements. The orchestration hunter adds that `AfterRun::StatedFailure(String)`
is prose; a coarse enum (rate limit, refusal, auth) would be worth having once
auto-resume policy grows past codex. Owner proposed: a `Stated` cause on the
typed outcome, separate from an `Unverified` cause for `review`'s own
diagnoses.

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

What an interrupted run says: claude's `interrupted_digest`/`interrupted_text`
and codex's interrupted block each write the "(interrupted by the operator
before a final answer)" text and the two `terminated_by_review` strings, and
each applies "an answer wins over an interrupt request": claude by matching
the `Answered` arm before checking the request, codex with
`!captured && !recovered`. maki and opencode would add a third and fourth
copy.

Owner proposed: the keeper reports whether it delivered a SIGINT
(`Event::Interrupted { by: Control | Signal }` or a field on `Done`), making
"interrupted" one fact from one source; the request file then only tells the
owner that an external SIGINT was operator intent, and even that can go if
`interrupt_cmd` writes `i` down a path the owner reads. The interrupt text and
the answer-wins rule move into the shared lifecycle driver.

## CON-010 - Answered, but the process exited non-zero or with unknown status

Reported by: providers.

claude's `Answered` arm and grok's `Answered` arm are near-verbatim copies:
`captured: true` plus `no_answer_digest(...)` with an "exited non-zero after
producing an answer" or "exit status is unknown" message. codex decides the
same case differently: the digest records `exit_code`, `captured = true` and
`suspicious = true`, which writes an incident bundle. Three providers, two
answers. Owner proposed: an `Answered { after: Option<PostAnswerAnomaly> }`
shape on the typed outcome, decided once.

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

## CON-018 - Who mints a fresh run's session id?

Reported by: providers, orchestration, config.

| Provider | Who mints the id today |
|---|---|
| claude | the runner calls `generate_uuid` |
| grok | the runner calls `generate_uuid` |
| codex | learned from the stream |
| maki | would be the caller (copy claude) |
| opencode | the caller, with a `ses_` prefix |

The per-provider fact is encoded by which runner calls `generate_uuid`, which
lives in `config.rs`. `Launch::invoke` derives `oneshot = session.is_empty()`,
so there is no way to say "fresh, with this id". Nothing would stop `review
message <id>` from handing a non-`ses` id to opencode. Owner proposed: an
`IdPolicy::CallerMinted(fn() -> SessionId) | StreamReported` capability, and a
per-provider `mint_session_id()`. The session types are in
`notes/structural.md`.

## CON-019 - Which writable roots apply to this run, and which are recorded?

Reported by: providers, config.

Sites:
- `provider::invoke`'s `derives` (codex workspace-write, or a grok fresh
  write) and its warning branch ("apply only to a codex or grok
  workspace-write profile").
- `invoke`'s three recording layers: derived roots, grok-canonicalised roots,
  and the codex `config` override parsed back out by
  `config_writable_roots_override` (see its defect in `notes/bugs.md`).
- codex itself, deciding from argv order.
- `Profile.writable_roots`' doc ("ignored unless sandbox is workspace-write")
  and the `writable_roots` module doc.
- `grok_write_launch`, which decides for grok, including whether
  `danger-full-access` grants.

The config hunter reads a subtle disagreement between the `Profile` doc and
what the code grants for grok. Owner proposed: a permission plan built once
per launch, with `derives` coming from the capability table.

## CON-020 - Which sandbox names are `review`'s levels?

Reported by: config, grok-man.

Sites: `sandbox_for`'s match arms; the test constant `SANDBOX_LEVELS`; the
`man` test's `["read-only", "workspace-write", "danger-full-access"]`; the
"reserved level names" prose; `every_documented_level_maps_somewhere`. They
agree, held by pairwise tests. Owner proposed: a `SandboxLevel` enum whose
named variants are the levels (`SandboxLevel::NAMED` or `ALL`), with the
`man` and doc checks iterating it.

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

## CON-038 - Is this run suspicious, and is it an incident?

Reported by: forensics.

Suspicion is decided once (`!captured || exit != 0 || signal`) and gates the
transcript read and the bundle, which the hunter calls fine. "Is this an
incident?" is `suspicious && !interrupted` inline, and `run_incidents`
re-decides "explained vs mystery" from `turn_error`. Owner proposed: an
`is_incident()` method on the typed outcome.

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

## CON-045 - Display truncation of the in-flight tool

Reported by: forensics.

The 200-char truncation is done twice, differently: `truncate` in
`provider.rs` adds `...`, `run_session_show` uses a bare `chars().take(200)`.
Owner proposed: one display on an `InFlightTool` type.
