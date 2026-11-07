# Hunt: providers (`src/provider.rs`, `src/provider_tests.rs`, `tests/provider_runs.rs`)

Read from code only. Cross-scope sites were followed where a provider decision
lands: `app.rs`, `config.rs`, `config_cmd.rs`, `timings.rs`, `interrupt_cmd.rs`.

## Summary verdict

`provider.rs` is a 3600-line file that is really five modules: process I/O
plumbing, the signal supervisor, the permission front end (`invoke`), three
runners, and output rendering. The runners do not share a skeleton. Each one
re-implements the same lifecycle (mint or learn a session id, clear stale
interrupt requests, check stop, mark in flight, spawn under a keeper, signal
launch, wait with a stop arm, consume the interrupt request, drop the marker,
drain the pipes, classify, build a codex-shaped `Digest`), and each one orders
these steps slightly differently. The run's outcome reaches callers as
`Result<String>` plus `Option<Digest>` plus four booleans/strings inside the
digest. Callers in `app.rs` work the outcome back out with predicates that read
those fields differently. The provider identity is a `&str`, matched in at
least nine places across five files.

Adding maki would mean copying `run_claude` and editing it, because maki's wire
format is claude's but its flags are not. Adding opencode would mean writing a
fourth lifecycle by hand. My recommendation is a rewrite around three things:

1. a `Provider` enum with one capability table,
2. a typed `Outcome` in place of `Option<Digest>` plus flags,
3. a single lifecycle driver that a per-provider `Adapter` plugs into (argv,
   env, stream decoder, classifier).

Details follow.

---

## 1. Axes that should be types

### 1.1 Provider identity is `&str` everywhere

`invoke(provider: &str, ...)` dispatches on `"claude" | "grok" | "codex"`, with an
`other => Err("unknown provider")` fallback. That fallback can only be reached
because the type allows any string. The same string is matched in:

- `provider::Unrecorded::for_provider` (`"codex" | "claude"`)
- `provider::effective_sandbox` (`"codex" | "grok"`)
- `provider::invoke` (`provider == "grok"`, `provider == "codex"` three times)
- `interrupt_cmd::interruptible` (`"codex" | "claude"`)
- `timings::stale_session` (`"codex"`)
- `config::sandbox_for` (`"grok"`)
- `config_cmd::sandbox_text` (`"claude"`)
- `app::after_run` (`"codex"`, twice)
- `app::run_session_show` (`latest.provider == "codex"`, twice)
- `config::KNOWN_PROVIDERS`

A sidecar row carrying `provider: String` is validated against
`KNOWN_PROVIDERS` at resume time and then passed on as a string again. The fix
is `enum Provider { Claude, Codex, Grok, Maki, Opencode }` with `FromStr`,
parsed once at the config and sidecar boundaries. The match in `invoke` then
becomes exhaustive, and adding a variant becomes a compile error at every
decision site listed in section 2. Today, adding maki compiles cleanly while
every `_ =>` arm quietly gives it whatever the wildcard means (codex's 27-minute
cutoff does not apply, for example, and `sandbox_text` claims it has a
read-only runner default).

### 1.2 Sandbox: two vocabularies in one `Option<&str>`

`invoke` receives `sandbox: Option<&str>` in `review`'s vocabulary on a fresh
run. On a resume it receives the *recorded* value, which is already in the
provider's native vocabulary: grok's `workspace`, or a `review-ws-<hash>`
profile name. It then runs `sandbox_for` on that value again. This only works
because `sandbox_for` passes unknown names through and happens to be idempotent
on its own outputs. `grok_write_launch` then recovers a profile kind by
string-prefix test (`starts_with(PROFILE_PREFIX)`). `derives` compares against
the native literal `"workspace-write"`. `effective_sandbox` writes the native
name back into the record.

Proposed types:

- `enum Level { ReadOnly, WorkspaceWrite, FullAccess, Custom(String) }` for the
  config surface.
- A per-provider `NativeSandbox`. For grok that is
  `enum { Builtin(GrokBuiltin), Profile(ProfileName) }`. For claude, maki and
  opencode it is "none".
- A recorded form that says which vocabulary it is in.

`GrokWrite` already exists as the typed shape, but it is reconstructed from a
string on every call.

opencode makes this pressing. Its read-only enforcement is an agent definition,
not a sandbox, so "how is the level enforced" becomes a third axis next to
"which level" and "native spelling". Today that would be a fourth `_ =>` arm
choice in `effective_sandbox`.

### 1.3 Fresh vs resume: `oneshot: bool` plus `session_id: &str`

`invoke` and every runner take both. `app::Launch::invoke` derives
`oneshot = session.is_empty()`. The impossible combinations (oneshot with an id,
resume with `""`) are representable, and every runner branches on both
separately. Use `enum Session { Fresh, Resume(SessionId) }`.

The session id deserves its own type too. `inflight` validates it as a filename
component. opencode requires a `ses` prefix, and claude, grok and maki take a
caller-minted UUID. Whether the caller or the provider mints the id is a
per-provider fact that is currently encoded by *which runner calls
`generate_uuid`*:

| Provider | Who mints the id today |
|---|---|
| claude | the runner calls `generate_uuid` |
| grok | the runner calls `generate_uuid` |
| codex | learned from the stream |
| maki | would be the caller (copy claude) |
| opencode | the caller, with a `ses_` prefix |

Make it a capability: `IdPolicy::CallerMinted(fn() -> SessionId)` or
`IdPolicy::StreamReported`.

### 1.4 The outcome travels as `Result<String>` plus `Option<Digest>` plus flags

This is the biggest finding. Whether a run answered, failed for a stated reason,
died, or was interrupted is spread across:

- `output: Result<String>`, where `Err` means it never reached a turn;
- `digest: Option<Digest>`, whose `None` means "clean answer" for claude and
  grok but cannot happen for codex;
- `captured`, `recovered_from_transcript`, `turn_error: Option<String>`,
  `interrupted`, and `terminated_by_review: Option<String>`.

Consequences:

- `app::got_final_answer` returns `false` when `digest` is `None`. That is a
  clean claude or grok answer, so the predicate is wrong for two of three
  providers. It is only harmless because only codex reaches
  `after_auto_resume`.
- `app::died_without_answer` returns `false` on `None`. The two predicates are
  not complements, and nothing says so.
- `turn_error` is overloaded. It carries the provider's stated reason (codex
  `turn.failed`, grok `stopReason`, claude `is_error`). It also carries
  `review`'s own remarks: "claude exited non-zero after producing an answer",
  "grok's exit status is unknown", "could not verify grok's background
  results", "wake-up turn ... failed", "still had unread background results".
  `app::stated_failure` reads all of these as "the provider stated a reason, do
  not retry". They print as `turn failed: ...`, and `review incidents` lists
  them as `turn failed:`. A verification failure in `review` is not the
  provider refusing.
- `wake_failure_digest` asserts `exit_code: Some(1)`, an exit nothing produced.
  The comment says so, but the field cannot tell an asserted exit code from an
  observed one.

Proposed shape:

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

Codex forensics (transcript, log lines, incident, quiet_secs) become an
optional `CodexForensics` attachment, not fields every provider zero-fills
through `no_answer_digest`. `DigestSummary` is the serialized projection of
`Outcome` plus `Usage`. `after_run`, `exit_status`, `print_digest`,
`digest_summary_note` and the `review incidents` verdict all become `match`es
on one enum.

### 1.5 Interrupt and termination reasons are prose

`terminated_by_review: Option<String>` holds one of these literals:

- `"review was signalled to stop"`
- `"operator interrupt (`review interrupt`)"`
- `"watchdog: stranded completion"`
- `format!("watchdog: stall timeout ({}s silent)")`

They are written in claude's runner and codex's runner separately, and in
`app::digest_summary_note` as a fallback default. Callers branch on
`interrupted: bool` beside them. Use an enum
`Termination { Signal, OperatorInterrupt, Watchdog(Kind, quiet_secs) }` with a
`Display`. The persisted form can stay a string, written from that enum.

### 1.6 Smaller ones

- **Usage.** `Usage` has four `u64` fields with provider-specific mappings
  (`claude_usage` and `grok_usage` each rebuild codex's "input includes cached"
  convention). The grok wake loop adds the fields up by hand, and `cost_usd` is
  summed with a hand-written `Option` merge. Implement `AddAssign` for `Usage`
  and `Served`. Name the convention in the type (`total_input`,
  `cached_input`), so a new provider's mapper cannot double-count claude-style
  cache-creation tokens.
- **`Served::model`.** It is a comma-joined `String`. Use `Vec<String>`, and
  join only at serialization.
- **Temp-file paths.** `new_output_file` and `new_temp_file` return `String`
  paths, and `RemoveOnDrop(String)` is created separately from the file it
  removes. Return a `TempFile` guard that owns both. Today `run_codex` creates
  the file, but `run_codex_json` installs the cleanup and also removes the file
  by hand on the stop path.
- **`ProviderRuntime`.** It has `codex_binary: String` and
  `claude_command: Vec<String>`, and grok has neither: it is hardcoded as
  `Command::new("grok")`. That is why there is no runner-level grok test. Use a
  map, or `fn command(Provider) -> Vec<String>`. maki and opencode will each
  need a stub hook.
- **Signal name.** `signal_name` maps 2, 9 and 15, and everything else becomes
  `"signal N"`. Fine, but `ExitStatus` handling is duplicated in
  `Digest.exit_code` plus `signal`. Keep an `Option<ExitStatus>` (or a small
  `Exit { Code(i32), Signal(i32), Unknown }`) and derive both.

---

## 2. Decisions made in more than one place

### 2.1 "Can this provider's turn be ended cleanly with SIGINT?"

This is answered in:

- `provider::Unrecorded::for_provider` (`"codex" | "claude"`)
- `interrupt_cmd::interruptible` (`"codex" | "claude"`)
- implicitly by which runners have a stop arm and set `record_child_pid`
  (claude and codex do; grok's `run_grok_turn` has neither)

They agree today, and nothing ties them together. maki and opencode both answer
"yes", so all three sites need editing. This belongs in one capability:
`Provider::interrupt() -> Option<InterruptStyle>`.

### 2.2 "Does this provider have a filesystem sandbox, and what is its default?"

This is answered in:

- `provider::effective_sandbox` (codex and grok get read-only; the rest get
  `None`)
- `config_cmd::sandbox_text` (`"claude"` means ignored; everything else shows
  "runner default: read-only")
- `config::sandbox_for` (only grok translates)
- the claude-specific comment and the absence of a `sandbox` argument in the
  `invoke` dispatch
- `run_grok_turn`'s own `(None, true) => read-only` default
- `run_codex`'s `sandbox.unwrap_or("read-only")`

They already disagree for any provider that is not one of the three:
`sandbox_text` would print "runner default: read-only" for maki, while
`effective_sandbox` would record `None`. The default "read-only" is stated three
times, in `effective_sandbox`, `run_grok_turn` and `run_codex`. Put the sandbox
model, the default and the translation in one owner per provider, as part of
the capability table.

### 2.3 "Did this run produce an answer?"

This is answered in:

- `app::died_without_answer` (`!captured && !recovered`, with `None` read as
  false)
- `app::got_final_answer` (`captured || recovered`, with `None` read as false,
  which is inconsistent: see 1.4)
- codex's `interrupted = interrupt_requested && !captured && !recovered`
- the grok wake loop's `turn_error.is_some() && !captured`
- `app::digest_summary_note`'s `turn_error.is_some() && !d.captured`
- `print_digest`'s `!d.captured` branch
- the fan-out `all_failed` and resume `exit_status` inputs

The digest-note comment says outright "The live `print_digest` has the same
gate". That is two copies kept in step by hand. With `Outcome` this collapses
into `Outcome::has_answer()`.

### 2.4 "Is this a stated failure that must not be retried?"

This is answered in `app::stated_failure` (any `turn_error`) and in the grok
wake loop's own check, written against the same fields with the same meaning.
Because of the overloading in 1.4, both treat `review`'s internal verification
failures as provider statements.

### 2.5 "How does a run end that was interrupted, and what does it say?"

claude's `interrupted_digest`/`interrupted_text` and codex's `interrupted`
block each:

- write the "(interrupted by the operator before a final answer)" text,
- write the two `terminated_by_review` strings,
- apply "an answer wins over an interrupt request".

claude applies the last rule by matching the `Answered` arm before checking the
request. codex applies it with `!captured && !recovered`. Same decision, two
implementations, and opencode and maki would add a third and a fourth.

### 2.6 "Answered, but the process exited non-zero or with unknown status"

claude's `Answered` arm and grok's `Answered` arm are near-verbatim copies:
`captured: true` plus `no_answer_digest(...)` with an "exited non-zero after
producing an answer" or "exit status is unknown" message. Codex decides the same
case differently: the digest records `exit_code`, `captured=true`, and
`suspicious=true`, which writes an incident bundle. Three providers give two
different answers to one question.

### 2.7 "When does the in-flight marker go up, and when is the pid offered?"

| Provider | Marker goes up | Pid offered |
|---|---|---|
| claude | always before spawn | after the `init` event |
| codex | before spawn on resume; in a task once `thread.started` arrives on a fresh run | after the first stdout byte |
| grok | before spawn on resume; *after the whole first turn* on a fresh run | never |

The real rule is "before spawn when the id is known, otherwise when it is
learned; offer the pid once the provider has shown its handler is installed".
That should be one lifecycle function parameterised by `IdPolicy` and a
`ready: watch<bool>` signal from the stream decoder. A fresh grok run is
invisible to `review sessions` for its entire first turn, which may be the
longest one.

### 2.8 "Clear stale interrupt requests before launch; consume after"

`take_interrupt_request` is called before launch and after the wait in
`run_claude` and `run_codex_json` separately. claude keys the post-run call on
`marker_sid`, and codex on `watched_session_id`. grok does neither. A grok
request cannot be made, so this is harmless today, but it is the kind of step a
new runner will forget.

### 2.9 "Was the stop signal raised before launch?"

`*runtime.stop.borrow()` is checked before spawn in `run_claude` and
`run_codex_json`. `run_grok_turn` does not check it, so a grok run still waiting
on its stagger when `review` is signalled will try to launch, and the wake loop
will try to launch wake turns. I did not verify whether `contained::spawn`
refuses after `terminate_all`. If it does not, this is a bug: a grok turn
launched after the operator asked to stop. Even if it does, the error then reads
as a launch failure rather than "not launched: review was signalled to stop".

### 2.10 Stale-session cutoff and resume hint

`timings::stale_session` decides by provider string, and `provider::resume_hint`
reads it. `cli.rs` tests restate the labels `("codex", "(codex)")` and
`("claude", "(claude, grok)")`. That is a pairwise test holding the copies
together. maki and opencode would silently get the 55-minute cutoff through the
wildcard. That may be right or wrong (their providers' caches differ), but
nobody would have decided it. Make the cutoff a capability field.

### 2.11 Digest rendering, done twice

`provider::print_digest` (live `Digest`) and `app::print_digest_summary` plus
`digest_summary_note` (persisted `DigestSummary`) render the same verdict in two
places with different branch orders. The live one has no "interrupted" note and
relies on `terminated_by_review`. The persisted one checks `interrupted` first.
`app::run_session_show` also re-renders the transcript block that `print_digest`
renders. Use one renderer over `DigestSummary`, or over `Outcome` and its
projection, and call it from both places.

### 2.12 Which writable roots get recorded

`invoke` decides this in three layers:

1. derived roots,
2. grok-canonicalised roots,
3. the codex `config` override parsed back out by
   `config_writable_roots_override`.

Codex itself decides from argv order. Two parsers of one value: see 4.4 for how
they can disagree.

---

## 3. Structure

### 3.1 Split `provider.rs`

Proposed layout:

- `provider/mod.rs`: the `Provider` enum, the capability table, and `invoke` as
  a thin driver.
- `provider/lifecycle.rs`: one async driver that takes an `Adapter`. It owns
  stop-before-launch, the marker policy, keeper spawn, the launch signal, the
  stop or interrupt arm, interrupt-request consumption, the drain, and the
  `Outcome` assembly from the classifier verdict plus interrupt facts. This is
  where 2.5 to 2.9 get one home.
- `provider/io.rs`: `SharedBuf`, `read_capped`, `collect_reader`,
  `write_stdin`, `TempFile`. The stream readers today are three hand-written
  loops: `read_capped`, `read_claude_stream` with head/tail plus init scan, and
  `read_capped_stdout` with id scan plus first-byte watch. Generalise them into
  one line-splitting reader with a pluggable `LineObserver` (publishes id,
  flips ready) and a buffer policy (head-capped, or head plus tail). claude's
  tail-keeping and codex's head-only capture are policy choices.
- `provider/{claude,codex,grok,maki,opencode}.rs`: each provides `argv(&Launch)`,
  `env(&Launch)`, its `LineObserver`, and
  `classify(stdout, stderr, status) -> Verdict`. The JSON event formats are
  local to these files.
- `provider/render.rs`: `print_result`, `print_digest`, `resume_hint`, merged
  with app's summary rendering.
- Signal supervisor, `STOP`, `Unrecorded`: these are process-lifecycle and do
  not belong in `provider`. Move them next to `contained` (or into a `stop.rs`).
  Their dependency on "which providers can be gracefully stopped" then becomes a
  capability lookup.
- `invoke`'s grok write preparation (`GrokWrite`, `prepare_grok_write`, the
  trust re-read) and codex `config_writable_roots_override`: these are
  provider-specific permission setup sitting in the generic front end. Move
  them into the grok and codex adapters behind a `prepare(&Launch) ->
  Prepared` hook. Then `invoke` stops branching on provider strings entirely.

### 3.2 claude and maki share a wire format, not an argv

maki reproduces claude's `stream-json`: an `init` event and a final `result`
with `is_error`. So `last_claude_result`, `ClaudeResult`,
`interpret_claude_output`, `claude_usage`, `result_served`, `is_claude_init` and
`read_claude_stream` should be a `claude_stream` decoder module shared by both
adapters. The argv builder must not be shared. maki silently ignores
`--effort`, `--permission-mode` and others. Copying `run_claude` would pass
`--permission-mode dontAsk` and `--effort` and record an `effort` the run never
got: `ProviderResult::effort` is filled from the request, not from what was
applied. Make the recorded model and effort come from the adapter ("applied" vs
"requested, unsupported"). Otherwise maki rows will claim an effort the run
never used, the same false-record problem the `writable_roots` override comment
warns about.

`result_served` is already shared between grok and claude by field-name
coincidence (`modelUsage`, `total_cost_usd`). maki does not report the served
model, so it will return empty, which is correct. But that correctness comes
from luck rather than design.

### 3.3 Grok's wake loop is a separate concept

`run_grok` (the wake loop) wraps `run_grok_turn` and merges usage, cost, turns
and leftovers by hand. A general "multi-turn run" wrapper with `Outcome`
merging would let a future provider that also backgrounds work reuse it.
opencode's `tool_use`/`step_finish` events may need the same "did the model see
its results" verification. At minimum, the merge logic belongs on the types
(`RunOutput::absorb(next)`).

### 3.4 Test layout mirrors the module accident

- `provider_tests.rs` tests the pure classifiers for claude and grok, codex
  sandbox argv, the writable-roots override parse and the announcement lines,
  all against private functions.
- `tests/provider_runs.rs` drives `run_codex_json` and `run_claude` directly.
  It bypasses `run_codex`, so codex's *argv* (`--ignore-rules`,
  `--ignore-user-config`, `approval_policy`, network grants, the `resume`
  placement of `-o`) is never exercised end to end. The tests also bypass
  `invoke`, so root derivation, the sandbox translation and the recorded fields
  are untested at the seam where they are combined.
- grok has no runner-level test at all, because its binary is hardcoded.

With adapters, each provider gets three tests:

1. a table test of `argv(Launch) -> Vec<String>`, which pins every
   safety-critical flag,
2. a fixture test of `classify`,
3. one shared lifecycle test suite run against a stub per provider, covering
   interrupt, stop, the marker and death.

maki and opencode then get the lifecycle tests for free.

### 3.5 `pub` surface

`run_codex_json`, `run_claude`, `RunOutput` and `new_output_file` are `pub` only
for `tests/provider_runs.rs`. The lifecycle-plus-adapter split gives the tests a
better seam, `lifecycle::run(adapter, stub_command)`, and the per-runner `pub`s
can go.

---

## 4. Bugs and smells noticed

### 4.1 Interrupted claude or grok resume prints no resume hint

`print_result` prints the "interrupted - continue within Nm" hint only when
`result.session_id` is `Some`. claude's and grok's runners return
`session_id: None` on a resume (`keep_id` returns `None` when not oneshot).
codex's returns the known id. So `review message <claude-id>`, if itself
interrupted, prints no `session:` line and no resume hint, while the same flow
on codex prints both. The `RunOutput.session_id` contract ("new information
only" vs "the session") differs between runners.

### 4.2 Non-provider failures reported as "turn failed"

`wake_failure_digest` and the post-answer status digests put `review`'s own
diagnosis into `turn_error` (see 1.4). Operators and `review incidents` see
`turn failed: could not verify grok's background results` as though grok
refused. Separately, `wake_failure_digest` turns a run that *had* an answer into
`captured: false`, so the run exits 1 while its answer text is printed beneath
"note: no conclusion was produced; the text below is not an answer". That is
false: it is an answer, just one whose background results were not verified.

### 4.3 grok ignores the stop channel

See 2.9. It does no pre-launch stop check, and the wake loop also ignores
`stop`, so a signalled `review` can launch new wake turns unless
`contained::spawn` refuses them (I did not verify this).

### 4.4 `config_writable_roots_override` can disagree with codex

- It picks the last entry with `starts_with("sandbox_workspace_write.writable_roots")`.
  A longer key with that prefix is picked, fails the nested lookup, and returns
  `None`, so an earlier real override is ignored.
- A value codex accepts but the TOML parser rejects also returns `None`. The
  record then shows the derived roots while codex ran with the override.

That is the exact false-record class the surrounding comment says it prevents.
The robust fix is to stop parsing it back: keep the override as a typed profile
field (`writable_roots` already exists) and forbid restating it in `config`.

### 4.5 The resume path drops profile `env` and `config`

The resume path passes `env: None, config: &[]`. For codex, that also means the
rollout baseline and transcript lookup use the default `CODEX_HOME`. A session
created under a profile with a custom `CODEX_HOME` resumes against the wrong
home: the baseline is unreadable, and the watchdog and recovery are disabled
with a warning, or worse. This is a documented choice (env values are not
recorded), but it is a silent cross-axis coupling: the env decides where
forensics look.

### 4.6 `claude_usage` reads only the last result line

With multiple result objects in one stream (the background-subagent case the
comments describe), earlier turns' usage is lost. This is moot while background
tasks are disabled, but it is a trap for maki, whose stream may differ.

### 4.7 `Digest::turns` means different things per provider

For codex it counts `turn.completed` events. For claude it is `num_turns` (API
round trips), and grok's `numTurns` is the same. The column is not comparable
across providers.

### 4.8 `#[allow(clippy::too_many_arguments)]` on every runner

Each waiver comment argues the arguments are independent axes. They are mostly
one value, a resolved launch (model, effort, sandbox, env, config, prompt, root,
session), passed one field at a time. The `Launch` struct already exists in
`app.rs`. Move it into `provider` and pass it whole.

### 4.9 A doc comment sits above the wrong function

The doc for `config_writable_roots_override` ("The writable roots a profile
`config` entry sets...") sits directly above `announcement_lines`, whose own doc
follows. The override function itself has no doc.

### 4.10 A comment that is no longer true

`run_claude`'s `runtime` parameter comment says "Named for codex ... claude uses
only the stop channel, the data root and the two grace periods". It also uses
`claude_command`, `teardown` and `orphan_log`.

---

## 5. What the new providers would touch today

**maki**: copy `run_claude`, remove `--permission-mode` and `--effort`, keep the
stream parsing. Add `"maki"` to these sites:

- `KNOWN_PROVIDERS`
- the `invoke` match
- `Unrecorded::for_provider`
- `interrupt_cmd::interruptible`
- `config_cmd::sandbox_text` (else it says "runner default: read-only")
- `effective_sandbox` (wildcard gives `None`, which is accidentally right)
- `stale_session` (wildcard, undecided)
- `ProviderRuntime` (a new binary field)

Effort-via-config means writing maki's config per run: another
`prepare_grok_write`-style setup inside `invoke`.

**opencode**: a new runner for the JSONL events, plus:

- a `ses_` id minter,
- a final-answer rebuild from `text` events,
- exit 130 on SIGINT treated as an interrupt, not a death,
- `--standalone` mandatory,
- an isolation env (private DB and config, no autoupdate, explicit `PWD`).
  This must be set *after* the profile env, or a profile could undo it. That
  is the opposite of the "set before profile env so a profile can restate it"
  ordering every current runner uses, so it is a new ordering policy that
  needs to be explicit.
- read-only expressed by writing an agent definition, which is a third
  permission mechanism beside OS sandbox and none.

And every site in the maki list.

With the proposed `Provider` enum, capability table, `Outcome` and
lifecycle-plus-adapter structure, each would be one adapter file plus one enum
variant, and the compiler would list the rest.
