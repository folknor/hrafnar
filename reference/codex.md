# Codex

What codex does that shapes how `review` drives it: observed behaviour,
measured evidence and upstream facts. The rules themselves, and the reason
for each flag, are on the code in `src/harness/codex.rs` and its modules. The
permission machinery shared across harnesses is in [sandbox.md](sandbox.md).

## Scoping a run inside a shared rollout

A resume appends to the same rollout file as every earlier turn, so each
read of the rollout must know where this run's bytes begin. The runner
measures the rollout's length before spawn and turns it into one `Scope`,
which every reader then uses:

- The watchdog (`harness/codex/watch.rs`) judges only bytes in that scope,
  so a previous turn's answer can't license killing a quiet resume.
- Recovery (`transcript.rs`) salvages an answer only from that scope, so a
  previous turn's answer can't be reported as this run's, which would also
  suppress auto-resume.
- The incident bundle (`harness/codex/forensics.rs`) tails and scans the
  same scope.

A byte offset, not a timestamp, because `review`'s own clock string is
second-resolution and an answer written in the launch second would pass a
time filter. A fresh run's scope starts at 0, exactly. A resume whose
rollout can't be measured gets `Scope::Untrusted`, which disables both the
watchdog and recovery: scoping it to 0 would admit the whole history, and no
recovery beats a wrong one.

The watchdog's answer is a separate input to recovery. A trailing real or
phantom turn can clear the rollout's current-turn answer after the watchdog
killed on it, so the runner prefers the exact answer the watchdog saw, and
the bundle's answer flag follows the same evidence.

## Two ways a run fails

A failed `codex exec --json` run looks the same on stdout either way:
`exit 1`, nothing captured, zero turns. The two causes are:

- **Truncated but completed**: the model wrote a real `final_answer` to the
  rollout, but codex exited non-zero at shutdown and dropped the closing
  stream events and the `-o` file.
- **Mid-turn death**: no `final_answer` was ever written; the turn was cut
  off, typically after a tool call.

`task_complete` fires on aborted turns too, so it can't tell them apart; the
rollout's phase-tagged final answer can. Codex records no reason for either
in the rollout: no `stream_error`, abort or limit event. Recovery salvages
the output; it doesn't explain the death, which is why suspicious runs get
an incident bundle with stderr and `RUST_BACKTRACE=1`.

A turn codex refuses is not a death. Codex ends it with an `error` and
`turn.failed` pair carrying the same message (content flagged, rate limit,
auth, or a harness error) and exits promptly on its own. Observed on
codex-cli 0.146.0.

## Hangs on codex 0.147.0

Codex 0.147.0 can finish a turn and never exit. Two hangs were diagnosed
against sessions that worked on 0.146.0:

- **Stranded completion**: every turn reached `task_complete` with an
  answer, the rollout stopped advancing, and the process hung over 10
  hours with the answer on disk.
- **Mid-turn wedge**: the rollout stopped mid-turn with 25 unified-exec
  background cells running.

The leading upstream explanation for the stranded case is the resume-only
`exclude_turns: true` that `thread_resume_params_from_config` sends from
0.147.0 (openai/codex#35621). After `TurnCompleted`, exec issues an unbounded
`thread/read` with `include_turns: true`
(`maybe_backfill_turn_completed_items`) while its single `select!` loop
isn't draining notifications. With turns excluded at resume, that read
rebuilds the whole thread from the rollout, which fits "resumes hang, fresh
runs don't, small sessions are fine". A
secondary candidate is the synchronous `Drop` of `OtelProvider`, bounded
only for the TUI by openai/codex#37109, with a Statsig metrics exporter on by
default in release builds. Neither is proven. `review` catches the hang with
the watchdog rather than explaining it.

Codex 0.147.0 also opens a phantom turn during `codex exec` teardown and
aborts it within 46 ms; its shape and the rollback are on the fold in
`harness/codex/rollout.rs`.

## The stall timeout

The stall verdict rests on an observed property, not a contract: codex
wakes itself every 2 to 5 minutes even while waiting on a long tool call,
and the observed wedge was silent for hours. The evidence and why the
default is safe are on `harness/codex/watch.rs`. No other harness has a
measured cadence, so the stall branch is off for them.

## Interrupting a run

A running `codex exec` turn can't be spoken to, only ended. Established
from the codex source (checkout at 50fc133d05, installed 0.156.1):

- Exec reads stdin once (`read_prompt_from_stdin`) and runs its app-server
  in-process, with no socket or remote-control endpoint.
- It handles one signal, SIGINT, which it turns into `turn/interrupt`: the
  turn ends `Interrupted` and exec exits 1.
- In-turn steering (`turn/steer`) exists, but only for a client of the
  process that owns the thread, and exec is its own only client.
- No other process can take the thread over. Each live thread holds an
  flock at `$CODEX_HOME/thread-writer-locks/ID.lock`, and `thread/resume`
  elsewhere fails "already has an active writer" until the kernel releases
  it at exit.
- `codex queue` reaches another process's thread only as a new turn after
  the current one ends, and an exec run is a single turn.

So the one lever is to end the turn with SIGINT, then resume. On an npm
install the pid is the node wrapper `codex-cli/bin/codex.js`, which forwards
one SIGINT to the native binary but installs that forwarding only after
spawning it. That is why the marker becomes signal-ready only at codex's
first stdout byte, and why the signal goes to the leader, not the group.

## The sandbox guarantee

The filesystem guarantee a profile advertises has needed four fixed flags,
because ambient operator config could void `--sandbox` twice over. Each
flag's mechanism and measurement is on the line in `run_codex` that passes
it; the facts in short:

- **`approval_policy="never"`**: from 0.150.x, `approvals_reviewer =
  "auto_review"` in the operator's config drops exec's headless `never`,
  and codex's guardian grants write escalations. Measured on 0.150.1.
- **`--ignore-rules`**: an execpolicy `allow` rule matching every segment
  of a command runs it with no sandbox at all, at every level, regardless
  of approval policy. Measured on 0.151.0.
- **`--ignore-user-config`**: drops `config.toml` wholesale. It doesn't
  close the rules hole: alone it leaves the command unsandboxed,
  `--ignore-rules` alone sandboxes it.
- **`--skip-git-repo-check`**: codex refuses non-repo directories even when
  trusted (0.149.0), and the flag doesn't touch the sandbox.

Network access is enabled at both sandboxed levels because codex's Linux
seccomp filter for restricted network denies the pathname socket syscalls,
so AF_UNIX sockets are unusable. `read-only` can get that grant only from a
named permissions profile, which `--sandbox` would make inert; the measured
table is on `codex_sandbox_args`, produced by
`scripts/readonly_network_check.py` on 0.151.0.

## Model selection

Codex resolves model and effort as `review` mirrors them in
`harness::ModelSpec`. Checked against source at 2bc8e7f545 with codex-cli
0.167.0 installed:

- `exec/src/lib.rs` builds `ConfigOverrides.model` from `-m`, and
  `core/src/config/mod.rs` takes `model.or(cfg.model)`, so `-m` beats a raw
  `model` override.
- Effort comes only from `cfg.model_reasoning_effort`, so the last `-c`
  assignment wins.
- `utils/cli/src/config_override.rs` splits a `-c` at the first `=`, trims,
  and parses a TOML value. Only on parse failure does it fall back to the
  literal text with quote characters trimmed.

## Source checkout and diagnostics

`research/codex` is a gitignored checkout of the codex source. When it's
present it's the authority on codex behaviour. The sandbox findings above
came from `protocol/src/protocol.rs` (`SandboxPolicy`),
`config/src/config_toml.rs` and `core/src/config/` (how `sandbox_mode` or
`default_permissions` compiles into a policy) and
`linux-sandbox/src/landlock.rs` (what the seccomp filter denies). Compare
`git log -1` and the newest tag there with `codex --version` before trusting
it: a key in the checkout may not exist in the installed binary.

The source says what should happen; the scripts in `scripts/` measure what
does, against whatever codex is installed:

- `ignore_rules_check.py`: regression check that `--ignore-rules` keeps
  rules from bypassing the sandbox
- `flag_matrix.py`: which isolation flags close the rules hole
- `rules_survey.py`: which command prefixes `~/.codex/rules` exempts
- `rollout_commands.py`: a rollout's commands and outputs, the only
  readable record of a run with reasoning summaries off
- `readonly_network_check.py`: what a `read-only` run can do with sockets
  across three ways of selecting the level; it drives `sandbox_probe.py`,
  the witness that runs inside the sandbox
