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

## BUG-017 - The incident replay command omits runner-injected env

Reported by: forensics.

`replay_command` records only the profile env. Anything the runner injects
never reaches the replay; `RUST_BACKTRACE` is hard-coded on both sides by
agreement. For opencode, whose isolation is entirely runner-injected env
(disabled project config, private DB, `PWD`), a replay built this way would
attach to the operator's real config and DB: not a reproduction, and mildly
dangerous. The proposed fix, building the replay from the actual spawned
`Command`, is in `notes/structural.md` under the incident split.

## BUG-032 - Stale and misplaced comments

Reported by: orchestration, grok-man, providers, and wave fixers.

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

## BUG-050 - Claude cost is now summed across result events, unverified

Reported by: a wave reviewer (lateral).

The claude runner now unions served models and sums `total_cost_usd` across
every result event in a stream. If claude ever emits a result whose
`total_cost_usd` is cumulative for the session rather than for that result,
this double counts. One `claude -p` invocation normally emits a single result,
so this is unverified rather than known wrong; a probe of a multi-result run
(a background subagent wake) would settle it. Not a wave item: it needs a real
provider run.

## BUG-055 - Small leftovers from the session-log and config changes

Reported by: a wave reviewer (lateral). Cosmetic.

- `src/config/resolve.rs`: the comment added about group and archetype name
  clashes ends in a blank line and is not attached to the loop it describes.
- `sessions::record` (taking a `&'static str` kind) is now a thin wrapper over
  `record_with_kind`; only `app/message.rs` still calls it, so it could pass
  `SessionKind::Resume` and the wrapper could go.

## BUG-053 - A fresh in-flight turn reads "since now"

Reported by: a wave reconciler (lateral). Cosmetic.

`review sessions` prints "turn in flight since <dur>" through
`sessions::format_age`, which returns "now" under 60 seconds, so a turn that
just started reads "turn in flight since now".
