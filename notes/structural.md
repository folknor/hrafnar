# Structural

Shape and placement findings from the design hunt over `review` (scopes:
providers, lifecycle, config, orchestration, forensics, grok-man): axes that
should be types, and the moves, splits and rewrites, each with the payoff the
hunters claim for it. Unverified.

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

Every scope's hunter reached the same three proposals independently: a
`Provider` enum with a capability table, a typed run outcome, and a single run
lifecycle that per-provider adapters plug into. The config hunter recommends
doing the enum, the sandbox level and the capability table as one change
before adding maki; the providers hunter calls for a rewrite around all three.

## Types

## STR-001 - A `Provider` enum with one capability table

Reported by: providers, lifecycle, config, orchestration, grok-man, forensics.

The provider is a `&str`/`String` end to end: `Cli.provider`,
`Launch.provider`, `ProviderResult.provider`, `SessionRecord.provider`,
`Marker.provider`, `invoke(provider: &str, ..)`. It is matched in at least
nine places across five files, listed per question in
`notes/consolidations.md` (known provider, interruptible, sandbox, stale
cutoff, auto-resume, session-id minting). Adding `"maki"` to `KNOWN_PROVIDERS`
compiles and silently falls into every `_ =>` arm.

Proposed: `enum Provider { Claude, Codex, Grok, Maki, Opencode }` with
`FromStr`, serde and clap `ValueEnum`, parsed once at the config, CLI and
sidecar boundaries, with an `Unknown`/`Retired(String)` arm for rows naming
removed providers. A capabilities struct per provider, owned by one module
(`provider::spec(Provider) -> &'static Capabilities`), carries what the
scattered sites decide today:
- filesystem sandbox or not, the default level, the native spelling of each
  level;
- interrupt style (SIGINT ends the turn, or none);
- stale cutoff;
- auto-resume policy;
- session-id policy (caller-minted, with any required prefix such as
  opencode's `ses`, or reported by the stream);
- whether `effort` is a flag, part of the model string, or config-only;
  whether `model` and codex-style `config` overrides are honoured;
- whether writable roots are derived;
- has an on-disk transcript; the binary name for `is_available`.

`KNOWN_PROVIDERS` becomes a projection of the registry. `Availability`
(today `BTreeMap<String, bool>` with `unwrap_or(false)`) becomes a total
function. `ProviderRuntime` (today `codex_binary: String`,
`claude_command: Vec<String>`, and grok hardcoded as `Command::new("grok")`,
which is why grok has no runner-level test) becomes a map or
`fn command(Provider) -> Vec<String>`, giving maki and opencode a stub hook.
Help text, STEERING and refusal prose are generated from the table, and the
`man` tests iterate it. The orchestration hunter's version is a `Provider`
trait or enum with methods: `capabilities()`, `run(spec)`,
`session_artifacts(id, record)`, `mint_session_id()`, so orchestration holds no
provider-name literal outside tests.

Payoff claimed: adding a provider becomes one adapter file plus one variant,
and the compiler lists every site that must decide something about it.

## STR-002 - Sandbox level, native sandbox and permission plan as separate types

Reported by: providers, config, orchestration, grok-man.

`sandbox: Option<&str>` carries `review`'s vocabulary on a fresh run and the
provider's native vocabulary on a resume (grok's `workspace`, or a
`review-ws-<hash>` profile name), round-tripped through `sandbox_for`, which
is safe only because it passes unknown names through and is idempotent on its
own outputs. `grok_write_launch` recovers the profile kind by
`starts_with(PROFILE_PREFIX)`; `derives` compares against `"workspace-write"`;
`effective_sandbox` writes the native name back into the record (the
recording defect is in `notes/bugs.md`). `app::inherited_settings`'s long
comment argues the safety of this, and its cross-provider guard is tested
though unreachable. `GrokWrite` already exists as the typed shape but is
reconstructed from a string on every call.

Proposed (the hunters' versions converge):
- `enum SandboxLevel { ReadOnly, WorkspaceWrite, DangerFullAccess, Custom(String) }`
  with `Default = ReadOnly`, parsed at `Profile` deserialisation; `Custom`
  keeps the deliberate pass-through and the named levels become reserved by
  construction.
- A per-provider native form: grok
  `enum GrokSandbox { Builtin(Level), ReviewProfile(ProfileName), Custom(String) }`;
  "none" for claude, maki and opencode.
- A permission plan per launch, because for maki and opencode read-only is not
  a sandbox at all (maki's permissions config, opencode's agent definition):
  `enum Permissions { Sandbox(NativeSandbox, Vec<GrantedRoot>), AgentPolicy(..), None }`
  (orchestration: `NativePermissions { Codex(level), Grok(Builtin|Profile(name, roots)), Opencode(agent), None }`).
  Deciding it is one function, not `invoke`'s
  `provider == "codex" && sandbox == Some("workspace-write")`.
- A recorded form that says which vocabulary it is in (grok-man: `level` plus
  optional `provider_sandbox`), so resume copies the native value verbatim with
  no round-trip, and "a grok level fed to codex" is unrepresentable.

The config hunter adds that the dependency arrow is backwards today: config
imports `sandbox_for` because `Profile` holds the level, but config should
hold `SandboxLevel` and know nothing about grok.

## STR-003 - Session start and session id as types

Reported by: providers, lifecycle, orchestration, forensics, grok-man.

Fresh vs resume travels as `oneshot: bool` plus `session_id: &str`, with the
empty string as the sentinel (`Launch::invoke` derives
`oneshot = session.is_empty()`; `run_session_resume` passes `session_id, ..,
false`). Impossible combinations are representable, every runner branches on
both, and "fresh, with this id" (claude today, maki and opencode next) cannot
be said. Session ids are `&str` everywhere; validation runs in several places
with different degradation (the allowlist defect in `notes/bugs.md`).

Proposed: `enum SessionStart { Fresh { id: Option<SessionId> }, Resume(SessionId) }`
(providers: `enum Session { Fresh, Resume(SessionId) }`), and a `SessionId`
newtype validated once where an id enters `review` (CLI argument, provider
stream, the minter), with `as_path_component()`, a length bound and the
provider-specific shape as an associated check. Every path in `inflight`
becomes infallible, and `find_transcript_path`, `grok_home`, `transcript` and
`sessions` stop re-validating or forgetting to. The minting policy is in
`notes/consolidations.md`.

## STR-004 - A typed run outcome in place of `Result<String>` plus `Option<Digest>` plus flags

Reported by: providers, orchestration, forensics. The providers hunter calls
it the biggest finding in that scope.

Whether a run answered, failed for a stated reason, died or was interrupted is
spread across `output: Result<String>` (`Err` meaning it never reached a
turn), `digest: Option<Digest>` (whose `None` means a clean answer for claude
and grok but cannot happen for codex), and `captured`,
`recovered_from_transcript`, `turn_error: Option<String>`, `interrupted`,
`exit_code`, `signal` and `terminated_by_review: Option<String>`. `captured`
plus `recovered_from_transcript` encode a three-state answer source with an
impossible fourth combination; `final_answer_present: Option<bool>` is another
tri-state. maki (exit 0 with `is_error: true`) and opencode (no result object)
would each have to fill this matrix in the exact combination the classifiers
expect. The duplicated predicates and renderings this produces are in
`notes/consolidations.md`; the overloaded `turn_error` is in `notes/bugs.md`.

Proposed (providers):

```rust
enum Outcome {
    Answered { text, provenance: AnswerSource /* Captured | Recovered | Result */,
               after: Option<PostAnswerAnomaly> /* NonZeroExit(status) | UnknownStatus */ },
    NoAnswer { cause: NoAnswerCause, partial_text: Option<String> },
    NotLaunched(LaunchError),
}
enum NoAnswerCause {
    Stated(String),                 // the provider said why
    Died { status: Option<ExitStatus> },
    Interrupted(InterruptSource),   // Operator | Signal
    Killed(WatchdogVerdict),
    Unverified(String),             // review could not vouch (grok wake loop)
}
```

Forensics proposes the equivalent `enum RunEnd { Answered(AnswerSource),
StatedFailure(msg), Interrupted(Terminator), KilledByReview(Terminator), Died,
NoMessage }`, and orchestration `RunOutcome { Answered, Recovered,
StatedFailure(reason), Interrupted(how), Died, LaunchFailed(error) }`. All
three agree it is computed once by the runner and persisted in
`DigestSummary`/the row, with `after_run`, `exit_status`, `print_digest`,
`digest_summary_note`, `run_incidents` and `is_incident` becoming `match`es on
it. maki and opencode, which state their outcome in the stream, would construct
it directly rather than fill codex-shaped booleans. `Digest` stays as
forensics only (see the forensics split below), and `DigestSummary` becomes
the serialised projection of the outcome plus usage.

## STR-005 - Termination reason as an enum

Reported by: providers, forensics.

`terminated_by_review: Option<String>` holds one of `"review was signalled to
stop"`, ``"operator interrupt (`review interrupt`)"``, `"watchdog: stranded
completion"`, or `format!("watchdog: stall timeout ({}s silent)")`. They are
written in the claude and codex runners separately and as a fallback default
in `app::digest_summary_note`; readers branch on presence alone, tests match
substrings, and the kind is lost by the time it reaches the sidecar ("how many
Stranded kills this week" is a substring grep). `Verdict.reason` (the
watchdog's own prose) and the short string are two phrasings of one verdict
built in two modules. Proposed:
`enum Termination { Signal, OperatorInterrupt, Watchdog(Kind, quiet_secs) }`
with one `Display`, persisted as a tag.

## STR-006 - Exit status as a typed value

Reported by: providers, lifecycle, forensics, grok-man.

- `Digest.exit_code` plus `signal` split one `ExitStatus`; `signal_name` maps
  2, 9 and 15 and formats the rest as `"signal N"`.
- The keeper's `Outcome.status: Option<i32>` is a raw `waitpid` status
  crossing a process boundary, turned back with `ExitStatus::from_raw` in two
  places in `finish`.
- `Incident`'s `exit_code: Option<i32>` sits beside `turns: u32` and can be
  swapped silently.
- Grok's `Unread::Finished { exit_code: Option<i64>, signal: Option<String> }`
  has an illegal both-set state, resolved by exit-code-wins in the wake
  prompt.

Proposed: `enum Exit { Code(i32), Signal(i32), Unknown }` on the wire and in
the records, with `Display` replacing `signal_name`. The lifecycle hunter adds
that opencode (130 on SIGINT) and maki (0 on error) mean runners must not
classify by exit status, and a plainly secondary typed field helps keep it so.

## STR-007 - Process exit codes as an enum with one exit site

Reported by: orchestration.

`1` and `EXIT_LEFT_RUNNING = 3` in `app::exit_status`, `130`/`143` in the
signal supervisor, `2` for help, and `man::run`'s return are bare integers in
four places, and `std::process::exit` is called from three. Proposed: an
`ExitStatus` enum with `code()`, `app` returning it, and a single `exit` in
`main`, so tests can assert on a whole run's status and `exit()` cannot skip
destructors (the `Unrecorded` and lock guards are dropped by then today, but
nothing guarantees it).

## STR-008 - Usage, served model and turn counts

Reported by: providers.

`Usage` has four `u64` fields with provider-specific mappings (`claude_usage`
and `grok_usage` each rebuild codex's "input includes cached" convention); the
grok wake loop adds them by hand and `cost_usd` is summed with a hand-written
`Option` merge. `Served::model` is a comma-joined `String`. `Digest::turns`
counts `turn.completed` events for codex but `num_turns` (API round trips) for
claude and grok, so the column is not comparable across providers. Proposed:
`AddAssign` for `Usage` and `Served`; the convention named in the type
(`total_input`, `cached_input`) so a new mapper cannot double-count
claude-style cache-creation tokens; `Vec<String>` joined only at
serialisation.

## STR-009 - A `TempFile` guard

Reported by: providers.

`new_output_file` and `new_temp_file` return `String` paths, and
`RemoveOnDrop(String)` is created separately from the file it removes:
`run_codex` creates the file, `run_codex_json` installs the cleanup and also
removes it by hand on the stop path. Proposed: a guard that owns both.

## STR-010 - In-flight marker fields

Reported by: lifecycle.

Both runners store the keeper pid in `Marker::child_pid`, which
`record_keeper` has already stored in `keeper`; the field's doc still says
"pid of the provider process ... the leader of its process group". So
`child_pid` really means "safe to signal", carried as a duplicate pid.
`interrupt_cmd` relies on the two being equal without saying so: it checks
`child_pid`'s identity against `marker.keeper_start`. If someone "fixes"
`child_pid` to hold the real provider pid (the `Spawned` event carries it, and
`go()` returns it unused), the start-time check breaks silently and the
`ppid == marker.pid` check fails, since the provider's parent is the keeper.
Proposed: `signal_ready: bool` or a `Readiness` enum, exactly one keeper
identity, and the identity types from the process-identity entry in
`notes/consolidations.md`.

## STR-011 - Typed refusals for `interrupt` and `message`

Reported by: lifecycle, orchestration.

`interrupt_cmd::Outcome { interrupted: bool, project: String, provider:
String }` is derived by scanning sidecar rows after a count baseline, with a
"look one more time" heuristic for auto-resume pairs. Its failures (owner
gone, keeper gone, no pid in time, not interruptible, cleanup pending, queued)
are `bail!` prose, so `review message` can only branch by string. The message
and resume refusals (no record, retired provider, stale, wrong project, audit
mismatch, cleanup pending, not interruptible, not installed) all exit 1,
indistinguishable from "the turn produced no answer": an orchestrator cannot
tell "start fresh" (stale) from "wait" (grok in flight, cleanup pending) from
"wrong id". The same holds for the fan-out's pre-launch refusals. Proposed:
`InterruptError { NotInFlight, Queued, CleanupPending, NotInterruptible(Provider),
NeverReady, KeeperGone, OwnerDiedUnrecorded }` and
`Outcome = Interrupted { row } | FinishedFirst { row }` (letting `run_message`
proceed on `KeeperGone`, "finishing on its own", rather than fail); a
`MessageRefusal` enum with a distinct exit code per actionable class, or at
least one for "refused before sending" vs "sent, no answer".

## STR-012 - Typed config and registry errors

Reported by: config.

Every config error is `anyhow::bail!` prose, though callers already branch by
kind indirectly: `load_optional` distinguishes `Located::Missing` from `Found`
while `load` flattens `Missing` into a string, and `message_target` needs
"config missing" vs "config broken" vs "audit mismatch" vs "root gone".
Orchestrating agents would benefit from telling "not in a project" from
"project config broken". Proposed: `ConfigError { NotFound { searched_to },
Parse { file, .. }, Reserved { name, kind }, UnknownProvider { name, where_ },
GroupMember { .. }, ProfileUndefined { provider, profile, searched },
Registry(..) }`, the place to hang exit codes later; and for
`project::lookup`, today `Result<PathBuf, String>`, a
`LookupError { Unknown { registered }, Ambiguous(Vec<String>), Expand(..),
NotAbsolute }`.

## STR-013 - Typed forensic lookups

Reported by: forensics.

`write_bundle` returns `Option<PathBuf>` and `eprintln!`s each failure;
`find_transcript_path` and `summarize_session` return `Option`; `list_recent`
returns `Vec` with errors swallowed. A caller cannot tell "no rollout exists"
from "unreadable" from "codex home unknown"; the custom `CODEX_HOME` defect in
`notes/bugs.md` is that confusion.
Proposed: `enum Lookup { Found(PathBuf), Absent, Unreadable(io::Error) }`, so
the baseline code can say which happened instead of the generic warning.

## STR-014 - Keeper `Spec` durations flattened for JSON

Reported by: lifecycle. Narrowed: the protocol bytes are now a shared
`keeper::Control` enum.

`Spec`'s six `*_ms: u64` fields are durations flattened for JSON. Proposed:
serialise the `TeardownSchedule` from the keeper-bounds entry in
`notes/consolidations.md` instead.

## STR-015 - Log scope and audit id

Reported by: orchestration, config.

`private: bool` beside `audit_id: &str` is threaded through `record_run`,
`RecordCtx`, `sessions::record`, `audit::log_result`,
`ProviderRuntime::from_config` and `orphan_log_path` to select which log
family a row goes to. `AuditConfig.id: Option<String>` is an identity compared
against rows, where `""` (rows' "none", `!rec.audit_id.is_empty()`) and `None`
are two spellings of absence reconciled by hand. Proposed: an `AuditId`
newtype with a validated format (with `generate_short_id` beside it), and one
`LogScope { audit_id: AuditId, private: bool }` (or
`enum LogScope { Public(AuditId), Private(AuditId) }`) resolved once from
config, so no call site can pair a private project's id with the public log.

## STR-016 - Row kind as an enum

Reported by: orchestration.

`SessionRecord.kind: String` and `SessionEntry.kind: &'static str` are written
as `"run"` or `"session"` (their doc comments are stale, in `notes/bugs.md`),
and the auto-resume inside a fan-out is recorded as `kind = "run"` though it is
a resume. Proposed: an enum.

## STR-017 - Optional epochs and an `Age` type

Reported by: orchestration.

`SessionRecord.epoch_secs` defaults to 0 and every reader re-checks `== 0`
(`age_secs`, `run_session_show`, `run_sessions`). Proposed: `Option<u64>` with
a serde default, or an `Epoch` newtype, under the `Age` type from the ages
entry in `notes/consolidations.md`.

## STR-018 - `ProjectRoot` and the resume target

Reported by: config.

Beyond the canonical-identity question in `notes/consolidations.md`:
`expected: Option<(PathBuf, Option<String>)>` in `run_session_resume` is a bare
tuple carrying the validated target, and should be named. `load` returns
`(ReviewConfig, PathBuf)` and `load_optional` `(ReviewConfig,
Option<PathBuf>)`, where the root is derivable from `files.local`. Proposed:
`project_root: Option<ProjectRoot>` on `ReviewConfig`, no tuples.

## STR-019 - Model and effort per provider, and profile fields only some providers honour

Reported by: config, providers.

For opencode, effort is the `#variant` suffix of the model string; for maki it
is config-only and the CLI flag is silently ignored; for claude and codex it is
a flag. The profile cannot express "this provider will ignore this field", so
`review config` prints `effort high` on a maki profile and the sidecar records
`effort: high` for a run that did not use it: `ProviderResult::effort` is
filled from the request, not from what was applied (the false-record problem
the `writable_roots` override comment warns about). Likewise `config` (codex
`-c` only), `writable_roots` (codex and grok write only) and `sandbox` (not
claude) are checked at run time with a warning, or not at all: `config` on a
grok or claude profile is silently dropped.

Proposed: since profiles are keyed by provider (`[codex.deep]`), the parse
rejects fields a provider does not honour, the same way `deny_unknown_fields`
rejects typos (a per-provider profile type, or `Profile` validated against
capabilities in `parse_profiles`); a `ModelSpec` the provider renders, so
opencode's `provider/model#variant` composition is in one place; and the
recorded model and effort come from the adapter ("applied" vs "requested,
unsupported"). The config hunter calls this the cheapest way to stop maki
users writing `effort =` and believing it.

## STR-020 - Pass a resolved launch, not its fields

Reported by: providers, orchestration, config.

`app` copies six fields out of `Profile` into `Launch` field by field (`model`,
`effort`, `sandbox`, `env`, `config`, `writable_roots`, plus env keys
separately), and `invoke` takes them again as seven positional parameters.
Every runner carries `#[allow(clippy::too_many_arguments)]` with a comment
arguing the arguments are independent axes; they are mostly one resolved
launch passed one field at a time. Which profile table a run used is lost,
though `review config` knows it. Proposed: move `Launch` into `provider` and
pass it whole as a `LaunchSpec { provider, start: SessionStart, settings:
RunSettings, prompt, root }`, where `RunSettings` is also what a resume builds
from `InheritedSettings`, so the profile and the inheritance are two
constructors of one type and "what a resume inherits" is visibly the same
struct as "what a run launches with" (today `env` and `config` are silently
absent on one side). `Launch` carries the resolved profile and its `Sourced`
origin so the row can record the table.

## STR-021 - Smaller forensic types

Reported by: forensics.

- `last_event: Option<String>` formatted `"type/ptype"` travels as
  `Digest.last_rollout_event`, `Verdict.last_event`,
  `Incident.last_rollout_event` and `TranscriptSummary.last_event`; with the
  rollout `Event` it becomes `EventKind` with a `Display`.
- `last_in_flight_tool: Option<(String, String)>` is a bare `(name, args)`
  pair of one type, swappable, nested again as `(call_id, (name, args))` in
  the parser; proposed `struct InFlightTool { call_id: Option<CallId>, name,
  args }`.
- `TranscriptSummary.path: String`, `Incident.transcript_path: Option<&str>`
  and `incident_path: Option<String>` store paths as strings; a non-UTF-8 home
  is mangled, and the transcript path is re-read from the mangled string in
  `write_bundle`.
- `Incident` is 28 loose `&str`/`Option<&str>`/bool/integer fields; `signal`,
  `stdin_write_error`, `turn_error` and `last_rollout_event` are all
  `Option<&str>`.
- `Incident.codex_version` and `Meta.codex_version` are schema fields named
  after one provider; rename to `provider_version`.

## STR-022 - Stall timeout without a zero sentinel

Reported by: forensics.

`watchdog::Timings` and `crate::timings` share a name but are different
things. `stall_grace: Option<Duration>` is converted in `provider.rs` from
config's "`secs == 0` means off". Proposed: the conversion belongs to the
config type (`StallTimeout::{Off, After(Duration)}`), so `0` never travels as
a sentinel.

## STR-023 - Grok outcome types

Reported by: grok-man.

- The cancel cause is a formatted `String`
  (`"permission_cancelled: permission for `x` was refused"`) fed into
  `explain_grok_no_answer`; proposed `{ category: String, refused_tool:
  Option<String> }`, so the digest carries the category as a greppable,
  branchable field (`permission_cancelled` and `max_turns` call for different
  moves).
- `unread_background_results` returns `Result<_, String>` while the module is
  `anyhow`, and `None` from `session_updates` is folded into a different
  string in `run_grok`; proposed
  `enum WakeCheck { Clean, Unread(Vec<Unread>), Unverifiable(Reason) }`,
  replacing three ad-hoc `wake_failure_digest(reason)` sites.

## Moves and rewrites

## STR-024 - Split `provider.rs` into a lifecycle driver and per-provider adapters

Reported by: providers, forensics, grok-man, orchestration.

`provider.rs` is about 3600 lines that the providers hunter reads as five
modules: process I/O plumbing, the signal supervisor, the permission front end
(`invoke`), three runners, and output rendering. The runners do not share a
skeleton: each re-implements the same lifecycle (mint or learn a session id,
clear stale interrupt requests, check stop, mark in flight, spawn under a
keeper, signal launch, wait with a stop arm, consume the interrupt request,
drop the marker, drain the pipes, classify, build a codex-shaped `Digest`),
each ordering the steps slightly differently. Adding maki means copying
`run_claude`; adding opencode means a fourth lifecycle by hand.

Proposed layout (providers):
- `provider/mod.rs`: the enum, the capability table, `invoke` as a thin
  driver.
- `provider/lifecycle.rs`: one async driver taking an `Adapter`, owning
  stop-before-launch, the marker policy, keeper spawn, the launch signal, the
  stop/interrupt arm, interrupt-request consumption, the drain, and outcome
  assembly from the classifier verdict plus interrupt facts (the home for the
  interrupt, marker, stop and shutdown questions in
  `notes/consolidations.md`).
- `provider/io.rs`: `SharedBuf`, `read_capped`, `collect_reader`,
  `write_stdin`, `TempFile`. The three hand-written stream readers
  (`read_capped`; `read_claude_stream` with head/tail and init scan;
  `read_capped_stdout` with id scan and first-byte watch) become one
  line-splitting reader with a pluggable `LineObserver` (publishes the id,
  flips readiness) and a buffer policy (head-capped, or head plus tail).
- `provider/{claude,codex,grok,maki,opencode}.rs`: each provides
  `argv(&Launch)`, `env(&Launch)`, its `LineObserver`, and
  `classify(stdout, stderr, status) -> Verdict`; the JSON event formats stay
  local.
- `provider/render.rs`: `print_result`, `print_digest`, `resume_hint`, merged
  with `app`'s summary rendering.
- The signal supervisor, `STOP` and `Unrecorded` move out of `provider` (see
  the lifecycle module below).
- `invoke`'s grok write preparation (`GrokWrite`, `prepare_grok_write`, the
  trust re-read) and codex's `config_writable_roots_override` move into the
  grok and codex adapters behind a `prepare(&Launch) -> Prepared` hook, so
  `invoke` stops branching on provider strings. The grok-man hunter adds that
  `ProviderResult` has a grok-only field (`grok_trust`) and that maki (its
  permissions config) and opencode (a custom agent definition, a private
  config and DB dir) need the same kind of hook.

The forensics hunter proposes the same seam from the other side: per-provider
directories (`src/codex/{mod.rs runner, stream.rs, rollout.rs, transcript.rs,
watchdog.rs, outcome.rs}`, `src/claude/`, `src/grok/`, `src/maki/`,
`src/opencode/`), `src/forensics/{incident.rs, replay.rs}`, and a
provider-agnostic `src/outcome.rs`. Today codex's NDJSON stream parser is
inline in a 600-line `run_codex_json` while its rollout parser is a separate
top-level module, a split that follows "which file on disk" rather than "which
provider".

Within this: claude and maki share a wire format, not an argv.
`last_claude_result`, `ClaudeResult`, `interpret_claude_output`,
`claude_usage`, `result_served`, `is_claude_init` and `read_claude_stream`
become a `claude_stream` decoder shared by both adapters, while the argv
builder is not shared (maki ignores `--effort`, `--permission-mode` and
others). `result_served` is already shared between grok and claude by
field-name coincidence (`modelUsage`, `total_cost_usd`); it would return empty
for maki, correctly, but by luck. Grok's wake loop (`run_grok` wrapping
`run_grok_turn` and merging usage, cost, turns and leftovers by hand) becomes a
general multi-turn wrapper with outcome merging, or at least
`RunOutput::absorb(next)` on the types. `run_codex_json`, `run_claude`,
`RunOutput` and `new_output_file` are `pub` only for `tests/provider_runs.rs`;
`lifecycle::run(adapter, stub_command)` is a better seam and the per-runner
`pub`s go.

## STR-025 - A `src/lifecycle/` module with a `Run` handle

Reported by: lifecycle, providers.

The lifecycle hunter calls the keeper the best-built part of the code base and
not in need of rewrite, and the trouble around it the problem: the run
lifecycle (stop requested, launched, signal-safe, interrupted, provider
exited, torn down, recorded) is a state machine no type owns. Where it lives
today:
- `keeper.rs`: the subreaper, cohesive.
- `contained.rs`: three unrelated jobs (the parent-side keeper client, the
  process-wide ledger and its human report, the registry and admission gate)
  plus `version_probe`, an incident feature.
- `provider.rs`: `STOP`, `UNRECORDED`/`Unrecorded`, `install_signal_supervisor`,
  there because codex ran first.
- `inflight.rs`: markers, launch locks and interrupt requests sharing a
  directory, plus the process-identity helpers the keeper and `interrupt_cmd`
  also need.
- `interrupt_cmd.rs`: the verb plus `interruptible` and
  `cleanup_pending_message`, policy owned by a CLI verb and imported by `app`.
- The runners: the load-bearing per-run ordering, commented as such in both.

Proposed `src/lifecycle/`:
- `process.rs`: `Pid`, `ProcessIdentity`, `PinnedProcess`, pidfd and `/proc`
  primitives, moved out of `keeper.rs`.
- `keeper.rs`: unchanged in substance, owning `Spec`, `Control`, `Event`,
  `Teardown`, `TeardownSchedule`.
- `client.rs`: `spawn`/`Contained`, extended into a `Run` handle owning the
  marker, readiness, the graceful-stop decision, the interrupt fact and the
  label, so a runner's lifecycle shrinks to
  `let run = Run::start(cmd, session, provider).await?; ... run.signal_ready(); ... let end = run.finish().await;`,
  with the marker/keeper ordering invariants in one function testable without
  a provider stub.
- `supervisor.rs`: the `Shutdown` state, registry, `Unrecorded`, ledger and
  signal handler: one module answering "what happens when `review` is told to
  stop".
- `store.rs` (from `inflight`): markers, launch locks and interrupt requests
  keyed by `SessionId`, returning a typed run state.
- `report.rs`: `report_lines`, moved out of the ledger.
- `version_probe` moves to `incident.rs`.

The hunter calls it a big move and the right one: each new provider then
contributes only its argv, env, stream parser and readiness trigger, instead
of copying about 150 lines of lifecycle ordering per runner. It overlaps the
providers hunter's `provider/lifecycle.rs` in the entry above; the two
readings differ on where the driver lives, and both put the supervisor beside
`contained`.

## STR-026 - Stop using the sidecar log as an IPC channel

Reported by: lifecycle, orchestration.

`interrupt_cmd` learns the result of the run it interrupted by polling
`sessions::read_all()`, which reads and parses both whole JSONL logs every
`INTERRUPT_POLL` for the life of the wait, so the cost grows with history. It
counts rows by position (`skip(rows_before)`), assuming no other process
appends a row for the session in between. More generally `sessions.rs` is
re-read per call: `run_sessions` calls `latest_for_session` per queued launch,
and `run_message` + `run_session_resume` + `interrupt` read it three or more
times per message. Proposed: the owner, which already knows the answer when it
consumes the request, writes a small `<session>.<run_id>.result` beside the
marker for the verb to read; and the log is parsed once per process.

## STR-027 - Replace the process-wide statics with a `Lifecycle` value

Reported by: lifecycle.

The `LEDGER`, `REGISTRY`, `STOP` and `UNRECORDED` statics make the lifecycle
unobservable per test: every test in a process shares one ledger, so a test
that leaves an unknown teardown can poison another's exit-status assertion.
Proposed: a `Lifecycle` value created in `app::run` (or one per runtime) and
passed down, making the ledger and registry assertable in unit tests and
removing the `Option<Registry>` lazy init.

## STR-028 - `contained::spawn` should pass exactly what the runner built

Reported by: lifecycle.

`spawn` rebuilds the keeper's `Command` from `get_envs()`, which reports
explicit sets and removes but not `env_clear()`. A runner that clears the
environment would get a provider that inherits all of `review`'s environment,
secrets included. No current runner calls `env_clear`; opencode's mandatory
isolation is where someone will reach for it, and nothing will fail when they
do. The same function passes `program` and `args` through `to_string_lossy`,
so a non-UTF-8 path or argument reaches the provider mangled. And `spawn`
applies `get_current_dir()` to the keeper while `PWD` stays whatever `review`
had, so every opencode runner must remember to set `PWD` (opencode takes its
directory from `PWD`). Proposed: the keeper's `Spec` carries the provider's
environment explicitly (a full `Vec<(OsString, OsString)>` sent as bytes, or
`Inherit | Exact(map)`) and its argv as bytes, so the keeper `execve`s exactly
what the runner built; and when a cwd is given, the spawn primitive sets `PWD`
to match for every provider.

## STR-029 - Split `config.rs` and fix its dependency edges

Reported by: config.

`config.rs` does six jobs: the file format (`parse_file`), the layered
resolver, filesystem discovery (`Selection`, `locate_config`, `load*`), the
`init` command, random id generation, and the provider sandbox vocabulary
(`sandbox_for`). Proposed:
- `config/file.rs`: `ConfigFile`, `Profile`, `parse_file`, pure.
- `config/resolve.rs`: `ReviewConfig`, `Sourced`, `ProfileEntry`, `resolve`,
  and the archetype resolver now in `app`.
- `config/locate.rs`: `Selection`, `locate_config`, `load`, `load_optional`,
  `project_root`, `global_config_path`. `Selection` arguably belongs with
  `project.rs`, whose `select` is its only interesting constructor; config
  should only consume a located path.
- `config/write.rs`: `config_write` plus `init` (the only writers of
  `.review.toml`) and `ensure_audit_id`.
- `generate_uuid` moves to the provider spec's session-id capability (the
  grok-man hunter: a leaf util; `grok_home` depending on `config` for a uuid
  is an edge pointing the wrong way). `generate_short_id` goes with the audit
  id type.
- `sandbox_for` moves to the provider layer.

Other edges and state:
- `config::check_projects` calls `project::check_name`, and `project` calls
  `config::global_projects` and `config::hostname`: a cycle. Merge the
  registry (parse, validate, lookup) into `project.rs`, with config handing it
  the raw table.
- `config_cmd` imports `provider::is_available` and hand-encodes claude's
  sandbox semantics; with a capability table it imports the table only.
- `ReviewConfig.files` is filled after `resolve` returns (`cfg.files = ...`),
  so `resolve` produces a value that is wrong until the caller patches it, and
  tests build configs with no files. Pass the file metadata into `resolve`, or
  return it alongside.
- `config_tests.rs` is included as a `#[path]` module beside a second
  `mod tests` in `config.rs` that also exercises `resolve`; after the split
  each file owns its tests.

## STR-030 - Split `writable_roots` into derivation and rendering; consider moving the toolchain facts to config

Reported by: config.

`writable_roots` is codex-shaped in name and output (`config_override` emits
`sandbox_workspace_write.writable_roots=`), while its derivation is generic and
grok reuses it. Proposed: split derivation ("what a build on this host needs")
from rendering (codex's `-c`, grok's profile hash), with `invoke`'s `derives`
from the capability table. It also hard-codes brokkr (`$HOME/.brokkr`,
`$XDG_DATA_HOME/brokkr`) and cargo and LFS facts: one operator's toolchain as
a sandbox policy, compiled in. The hunter grants the module doc's argument
that they are deterministic and flags it as the one place config-ish policy
lives in code, not urgent: a global `[_sandbox].derive = [...]` or named
derivers selected in config would move it out of the binary.

## STR-031 - Archetype selection belongs to config

Reported by: config, orchestration.

The `-a` expansion (`all`, groups, comma lists, the unknown-name error listing
configured names, dedup, the empty check) is inline in `app::run`, splitting a
comma string and matching `"all"` and group/archetype names there. It relies
on a config invariant ("Resolution checked every member exists") through
`unwrap_or_default()`, a silent empty prime if the invariant breaks. The
results are a bare `Vec<(String, String)>` of (label, prime). Proposed:
`cfg.select_archetypes(&str) -> Result<Vec<ArchetypeRun { label, prime }>>`
owning `all`, `bare`, groups and dedup together, with config owning the
messages.

## STR-032 - Split `app.rs` along its seams

Reported by: orchestration.

`app.rs` (about 2100 lines) does five jobs: top-level dispatch; the fan-out
(archetype expansion, provider selection, profile validation, launch
construction, stagger and lock, collection, printing, exit status);
codex-specific auto-resume policy; message and resume (target validation,
interrupt, inheritance, stale gate, lock handshake, recording); and three
read-only views (`sessions`, `sessions <id>` with codex transcript rendering,
`incidents`) plus a `DigestSummary` renderer. The hunter reads it as the de
facto provider registry and run-outcome classifier without admitting to
either. Proposed:
- `app.rs`: one `match cli.command` returning an exit status (today a chain of
  `if let Some(Command::X)` with the run as the fallthrough and `--session`
  handled after the subcommands).
- `run.rs`: the fan-out.
- `message.rs`: message and resume, with `interrupt_cmd` folded beside it
  (`run_message` already calls its internals such as
  `cleanup_pending_message`).
- `session.rs`: the session lookup and state query.
- `views.rs` or per-verb files for `sessions`, `sessions <id>`, `incidents`.
- Outcome rendering moves to the provider side with the typed outcome.

Also in this scope:
- Provider knowledge in orchestration: `run_session_show` renders codex
  rollouts directly (`transcript::summarize_session`, `provider == "codex"`
  twice) and cannot honour `CODEX_HOME` overrides; maki and opencode have their
  own session stores (opencode's private database), so a per-provider "show
  what this session left" hook is the seam. Same for `RESUME_NUDGE`,
  `after_run`'s codex gates and `Unrecorded::for_provider`.
- `run_session_resume` carries vestigial optionality: `instructions:
  Option<String>` is always `Some` (the `None => read_stdin()` branch has been
  dead since `resume` became an alias of `message`); `expected` is always
  `Some`; `launch_lock` is `None` only on a dry run, which returns before it
  matters. Fold it into `run_message` or make the parameters non-optional.
- Fan-out bookkeeping is split from its data: results are collected as
  `Vec<(String, ProviderResult)>` keyed by a label cloned out of `RecordCtx`,
  and printing groups by comparing consecutive labels, which only works because
  tasks are awaited in launch order. A
  `Vec<ArchetypeRun { archetype, launches: Vec<JoinHandle> }>` makes grouping
  structural.
- `cli.rs`: `RUN_ONLY` is a hand-kept list of clap ids that must be updated for
  any new run flag; a run-flag `ArgGroup` or a flattened `RunArgs` struct
  checked by "any arg present" removes it. `provider` should be
  `Vec<Provider>`. The hunter finds `prompt.rs`, `input.rs`, `main.rs` and
  `lib.rs` small and fine, and `main.rs`'s keeper-first dispatch right.

## STR-033 - `sessions.rs`: one row type for both directions

Reported by: orchestration.

`sessions.rs` has a 19-argument `record` (with `#[allow(too_many_arguments)]`),
a write struct `SessionEntry` and a read struct `SessionRecord` maintained in
parallel by hand. `served_model`, `cost_usd`, `grok_trust` and `leftovers` are
write-only (deliberate for `served_model`, per its comment, undocumented for
the rest), and several `#[allow(dead_code)]` sit on fields that are in fact
read (`sandbox`, `writable_roots`, `audit_id`). Proposed: one `SessionRow` for
both directions, with `served` excluded from inheritance by the `RunSettings`
constructor rather than by its absence from the read struct; `record` taking
it whole; and a reader that parses the file once per process.

## STR-034 - Split forensics into a generic core and provider extensions

Reported by: forensics.

- `Digest` is codex's struct dressed as everyone's: `transcript`,
  `last_rollout_event`, `quiet_secs`, `recovered_from_transcript`, `log_lines`
  ("codex log lines" in `print_digest`), and `task_complete`/`stream_error` in
  `DigestSummary` are codex concepts that claude and grok fill with `None`
  (`no_answer_digest`, `wake_failure_digest`), and maki and opencode would too;
  for opencode, "recovered" vs "captured" is meaningless. Proposed: a common
  core (outcome, usage, exit, terminator, incident path, cost) plus
  `enum ProviderForensics { Codex(CodexForensics), Claude(..), Maki(..),
  Opencode(..) }`, serialised tagged, with `print_digest` dispatching the
  provider block.
- `incident` is about 60% generic and 40% codex. Generic: stderr, stdout tail,
  prompt, argv, cwd, env, exit, duration, the replay command, directory
  naming. Codex: `codex_version`, `transcript_path`, `final_answer_present`,
  `rate_limits`, `quiet_secs`, `last_rollout_event`, `turns`. Proposed: build
  the generic part from the actual `Command` that `contained::spawn` launched
  (`tokio::process::Command::as_std()` exposes `get_program`, `get_args`,
  `get_envs`, `get_current_dir`), so the replay is the spawn by construction:
  opencode's isolation env and `PWD` appear automatically, the hand-maintained
  `RUST_BACKTRACE=1` goes, and so does the comment explaining why `binary` is
  not `"codex"`. Each provider adds an extension block. This is also what would
  give maki and opencode bundles at all; today claude and grok get none, and
  maki's "is_error but exit 0" is the kind of failure a bundle exists for.
- Generalise the watchdog into "progress source plus answer evidence",
  thresholds provider-owned. The loop shape is provider-agnostic; only the
  progress source (rollout length) and the answer predicate are codex's.
  opencode streams every event on stdout, so its stdout byte count is a
  progress source, and its stream says when the last assistant `text` arrived.
  Whether opencode shares codex's "cannot stay silent" property is unknown and
  the module's doc says claude does not, so `stall_grace` must default to off
  for any unmeasured provider: a per-provider policy with one home. The 0.147.0
  history belongs in the codex reference, not the generic loop. Today whether
  the watchdog applies is implicit: it runs only because only
  `run_codex_json` constructs one.
- `IncidentSummary` is a hand-maintained subset of `Meta`, mirroring field
  names by convention with `#[serde(default)]` everywhere, so renaming a
  `Meta` field makes `review incidents` silently show defaults. The listing's
  verdict ladder lives in `app.rs`, away from the schema. Proposed: `Meta`
  itself `Deserialize` with defaults (or a shared struct), and the verdict a
  method on the persisted type, ideally reading the persisted outcome.
- `schema: 1` is written and never read; check it on read or drop it, now,
  while the bundle schema is changing.

## STR-035 - Split `grok_home.rs` and give grok a module of its own

Reported by: grok-man.

`grok_home.rs` is four modules in one file: the trust store mirror and
decision; sandbox profile naming, TOML merge and the locked atomic write; the
session log location; and background-result analysis with the wake prompt.
The last is turn-outcome logic that belongs with the grok runner
(`provider/grok/wake.rs`). The first two are provider-local permission setup,
the job the `prepare` hook in the provider split above generalises.
`locked_update` is a generic flock-plus-atomic-replace primitive used once;
opencode's isolated config will want it, so it belongs in a small `fsutil`
module. `provider.rs` depends on grok internals by path (`crate::grok_home::...`
at a dozen sites, plus `provider_tests.rs` hashing a profile name); once the
grok runner has its own module, `grok_home` becomes its private child and
nothing else sees `PROFILE_PREFIX`. `Grok::resolve` is called four times per
run (`prepare_grok_write`, the landed check, `run_grok`, `run_grok_turn`) from
the same env, each re-reading the process environment and re-canonicalizing;
resolve once per launch and pass it, which also makes the child's view a value
rather than a re-derivation that could see a changed environment.

`man.rs` is small and well-shaped. A `build.rs` generating `TOPICS` from
`docs/*.md` would remove the catalog test but lose the hand-written
summaries; the hunter calls it not obviously better.

## STR-036 - Test layouts that follow the design

Reported by: providers, lifecycle, forensics, grok-man, config.

- Providers: `provider_tests.rs` tests the pure claude and grok classifiers,
  codex sandbox argv, the writable-roots override parse and the announcement
  lines, all against private functions. `tests/provider_runs.rs` drives
  `run_codex_json` and `run_claude` directly, bypassing `run_codex`, so codex's
  argv (`--ignore-rules`, `--ignore-user-config`, `approval_policy`, network
  grants, the `resume` placement of `-o`) is never exercised end to end, and
  bypassing `invoke`, so root derivation, the sandbox translation and the
  recorded fields are untested where they combine. Grok has no runner-level
  test. Proposed per adapter: a table test of `argv(Launch) -> Vec<String>`
  pinning every safety-critical flag, a fixture test of `classify`, and one
  shared lifecycle suite run against a stub per provider (interrupt, stop,
  marker, death), which maki and opencode get for free.
- Keeper: `tests/keeper.rs` covers what only a protocol test can (owner
  vanishing, terminate before go), but its spawn helper hand-copies
  `contained::spawn` (pre-exec hook, fd un-cloexec, spec), and `keeper_given`
  omits `block_handled_signals`, testing a keeper started differently from
  production; a `keeper::Launcher` used by both removes the copy. The
  admission rules (`admit`, leaves-first targeting, the verdict) have no unit
  tests; `keeper.rs`'s only unit tests check `/proc` parsing. Separating the
  teardown policy (which members to signal this round given a tree snapshot,
  and the verdict) from the syscalls makes it testable with synthetic trees.
- Forensics: the behaviour that matters is cross-module (watchdog kill, then
  recovery, then `after_run`), covered only through the stub harness, and the
  Stranded-kill defect lives in that gap. With one rollout fold and one typed
  outcome, the chain becomes a pure function from (rollout bytes, scope, `-o`
  contents, exit, terminator) to outcome, table-testable without processes.
- `man`: the sandbox-table and level-list tests check docs against
  hand-maintained copies, so they will not catch a provider being added; with a
  registry the sandbox test requires one column per registered provider.
- Config: the pairwise-agreement tests (`man` sandbox table vs `sandbox_for`,
  `man` reserved-words table vs `RESERVED_NAMES`, `cli` subcommands vs
  `RESERVED_NAMES`, `every_documented_level_maps_somewhere`) each guard a copy
  that a type or a derived list would remove (see `notes/consolidations.md`).
  `config_write` tests write into `target/test-scratch` with a pid-and-thread
  name and best-effort removal, fine, but lack the read-error test that would
  have caught the config-wipe defect in `notes/bugs.md`.

## STR-037 - Requirements the new providers bring that no current runner has

Reported by: providers, lifecycle.

- opencode's isolation env must be applied after the profile env, or a
  profile could undo it: the opposite of the "set before profile env so a
  profile can restate it" ordering every current runner uses, so a new
  ordering policy that needs to be explicit.
- opencode's `--standalone` must be non-optional, because without it the run
  attaches to a background service outside the keeper's tree, whose "clean"
  verdict would then be true but meaningless. Nothing in the lifecycle can
  detect a violation, so the lifecycle hunter proposes encoding the rule in the
  opencode argv builder with a test.
- opencode exits 130 on interrupt and maki exits 0 on error, so the keeper's
  `Finished.status` must stay advisory and outcome classification must stay in
  the stream parsers.
- opencode's read-only is an agent definition: a third permission mechanism
  beside an OS sandbox and none (the permission plan in the sandbox entry
  above).
- maki's readiness comes after `init` (its SIGINT handler is installed by
  then), opencode's after its first event; both are SIGINT-once, and the
  keeper's forward-once suits both, since maki's second SIGINT kills it.
- Effort via maki's config would mean writing maki's config per run, another
  `prepare_grok_write`-style setup inside `invoke` unless the `prepare` hook
  exists.
