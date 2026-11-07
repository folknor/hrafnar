# Bugs

Defects turned up by the design hunt over `review` (scopes: providers,
lifecycle, config, orchestration, forensics, grok-man). Unverified: each is a
hunter's reading of the code, and some say so explicitly.

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

## BUG-011 - Incident bundles read the whole rollout and judge "answered" their own way

Reported by: forensics. Narrowed: the invalid-UTF-8 loss is fixed (the read is
now lossy, like the other readers).

What remains needs the run's rollout offset, which `incident.rs` does not
receive: `Incident` carries a transcript path but no offset. `write_bundle`
scans the whole rollout, and `transcript.tail.jsonl` is unscoped too, so on a
resume the tail can be entirely previous turns. The scanner judges "answered"
as `task_complete.last_agent_message != null`, without phantom-turn handling
or the shared phase-tagged recognizer, so it can disagree with
`transcript::parse` (a codex 0.147.0 teardown phantom reads as suspicious; a
commentary-only turn reads as completed). The fixer's proposed route: the
codex runner (now `src/provider/codex.rs`) already computes `recovery_scope`;
pass that offset into `Incident`, then scope the excerpt with
`transcript::slice_from_offset` and classify with the existing scoped
transcript summary. Comments at the bundle read and scanner sites record the
limitation.

## BUG-012 - A custom `CODEX_HOME` blinds resume forensics and `review sessions <id>`

Reported by: forensics, providers. Narrowed: within one run, codex's home is
now resolved once and used for the baseline, the watchdog and recovery alike.

What remains crosses runs. Only env var names are recorded, so `CODEX_HOME` set
by a profile is lost after the run. The resume path passes no profile env, so
the rollout baseline and transcript lookup use the default `CODEX_HOME`: a
session created under a custom home resumes against the wrong one, and the
watchdog and recovery are disabled with a warning. `run_session_show` passes no
home override for the same reason and cannot find the rollout. The run itself
knew the path, but it is not persisted in `DigestSummary`, and on a clean run
no transcript is read at all. Proposed: persist the resolved rollout path in
the sidecar row and read it on resume and in `review sessions <id>`.

## BUG-017 - The incident replay command omits runner-injected env

Reported by: forensics.

`replay_command` records only the profile env. Anything the runner injects
never reaches the replay; `RUST_BACKTRACE` is hard-coded on both sides by
agreement. For opencode, whose isolation is entirely runner-injected env
(disabled project config, private DB, `PWD`), a replay built this way would
attach to the operator's real config and DB: not a reproduction, and mildly
dangerous. The proposed fix, building the replay from the actual spawned
`Command`, is in `notes/structural.md` under the incident split.

## BUG-020 - The grok runner ignores the stop channel

Reported by: providers. Not verified by the hunter.

`*runtime.stop.borrow()` is checked before spawn in the claude and codex
runners. The grok runner's turn does not check it, and the wake loop ignores
`stop` too, so a grok run still waiting on its stagger when `review` is
signalled will try to launch, and the wake loop will try to launch wake turns.
Whether `contained::spawn` refuses after `terminate_all` was not verified. If
it does not, a grok turn launches after the operator asked to stop. If it does,
the error reads as a launch failure rather than "not launched: review was
signalled to stop". The duplicated shutdown flags behind this are in
`notes/consolidations.md`.

## BUG-029 - The global lock lives in `TMPDIR`

Reported by: orchestration, and again by a wave fixer. Called the most likely
real-world bug in that scope.

`std::env::temp_dir().join("review.lock")` is computed twice in the app layer
and depends on `TMPDIR`. Two `review` processes with different `TMPDIR` (a
sandboxed agent often has one) do not serialise at all, contrary to the lock's
purpose. The launch locks already live under the data dir. Audit-id minting is
now protected by its own lock on the config file, so the remaining cost is
launch spacing. Proposed: `lock::global()` beside `inflight`'s locks.

## BUG-032 - Stale and misplaced comments

Reported by: orchestration, grok-man, providers, and wave fixers.

- `sessions.rs`: the `kind` doc comments on `SessionRecord` and `SessionEntry`
  say `"oneshot"`, `"session"` or `"prime"`; the values written are `"run"`
  and `"session"`.
- `sessions.rs`: `digest` "Absent for claude (no machine-readable stream)" is
  stale; claude produces digests now.
- `config::RESERVED_NAMES`'s doc comment says to keep it in step with "the
  Reserved words table in CLAUDE.md"; the table is in `docs/config.md` and is
  tested.
- The claude runner's `runtime` parameter comment says claude uses only the
  stop channel, the data root and the two grace periods; it also uses
  `claude_command`, `teardown` and `orphan_log`.
- `app::run` at the global lock still says "Re-read under the lock" about the
  audit id; `ensure_audit_id` now takes its own lock on the config file, so the
  global lock is no longer what makes it safe.

## BUG-045 - An empty audit id gives a garbled mismatch message

Reported by: a wave reviewer (lateral).

`message_target` compares `cfg.audit.id` with the session record before
`ensure_audit_id` runs, so a project whose file holds `id = ""` reports a
mismatch with an empty id in the text rather than a clear message.

## BUG-047 - An empty legacy agent message counts as a codex final answer

Reported by: a wave fixer (lateral).

`transcript::final_answer_text` accepts an empty legacy `agent_message` as an
answer but rejects empty assistant response-item text. Since the watchdog's
Stranded kill now carries the answer it found, an empty legacy message can
license killing codex with nothing useful to recover.

## BUG-048 - A custom grok profile named `review-ws-*` is taken for a generated one

Reported by: a wave fixer (lateral), and originally the grok-man hunter.

Grok sandbox names beginning `review-ws-` still enter the generated-profile
path and must pass its hash check, so an operator-defined grok profile that
happens to use that prefix is treated as one of `review`'s and is refused on
resume when its roots do not hash to the name.

## BUG-050 - Claude cost is now summed across result events, unverified

Reported by: a wave reviewer (lateral).

The claude runner now unions served models and sums `total_cost_usd` across
every result event in a stream. If claude ever emits a result whose
`total_cost_usd` is cumulative for the session rather than for that result,
this double counts. One `claude -p` invocation normally emits a single result,
so this is unverified rather than known wrong; a probe of a multi-result run
(a background subagent wake) would settle it.

## BUG-051 - Env-derived writable roots spelled with `..` are now refused

Reported by: a wave reconciler (lateral).

The `..` check in `push_root` now runs on the spelled path. The `./target`
symlink is canonicalized at derivation so it is unaffected, but
`$CARGO_TARGET_DIR` and `$CARGO_HOME` are not: an exported value spelled with
`..` (for example `/media/disk/x/../cargo`) is now refused with a warning where
it used to be granted. Unlikely in practice and arguably correct for a mangled
variable, but a behaviour change; if wanted, canonicalize env-derived roots at
derivation the same way.

## BUG-049 - The writable-roots test host does not canonicalize like the real one

Reported by: a wave fixer (lateral).

`FakeHost`'s default canonicalizer returns a path unchanged, so it does not
model how real canonicalization removes `..` from existing paths. That is why
the existing parent-component test did not expose the ordering hole just
fixed in `push_root`, and other path filters are tested against the same
unrealistic host.
