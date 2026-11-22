# Grok

What grok does that the adapter in `src/harness/grok/` is built around:
observed behaviour, measurements and upstream facts the code cannot state
for itself, plus the two flows that span several modules. The rules and
their reasons are on the code. The cross-harness permission machinery is
in [sandbox.md](sandbox.md).

## Turns classify themselves

Grok's `--output-format json` prints one result object per run, and its
`stopReason` is the only answer signal: `end_turn` means `text` is an
answer, while a turn cut off mid-flight comes back `cancelled` with interim
commentary in `text`. Every field of that object is optional, and a
pre-turn failure prints a typed `{"type":"error"}` line that parses as an
empty result. That is why `interpret_grok_output` checks the error type
first and requires a field grok sets only after running a turn.

## Folder trust

In a folder with no decision in `trusted_folders.toml`, grok raises a
permission prompt on every file edit. Under `--permission-mode dontAsk`
the prompt resolves `cancelled`, and grok ends the whole turn instead of
handing the model a denial. The sandbox level does not matter. Observed on
grok 1.0.40: six `workspace-write` runs in an untrusted checkout each read,
diagnosed and announced a fix, then were cancelled at their first
`search_replace` after 5 to 16 minutes. The same edit with the same flags
succeeds in a trusted folder (A/B verified).

Upstream facts the trust code models, from grok's source:

- `--trust` is hidden (`xai-grok-pager/src/app/cli.rs`) and honoured in
  headless mode by `headless.rs` through `folder_trust::grant_folder_trust`.
  Verified on 1.0.40 against a scratch `GROK_HOME`: launched from a
  subdirectory, it recorded the repo root, before the auth check.
- A grant is VS-Code-style workspace trust: it also lets the repo's own
  `.grok` MCP/LSP config spawn servers (`trust.rs` module docs). The entry
  is permanent and host-wide, so the operator's interactive sessions in
  that repo run that config too.
- Grok's gate queries the store with the workspace key
  (`folder_trust::decide_inputs`), and `is_trusted` counts only entries that
  prefix the key and share its workspace id. Only the key's own entry can
  decide: a `false` on a subdirectory is never consulted.
- Grok's grant overwrites a `trusted = false`, and grok offers no "grant
  unless declined" operation. The decline check and the grant are separate
  reads, so a `false` written between them is lost; re-checking right
  before spawn shrinks that window to grok's startup.
- A grok-managed `-w` worktree is keyed through grok's sqlite registry,
  consulted only for paths under `$GROK_HOME/worktrees`
  (`worktree_record_for_cwd_in`). That is exactly the set `review` refuses.
- A failed grant (`KeyMoved`, `Unreadable`, a process-local fallback) is
  printed only to stderr, and success prints nothing
  (`folder_trust::report_cli_trust_grant`).

The grant spans three steps in `mod.rs`: `prepare` announces a grant only
for an undecided key, `run_grok_turn` re-checks that key just before spawn,
and `Prepared::verify_trust` re-reads the store after the run. Only a
trusted entry on the announced key reaches the sidecar as `grok_trust`, so
`jq 'select(.grok_trust)'` lists every grant `review` made.

## Writable roots

Grok's built-in `workspace` profile writes only to the workspace, the grok
home and the temp dirs, and leaves `restrict_network = false`
(`profiles.rs`), so codex's unix-socket grant has no grok counterpart.
Grok offers no flag or env var for extra writable paths: `config.toml`'s
`[sandbox]` and `GROK_SANDBOX` select a profile by name only, and moving
`GROK_HOME` would move auth, sessions and trust with it. The one surface is
a named profile in the global or a project `.grok/sandbox.toml`.

Without the extra roots, trusted write runs edited fine and then every
`brokkr test` died at `lock: failed to open lock file` on `~/.brokkr`. Two
runs worked around it by moving `HOME` under `target/`, which took a
private brokkr lock and bypassed the host's build-contention gate.

Grok lists `sandbox.toml` among its trust-boundary files and refuses to
load one through a symlink (`resolve_trust_boundary_sources`,
`ensure_grok_trust_boundary_slots`). It deserializes the global file whole
and drops it on one bad entry, after which a checked-out repo's project
file may define a `review-ws-*` name: a project profile loses only to a
loaded global one of the same name.

On resume grok fixes a session's profile for its lifetime and refuses an
explicit `--sandbox` that differs (`resolve_startup_sandbox` returns
`Conflict`); no flag applies the saved one. The resume selector is decided
across three places. The sidecar stores the exact native name in
`provider_sandbox` beside the normalized `sandbox` level, and resume
prefers `provider_sandbox`. `Permissions::restore` keeps that native name
untranslated. A generated `review-ws-*` name maps to `GrokWrite::Resume`
with the roots recorded alongside, `workspace` to
`GrokWrite::BuiltinWorkspace`, and no recorded level to no `--sandbox` and
no write setup.

## Background commands

Grok moves any shell command that blocks longer than 15s to the background
(`toolset.bash.auto_background_on_timeout`, on by default). In `-p` mode
`headless.rs` waits only for the task to finish, exits once `pending_bg` is
empty, and queues the completion as a wake-up for a model that never gets
another turn. Which happens depends on the model:

- Session `db7ea2b1` called `get_command_or_subagent_output` with a timeout
  for two `brokkr test` runs, guided by grok's bundled
  `long-running-background-tasks` skill, and read both in-turn.
- Two other runs ended their turn ("I'll read the result when that build
  finishes"), and a live probe showed `brokkr test` passing 13s after grok
  had exited.
- A warm `brokkr test` that the model had backgrounded finished before the
  model's turn ended, and its wake-up message still sat unread afterwards:
  grok never hands a background result to the running turn.

Grok records the difference on every shell completion: `will_wake: true`
with `block_waited: false` when the model was not waiting (both lost
sessions), `will_wake: false` with `block_waited: true` when it was blocked
on it (`db7ea2b1`). Subagent completions carry no such flag, which is the
one known gap in the unread-work detection.

Limits that shape the timeouts in `run_grok_turn`, from grok's source:

- The auto-background switch is `config.toml` only, not on the
  `GROK_CONFIG` allowlist (`config_override::OVERLAY_ALLOW_PATHS`).
  Turning it off kills a command at `min(timeout, max(5min, configured))`.
- Production grok always sends `max_timeout_secs` of 10h
  (`PRODUCTION_MAX_TIMEOUT_SECS`); the 5-minute `DEFAULT_MAX_TIMEOUT_MS` is
  not the ceiling. The model's default timeout is 120s (`DEFAULT_TIMEOUT`),
  raisable only through `config.toml`.
- `--background-wait-timeout` defaults to 600s, after which grok kills the
  pending work.
- Grok starts its own wake turns when background work finishes (prompt ids
  `task-completed-*` and similar, `notification_bridge.rs`).

Verified: with the raised budgets, a 30s command ran in the foreground and
its output was reported in the same turn.

Grok also honours the repo's Claude Code permission file
(`.claude/settings.local.json`): a probe's `sleep` was refused by its deny
rules, so an agent's allowlist shapes what a grok run can execute.

## The served model

Grok reports the model that served a run in its result JSON (`modelUsage`
keys), and it is a different name from the selected one: `-m grok-4.7` is
served as `grok-4.7-build`, which `grok models` does not list and `-m`
refuses as unknown. That is why `served_model` is recorded but kept out of
`SessionRecord`, so no resume reads it back into `-m`. Grok also stores the
model with the session, so the `-m` a resume inherits is redundant for grok
(see [sandbox.md](sandbox.md#what-a-resume-inherits)).

## Source checkout

`research/grok-build` is a gitignored checkout of grok's source, not
guaranteed present. When it is, it is the authority for the facts above:
`xai-grok-pager/src/app/cli.rs` for the hidden `--trust` and
`--background-wait-timeout` flags, `headless.rs` for what `-p` mode waits
for, `trust.rs` and `folder_trust` for the trust key, and `profiles.rs` for
the built-in sandbox profiles.
