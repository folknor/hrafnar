# maki

How `review` drives maki (`research/maki` when checked out), a coding agent
that runs open models - DeepSeek, Qwen, Kimi, GLM and others, directly or
through OpenRouter - for mechanical work. Source findings are from the
checkout at `b58c319d`; probes ran against maki 0.7.1
(`scripts/maki-probe.sh`, `scripts/maki-probe-signal.py`). Re-run both after a
maki upgrade: upstream churns fast.

maki's headless mode reproduces Claude Code's `-p` stream-json format, so
maki is driven by claude's runner (`src/provider/claude_compat.rs`) and
decoder (`src/provider/claude_stream.rs`). Everything in
[claude.md](claude.md) about the in-flight marker, the keeper, the launch lock,
stream capture and the stop path holds for maki unless this page says
otherwise. What differs is an exhaustive match on `Dialect`: the argv, the
forced environment, and how the result and its cost are read. Wire
compatibility does not mean equal semantics, which is why the result reading
is not shared.

## Invocation

A fresh run is `maki --session-id <generated UUID> -p --output-format
stream-json`; a resume has `--resume <id>` in its place. The profile's `model`
becomes `--model <provider/model-id>`. The prompt goes on stdin, which maki
reads in full when no prompt argument is given. `--verbose` is not needed for
stream-json.

`--model` is maki's own `provider/model-id`: OpenRouter's `openrouter/free`
router is `openrouter/openrouter/free` to maki, and `openrouter/free` names a
model `free` that OpenRouter does not have (probed: a 404 result). Without a
model maki takes the last one it used, then its configured default.

maki accepts 15 Claude Code flags only to ignore them with a warning
(`Cli::warn_ignored_flags`), and reads `--permission-mode` only in its SDK mode,
dropping it under `-p` without a word. The argv builder is therefore maki's own
arm of `launch_args`, and a unit test asserts it never emits one of those flags,
`--permission-mode`, `--trust` or `--yolo` (`MAKI_IGNORED_FLAGS` is a copy of
maki's list; re-check it on upgrade).

Session IDs: maki parses a hyphenated UUID or its own base58 and echoes the
caller's string verbatim, so `review` mints a UUID as for claude. A fresh maki
launch refuses anything but a UUID before it starts; a resume ID stays opaque,
so a session maki itself named (base58) can be resumed.

No environment is forced. Background tasks are already off under `-p`, so bash
stays in the foreground and no wake loop is needed. The bash timeout is a maki
config option (`[plugins.bash] timeout_secs`), the operator's to set.

## Permissions

The same model as claude: no OS sandbox, the provider's own permission
configuration decides. Profiles must omit `sandbox`, non-empty
`writable_roots` and non-empty `config`, and the run records no level.

- Under `-p` nobody can answer a prompt, so any call that would prompt is
  denied: maki's equivalent of claude's `dontAsk`.
- maki reads its global `permissions.toml` and the project's
  `.maki/permissions.toml`. An untrusted project's config sets no defaults
  while its deny rules still apply, so it can tighten maki's permissions but
  not loosen them. `review` never passes `--trust`, but that only means
  `review` grants nothing: headless maki still applies a trust answer saved
  earlier and the operator's `trust.paths` patterns (`cmd/tui.rs`,
  `policy_grant`), and a trusted project's allow rules and defaults load.
- maki's built-in rules allow `write`, `edit`, `multiedit` and `edit_lines`
  inside the working directory, and `task` everywhere. A maki run with no
  permissions file can edit the project. `docs/sandbox.md` says so.

maki runs against the operator's normal config and state, as claude does.
Isolating its XDG directories per run was rejected: sessions live under the
state directory, so isolation breaks resume, and isolating only the config
hides the operator's providers and permissions.

## Effort is refused

A profile `effort` for maki fails config parsing. maki accepts `--effort` and
ignores it. Its thinking level comes from config (`always_thinking`), else
from the level last used anywhere on the host (`setup::remember_thinking`);
a resumed headless session does **not** take the level stored with the
session, so there is no inheritance to rely on either. `review` records no
effort for maki.

## Reading the run

The first line is `{"type":"system","subtype":"init",session_id,cwd,tools,model}`,
the last a `result` with `is_error`, `subtype` (`success` or `error`),
`num_turns`, `result`, `stop_reason`, `session_id`, `total_cost_usd` and
`usage` (Claude's four token fields). Assistant events come between.

**`stop_reason` decides, with `is_error`.** maki sets `is_error` only for
`cancelled`, so `max_tokens`, `max_turns`, `dropped` and `compact` arrive with
`is_error: false`. `stop_reason` is kept as raw JSON, so a missing key, `null`
and an unfamiliar value stay distinct and an unknown value cannot fail the
whole result:

| `is_error` | `stop_reason` | verdict |
|---|---|---|
| false | `end_turn` | answer |
| false | `max_tokens`, `max_turns`, `dropped`, `compact`, `cancelled` | stated failure, partial text kept |
| true | present (a provider failure has `null`) | stated failure |
| any | key missing | unverified, beside any stated failure |
| false | `null` or unrecognised | unverified |
| missing | any | unverified |

**The exit code decides nothing.** A provider failure exits 0 with
`is_error: true` (probed: an API 402, and OpenRouter's 404 for an unknown
model, each with `stop_reason: null` and the API error as `result`). A setup
failure - an unknown `--model` provider, an unknown `--resume` ID - exits 1
with nothing on stdout and an unconditionally ANSI-coloured banner on stderr,
which becomes the `Err` with its escapes stripped. 101 is a panic; 130 and 143
are signals.

**Cost.** `total_cost_usd` is always present and is `0.0` for a model maki has
no price for, so a zero is recorded as unknown. A positive total is recorded as
reported; maki's accumulator skips unpriced turns, subagents included, so it
can be a partial sum. A priced probe (`openai/gpt-5.5`) reported a cost; the
free router reported `0.0`.

**Served model: none.** The result has no `modelUsage`, and `message.model` in
other events is the model maki asked for, not what the backend reported (the
OpenAI-compatible provider never reads the response's `model`). Nothing is
recorded as served.

## Sessions and deaths

maki saves a session once it holds a message, which is after it has connected
to the model provider: a first turn that failed on an API error is saved and
resumes (probed), but a maki killed between `init` and that first save leaves
an ID with no session behind it. No stream event tells the two apart, so a
death after `init` keeps its ID as claude's does: the cost of a false keep is
one resume that maki refuses cleanly ("not found", exit 1), while a dropped ID
loses the turn most worth resuming.

maki also holds its own per-session lock for the whole run (`SessionClaim`).
`review`'s launch lock still serialises messages; maki's is a second line of
defence.

Transcripts live under maki's state directory; `review` reads nothing from
them.

## Stopping a run

The first SIGINT or SIGTERM cancels the turn, tears down what its tools
started, prints a `cancelled` result (`is_error: true`) with the text so far,
and exits 130 or 143; a second exits at once (`src/interrupt.rs`). maki
installs that handler after reading stdin and before printing `init`, so
claude's rule - signal-ready once `init` is seen, killed as a group before -
holds unchanged. Probed mid-turn through OpenRouter (`openrouter/free` and
`nvidia/nemotron-3.5-super-vl:free`): both signals produced the cancelled
result at once, and the session resumed and answered.

`review` forwards SIGINT, as for claude. A `cancelled` result with no
interrupt request from `review` - someone signalled maki directly - is maki's
stated cancellation, not an operator interrupt.

A signal maki inherits as ignored stays ignored. A non-interactive shell starts
a background job with SIGINT ignored, which is why the signal probe is Python:
a shell-backgrounded maki ran its whole turn through a SIGINT. The keeper
starts it with default dispositions.

`tests/provider_runs.rs` runs every claude lifecycle scenario for maki with
maki's own wire fixtures and exit statuses, plus maki's own: an error result
with exit 0, a cancellation from outside, and a `max_tokens` answer.

## Cache

`stale_cutoff` is the general 55 minutes. The real lifetime is the backend's:
maki sends no cache hints on the OpenAI-compatible path and relies on the
host's prefix caching, which differs per host. Unmeasured.

## Upstream patches worth sending

1. `--effort` sets the session's thinking level, and a resumed headless
   session takes its stored level unless `--effort` overrides it.
2. Read the response's `model` into the assistant event, so a served model can
   be reported.
3. Malformed tool-call JSON on the OpenAI-compatible path is replaced with `{}`
   and the tool runs with empty arguments; it should go back to the model as an
   error. This hits open models hardest.
4. A flag on the cost saying whether every turn was priced.
