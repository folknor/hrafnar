# Structural

Shape and placement findings from the design hunt over `review` (scopes:
providers, lifecycle, config, orchestration, forensics, grok-man): axes that
should be types, and the moves, splits and rewrites, each with the payoff the
hunters claim for it. Unverified.

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

Every scope's hunter reached the same three proposals independently: a
`Provider` enum with a capability table, a typed run outcome, and a single run
lifecycle that per-provider adapters plug into. The config hunter recommends
doing the enum, the sandbox level and the capability table as one change
before adding maki; the providers hunter calls for a rewrite around all three.

## Types

## STR-011 - Typed refusals for `interrupt` and `message`

Reported by: lifecycle, orchestration.

`interrupt_cmd::Outcome { interrupted: bool, project: String, provider:
String }` is derived by scanning sidecar rows after a count baseline, with a
"look one more time" heuristic for auto-resume pairs. Its failures (owner
gone, keeper gone, no pid in time, not interruptible, cleanup pending, queued)
are `bail!` prose, so `review message` can only branch by string. The message
and resume refusals (no record, retired provider, stale, wrong project, audit
mismatch, cleanup pending, not interruptible, not installed) all exit 1,
indistinguishable from "the turn produced no answer": an orchestrator cannot
tell "start fresh" (stale) from "wait" (grok in flight, cleanup pending) from
"wrong id". The same holds for the fan-out's pre-launch refusals. Proposed:
`InterruptError { NotInFlight, Queued, CleanupPending, NotInterruptible(Provider),
NeverReady, KeeperGone, OwnerDiedUnrecorded }` and
`Outcome = Interrupted { row } | FinishedFirst { row }` (letting `run_message`
proceed on `KeeperGone`, "finishing on its own", rather than fail); a
`MessageRefusal` enum with a distinct exit code per actionable class, or at
least one for "refused before sending" vs "sent, no answer".

## Moves and rewrites

## STR-030 - Consider moving toolchain root facts to config (new feature)

Reported by: config. Derivation and adapter rendering are now separated. The
remaining proposal is a new feature, outside the type/owner consolidation:
`writable_roots` derives brokkr (`$HOME/.brokkr`, `$XDG_DATA_HOME/brokkr`), cargo
and LFS facts in code. These are deterministic host contracts, but also the one
place toolchain-specific sandbox policy lives in the binary. A global
`[_sandbox].derive = [...]` or named derivers selected in config would make that
policy configurable. No config surface or behavioral change is implemented.

## STR-037 - Requirements the new providers bring that no current runner has

Reported by: providers, lifecycle. Narrowed: env ordering is explicit
(`ProcessSpec` layers adapter defaults, then profile, then adapter-forced
values, so opencode's isolation goes in the forced layer and a profile cannot
undo it), `PWD` is set to the working directory for every provider, readiness
is a lease fact the adapter's reader sets (maki after `init`, opencode after
its first event), and `Ended.status` is documented as forensics only.

What remains, each landing with its provider:
- opencode's `--standalone` must be non-optional, because without it the run
  attaches to a background service outside the keeper's tree, whose "clean"
  verdict would then be true but meaningless. Nothing in the lifecycle can
  detect a violation: encode it in the opencode argv builder with a test.
- opencode's read-only is an agent definition: a third permission mechanism
  beside an OS sandbox and none.
- Effort via maki's config still needs its actual per-run config implementation
  when maki lands. The adapter `prepare` operation and owned prepared context
  now exist; no maki configuration or permission mechanism is implemented.
