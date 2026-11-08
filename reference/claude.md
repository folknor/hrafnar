# Claude

Session launch intent is typed as SessionStart. The shared invocation owner
prepares the fresh UUID before dispatch; the runner validates the intent and
never mints an ID. Resumes carry a checked SessionId. Historical log fields
and provider-reported evidence remain strings; they are checked before reuse
as launch input. Fresh results prefer the echoed ID, falling back to the
requested UUID; resume results keep the requested ID.

How `review` drives Claude Code: its invocation and environment, how a run's
outcome is read, and how a run is stopped. The cross-provider pieces are in
[sandbox.md](sandbox.md).

## Invocation

A fresh run is `claude --session-id <generated UUID> --print --permission-mode
dontAsk --output-format stream-json --verbose`; a `review resume` is the same
with `--resume <id>` in place of `--session-id`. Profile settings: `model` as
`--model`, `effort` as `--effort`, `env`. The prompt is piped via stdin. The
session ID is generated up front, so it is known before the run starts, and the
run's in-flight marker goes up at launch.

`--permission-mode dontAsk` uses pre-approved permissions and rejects
interactive prompts, which a headless run could never answer.

`stream-json` (which needs `--verbose` under `--print`) rather than `json` for
one reason: its first event, an `init` within half a second of launch, is the
sign that claude can be signalled safely (see [Stopping a run](#stopping-a-run)).
`json` prints nothing until the end. Both end in the same result object, which
is all `review` reads.

## Turns classify themselves

The stream ends in one result object per run, whatever the outcome, and its
`is_error` says whether `result` is an answer (a run in which claude woke the
model for a background subagent carries one per turn; the last is the answer).
That is the grok arrangement ([grok.md](grok.md#turns-classify-themselves)), and
`run_claude_compat` handles it the same way: `is_error: false` is the answer; an
ordinary failure with explicit `is_error: true` is `Ok` carrying a digest whose
`turn_error` names what claude reported, and keeps the session ID. A result object missing
`is_error` counts as a failure, so a claude that stopped reporting it fails
loudly rather than passing every run off as answered. Since that failure is
inferred by review, its explanation is recorded as `review_error`, not
`turn_error`.

Resumed output retains the session ID supplied to the runner. This lets the
shared result printer show the same session line and interruption hint for a
resume as for a fresh run.

No result object at all splits on whether claude printed its first event, the
`init` that carries the session ID. Before it, nothing ran and there is no
session: an `Err`. After it, the session exists - its transcript is on disk -
so a claude killed from outside or crashed mid-turn is `Ok` with a death digest
(no `turn_error`, since claude stated no reason) and keeps the ID, as codex's
deaths do. The error text for a missing result is claude's stderr, or else the
stream's last line, truncated: the whole stream is every event of the run.

An answer followed by a non-zero exit keeps `captured: true` and the observed
exit code or signal. That status is not a provider-stated `turn_error`.

The stream is buffered up to 64 MiB from the start and, past that, the last
8 MiB or more, because the result is the *last* line: keeping only the head
would lose the answer of any run that outgrew the cap. The shared reader in
`src/provider/stream.rs` owns capture and physical-line framing. When the head
and tail are discontinuous, it removes the cut fragments and keeps a newline
between surviving regions, so retention cannot fabricate a result object.
An intact final line without a newline is still eligible at EOF or read error.
The live observer uses a separate 64 MiB physical-line cap for metrics and
accepts init only on lines at most 1 MiB. Oversized lines are discarded through
their delimiter. Capture keeps draining after it fills; observation continues
independently. Completed metrics and readiness observations survive a cancelled
reader; an unfinished line is not observed on cancellation.

`src/provider/claude_stream.rs` owns pure Claude-compatible result, init and
metric decoding. Claude and maki share it and the runner in
`src/provider/claude_compat.rs`; what differs between them - argv, forced
environment, how a result and its cost are read - is an exhaustive match on
the `Dialect` ([maki.md](maki.md)).

Two details from Claude Code 2.1.284 decide the shape. `subtype` is no guide:
an unknown `--model` came back `subtype: "success"`, `is_error: true`,
`terminal_reason: "api_error"`, `api_error_status: 404`, exit 1 - so `is_error`
decides, and the other three are only named in the reason. And that failed
launch still has a session: the result carries its ID and the transcript is on
disk. Text output, which `review` used before, gave only the non-zero exit, and
the ID was discarded with it (`invoke` sets none on an `Err`).

The `modelUsage` keys from every result event are unioned into the served model
(sorted and comma-joined), and `total_cost_usd`, usage and turn counts are summed
across those events. The runner accumulates these metrics as it reads the stream,
so the head-tail output cap does not discard result events from the middle.
Other event types do not contribute, and only the last result decides the
answer. Usage is attached to the digest when a run needs one. A clean run has no
digest, as for grok. Claude has no auto-resume: that is codex's workaround for a
death, and a claude turn that fails says why.

## Shell commands stay in the foreground

**`--print` kills a background Bash command when the model ends its turn.**
Probed on Claude Code 2.1.284 (`scripts/claude_background_probe.py`): asked only
to run a 60-second command in the background and report its output, the model
replied "I'll report exactly what it prints when it finishes" and ended its
turn. About five seconds later claude marked the command `killed` and exited 0.
The result object says nothing about it - `is_error: false`,
`terminal_reason: "completed"` - so to `review` it was a clean run with no
report: the loss grok's wake loop exists for
([grok.md](grok.md#background-commands)). A background *subagent* is handled
differently: claude waits for it and wakes the model with its result in a
second turn, whose result object is the last in the stream.

Claude, unlike grok, has a switch that removes the failure rather than
detecting it: `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1` takes the
`run_in_background` parameter off the Bash tool altogether. Under it the same
prompt ran the command in the foreground and reported its output. `run_claude_compat`
sets it on every run, so no wake loop is needed.

That exposes Bash's timeout, which kills a foreground command at 2 minutes by
default. The model may pass a longer `timeout`, but under `dontAsk` a permission
allowlist naming commands (`Bash(python3 *)`) denied every call carrying one -
the probe's model tried 200s and was refused - so the default is the limit that
actually binds. `run_claude_compat` sets `BASH_DEFAULT_TIMEOUT_MS` and
`BASH_MAX_TIMEOUT_MS` to 20 minutes (`CLAUDE_COMMAND_WAIT_SECS`, grok's
allowance too); a 150-second command then ran to completion. All three are set
before the profile's `env`, so a profile can restate any of them.

## No filesystem sandbox

Claude's `--permission-mode` is a tool-approval policy, a different axis from
an OS filesystem sandbox. Claude profiles must omit `sandbox` and non-empty
`writable_roots`; parsing refuses these previously ignored fields with removal
instructions. Non-empty `config` is likewise refused because it is codex-only.
A claude run records no sandbox level. What it may touch is decided by the
permissions Claude Code itself is configured with.

## Transcript, no watchdog

Claude writes each session's transcript to
`~/.claude/projects/<cwd with / as ->/<session-id>.jsonl`, but `review` reads
nothing from it: the result object already says whether the turn answered,
which is what codex's rollout forensics reconstruct. No stall watchdog either -
it rests on codex's habit of writing to its rollout every few minutes (see
[codex.md](codex.md#the-stall-timeout)), which nothing has established for
claude.

## Resume

A `review resume` carries the model and effort recorded for the session
(`--model`/`--effort`), as every provider's does - see
[sandbox.md](sandbox.md#what-a-resume-inherits).

## Stopping a run

**A claude turn ends cleanly on `SIGINT`, so `review interrupt`, `review
message` and a signalled `review` treat it as they treat codex's.** Probed on
Claude Code 2.1.284 with a command running mid-turn: `SIGINT` to the claude
process ended the turn at once, killed the command, exited 0, and still printed
a result object - `is_error: true`, `subtype: "error_during_execution"`,
`terminal_reason: "aborted_tools"`, the session ID - and the session resumed
normally, the model seeing its command as rejected. `SIGTERM` also left a
resumable session but printed nothing (exit 143), so nothing could be recorded
from it; that is why the graceful path sends `SIGINT`.

The mechanics are codex's ([codex.md](codex.md#interrupting-a-run)), minus the
node wrapper: claude runs under a keeper ([teardown.md](teardown.md)), which
gives it a process group of its own and tears down every shell it started when
the run ends; its in-flight marker stores one keeper identity and becomes
signal-ready once claude has printed its first event (a `SIGINT` before its handler is installed would kill
it with nothing printed); `review interrupt` leaves a request beside the marker
and signals the keeper, which forwards one `SIGINT` to claude alone; and a
signalled `review` has the keeper send it `SIGINT` too, waiting for the run to
record its session before exiting. A run that had produced no first event is
torn down by its keeper instead, and reported as never started,
with no session ID: claude cannot have written one, and recording it would
print a resume command that fails. A run that has not spawned yet when `review`
is told to stop - still waiting on the lock or its stagger - is not launched at
all. An interrupted run keeps its session ID even if claude printed no result,
is flagged `interrupted` rather than carrying claude's complaint about the abort
as a `turn_error`, and is not retried. If claude answers before the signal
lands, the answer stands. Before a resume launches, any interrupt request left
for its session by an earlier `review` that died before consuming it is
cleared, so it cannot mark this run interrupted.

Claude has no session lock of its own, so two `review message` calls to an idle
session used to both launch, running two turns on one session at once. `review
message` now holds a per-session launch lock from before it checks for a turn in
flight until its run has launched, and a run's marker is written before its
spawn, so the second message waits, then finds the first in flight and
interrupts it. As a second line of defence, a run removes or updates its own
marker at an exclusively owned per-invocation path. Pending handoff remains
visible through app recording and receipt publication; two unrelated runs of one
session cannot overwrite or delete each other. Ambiguous observations are refused.

`tests/provider_runs.rs` drives `invoke` and the complete adapter against a
stub (`ProviderRuntime.commands` with the provider's prefix) for each of these,
once per dialect with that dialect's own wire fixtures: the interrupt, an answer
beating it, a stop after and before the first event, a refused launch after a
stop, a death after starting, and a stale request before a resume.

Verified end to end through `review`: a `SIGTERM` to `review` mid-command ended
the turn in 0.2s, recorded it, printed the `review message` command and left
nothing running; `review interrupt <ID>` from a second process did the same;
and `review message <ID>` interrupted the turn and resumed it with the new
message, which the model answered.

The argv table covers fresh/resume, omitted and named review sandbox levels,
stream-json, dontAsk and model/effort selection. Claude still receives no sandbox
flag or effective root grants. Direct runner helpers and their intermediate
result are private; fixtures retain the invocation result through pending-marker
and receipt assertions. Runtime timings and keeper settings remain injectable.
