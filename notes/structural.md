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

## STR-003 - Session identity plumbing residue

Executable launch identity is typed. Inflight marker/interrupt facades, Grok
session-file facades and Codex transcript facades now parse raw evidence before
passing SessionId to their actual path builders/search. Malformed first stream
IDs remain accepted evidence without path authority; historical rows still read.

Remaining: the operator-frozen inflight LaunchLock API remains a string facade
with its existing validator. Lease's channel and result evidence remain strings;
callers that already have SessionId still round-trip through some string facades.
A future API-only conversion can remove those redundant parses, but must retain
raw malformed evidence and leave frozen lock handling untouched. Do not claim
these types establish unique Codex suffix matches or symlink confinement.

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

Reported by: config. Narrowed: the separate resume function and its
expected-target plumbing were folded into `run_message`; the root type and the
`load` tuples remain.

Beyond the canonical-identity question in `notes/consolidations.md`:
`expected: (PathBuf, Option<String>)` in `run_message` is a bare tuple
carrying the validated target, and should be named. `load` returns
`(ReviewConfig, PathBuf)` and `load_optional` `(ReviewConfig,
Option<PathBuf>)`, where the root is derivable from `files.local`. Proposed:
`project_root: Option<ProjectRoot>` on `ReviewConfig`, no tuples.

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
- (`Incident` already carries `provider_version`; `Meta.codex_version` is the
  schema-1 wire key, kept on purpose for existing bundles.)

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
- `run_codex_json`, `run_claude`, `RunOutput` and `new_output_file` are `pub`
  only for `tests/provider_runs.rs`; a test seam through `invoke` (or a
  lifecycle-level stub suite) lets them go.

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

## STR-028 - `contained::spawn` should pass exactly what the runner built

Reported by: lifecycle. Narrowed: `ProcessSpec` already owns env layering and
sets `PWD` to the cwd for every provider; the exact-bytes argv/env transport
and the `env_clear` hole below remain.

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

## STR-030 - Split `writable_roots` into derivation and rendering; consider moving the toolchain facts to config

Reported by: config. Narrowed: derivation, filtering, host and output are now
separate files under `src/writable_roots/`. The residue of the split is that
codex's `-c sandbox_workspace_write.writable_roots=` rendering
(`writable_roots/output.rs`) still lives in the generic module rather than the
codex adapter; the toolchain-facts-in-config half is a new feature.

Original claim: `writable_roots` is codex-shaped in name and output (`config_override` emits
`sandbox_workspace_write.writable_roots=`), while its derivation is generic and
grok reuses it. Proposed: split derivation ("what a build on this host needs")
from rendering (codex's `-c`, grok's profile hash), with `invoke`'s `derives`
from the capability table. It also hard-codes brokkr (`$HOME/.brokkr`,
`$XDG_DATA_HOME/brokkr`) and cargo and LFS facts: one operator's toolchain as
a sandbox policy, compiled in. The hunter grants the module doc's argument
that they are deterministic and flags it as the one place config-ish policy
lives in code, not urgent: a global `[_sandbox].derive = [...]` or named
derivers selected in config would move it out of the binary.

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

## STR-036 - Test layouts that follow the design

Reported by: providers, lifecycle, forensics, grok-man, config.

- Providers: `provider_tests.rs` tests the pure claude and grok classifiers,
  codex sandbox argv, the writable-roots override parse and the announcement
  lines, all against private functions. `tests/provider_runs.rs` drives
  `run_codex_json` and `run_claude` directly, bypassing `run_codex`, so codex's
  argv (`--ignore-rules`, `--ignore-user-config`, `approval_policy`, network
  grants, the `resume` placement of `-o`) is never exercised end to end, and
  bypassing `invoke`, so root derivation, the sandbox translation and the
  recorded fields are untested where they combine. (Grok does now have
  runner-level stub tests in `tests/provider_runs.rs`.) Proposed per adapter: a table test of `argv(Launch) -> Vec<String>`
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
- Effort via maki's config still needs its actual per-run config implementation
  when maki lands. The adapter `prepare` operation and owned prepared context
  now exist; no maki configuration or permission mechanism is implemented.
