# Hunt: orchestration (app.rs, cli.rs, main.rs, lib.rs, input.rs, prompt.rs, sessions.rs)

Read against the arrival of maki and opencode. Where I followed a question into
`provider.rs`, `interrupt_cmd.rs`, `timings.rs`, `audit.rs` or `config.rs` it is
said so.

## Headline

`app.rs` is the de facto provider registry and run-outcome classifier, without
admitting to either. The provider is a `String` everywhere; "what can this
provider do" is answered by `== "codex"` / `matches!(p, "codex" | "claude")` at
a dozen sites spread across five modules; and "did this run produce an answer"
is answered by at least six independent predicates over `Digest` /
`DigestSummary` fields. Adding maki and opencode under the current shape means
editing every one of those sites and hoping the help text, the cutoff table and
the signal supervisor were all remembered. The right move is a rewrite of the
seam between orchestration and providers, not an extension of it:

1. A `ProviderKind` enum (clap `ValueEnum`, serde) with a capability table:
   interruptible, waited-for on signal, auto-resumes, stale cutoff, session-id
   policy (caller-chosen with a required prefix vs provider-assigned), sandbox
   vocabulary, has an on-disk transcript. Every `"codex"` literal in
   orchestration becomes a capability query.
2. A `RunOutcome` enum computed once, by the runner, and persisted:
   `Answered`, `Recovered`, `StatedFailure(reason)`, `Interrupted(how)`,
   `Died`, `LaunchFailed(error)`. Every predicate below becomes a `match`.
3. One `Run` recording path (`RunRecord` built from a `ProviderResult` plus a
   context) used by fan-out, auto-resume, resume and the panic path.
4. Split `app.rs` along its real seams (dispatch, fan-out, message, session
   views) and move the session-state query into its own module.

Details follow.

---

## 1. Axes that should be types

### 1.1 Provider name is a `String` end to end
`Cli.provider: Option<Vec<String>>`, `Launch.provider: String`,
`ProviderResult.provider: String`, `SessionRecord.provider: String`,
`Marker.provider`, `provider::invoke(provider: &str, ..)`. Validation is a
`KNOWN_PROVIDERS.contains` loop in `app::run` (fan-out) and again in
`app::resume_record`, plus whatever `config.rs` does for `[_defaults]`. With a
`ProviderKind` enum parsed by clap and by serde, the unknown-provider error
disappears from app entirely, and the dispatch in `provider::invoke`'s `match
provider { "claude" | "grok" | "codex" | other => Err("unknown provider") }`
becomes exhaustive - the compiler lists every site maki has to fill in.

Rows from an unknown/removed provider deserialize into
`ProviderKind::Retired(String)` (or fail per-row) rather than being caught by a
string test.

### 1.2 "Fresh or resume" is a `&str` that may be empty plus a `bool`
`Launch::invoke(session: &str, ..)` passes `session.is_empty()` as `oneshot`;
`run_session_resume` passes `session_id, .., false`. Two primitives carry one
fact, and the empty-string sentinel is the only thing tying them. It is also
already wrong in shape for three of five providers: claude (per inflight docs),
maki (`--session-id`) and opencode (`--session ses_...`) pick the ID up front
for a *fresh* run. The real type is

```rust
enum SessionStart { Fresh { id: Option<SessionId> }, Resume(SessionId) }
```

with `SessionId` a validated newtype (the allowlist `inflight.rs` applies today
belongs in its constructor), and a per-provider `mint_session_id()` that knows
opencode needs `ses`. Today the `ses` prefix rule would have to be checked
somewhere in a runner and nothing would stop `review message <id>` from handing
a non-`ses` id to opencode.

### 1.3 Sandbox level travels in two vocabularies as one `Option<String>`
Profiles hold `review`'s vocabulary; `ProviderResult.sandbox` and
`SessionRecord.sandbox` hold the provider's native one; a resume feeds the
native value back through `config::sandbox_for`, which is safe only because
the mapping happens to be idempotent on its own output (the long comment on
`inherited_settings` argues this). Grok additionally smuggles a profile *name*
(`review-ws-<hash>`) through the same string, recognised by prefix in
`grok_write_launch`. For opencode, "read-only" is an agent definition; for
maki and claude there is no sandbox at all. Types:
`SandboxLevel` (review vocabulary, enum) on the request side, and a
per-provider `NativePermissions` on the record side
(`Codex(level)`, `Grok(Builtin|Profile(name, roots))`, `Opencode(agent)`,
`None`). Resume inheritance then copies the native value verbatim, with no
round-trip through a mapping, and "a grok level fed to codex" (the guard in
`inherited_settings`) is unrepresentable rather than tested.

### 1.4 Run outcome is spread across `Result<String>` and digest booleans
`ProviderResult.output: Result<String>` is `Ok` for a dead run; whether it is
an answer depends on `digest.captured`, `recovered_from_transcript`,
`interrupted`, `turn_error`, `exit_code`, `signal` and whether `digest` exists
at all. `None` digest means "clean" for `died_without_answer` but "no answer"
for `got_final_answer` (see 2.1). maki exits 0 with `is_error: true`; opencode
has no result object at all; both will need to fill this matrix of booleans in
the exact combination the classifiers expect. A `RunOutcome` enum (see headline)
emitted by each runner removes the matrix. Keep `Digest` as forensics only.

### 1.5 `private: bool` beside `audit_id: &str`
Threaded through `record_run`, `RecordCtx`, `sessions::record`,
`audit::log_result`, `ProviderRuntime::from_config`, `orphan_log_path`. It
selects which log family a row goes to. It should be one `LogScope { audit_id:
AuditId, private: bool }` (or `enum LogScope { Public(AuditId),
Private(AuditId) }`) resolved once from config, so no call site can pair a
private project's id with the public log.

### 1.6 `SessionRecord.kind: String`, `SessionEntry.kind: &'static str`
Written as `"run"` or `"session"`. The doc comments on both structs say
`"oneshot"`, `"session"` or `"prime"` - stale. Enum it. Also: the auto-resume
inside a fan-out is recorded as `kind = "run"` though it is a resume.

### 1.7 Timestamps: `u64` epoch with `0` meaning unknown
`SessionRecord.epoch_secs` defaults to 0 and every reader re-checks `== 0`
(`age_secs`, `run_session_show`, `run_sessions`). `Option<u64>` (serde
default) or an `Epoch` newtype. There are three `now_epoch_secs` (sessions.rs,
provider.rs, inline in `run_sessions`); the age/`format_age`/"now" vs "just now"
vs "?" rendering is decided in three places (`run_session_resume`,
`run_session_show`, `run_sessions`). An `Age` type with one `Display` would own
it.

### 1.8 Exit codes are bare integers in four places
`1` and `EXIT_LEFT_RUNNING = 3` in `app::exit_status`, `130`/`143` in the
signal supervisor, `2` for help, `man::run`'s return. An `ExitStatus` enum with
`code()` and a single `std::process::exit` site in `main` (app returning the
status rather than calling `exit` from three places) would let tests assert on
the status of a whole run, and would stop `exit()` from skipping destructors
(the `Unrecorded`/lock guards are dropped by then, but nothing guarantees it).

### 1.9 Failures that reach callers only as prose
- Message/resume refusals (no record, retired provider, stale, wrong project,
  audit mismatch, cleanup pending, non-interruptible, not installed) are all
  `bail!` strings with exit 1, indistinguishable from "the turn produced no
  answer". An orchestrator cannot tell "start a fresh run" (stale) from "wait"
  (grok in flight, cleanup pending) from "you have the wrong id". A
  `MessageRefusal` enum with a distinct exit code per actionable class, or at
  least one for "refused before sending" vs "sent, no answer", would let it
  branch. Same for the fan-out's pre-launch refusals (unknown provider/profile,
  not installed).
- `AfterRun::StatedFailure(String)` and `turn_error: Option<String>`: the
  provider's reason is prose; maki/opencode will produce their own. Fine as
  payload, but rate-limit vs refusal vs auth would be worth a coarse enum when
  auto-resume policy grows past codex.

### 1.10 `Launch` duplicates `Profile`
`Launch` copies model/effort/sandbox/env/config/writable_roots field by field
out of the resolved profile, and `invoke` takes them again as seven positional
parameters (`#[allow(clippy::too_many_arguments)]`). Pass a `LaunchSpec {
provider, start: SessionStart, settings: RunSettings, prompt, root }` where
`RunSettings` is also what a resume builds from `InheritedSettings`. Then
`InheritedSettings` and the profile become two constructors of one type, and
"what a resume inherits" is visibly the same struct as "what a run launches
with" (today `env` and `config` are silently absent on one side).

---

## 2. Decisions made in more than one place

### 2.1 Did this run produce an answer?
Sites:
- `app::died_without_answer` (digest present and not captured and not
  recovered; `None` digest = answered).
- `app::got_final_answer` (digest present and captured or recovered; `None`
  digest = not answered). Not the negation of the above: for a digest-less
  result both are false.
- `app::run` exit-status `all_failed` = `output.is_err() || died_without_answer`.
- `app::run_session_resume` exit status, same expression restated.
- `app::digest_summary_note` (captured / turn_error / exit_code / signal).
- `provider::print_digest` (the comment in `digest_summary_note` says "The live
  `print_digest` has the same gate" - an explicit acknowledgement that two
  renderers decide the same thing).
- `app::run_incidents` verdict (`turn_error`, `recovered_from_transcript`,
  `final_answer_present`).
- `run_session_show`'s "rollout has no final answer" note from the transcript.

They already disagree in one place: `after_auto_resume` uses
`got_final_answer` while the exit status uses `!died_without_answer`; a codex
result always has a digest so it does not bite today, but a maki result that
carries none would be "answered" for the exit code and "unanswered" for
auto-resume. Owner: a `RunOutcome` computed in the runner (provider module),
persisted in `DigestSummary`/the row, read by everyone. Print from the summary
only, so the live and post-hoc renderers are one function.

### 2.2 Which providers can be ended with SIGINT and stay resumable?
- `provider::Unrecorded::for_provider`: `matches!(p, "codex" | "claude")`.
- `interrupt_cmd::interruptible`: `matches!(p, "codex" | "claude")`.
- Help text in `cli.rs` (`AFTER_HELP`, `MESSAGE_AFTER_HELP`,
  `INTERRUPT_AFTER_HELP`, `Command::Message`/`Interrupt` docs): "codex or
  claude" five times.
- `install_signal_supervisor` messages.
Same question, two code copies and prose copies, nothing tying them. Both maki
and opencode answer yes. Owner: `ProviderKind::interruptible()`; help text
generated or at least tested against it (see 2.10).

### 2.3 Which providers auto-resume, and on what?
`app::after_run` hard-codes `provider_name == "codex"` twice, and
`RESUME_NUDGE` is codex wording in app. The comment says "only codex
auto-resumes, so only codex has one to withhold". The decision of *whether a
death class is resumable* is provider knowledge (it rests on codex's mid-turn
wedge) living in orchestration. Owner: a capability on the provider plus the
`RunOutcome::Died` variant; `after_run` then needs no provider name.

### 2.4 Stale cutoff per provider
`timings::stale_session` matches on the name; `cli::MESSAGE_AFTER_HELP`
restates 27/55 and the provider grouping "(claude, grok)"; the test
`message_help_states_the_current_stale_cutoffs` is the pairwise-agreement kind
the brief calls a finding - it checks the two numbers and that grok equals
claude, but adding maki with the default cutoff leaves the help reading
"(claude, grok)" and the test passing. `resume_hint` computes it a third time
for display. Owner: `ProviderKind::stale_cutoff()`, and the help line built
from the table (clap accepts `String` via `after_help(..)` at runtime if the
derive's `&'static str` is replaced with a builder hook, or a `LazyLock`).

### 2.5 Is this a known provider?
`app::run` loop over `--provider`; `app::resume_record`; `config.rs` at least
three `KNOWN_PROVIDERS.contains` sites; `provider::invoke`'s `other =>` arm;
`provider::is_available`. Five answers to one question. Note the test
`a_recorded_provider_review_no_longer_supports_is_refused` uses `"opencode"`
as its example of a *removed* provider: when opencode returns, rows written by
the old opencode integration (if any survive on a host) become resumable by
the new runner, with whatever sandbox/model strings the old one recorded. The
retired/new distinction is not representable today; it needs either a
`review_version` floor per provider or a distinct `ProviderKind` spelling for
the retired one. Flagging as a latent bug.

### 2.6 Is there a record for this session, and which project is it in?
`resume_record` and `message_target` each emit the identical "no record of
session ... on this host" text. `run_session_show` re-implements the lookup
(`read_all` + filter + sort) instead of `latest_for_session`, and re-implements
the `-C` assertion that `message_target` does (`selected_root` +
`same_project`), with different wording. `run_sessions` does its own project
filtering. `interrupt_cmd::interrupt` takes `sessions::read_all` as a function
and does its own. The re-validation in `run_session_resume` after
`run_message` is a deliberate re-check (documented) and fine; the parallel
implementations are not. Owner: a `sessions::Session` query object
(`Session::find(id) -> Result<Session, NotFound>` with `latest()`, `rows()`,
`project()`, `assert_project(selector)`).

### 2.7 What state is a session in?
Answered independently by `run_sessions` (marker -> `[in flight]` /
`[cleanup pending]`, launch lock -> `[queued]`, else rows), by `run_message`
(marker, `cleanup_pending`, `interruptible`), by `interrupt_cmd` (marker, pid
wait, queued), and implicitly by `resume_record` (row exists). Today's
precedence (marker beats row, queued only if no marker) lives as control flow
in each. Owner: one `SessionState { Queued, InFlight(Marker), CleanupPending,
Idle(Record), Unknown }` resolver in a session module combining inflight and
sessions. This is also the natural home for 2.6.

### 2.8 Ensure an audit id exists
`app::run` (under the global lock; warns on write failure; updates `cfg`) and
`app::run_session_resume` (after the global lock has been dropped; ignores the
write error with `let _`; does not update `cfg`). Two answers, already
disagreeing on error handling and on locking - two concurrent resumes of an
id-less project can each append a different id to `.review.toml`. Owner:
`config` (`cfg.ensure_audit_id(root)`), called under the lock by both.

### 2.9 Where review's data lives
`sessions::sessions_path` (XDG_DATA_HOME else HOME/.local/share, then
`review/`), `provider::orphan_log_path` (same logic, with a `data_root`
override), `inflight` and `incident` (their own, also with `data_root`),
`audit::audit_dir`. The override exists only for some, which is why the
sidecar cannot be redirected in tests and nothing in `tests/` exercises
`sessions::record` round-trips with the runners. Owner: one `DataDir` resolved
once at startup and passed in (it also answers the `private` routing, 1.5).

### 2.10 The global lock path
`std::env::temp_dir().join("review.lock")` is computed twice in `app.rs`. It
also depends on `TMPDIR`, so two `review` processes with different `TMPDIR`
(a sandboxed agent often has one) do not serialise at all - contrary to the
lock's purpose; the launch locks already live under the data dir. Owner:
`lock::global()` beside `inflight`'s locks.

### 2.11 How a run is recorded
`record_run` (fan-out, auto-resume, panic) and the inline block in
`run_session_resume` both build audit + sidecar rows. They already differ:
- resume writes `env_keys: Vec::new()` (true, it inherits no env) but the
  fan-out's auto-resume, which *does* use the profile's env, writes the
  profile's keys - fine - while a manual resume of that session then records
  none, so the chain loses them;
- resume writes the sidecar only when `output.is_ok()`, the fan-out whenever a
  session id exists;
- resume passes the CLI session id to the audit row, fan-out passes
  `result.session_id` (None -> "").
- audit rows stamp `chrono_now()` at write time, sidecar rows stamp
  `completed_epoch`; the two logs disagree on when a run ended.
Owner: one `record(RunContext, &ProviderResult, Kind)`.

### 2.12 What "bare" means
`app::BARE` (label for no archetype), `resolve_archetype_arg` (positional
`bare` = none), `config`'s `bare` reserved-word exception, `prompt::assemble`
(empty or whitespace prime = verbatim). Three modules each own part of it.
Owner: config's archetype expansion (3.3) returning `Archetype { label, prime:
Option<Prime> }`.

### 2.13 Which commands are project-scoped (`-C`)
The refusal `match` at the top of `app::run`, the `AFTER_HELP` paragraph, and
the bespoke handling inside each verb (`message_target`, `run_session_show`,
`run_sessions`). Owner: `cli::Command::project_scope() -> Scope { None,
Asserted, Selected }`, used both for the refusal and to drive the verbs.

### 2.14 Is this invocation "bare review with nothing piped"?
`app::run` re-reads `std::env::args_os().len() == 1` instead of asking the
parsed `Cli`. It is a second parse of argv that can drift from clap's (e.g. a
future default subcommand). Use the parsed struct.

---

## 3. Structure

### 3.1 `app.rs` does five jobs
2,100 lines: top-level dispatch; the fan-out (archetype expansion, provider
selection, profile validation, launch construction, stagger and lock,
collection, printing, exit status); auto-resume policy (codex-specific);
message/resume (target validation, interrupt, inheritance, stale gate, lock
handshake, recording); and three read-only views (`sessions` list, `sessions
<id>` with codex transcript rendering, `incidents`) plus a `DigestSummary`
renderer. Proposed split:
- `app.rs`: a single `match cli.command` dispatch returning `ExitStatus`
  (today it is a chain of `if let Some(Command::X)` with the run as the
  fallthrough and `--session` handled after the subcommands).
- `run.rs`: fan-out.
- `message.rs`: message/resume (and `interrupt_cmd` folded beside it, since
  `run_message` already calls into its internals like
  `cleanup_pending_message`).
- `session.rs` (new): the state/lookup query of 2.6/2.7 over `sessions` +
  `inflight`.
- `views.rs` or per-verb files: `sessions`, `sessions <id>`, `incidents`.
- Run-outcome rendering moves to the provider side with `RunOutcome`, one
  renderer for live and persisted digests.

### 3.2 Provider knowledge lives in orchestration
`run_session_show` renders codex rollouts directly (`transcript::summarize_session`,
`provider == "codex"` twice) and admits it cannot honour `CODEX_HOME`
overrides. maki and opencode each have their own session stores (opencode's
private database the run is forced to use); a "show me what this session left"
hook per provider is where that belongs. Same for `RESUME_NUDGE`, `after_run`'s
codex gates, `Unrecorded::for_provider`. A `Provider` trait (or enum with
methods) - `capabilities()`, `run(spec)`, `session_artifacts(id, record)`,
`mint_session_id()` - is the seam; orchestration should not contain a provider
name literal outside tests.

### 3.3 Archetype selection belongs to config
The `-a` expansion loop (`all`, groups, comma lists, unknown-name error listing
configured names, dedup, empty check) is inline in `app::run`, and relies on a
config invariant ("Resolution checked every member exists") via
`unwrap_or_default()` - a silent empty prime if that invariant ever breaks.
The unknown-profile error also hard-codes config's lookup tables
(`[{prov}.{profile}]` and `[{host}.{prov}.{profile}]`) in app. Both should be
`cfg.select_archetypes(&str) -> Result<Vec<Archetype>>` and
`cfg.require_profile(provider, name) -> Result<&Profile>` with config owning
the messages.

### 3.4 `sessions.rs` is a log writer pretending to be a session store
It has a 19-argument `record` (with `#[allow(too_many_arguments)]`), a write
struct `SessionEntry` and a read struct `SessionRecord` that are maintained in
parallel by hand (fields added to one and not the other: `served_model`,
`cost_usd`, `grok_trust`, `leftovers` are write-only - deliberate for
`served_model`, per its comment, but not documented for the rest), and several
`#[allow(dead_code)]` on fields that are in fact read (`sandbox`,
`writable_roots`, `audit_id`). Better: one `SessionRow` type used for both
directions (with `served` excluded from inheritance by the `RunSettings`
constructor rather than by being absent from the read struct), `record` taking
it whole, and a reader that parses the file once per process (`run_sessions`
calls `latest_for_session` per queued launch, each re-reading and re-parsing
both logs; `run_message` + `run_session_resume` + `interrupt` read it three or
more times per message).

### 3.5 `run_session_resume` carries vestigial optionality
`instructions: Option<String>` is always `Some` (the `None => read_stdin()`
branch is dead since `resume` became an alias of `message`); `expected:
Option<..>` is always `Some`; `launch_lock` is `None` only on a dry run, which
returns before the lock would matter. Fold it into `run_message` or make the
parameters non-optional.

### 3.6 Fan-out bookkeeping split from its data
`archetypes_to_run: Vec<(String, String)>` (label, prime) as a bare tuple;
results collected as `Vec<(String, ProviderResult)>` keyed by archetype label
cloned out of `RecordCtx`; the printing loop groups by comparing consecutive
labels, which only works because tasks are awaited in launch order. A
`Vec<ArchetypeRun { archetype, launches: Vec<JoinHandle> }>` makes grouping
structural.

### 3.7 `cli.rs`
Reasonable. Two notes: `provider` should be `Vec<ProviderKind>` (1.1); the
help prose duplicates capability facts (2.2, 2.4) that would ideally be
generated. `RUN_ONLY` is a hand-kept list of clap ids that must be updated for
any new run flag - a run-flag `ArgGroup` or `#[command(flatten)] RunArgs`
struct, checked by "any arg of RunArgs present", removes the list.

### 3.8 `prompt.rs`, `input.rs`, `main.rs`, `lib.rs`
Small and fine. `prompt::assemble` is a one-liner whose only real decision is
"whitespace prime = bare" (see 2.12). `input::MAX_STDIN_BYTES` counts bytes
read, the error says bytes - consistent. `main.rs`'s keeper-first dispatch is
right.

---

## What maki/opencode would touch today (inventory)

- `config::KNOWN_PROVIDERS`, `provider::invoke` dispatch, `is_available`.
- `Unrecorded::for_provider`, `interrupt_cmd::interruptible` (both).
- `timings::stale_session` and `cli::MESSAGE_AFTER_HELP` and its test.
- `config::sandbox_for`, `provider::effective_sandbox`, `config_cmd`'s claude
  special cases, the `man` sandbox table test.
- `app::after_run` if either should auto-resume.
- `app::run_session_show` if their transcripts should show.
- The fresh-run session-id path (`""` + `oneshot`), which has no way to say
  "fresh, with this id" (1.2), and opencode's `ses` prefix.
- Help text in four constants.
- The `opencode` "removed provider" test (2.5).
- Every runner must synthesise a `Digest` with the boolean combination the
  classifiers in 2.1 expect, maki in particular because its exit code is 0 on
  `is_error`.

## Other things noticed

- Panicked fan-out task is recorded with `provider: "unknown"`; the provider
  name is available in the pending entry and should be kept.
- Audit and sidecar timestamps disagree (2.11).
- The global lock in `TMPDIR` (2.10) is the most likely real-world bug here.
- `sessions.rs` doc comments: `kind` values stale (1.6); `digest` "Absent for
  claude (no machine-readable stream)" is stale - claude produces digests now.
- `inherited_settings`'s cross-provider guard is tested though unreachable;
  with typed native permissions (1.3) the test and guard both go.
