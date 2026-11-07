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

## BUG-005 - `-a all` includes `bare`

Reported by: config.

`all` expands to every archetype, and most configs carry `bare = ""`, so
`-a all` launches an extra unprimed run per provider. The hunter calls this
probably unintended: `all` should mean every priming archetype. The wider
question of who owns `bare` is in `notes/consolidations.md`.

## BUG-006 - `bare` behaves differently as a positional and as `-a`

Reported by: config.

The legacy positional `review bare` means "no archetype" whatever the config
holds, but `-a bare` only works when the config defines `bare = ""`; without
it, `-a bare` is "not found". The parser forbids a non-empty `bare` to keep
the two in step. Proposed: resolve `bare` in the archetype resolver regardless
of config, so the config entry is no longer needed.

## BUG-008 - A Stranded watchdog kill can be reported as a death and auto-resumed

Reported by: forensics.

`watchdog::has_final_answer` asks whether any event since the baseline was a
`final_answer`, deliberately, to survive the trailing micro-turns codex
sometimes appends (each with its own `task_complete`). The watchdog kills codex
on that basis (`Kind::Stranded`). Recovery then runs
`transcript::summarize_session`, i.e. `transcript::parse`, which resets
per-turn state at every `task_started`. If the trailing micro-turn has a
`user_message`, reasoning, `task_complete` or a tool call,
`turn_produced_content` is true, so no phantom rollback happens and
`cur.final_answer` is `None`. `recovered_from_transcript` is false, `captured`
is false (killed before `-o`), `app::died_without_answer` is true, and
`after_run` returns `AutoResume`. The run killed because it had the answer is
reported as having none, and a paid resume is launched.

Whatever licensed the kill must be what the report recovers. Proposed: the
watchdog verdict carries the answer text (or the byte range of the event that
held it) and recovery prefers it; or one rollout fold returns both "last-turn
state" and "last final_answer since baseline", and recovery takes the latter
when the terminator is `Stranded`. Suggested test: a rollout with a
substantive turn ending in `final_answer` + `task_complete`, then a
`task_started` / `user_message` / `task_complete` micro-turn, through the stub
harness to a Stranded kill.

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
commentary-only turn reads as completed). The fixer's proposed route:
`provider.rs` already computes `recovery_scope`; pass that offset into
`Incident`, then scope the excerpt with `transcript::slice_from_offset` and
classify with the existing scoped transcript summary. Comments at the bundle
read and scanner sites record the limitation.

## BUG-012 - A custom `CODEX_HOME` blinds resume forensics and `review sessions <id>`

Reported by: forensics, providers.

Only env var names are recorded, so `CODEX_HOME` set by a profile is lost after
the run. The resume path passes `env: None, config: &[]`, so the rollout
baseline and transcript lookup use the default `CODEX_HOME`: a session created
under a custom home resumes against the wrong one, the baseline is unreadable,
and the watchdog and recovery are disabled with a warning (or worse, per the
providers hunter). `run_session_show` passes `override_home = None` for the
same reason and cannot find the rollout. The run itself knew the path
(`TranscriptSummary.path`, or the watchdog's lookup), but it is not persisted
in `DigestSummary`, and on a clean run no transcript is read at all. Proposed:
persist the resolved rollout path in the sidecar row. The providers hunter
notes not recording env values is a documented choice, and calls this a silent
cross-axis coupling: the env decides where forensics look.

## BUG-016 - Stub tests probably append to the operator's real audit log

Reported by: forensics.

The data directory is resolved independently in `incident.rs`, `inflight.rs`,
`audit.rs`, `sessions.rs` and `provider.rs`, and only some honour the
`data_root` test override. `audit::audit_dir` has none, so the stub tests very
likely append to the operator's real audit log. The hunter says this is worth
checking. The data-dir question as a whole is in `notes/consolidations.md`.

## BUG-017 - The incident replay command omits runner-injected env

Reported by: forensics.

`replay_command` records only the profile env. Anything the runner injects
never reaches the replay; `RUST_BACKTRACE` is hard-coded on both sides by
agreement. For opencode, whose isolation is entirely runner-injected env
(disabled project config, private DB, `PWD`), a replay built this way would
attach to the operator's real config and DB: not a reproduction, and mildly
dangerous. The proposed fix, building the replay from the actual spawned
`Command`, is in `notes/structural.md` under the incident split.

## BUG-018 - An interrupted claude or grok resume prints no resume hint

Reported by: providers.

`print_result` prints the "interrupted - continue within Nm" hint only when
`result.session_id` is `Some`. The claude and grok runners return
`session_id: None` on a resume (`keep_id` returns `None` when not oneshot);
codex's returns the known id. So an interrupted `review message <claude-id>`
prints no `session:` line and no hint, while the same flow on codex prints
both. The `RunOutput.session_id` contract ("new information only" vs "the
session") differs between runners.

## BUG-020 - The grok runner ignores the stop channel

Reported by: providers. Not verified by the hunter.

`*runtime.stop.borrow()` is checked before spawn in `run_claude` and
`run_codex_json`. `run_grok_turn` does not check it, and the wake loop ignores
`stop` too, so a grok run still waiting on its stagger when `review` is
signalled will try to launch, and the wake loop will try to launch wake turns.
Whether `contained::spawn` refuses after `terminate_all` was not verified. If
it does not, a grok turn launches after the operator asked to stop. If it does,
the error reads as a launch failure rather than "not launched: review was
signalled to stop". The duplicated shutdown flags behind this are in
`notes/consolidations.md`.

## BUG-021 - `config_writable_roots_override` can record roots codex did not use

Reported by: providers.

- It picks the last entry with `starts_with("sandbox_workspace_write.writable_roots")`.
  A longer key with that prefix is picked, fails the nested lookup and returns
  `None`, so an earlier real override is ignored.
- A value codex accepts but the TOML parser rejects also returns `None`; the
  record then shows the derived roots while codex ran with the override.

That is the false-record class the surrounding comment says it prevents.
Proposed: stop parsing it back; keep the override as the typed profile field
(`writable_roots` already exists) and forbid restating it in `config`.

## BUG-023 - Grok write runs are recorded under grok-native sandbox names

Reported by: grok-man, orchestration, config, providers.

`invoke` translates the level first (`sandbox_for` shadows `sandbox`), swaps in
the profile name (`grok_profile.or(sandbox)`), then records
`effective_sandbox(provider, sandbox)`. A grok write run is recorded as
`review-ws-<hash>` (or `workspace` for an old session), and grok
`danger-full-access` as `none`, while codex records `workspace-write`. So
`jq 'select(.sandbox=="workspace-write")'`, the query the `effective_sandbox`
doc comment cites as its reason to exist, misses every grok write run. Resume
works because of this: the recorded profile name flows back, passes
`sandbox_for` unchanged (unknown-name pass-through, idempotent on its own
outputs), and `grok_write_launch` recognises it by `PROFILE_PREFIX`. One string
field carries a `review` level, a native level and a grok profile name,
depending on provider and on fresh vs resume. Any operator-defined grok profile
starting with `review-ws-` is taken for one of `review`'s, and the resume path
demands recorded roots hash to it. Proposed by grok-man: the record carries
`level` in `review`'s vocabulary plus an optional `provider_sandbox`, so the
query works and resume still has the profile name. The type design is in
`notes/structural.md`.

## BUG-024 - Two session-id allowlists disagree on `_`

Reported by: grok-man, lifecycle, providers, orchestration, forensics.

`inflight` accepts `[A-Za-z0-9_-]`; `grok_home::session_file` accepts
`[A-Za-z0-9-]` and silently returns `None` otherwise. opencode's ids are
`ses_<...>`: a reader modelled on `session_file` would fail every opencode
lookup, as "no log" rather than "invalid id". For grok, `None` from
`session_updates` becomes a wake-verification failure ("no update log for grok
session"), so a rejected id reads as a missing file. Within `inflight` the
check also runs three separate times (mark, launch-lock path, interrupt path),
each degrading differently: a skipped marker with a warning, `Ok(None)`, and an
`InvalidInput` error. The `SessionId` newtype proposed as the single owner is
in `notes/structural.md`.

## BUG-027 - A grok write run announces a trust grant grok will refuse

Reported by: grok-man.

`decision` returns `Undecided` for an unsafe root key (for example a project at
`$HOME` with no repo), so `prepare_grok_write` announces a permanent trust
grant that grok (per `is_unsafe_trust_root`) will refuse. The post-run landed
check catches it, but only after a write run whose edits were all cancelled.
Proposed: refuse up front ("grok never trusts $HOME").

## BUG-029 - The global lock lives in `TMPDIR`

Reported by: orchestration, and again by a wave fixer. Called the most likely
real-world bug in that scope.

`std::env::temp_dir().join("review.lock")` is computed twice in `app.rs` and
depends on `TMPDIR`. Two `review` processes with different `TMPDIR` (a
sandboxed agent often has one) do not serialise at all, contrary to the lock's
purpose. The launch locks already live under the data dir. Audit-id minting is
now protected by its own lock on the config file, so the remaining cost is
launch spacing. Proposed: `lock::global()` beside `inflight`'s locks.

## BUG-030 - A panicked fan-out task is recorded as provider "unknown"

Reported by: orchestration.

The panic path records `provider: "unknown"`, although the provider name is
available in the pending entry and could be kept.

## BUG-031 - A keeper interrupt outside `Running` is dropped silently

Reported by: lifecycle.

`Keeper::interrupt` ignores `i`/SIGINT unless `phase == Running`. A SIGINT
during `Starting` (between fork and exec) is lost, and `review interrupt` then
waits for the run to end on its own and reports "ended before the interrupt
took effect". The runners' readiness gate makes this unlikely today, but the
keeper never reports whether it forwarded anything (its `interrupted` flag
never leaves the process), so `review` cannot tell delivered from dropped. The
"was this run interrupted" consolidation in `notes/consolidations.md` builds on
this.

## BUG-032 - Stale and misplaced comments

Reported by: orchestration, grok-man, providers, and a wave fixer.

- `sessions.rs`: the `kind` doc comments on `SessionRecord` and `SessionEntry`
  say `"oneshot"`, `"session"` or `"prime"`; the values written are `"run"`
  and `"session"`.
- `sessions.rs`: `digest` "Absent for claude (no machine-readable stream)" is
  stale; claude produces digests now.
- `config::RESERVED_NAMES`'s doc comment says to keep it in step with "the
  Reserved words table in CLAUDE.md"; the table is in `docs/config.md` and is
  tested.
- The doc for `config_writable_roots_override` ("The writable roots a profile
  `config` entry sets...") sits directly above `announcement_lines`, whose own
  doc follows; the override function itself has no doc.
- `run_claude`'s `runtime` parameter comment says claude uses only the stop
  channel, the data root and the two grace periods; it also uses
  `claude_command`, `teardown` and `orphan_log`.
- `incident.rs` describes its directory suffix from `generate_short_id` as a
  "short random suffix"; audit ids now use 128 random bits, so the suffix is 32
  hex characters. (The reconciler rewrote this one; check before acting.)
- `app.rs` at the global lock still says "Re-read under the lock" about the
  audit id; `ensure_audit_id` now takes its own lock on the config file, so the
  global lock is no longer what makes it safe.

## BUG-033 - Grok submodule checkouts are untested in `main_checkout_of_worktree`

Reported by: grok-man.

`main_checkout_of_worktree` handles `.git` files without the `/worktrees/`
shape (submodules: `gitdir: ../.git/modules/x`) by returning `None`, keying on
the submodule root. The hunter thinks this probably matches grok and calls it
worth one test.

## BUG-034 - The keeper test cleans up with a bare-pid kill that can fall back to pid 0

Reported by: a wave fixer (lateral).

The cleanup in `tests/keeper.rs` signals a detached process by bare pid with
`kill`, with a fallback to pid 0, which would signal the test's own process
group. The fixer replaced it with pidfd signalling, then reverted that with
the rest of a redesign that was out of scope; the bare-pid cleanup is back.

## BUG-035 - Claude result aggregation is partial

Reported by: a wave fixer (lateral).

Usage is now summed across all result events in a claude stream, but the
served model and cost are still read from the last result only; a clean claude
run still records no usage, because it has no digest; and the stream's
head-plus-tail truncation can discard result events from the middle of a long
stream before any of them are read.

## BUG-036 - Missing result fields are still reported as provider turn errors

Reported by: a wave fixer (lateral).

When a claude result lacks `is_error`, or a grok result lacks `stopReason`,
`review` infers a failure and records it in `turn_error`, the provider-stated
field, rather than in the new `review_error` field for `review`'s own
diagnoses.

## BUG-038 - The persisted digest view omits the unconfirmed-exit note

Reported by: a wave reviewer (lateral).

The live `print_digest` prints `note: answer captured; process exit status
could not be confirmed`, but `app::print_digest_summary`, which renders the
persisted row for `review sessions <ID>`, has no matching note, so the
after-the-fact view shows less than the live one.

## BUG-039 - An empty audit id is accepted and never regenerated

Reported by: a wave reviewer (lateral).

`config_write::configured_audit_id`, like the older parse path, treats
`id = ""` as a configured id, so `ensure_audit_id` returns the empty string
instead of minting one. Rows elsewhere use `""` to mean "no audit id".

## BUG-040 - Incident listing's new filter has no test

Reported by: a wave reviewer (lateral).

`incident::list_recent_from` now drops bundles without matching schema-1
metadata, a provider and a UTC timestamp, and sorts by the recorded time. That
is new behaviour with no unit test, though the helper takes an explicit
directory precisely so it can have one.
