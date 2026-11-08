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

## BUG-050 - Claude cost is now summed across result events, unverified

Reported by: a wave reviewer (lateral).

The claude runner now unions served models and sums `total_cost_usd` across
every result event in a stream. If claude ever emits a result whose
`total_cost_usd` is cumulative for the session rather than for that result,
this double counts. One `claude -p` invocation normally emits a single result,
so this is unverified rather than known wrong; a probe of a multi-result run
(a background subagent wake) would settle it. Not a wave item: it needs a real
provider run.

## BUG-052 - Codex rollout suffix lookup does not prove exact session identity

Reported by: app/config/path spar (lateral), verified in transcript::find_session_path.

The search accepts the first filename ending in `-<session_id>.jsonl`.
Both `foo` and `bar-foo` are valid SessionId values, so looking up `foo`
can select `rollout-date-bar-foo.jsonl`. The type prevents malformed identity
paths but does not make this suffix match unique. Real Codex IDs are currently
UUIDs, which is the assumption the existing search comment makes; executable
identity validation intentionally accepts historical opaque IDs too. Establish
an exact identity check from the rollout metadata or documented filename format
without rejecting old opaque IDs. This link preserves discovery semantics and
does not claim exact matching or symlink confinement.
