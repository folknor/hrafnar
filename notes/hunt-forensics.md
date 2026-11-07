# Hunt: forensics (transcript.rs, incident.rs, watchdog.rs)

Scope read: `src/transcript.rs`, `src/incident.rs`, `src/watchdog.rs`, and their
callers in `src/provider.rs` (`run_codex_json`, `Digest`, `DigestSummary`,
`print_digest`), `src/app.rs` (`after_run`, `died_without_answer`,
`got_final_answer`, `run_incidents`, `run_session_show`,
`print_digest_summary`, `digest_summary_note`), `src/audit.rs`,
`src/timings.rs` (constants only).

The short version: "forensics" is three codex-only modules that each re-parse
the same codex rollout format with their own line loop, and their outputs
reach the rest of the program as loose strings and bools. Five other sites then
work out from those bools again how the run ended. Nothing in here is shaped
for a fourth provider. maki and opencode need the generic half (an
invocation's replayable record, a stall over a progress source, a typed
outcome). Today that half is welded to the codex-specific half (rollout
parsing).

---

## Probable bugs first

### B1. A Stranded kill can be reported as a death and then auto-resumed

`watchdog::has_final_answer` asks "has any event since the baseline been a
`final_answer`?" Its doc says this is deliberate, so that it survives "trailing
micro-turns codex sometimes appends after the substantive turn (... each with
its own `task_complete`)". The watchdog kills codex on that basis
(`Kind::Stranded`). Recovery then runs `transcript::summarize_session`, i.e.
`transcript::parse`, and that resets per-turn state at every `task_started`. If
the trailing micro-turn has a `user_message`, reasoning, `task_complete` or a
tool call, `turn_produced_content` is true, so no phantom rollback happens.
`cur.final_answer` is then `None`. `recovered_from_transcript` comes out
false, `captured` is false (we killed it before `-o`), and
`app::died_without_answer` is true. `after_run` returns `AutoResume`. The run
the watchdog killed *because* it had the answer is reported as having none,
and a paid resume is launched.

The two readers answer different questions on purpose ("answered at any point
since baseline" vs "state of the last turn"). But the kill decision uses the
first and the report uses the second. Whatever licensed the kill must be what
the report recovers. Fix: the watchdog verdict carries the answer text (or the
byte range of the event that held it), and recovery uses it in preference to
re-deriving. Or: one rollout fold returns both "last-turn state" and "last
final_answer since baseline", and recovery takes the latter when the
terminator is `Stranded`. A test is worth writing: a rollout with a
substantive turn ending in `final_answer` + `task_complete`, then a
`task_started` / `user_message` / `task_complete` micro-turn, run through the
stub harness to a Stranded kill.

### B2. `find_transcript_path` gives up on the first unreadable directory

```rust
let entries = std::fs::read_dir(&dir).ok()?;
```

This sits inside the DFS loop. One unreadable (or concurrently removed)
subdirectory anywhere under `sessions/` makes the whole lookup return `None`,
not just skip that branch. The fallout is silent:
- the watchdog never fires;
- a resume gets `rollout_baseline = None`, so recovery and the watchdog are
  disabled with a misleading "could not read the rollout" warning;
- `review sessions <id>` reports "not found".

It should be `continue`, with `None` kept only for a missing root.

### B3. The watchdog re-walks the whole sessions tree on every poll

`wait_for_stranded_completion` calls `find_transcript_path` every
`poll_interval`, for the entire run, even after the path is known. That is an
unbounded recursive `read_dir` over every session codex has ever written on
this host. The cost grows with the operator's history, not with the run. Cache
the path once found. The rollout name is stable for a session. The baseline
lookup in `run_codex_json` already did the same walk before spawn and could
hand the path in.

### B4. `incident::scan_transcript` is neither offset-scoped nor phantom-aware

- It reads the whole rollout (`read_to_string`, no `slice_from_offset`). The
  rest of the forensics went to some length to scope reads to the run's
  bytes.
- It resets `final_answer_present` on `task_started` but knows nothing of the
  phantom shutdown turn that `transcript::parse` rolls back. A clean session
  followed by codex 0.147.0's teardown phantom therefore reads as
  `final_answer_present = None` ("suspicious") in `review incidents`, where
  `transcript::parse` would say complete.
- It defines "answered" as `task_complete.last_agent_message != null`. That is
  a third definition, alongside `transcript::final_answer_text` (phase-tagged)
  and the `-o` file. They can disagree: a commentary-only turn whose
  `last_agent_message` is the commentary text reads "completed" here and "no
  final answer" everywhere else.
- `read_to_string` fails outright on invalid UTF-8. The other two readers are
  lossy, so a bundle can lack the transcript tail for a rollout the run itself
  parsed fine.
- `transcript.tail.jsonl` is also unscoped. On a resume the tail can be
  entirely previous turns.

### B5. A custom `CODEX_HOME` makes `review sessions <id>` blind

`run_session_show` passes `override_home = None`, because only env *names*
are recorded. The run itself knew the path (`TranscriptSummary.path`, or the
watchdog's lookup), but the path is not persisted in `DigestSummary`, and on a
clean run no transcript is read at all. Persist the resolved rollout path in
the sidecar row, so the show command never has to re-derive codex's home.

### B6. Smaller ones

- `transcript::parse`: a `_call` without `call_id` is stored under `""`. Every
  later id-less output then retires *all* of them, and two id-less calls
  collide. Also, `name` falls back to the payload type (`pt`), so "the tool
  name" is sometimes a tool name and sometimes an event type.
- `incident::write_bundle` calls `chrono_now()` twice (directory stamp and
  `meta.timestamp`). They can straddle a second boundary, so the directory
  name and `meta.json` disagree.
- `list_recent` relies on directory names sorting chronologically, and it
  lists any directory under `incidents/`, foreign ones included. It also
  always uses the real location. That is fine for the operator view, but it
  means no test can cover it without touching the operator's home.
- The data directory (`XDG_DATA_HOME` else `~/.local/share`) is resolved
  independently in `incident.rs`, `inflight.rs`, `audit.rs`, `sessions.rs`
  and `provider.rs`. Only some of them honour the `data_root` test override.
  `audit::audit_dir` has no override, so the stub tests very likely append to
  the operator's real audit log. Worth checking.
- `replay_command` records only the *profile* env. Whatever the runner injects
  itself never reaches the replay: `RUST_BACKTRACE` is hard-coded on both
  sides by agreement, and anything else is lost. For opencode, where
  isolation is *entirely* runner-injected env (disabled project config,
  private DB, `PWD`), a replay built this way would attach to the operator's
  real config and DB. That is not a reproduction, and it is mildly dangerous.
  See S3.
- `Incident.codex_version` and `Meta.codex_version` are a schema field named
  after one provider.

---

## 1. Axes that should be types

**T1. The rollout event, parsed once.** Four hand-rolled `serde_json::Value`
line loops read the same format: `transcript::parse`,
`watchdog::has_final_answer`, `watchdog::last_event` and
`incident::scan_transcript`. Each does its own `get("payload").get("type")`
string matching. A `codex::rollout::Event` enum would make "a new codex shape
was handled in one reader and not the others" impossible to write: the
0.157.0 `message`/`output_text` change was already fixed in only one place,
`final_answer_text`, and nearly missed the watchdog. Variants would be
`TaskStarted`, `TaskComplete { last_agent_message }`, `TurnAborted`,
`UserMessage`, `Reasoning`, `Message { role, phase, text }`, `AgentMessage
{ phase, text }`, `ToolCall { call_id, name, args }`, `ToolOutput { call_id }`,
`StreamError`, `TokenCount { rate_limits }` and `Other { top, ptype }`. Each
reader becomes a fold over `impl Iterator<Item = Event>`.

**T2. `last_event: Option<String>` formatted as `"type/ptype"`.** It is
produced by two independent `format!("{}/{}", ...)` sites
(`transcript::parse`, `watchdog::last_event`), and travels as
`Digest.last_rollout_event`, `Verdict.last_event`,
`Incident.last_rollout_event` and `TranscriptSummary.last_event`, all as
strings. With T1 it is `EventKind` with a `Display`.

**T3. `last_in_flight_tool: Option<(String, String)>`.** It is a bare
`(name, arguments)` tuple, and name and args are the same type, so they can
be swapped. In the parser it is nested again as `(call_id, (name, args))`.
Use `struct InFlightTool { call_id: Option<CallId>, name, args }`. The
200-char truncation for display is done twice, differently: `truncate` in
`provider.rs` adds `...`, while `run_session_show` uses a bare
`chars().take(200)`.

**T4. `terminated_by_review: Option<String>`.** It is a typed decision
(`watchdog::Kind::Stranded`, `Kind::Stalled { quiet_secs }`, signal stop,
operator interrupt) flattened to prose in `run_codex_json` and persisted as
prose. Readers then branch on presence alone: `print_digest`,
`digest_summary_note`, and the tests (which match on substrings of it). Kind
is lost by the time it reaches the sidecar, so "how many Stranded kills this
week" is a substring grep. Use an enum `Terminator { Watchdog(Kind),
Signal, Interrupt }` serialised as a tag, with the prose generated by
`Display` in one place. Right now `Verdict.reason` (the watchdog's own long
prose) and the short `terminated_by_review` string are two phrasings of one
verdict, built in two modules.

**T5. Rollout offsets: `Option<u64>` used three ways.** `rollout_baseline:
Option<u64>` means "None = untrustworthy". `slice_from_offset(.., None)`
means "whole file". `recovery_scope = Some(baseline.unwrap_or(u64::MAX))`
turns "untrustworthy" into "past EOF", because `None` would have meant
"everything". The same `Option<u64>` therefore means opposite things at two
adjacent call sites, and the comment explaining `u64::MAX` is what holds
them apart. Make it `enum Scope { WholeFile, From(RolloutOffset),
Untrusted }`, with `slice` total over it. The watchdog's
`None => pending()` and recovery's `u64::MAX` then become the same match
arm.

**T6. `TranscriptSummary.path: String`, `Incident.transcript_path:
Option<&str>`, `incident_path: Option<String>`.** These are paths stored as
strings for no reason. A non-UTF-8 home is lossily mangled, and the
transcript path is then re-read from the mangled string in
`write_bundle`.

**T7. `Incident` is 28 loose fields of `&str` / `Option<&str>` / bools /
integers.** Several are sibling primitives of one type that can be swapped
silently: `exit_code: Option<i32>` vs `turns: u32`, and `signal`,
`stdin_write_error`, `turn_error` and `last_rollout_event` are all
`Option<&str>`. `captured` and `recovered_from_transcript` are two bools
encoding a three-state "where did the answer come from" (`-o` file,
transcript, none; the fourth combination is impossible). Use an
`AnswerSource` enum. `exit_code` + `signal` are a split `ExitStatus`: carry
the status, or an `enum Exit { Code(i32), Signal(Sig), Unknown }`, and
`signal_name`'s string formatting becomes `Display`.

**T8. `final_answer_present: Option<bool>`** in the bundle is another
tri-state that reaches `run_incidents` and gets branched on as `Some(false)` /
`Some(true)` / `None`.

**T9. Session IDs are `&str` / `String` everywhere here.** For codex it is a
UUID, matched by filename suffix. opencode requires a `ses` prefix, and maki
picks its own up front. A per-provider `SessionId` newtype minted by the
provider (validated once) would let `find_transcript_path` and `inflight`
stop re-validating, or forgetting to.

**T10. `watchdog::Timings` vs `crate::timings`.** Same name, different
things. `stall_grace: Option<Duration>` is converted from config's `secs ==
0 means off` in `provider.rs`. That conversion belongs to the config type
(`StallTimeout::{Off, After(Duration)}`) so `0` never travels as a
sentinel.

**T11. Failures as prose.** `write_bundle` returns `Option<PathBuf>` and
`eprintln!`s each failure, `find_transcript_path` / `summarize_session`
return `Option`, and `list_recent` returns `Vec` with errors swallowed. A
caller cannot tell "no rollout exists" from "unreadable" from "codex home
unknown", and B2 and B5 are both that confusion. A small
`enum Lookup { Found(PathBuf), Absent, Unreadable(io::Error) }` would let the
baseline code say which one happened instead of the generic warning.

---

## 2. Decisions made in more than one place

**D1. "Did this run produce an answer?"** This is the central question, and
it has at least seven answerers:
1. `run_codex_json`: `captured = -o file non-empty`.
2. `transcript::parse`: `final_answer` of the *last* turn (phase-tagged).
3. `watchdog::has_final_answer`: any phase-tagged answer since the baseline.
4. `incident::scan_transcript`: last `task_complete.last_agent_message` is
   non-null.
5. The stream parser in `run_codex_json`: last `item.completed` agent_message
   (used as fallback text, unphased).
6. `app::died_without_answer` / `got_final_answer`: `captured ||
   recovered_from_transcript`, defined as two separate functions that are
   negations of each other, maintained by hand.
7. `run_session_show`: `task_complete && final_answer.is_none()` means "likely
   died mid-work".

They already disagree. B1 is 2 vs 3, B4 is 4 vs 2, and 5 can be commentary
that the user then sees as the answer under a death note. The owner should be
a single codex `Outcome` computed once at the end of the run, from one
rollout fold (T1) plus the `-o` file. `got_final_answer` becomes a method on
it, and `died_without_answer` is its negation, not a sibling.

**D2. "How did the run end?", rendered as prose, five times.**
- `run_codex_json`'s fallback text ("codex ended the turn", "interrupted by
  the operator", "codex produced no final message", "codex died without a
  final answer");
- `print_digest`'s note ladder;
- `digest_summary_note`'s ladder;
- `run_incidents`' `verdict` ladder;
- `run_session_show`'s "rollout has no final answer" note.

Each ladder checks `turn_error`, `terminated_by_review`, `interrupted`,
`recovered_from_transcript`, `captured`, `exit_code` and `signal` in its own
order. They already differ: `print_digest` checks `turn_error` before
`terminated_by_review`, while `digest_summary_note` checks `interrupted`
before `turn_error` and ignores `terminated_by_review` unless interrupted. A
code comment ("The live `print_digest` has the same gate") is the only thing
keeping two of them aligned. The owner should be an
`enum RunEnd { Answered(AnswerSource), StatedFailure(msg),
Interrupted(Terminator), KilledByReview(Terminator), Died, NoMessage }`.
Compute it once, persist it in `DigestSummary` (so `review incidents` and
`review sessions` read the verdict instead of re-inferring it from bools),
and let each view only render it. This is also where maki and opencode plug
in: both state their outcome in the stream, so they would construct a
`RunEnd` directly rather than fill in codex-shaped bools for five ladders to
re-interpret.

**D3. "Is this run suspicious?"** It is decided once
(`!captured || exit != 0 || signal`) and then gates two things (transcript
read, bundle). That part is fine. But "is this an incident?" is
`suspicious && !interrupted` inline, and `run_incidents` re-decides
"explained vs mystery" from `turn_error`. With `RunEnd`,
`is_incident()` is one method.

**D4. "Which bytes belong to this run?"** `slice_from_offset` is shared, and
that part is correct. The decision about what to do when the baseline is
unknown, though, is made twice with different mechanisms: the watchdog
pends forever, and recovery uses `u64::MAX`. `scan_transcript` and the
transcript tail skip scoping entirely. Owner: `Scope` (T5), passed to every
rollout reader, with no reader accepting raw bytes.

**D5. "Where is codex's home?"** `env.and_then(|e| e.get("CODEX_HOME"))` is
evaluated three times in `run_codex_json` (baseline, watchdog, recovery).
Then `transcript::codex_home` applies the parent env and `HOME` fallback.
`run_session_show` passes `None`. Owner: resolve the rollout *path* once per
run and carry it (B3, B5).

**D6. "What is a secret env var?"** `incident::looks_secret` redacts values
by name pattern. The sidecar and audit sidestep the question by never storing
values. That makes two policies, but compatible ones. If the incident layer
becomes generic (S3), the redaction rule should live with the env
recording, not in incident.

**D7. "What is the tool in flight?"** There are two definitions of a call
event: `ends_with("_call")` / `ends_with("_call_output")` in `parse`, and
nothing in the watchdog. The watchdog's `last_event` reports `custom_tool_call`
etc. but cannot say whether it is still open. With T1 they share
`ToolCall`/`ToolOutput`.

**D8. Rollout format fixtures.** The same JSON shapes (`FINAL_ANSWER`,
`TASK_STARTED`, the 0.157.0 assistant message) are hand-typed separately in
`transcript.rs` tests, `watchdog.rs` tests, `incident.rs` tests and
`tests/provider_runs.rs`. They agree only by copy. A `codex::rollout::fixtures`
module (test-only) with builders per codex version would turn "codex changed
its shape" into one fixture change that every reader's tests pick up.

---

## 3. Structure

**S1. These modules belong under `src/codex/`.** `transcript.rs` and
`watchdog.rs` are 100% codex: rollout file layout, codex event vocabulary,
codex's empirical wake cadence. `incident.rs` is about 60% generic and 40%
codex. They sit at the crate root beside provider-agnostic modules, and
`provider.rs` (3600+ lines, holding all three runners) is their only real
caller. A layout that follows the seams:

```
src/codex/{mod.rs (runner), stream.rs (--json NDJSON parse), rollout.rs
           (Event, Scope, path lookup), transcript.rs (fold -> summary),
           watchdog.rs, outcome.rs}
src/claude/..., src/grok/..., src/maki/..., src/opencode/...
src/forensics/{incident.rs (generic bundle), replay.rs}
src/outcome.rs  (RunEnd, AnswerSource, Terminator - provider-agnostic)
```

Today the codex NDJSON stream parser lives inline in a 600-line
`run_codex_json`, while the rollout parser for the same provider lives in a
separate top-level module. The split follows "which file on disk" rather than
"which provider".

**S2. `Digest` is codex's struct dressed as everyone's.** `transcript`,
`last_rollout_event`, `quiet_secs`, `recovered_from_transcript`,
`log_lines` ("codex log lines" in `print_digest`) and `task_complete` /
`stream_error` in `DigestSummary` are codex concepts. claude and grok fill
them with `None` (`no_answer_digest`, `wake_failure_digest`). maki and
opencode would do the same. The right split is a common core (`RunEnd`, usage,
exit, terminator, incident path, cost) plus
`enum ProviderForensics { Codex(CodexForensics), Claude(..), Maki(..),
Opencode(..) }`, serialised tagged. `print_digest` then dispatches the
provider-specific block instead of special-casing codex fields for all
providers.

**S3. Split `incident` into a generic bundle and a provider extension.**
What is generic: stderr, stdout tail, prompt, argv, cwd, env, exit, duration,
the replay command and the directory naming. What is codex: `codex_version`
(rename to `provider_version`), `transcript_path`, `final_answer_present`,
`rate_limits`, `quiet_secs`, `last_rollout_event` and `turns`.

The generic part should be built from the *actual* `Command` that
`contained::spawn` launched, not from parallel fields the runner passes in by
hand. `tokio::process::Command::as_std()` exposes `get_program`, `get_args`,
`get_envs` and `get_current_dir`. That makes the replay the spawn, by
construction:
- opencode's mandatory isolation env and `PWD` appear in the replay
  automatically;
- the hand-maintained `RUST_BACKTRACE=1` string goes;
- the comment explaining why `binary` is not `"codex"` goes.

Each provider then contributes a
`serde_json::Value` (or typed) extension block. This is also what gives maki
and opencode incident bundles at all. Today claude and grok get none, and the
"is_error but exit 0" class in maki is exactly the kind of failure a bundle
exists for.

**S4. Generalise the watchdog into "progress source + answer evidence",
keeping the thresholds provider-owned.** The loop shape (poll a monotone
progress measure, two verdicts keyed on "answer evidenced?" plus quiet time)
is provider-agnostic. Only the progress source (rollout length) and the
answer predicate (phase-tagged message since the baseline) are codex's.
opencode streams every event on stdout, so its stdout byte count *is* a
progress source, and its stream says when the last assistant `text` has
arrived. Whether opencode has codex's "cannot stay silent" property is
unknown, and the module's own doc rightly says claude does not. So
`stall_grace` must default to off for any provider not measured, which is a
per-provider policy decision with one home. Keep the codex-specific prose
(the 0.147.0 history) in the codex reference. The generic loop should not
carry it.

**S5. The watchdog depends on `transcript` for half its job and deliberately
avoids it for the other half.** It uses `final_answer_text`,
`slice_from_offset` and `find_transcript_path`, and avoids `parse` with a
paragraph explaining why. That paragraph is the symptom: the two modules are
two folds over one event stream that never got its own module. With
`rollout.rs` holding `Event`, `Scope` and the path lookup, `transcript` and
`watchdog` become siblings that depend on it, instead of one reaching into
the other's helpers. `scan_from_baseline` is a one-line wrapper that exists
only to restate this dependency.

**S6. Persisted forensic state is split from the behaviour that reads it.**
`IncidentSummary` (incident.rs) is a hand-maintained subset of `Meta`. It
mirrors field names by convention, with `#[serde(default)]` everywhere, and
nothing ties the two together. Rename a `Meta` field and `review incidents`
silently shows defaults. Make `Meta` itself `Deserialize` (with defaults),
or derive the summary from a shared struct. The verdict ladder for listing
lives in `app.rs`, away from the module that owns the schema. It should be a
method on the persisted type, ideally just reading a persisted `RunEnd`
(D2).

**S7. `schema: 1` is written and never read.** Either check it on read or
drop it. With S2/S3 the bundle schema changes, so this is the moment to make
it mean something.

**S8. Test layout.** Unit tests for each reader are right next to it, but the
behaviour that matters is cross-module: watchdog kill, then recovery, then
`after_run`. That chain is only covered via the stub harness in
`tests/provider_runs.rs`, and B1 lives exactly in that gap. With a single
fold (T1) and a single `RunEnd` (D2), the chain becomes a pure function from
`(rollout bytes, Scope, -o contents, exit, Terminator)` to `RunEnd`. It can
then be table-tested without processes, and the stub tests shrink to
plumbing.

---

## What adding maki and opencode would touch in this scope, today

- They get no forensics at all unless `incident` is generalised (S3), because
  `write_bundle` is called only from `run_codex_json` and its schema is
  codex-named.
- Outcome rendering: each of the five ladders (D2) would need maki/opencode
  cases, or would misread their bools. For example, maki's `is_error: true`
  with exit 0 maps onto `captured`/`turn_error`/`exit_code`, and each ladder
  orders those differently.
- `Digest` would gain more provider-specific `Option`s (S2). For opencode,
  where the answer is rebuilt from `text` events, "recovered" vs "captured"
  is meaningless, and it would have to pick one of two bools that encode
  codex's `-o`-vs-rollout distinction.
- The replay command would be wrong for opencode (B6, S3).
- The watchdog would need a decision on whether it applies at all (S4). Today
  that decision is implicit: it runs only because only `run_codex_json`
  constructs one.
