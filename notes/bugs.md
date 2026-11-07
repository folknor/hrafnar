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
- `run_claude`'s `runtime` parameter comment says claude uses only the stop
  channel, the data root and the two grace periods; it also uses
  `claude_command`, `teardown` and `orphan_log`.
- `app.rs` at the global lock still says "Re-read under the lock" about the
  audit id; `ensure_audit_id` now takes its own lock on the config file, so the
  global lock is no longer what makes it safe.

## BUG-035 - Claude result aggregation is partial

Reported by: a wave fixer (lateral).

Usage is now summed across all result events in a claude stream, but the
served model and cost are still read from the last result only; a clean claude
run still records no usage, because it has no digest; and the stream's
head-plus-tail truncation can discard result events from the middle of a long
stream before any of them are read.

## BUG-038 - The persisted digest view omits the unconfirmed-exit note

Reported by: a wave reviewer (lateral).

The live `print_digest` prints `note: answer captured; process exit status
could not be confirmed`, but `app::print_digest_summary`, which renders the
persisted row for `review sessions <ID>`, has no matching note, so the
after-the-fact view shows less than the live one.

## BUG-041 - The README still describes the old `-a all` and `-a bare`

Reported by: a wave fixer (lateral).

`-a all` now skips the special `bare` entry, and `-a bare` means no archetype
whether or not the config defines it; the CLI help, `docs/run.md` and
`docs/config.md` say so. `README.md` still describes `-a all` as every
archetype, and its archetype option table does not say `-a bare` works without
a config entry.

## BUG-042 - The incident timestamp check accepts impossible dates

Reported by: a wave fixer (lateral).

`incident::is_utc_timestamp`, which decides whether a directory is a real
bundle, checks only the timestamp's shape and its `Z` suffix, so a value such
as `2026-99-99T99:99:99Z` passes.

## BUG-044 - Claude's missing-`is_error` diagnosis still reads as a claude statement

Reported by: a wave reviewer (lateral).

A claude result without `is_error` is now filed as `review_error`, but its text
still reads "claude reported an error (no is_error flag): ...", claiming a
provider statement that was never made. The exact-string assertion in
`claude_result_without_is_error_fails_closed` (`src/provider_tests.rs`) has to
change with it.

## BUG-045 - An empty audit id gives a garbled mismatch message

Reported by: a wave reviewer (lateral).

`message_target` compares `cfg.audit.id` with the session record before
`ensure_audit_id` runs, so a project whose file holds `id = ""` reports a
mismatch with an empty id in the text rather than a clear message.

## BUG-046 - `push_root` checks `..` only after canonicalizing

Reported by: a wave fixer (lateral).

`writable_roots`' `push_root` canonicalizes a root before it checks for `..`,
although its comments say every filter runs against both the spelled and the
resolved path. A spelled `..` is therefore never seen as such.

## BUG-043 - `contained::spawn` records keeper pid 0 when the id is missing

Reported by: a wave fixer (lateral).

`contained::spawn` maps a missing `child.id()` to `0` when storing the keeper
pid. If the keeper has already been reaped at that point, the marker or report
can carry a misleading pid 0.
