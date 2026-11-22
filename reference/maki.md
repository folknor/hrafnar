# maki

What maki was observed to do, which the maki arms of
`src/harness/claude_compat.rs` and `claude_stream.rs` rest on. maki is a
coding agent that runs open models (DeepSeek, Qwen, Kimi, GLM and others,
directly or through OpenRouter) and is driven by claude's runner because
its headless mode reproduces Claude Code's stream-json format. Source
findings are from the `research/maki` checkout at `v0.7.3`; probes ran
against maki 0.7.3 (`scripts/maki-probe.sh`,
`scripts/maki-probe-signal.py`). Re-run both after a maki upgrade, because
upstream changes fast.

## Flags maki ignores

maki accepts 15 Claude Code flags only to ignore them with a warning
(`Cli::warn_ignored_flags`). `MAKI_IGNORED_FLAGS` is a copy of that list.
`--effort` is among them: the thinking level comes from config
(`always_thinking`), else from the level last used anywhere on the host
(`setup::remember_thinking`), and a resumed headless session does not take
the level stored with it.

A flag only some run paths read is refused with exit 1 on the others
(`Cli::check_run_path_flags`): `--permission-mode`, which only SDK mode
reads, fails a plain `-p` launch (probed).

The model id is maki's own `provider/model-id`: `openrouter/free` names a
model `free` that OpenRouter doesn't have (probed: a 404 result).

## Permissions

Under `-p` any call that would prompt is denied. maki reads its global
`permissions.toml` and the project's `.maki/permissions.toml`; an untrusted
project's config sets no defaults while its deny rules still apply.
Headless maki still applies a saved trust answer and the operator's
`trust.paths` patterns (`cmd/tui.rs`, `policy_grant`). The built-in rules
allow `write`, `edit`, `multiedit` and `edit_lines` inside the working
directory and `task` everywhere.

## Reading the run

The first line is
`{"type":"system","subtype":"init",session_id,cwd,tools,model}`, the last a
`result` with `is_error`, `subtype` (`success` or `error`), `num_turns`,
`result`, `stop_reason`, `session_id`, `total_cost_usd` and `usage`
(Claude's four token fields). maki sets `is_error` only for `cancelled`, so
`max_tokens`, `max_turns`, `dropped` and `compact` arrive with
`is_error: false`.

Exit codes, probed:

- **0**: also a provider failure, with `is_error: true` and
  `stop_reason: null` (an API 402, and OpenRouter's 404 for an unknown
  model, the API error as `result`)
- **1**: a setup failure (an unknown `--model` provider, an unknown
  `--resume` id), with nothing on stdout and an ANSI-coloured banner on
  stderr whatever the terminal
- **101**: a panic
- **130 and 143**: a signal

`total_cost_usd` is `0.0` for a model maki has no price for, and maki's
accumulator skips unpriced turns, subagents included. A priced probe
(`openai/gpt-5.5`) reported a cost; the free router reported `0.0`. The
OpenAI-compatible provider never reads the response's `model`, so
`message.model` is the requested model.

## Sessions and stopping

maki saves a session once it holds a message, after it has connected to
the model provider: a first turn that failed on an API error is saved and
resumes (probed). maki also holds its own per-session lock for the whole
run (`SessionClaim`).

The first SIGINT or SIGTERM cancels the turn, tears down what its tools
started, prints a `cancelled` result (`is_error: true`) with the text so
far, and exits 130 or 143; a second exits at once (`src/interrupt.rs`).
maki installs that handler after reading stdin and before printing `init`.
Probed mid-turn through OpenRouter (`openrouter/free` and
`nvidia/nemotron-3.5-super-vl:free`): both signals produced the cancelled
result at once, and the session resumed and answered.

A signal maki inherits as ignored stays ignored. A non-interactive shell
starts a background job with SIGINT ignored, so a shell-backgrounded maki
ran its whole turn through a SIGINT; that is why the signal probe is
Python.

## Prompt cache lifetime

The host serving the model sets a prompt cache's lifetime, and maki's host
varies with `--model` (OpenRouter, DeepSeek, Moonshot and others each set
their own). No per-harness `stale_cutoff` can be right for every maki
session, so maki uses the general one.

## Upstream patches worth sending

These maki changes would fix gaps `hrafnar` works around or can't report:

1. `--effort` sets the session's thinking level, and a resumed headless
   session takes its stored level unless `--effort` overrides it.
2. Read the response's `model` into the assistant event, so a served model
   can be reported.
3. Malformed tool-call JSON on the OpenAI-compatible path is replaced with
   `{}` and the tool runs with empty arguments; it should go back to the
   model as an error. This hits open models hardest.
4. A flag on the cost saying whether every turn was priced.
