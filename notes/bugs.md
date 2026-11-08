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

## BUG-053 - Exec-status EOF can also mean pre-exec death

Verified while reviewing the keeper launch handshake. In
src/keeper/runtime/launch.rs, a successful exec closes the CLOEXEC status pipe;
read_exec_status treats EOF as Spawned. A child killed externally between fork
and exec also closes that pipe without writing errno. This predates the keeper
protocol rewrite. Retrying interrupted errno writes does not distinguish these
two cases. Establish the intended startup-report contract and add a controlled
pre-exec-death regression before claiming EOF proves exec success. Provider wait
status and teardown still follow; this is the launch acknowledgement, not a
claim that containment loses the child.

## BUG-054 - Opaque audit ids are interpolated into directory paths

Verified in src/data.rs (previously audit::audit_dir) and config_write's
configured_audit_id. Existing config accepts any nonempty string unchanged.
An id such as `x/../../elsewhere` adds traversal components to the historical
`<hostname>-<project>-<id>` audit directory. AuditId deliberately guarantees
nonemptiness only, preserving accepted operator config and identity comparisons.
Choose a safe path encoding or containment policy with an explicit plan for
existing audit locations; a generated-hex-only validator would reject existing
valid operator ids and does not handle migration. This predates the data owner
move and remains unfixed; sessions and identity text must remain readable.
