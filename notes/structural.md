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

## STR-001 - Provider residue: session artifact ownership

Reported by: providers, lifecycle, config, orchestration, grok-man, forensics.

Provider identity and runtime command prefixes are shared and typed. The remaining
artifact selection in app::history (review sessions <id>) already parses the
persisted provider and matches exhaustively: Codex has a rollout; Claude and Grok
do not. The old string-comparison claim is stale. Move artifact discovery and
rendering to their provider/forensic owners with the forensics split, rather
than inventing a static artifact capability flag. That ownership move remains.

## STR-003 - Session start and session id as types

Reported by: providers, lifecycle, orchestration, forensics, grok-man.

Launch intent and caller-side minting are now typed in provider::session:
SessionStart reaches all runners and SessionId checks supplied launch IDs.
The shared grammar owns the 128-byte path-component bound. Historical rows
and provider evidence intentionally remain strings, not validated serde types.

Residue: inflight's string-facing marker/interrupt/lock APIs and Grok session
file reads still use fallible safety guards; Codex live stream IDs, transcript
lookup and session artifact APIs still carry strings. Convert executable/path
identities at those boundaries without rejecting malformed historical rows or
losing malformed provider evidence. Do not claim all paths infallible until
those consumers take SessionId. Provider result strings need not themselves
become validated evidence: evidence and executable identity are different.

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

## STR-018 - `ProjectRoot` and the resume target

Reported by: config.

Beyond the canonical-identity question in `notes/consolidations.md`:
`expected: Option<(PathBuf, Option<String>)>` in `run_session_resume` is a bare
tuple carrying the validated target, and should be named. `load` returns
`(ReviewConfig, PathBuf)` and `load_optional` `(ReviewConfig,
Option<PathBuf>)`, where the root is derivable from `files.local`. Proposed:
`project_root: Option<ProjectRoot>` on `ReviewConfig`, no tuples.

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

## STR-024 - Provider adapters: the parts the lifecycle driver left behind

Reported by: providers, forensics, grok-man, orchestration. Narrowed twice: the
file was split mechanically into `src/provider/`, and the shared lifecycle now
exists as `src/provider/lifecycle.rs` (`ProcessSpec`, `Lease`, `Run`), which
all three runners drive. The driver is a handle the adapter calls into, not an
`Adapter` trait the driver calls: codex's rollout baseline before spawn and
recovery after, and grok's multi-turn wake loop with its marker alias, are
adapter control flow a trait would have to expose as a hook per step with
shared state between them.

What remains:
- One line-splitting stream reader with a capture policy (head cap, or head
  plus rolling tail) and a per-adapter observer, replacing `read_capped`,
  `read_claude_stream` and codex's `read_capped_stdout`. Its framing rules,
  each load-bearing today: raw capture continues after the observer stops
  scanning; observer parsing continues after capture fills; line accumulation
  is bounded independently of capture; an oversized line is discarded through
  its delimiter, never parsed from an arbitrary suffix; an unterminated final
  line is still offered at EOF (claude's metrics need it); observer facts
  survive reader cancellation; claude's head and tail are joined with a
  delimiter so fragments cannot combine into a fabricated JSON line; codex's
  readiness is its first byte, not its first line.
- A `claude_stream` decoder (`last_claude_result`, `ClaudeResult`,
  `interpret_claude_output`, `ClaudeMetrics`, `is_claude_init`) shared by the
  claude and maki adapters, whose argv builders stay separate (maki ignores
  `--effort`, `--permission-mode` and others). `result_served` is shared by
  grok and claude by field-name coincidence.
- A `prepare(&Launch)` hook per adapter for permission setup (grok's
  `prepare_grok_write` and trust re-read, codex's
  `config_writable_roots_override`, maki's permissions config and per-run
  effort config, opencode's agent definition and private config and DB), so
  `invoke` stops carrying the grok-only `grok_trust` field and its post-run
  trust check.
- Grok's wake loop merges usage, cost, turns and leftovers by hand;
  `RunOutput::absorb(next)` on the types.
- `provider/render.rs`: `print_result`, `print_digest`, `resume_hint`, merged
  with `app`'s summary rendering.
- `run_codex_json`, `run_claude`, `RunOutput` and `new_output_file` are `pub`
  only for `tests/provider_runs.rs`; a test seam through `invoke` (or a
  lifecycle-level stub suite) lets them go.

## STR-025 - Lifecycle module moves around the `Run` handle

Reported by: lifecycle, providers. Narrowed: the `Run` handle exists as
`provider::lifecycle::Run` with its `Lease` (marker, readiness and session
facts, interrupt-request consumption, one-shot stop and kill arms); a runner's
lifecycle is `Run::spawn`, `launch`, `wait`, `finish`, and the ordering
invariants are unit-tested on the lease without a provider stub.

What remains is placement, not mechanism:
- `process.rs`: `Pid`, `ProcessIdentity`, `PinnedProcess`, pidfd and `/proc`
  primitives, out of `keeper/` and `inflight.rs`, which `interrupt_cmd` also
  needs.
- `supervisor.rs`: the `STOP` channel, `Unrecorded`, the registry and
  admission gate, the ledger and `install_signal_supervisor` (today split
  between `provider/runtime.rs` and `contained.rs`): one module answering
  "what happens when `review` is told to stop". See STR-027 for making it a
  value.
- `store.rs` (from `inflight`): markers, launch locks and interrupt requests
  keyed by `SessionId`, returning a typed run state.
- `report.rs`: `report_lines`, out of the ledger.
- `contained::version_probe` moves beside the incident writer.
- `interrupt_cmd`'s `cleanup_pending_message` is policy owned by a CLI verb
  and imported by `app`.

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

Reported by: config. Narrowed: the file has been split mechanically into
`src/config/` (parse, resolve, load, sandbox, ids) with every path kept; what
remains is the moves and edge fixes below.

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

Reported by: orchestration. Narrowed: the file has been split mechanically into
`src/app/` (dispatch, launch, message, history) with every path kept; what
remains is the provider knowledge, vestigial optionality and bookkeeping below.

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

Reported by: grok-man. Narrowed: the file has been split and moved under the
grok runner as `src/provider/grok/state/` (trust, sandbox, events,
background), private to the runner, and grok's write setup moved from the
dispatcher into the runner. What remains is a general `prepare` hook for every
provider, the `fsutil` move and resolving `Grok` once.

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

Reported by: providers, lifecycle. Narrowed: env ordering is explicit
(`ProcessSpec` layers adapter defaults, then profile, then adapter-forced
values, so opencode's isolation goes in the forced layer and a profile cannot
undo it), `PWD` is set to the working directory for every provider, readiness
is a lease fact the adapter's reader sets (maki after `init`, opencode after
its first event), and `Ended.status` is documented as forensics only.

What remains, each landing with its provider:
- opencode's `--standalone` must be non-optional, because without it the run
  attaches to a background service outside the keeper's tree, whose "clean"
  verdict would then be true but meaningless. Nothing in the lifecycle can
  detect a violation: encode it in the opencode argv builder with a test.
- opencode's read-only is an agent definition: a third permission mechanism
  beside an OS sandbox and none.
- Effort via maki's config means writing maki's config per run, a setup step
  for the `prepare` hook in STR-024.
