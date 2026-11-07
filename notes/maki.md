# maki provider: design plan

Plan for adding maki (https://maki.sh, checkout in `research/maki`) as a fourth
provider, to run open models (DeepSeek, Qwen, Kimi, GLM and others) for
mechanical work while claude and codex keep planning, review and complex work.

Transient: this is a plan, not reference. What survives implementation moves
into `reference/maki.md` and `docs/`. Findings below come from reading the
source at a 2026-11-07 checkout; nothing has been run yet. Re-check each one
against the version we pin.

## Why maki

Nine harnesses were audited from source (Codewhale, crush, fx, goose,
keen-code, maki, opencode, zero, zot). maki fit best:

- Single Rust binary, MIT.
- Its headless mode deliberately reproduces Claude Code's `-p` wire format
  (`src/print.rs`: "never invents fields of its own"), so `run_claude` is the
  template.
- The caller picks the session ID up front (`--session-id`), so the in-flight
  marker and launch lock work exactly as for claude.
- SIGINT ends the turn cleanly and keeps the session, so `review interrupt`
  and `review message` work as for claude and codex.
- Bash stays in the foreground under `-p` (background tasks off by default),
  so no wake loop.

opencode is the parked runner-up, review#6.

## Decisions

### Same permission model as claude, no sandbox

claude runs on Claude Code's own permissions with `--permission-mode
dontAsk`, and `sandbox` is ignored (`reference/claude.md`, "`sandbox` is
ignored"). maki gets the same treatment:

- `-p` runs are unattended: any call that would prompt is denied, which is
  maki's equivalent of `dontAsk`.
- What a run may do is decided by maki's own `permissions.toml` (global in its
  config dir, project at `.maki/permissions.toml`), as claude's is decided by
  its settings files.
- `sandbox_for` drops the profile's `sandbox` for maki, and the sidecar
  records no sandbox level, as for claude.

Note the defaults differ from claude's: maki's built-in rules pre-allow
`write`, `edit`, `multiedit` and `edit_lines` inside the working directory, and
`task` everywhere. A maki run with no permissions file can edit the tree.
`docs/` must say so plainly, since a `review` run is read-only by default for
codex and grok.

### maki reads the operator's own config, not an isolated one

The audit suggested per-run XDG dirs for isolation. Rejected as the default:

- Every maki path derives from the XDG dirs (`maki-storage/src/paths.rs`).
  Sessions live under the state dir, so isolating it per run breaks resume.
- Isolating only `XDG_CONFIG_HOME` would hide the operator's providers,
  permissions and `init.lua`, which is the opposite of how claude is driven.

So maki runs against the operator's normal config, as claude does. The two
things that pushed toward a generated config (effort, below) are better fixed
upstream.

Things that stay safe without isolation:

- Headless never trusts a project without `--trust`, and `review` never passes
  it. An untrusted project's config "sets no defaults", and its deny rules
  still apply, so a reviewed repo can tighten maki's permissions but not loosen
  them (`maki-config/src/lib.rs` tests around folder trust).
- `--no-plugins` and `--no-commands` exist if a profile wants a clean run;
  not passed by default.

### Effort: upstream patch, generated config only as a fallback

`--effort` is accepted and ignored (`src/cli.rs`, `warn_ignored_flags`).
Effort comes from `always_thinking` in config or from the level the session
last used, and the session stores its own level (`maki-storage/src/sessions.rs`),
so a resume inherits it, which matches `review`'s inheritance rule.

Plan: send upstream a patch making `--effort` set the new session's thinking
level through the existing parser (the one `/thinking` and `always_thinking`
share: off, adaptive, minimal to max, or a token budget). Small, and keeps
the flag honest for Claude Code SDK callers too.

Until it lands, a profile `effort` for maki is refused at config time with a
message saying why, rather than silently ignored. The fallback, a generated
config dir, is only worth building if upstream declines.

For custom OpenAI-compatible providers, effort only reaches the request when
the model declares `thinking_fields`. That is the operator's provider config,
not `review`'s; `docs/` should mention it.

### Served model: record nothing until maki reports it

The stream's `message.model` is maki's own model ID (`self.model.id`,
`maki-agent/src/agent/run.rs`). The OpenAI-compatible provider writes `model`
into the request and never reads it back (`maki-providers/src/providers/openai_compat.rs`).
Copying the requested model into `served_model` would make the field lie,
which matters most behind OpenRouter's routing.

Plan: upstream patch to read the response's `model` into the assistant event.
Until then `served_model` stays empty for maki. Cost (`total_cost_usd`) is
recorded, with the caveat that an unpriced model reports 0.

### Own argument builder, tested against maki's ignored flags

maki accepts 15 Claude Code flags and ignores them with a stderr warning, and
reads `--permission-mode` only in SDK mode, so under `-p` it is dropped with no
warning at all (`src/sdk_mode.rs`). Reusing claude's builder would therefore
lose permission and effort settings silently.

So `run_maki` has its own builder, and a unit test asserts its output never
contains a flag on maki's ignored list (a copy of the list, with a comment
naming its source so an upgrade re-checks it), nor `--permission-mode`, nor
`--trust`.

## Invocation

```
maki -p --session-id <uuid> --output-format stream-json [--model provider/id]
```

Resume: `--resume <id>` in place of `--session-id`. Prompt on stdin (read in
full when no prompt argument is given). Profile `env` applied as for claude.
Whether `stream-json` needs `--verbose` as Claude Code's does: unchecked.

## Reading the run

Mirror `run_claude`:

- The first event is `{"type":"system","subtype":"init",session_id,...}`.
- The stream ends in a `result` object: `is_error`, `result`, `stop_reason`,
  `session_id`, `num_turns`, `total_cost_usd`, `usage`. Subagent text is
  excluded from `result`.
- `is_error: false` is the answer; anything else is `Ok` with a digest whose
  `turn_error` names `stop_reason`; a result missing `is_error` is a failure.
- **Do not trust the exit code.** maki exits 0 when the result says
  `is_error: true` (`print::run` returns `Ok`). Exit 1 is a setup failure, 101
  a panic, 130/143 a signal.
- No result after the init event is a death digest keeping the session ID; no
  init at all is an `Err` with no session, as for claude.

## Stopping a run

`src/interrupt.rs`: first SIGINT or SIGTERM cancels the session, tears down
what its tools started, prints the result and exits 128+signal; a second exits
at once; a signal ignored by the parent stays ignored. A cancelled run reports
`stop_reason` Cancelled with `is_error: true`, and the session persists.

So maki joins `interrupt_cmd::interruptible`, and the keeper forwards SIGINT
as for claude. To confirm by probe: when the handler is installed relative to
the init event (claude's rule is that a signal before init kills it with
nothing printed), and that SIGTERM also prints a result (claude's does not).

maki holds a per-session lock for the whole run (`SessionClaim`), unlike
claude. The launch lock still applies; maki's own lock is a second line of
defence.

## Config and wiring

- `KNOWN_PROVIDERS` gains `maki`; `review config` reports whether it is on
  `PATH`.
- Profiles: `[maki.<profile>]` with `model` (`provider/model-id`), `env`,
  `comment`. `sandbox` and `writable_roots` warn as ignored, as for claude.
  `effort` refused until the upstream patch lands (see above).
- `timings::stale_session` for maki: open. The cache lifetime depends on the
  backend (DeepSeek, Kimi and GLM each differ, and maki sends no cache hints on
  the OpenAI-compatible path, relying on provider-side prefix caching). Start
  at the non-codex 55 minutes and measure.
- Transcript: `<uuid>.jsonl` under `$XDG_STATE_HOME/maki/sessions/`.
  `review` reads nothing from it, as for claude.

## Upstream patches to send

1. `--effort` sets the new session's thinking level.
2. Read the response's `model` into the assistant event, so the served model
   is reported.
3. Malformed tool-call JSON on the OpenAI-compatible path is replaced with
   `{}` and the tool runs with empty arguments (`openai_compat.rs`, "malformed
   tool JSON, falling back to {}"). It should go back to the model as an
   error. This one hits open models hardest, which is the whole use case.

## Open questions

- Translating Claude settings into maki permissions. maki does not read
  `.claude/settings*.json`. A translator is feasible (`Bash(cargo *)` to
  `[bash] allow = ["cargo *"]`, `Edit(path)`/`Write(path)` to scopes) but has
  gaps: `read`, `glob` and `grep` declare no scopes and cannot be denied by
  rule; command substitution, process substitution, subshells and arithmetic
  expansion always prompt, so headless always denies them; glob semantics
  differ slightly (`cmd *` matches bare `cmd`, `dir/**` matches `dir`).
  Undecided whether `review` should do this at all, given the decision to read
  the operator's own maki config.
- Whether built-in allows (writes inside cwd) sit above or below a config
  `default = "deny"`. The docs only say any deny wins, so a read-only profile
  may need explicit denies per write tool.
- Whether plugin prompt slots (memory, todo) change per turn and force frame
  rebuilds. The frame design otherwise appends changes rather than rewriting
  the prefix (`maki-agent/src/agent/frame.rs`). Measure under review#4.
- The legacy `~/.maki` migration runs on every start. Harmless when reading
  the operator's own config, but worth knowing if a host has one.
- Upstream churn is very high (many commits a day). Pin a version in the
  reference doc and keep the stub-script tests in `tests/provider_runs.rs`
  close to the real stream.

## Steps

1. Probe a real `maki -p` run: stream shape, `--verbose`, SIGINT before and
   after init, SIGTERM, a failed model. Script in `scripts/` if it earns a
   place in the reference doc.
2. Send the three upstream patches.
3. `run_maki` with its own builder and result parsing, sharing what it can
   with `run_claude`.
4. Wiring: `KNOWN_PROVIDERS`, `interruptible`, `sandbox_for`, config
   validation for `effort`, `review config`.
5. Stub tests in `tests/provider_runs.rs` mirroring claude's: interrupt,
   answer beating it, stop before and after init, death after start,
   `is_error` with exit 0.
6. `reference/maki.md`, `docs/` topics, `--help`, AGENTS.md architecture line.
