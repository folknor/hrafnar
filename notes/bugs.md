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

## BUG-001 - `interrupt_cmd` decides owner liveness by bare pid and can hang

Reported by: lifecycle.

`inflight::read_live` decides whether the owning `review` is alive with
`identity_alive(marker.pid, marker.pid_start)` (pidfd plus start time).
`interrupt_cmd::interrupt` answers the same question in both of its wait loops
with `inflight::pid_alive(marker.pid)`, which is `kill(pid, 0)`. If the owner
dies and its pid is reused while the interrupt waits, `pid_alive` stays true.
The first loop (`while interrupt_pending`) never ends because nobody consumes
the request; the second never ends because no rows come and the "owner exited"
bail never fires. `review message` calls the same function, so a message can
hang behind it forever. Fix proposed: one liveness predicate per recorded
identity; `pid_alive` should not be public. The single-owner design is in
`notes/consolidations.md` under the process-identity question.

## BUG-002 - `config_write::load_doc` can wipe a project config

Reported by: config.

`load_doc` does `read_to_string(..).unwrap_or_default()`, so any read error
(permissions, EIO, a non-UTF-8 byte) becomes an empty document, and
`append_audit_id` then writes a file holding only `[_audit] id = ...` over the
original. The comment says "or an empty doc if missing", but every failure is
treated as missing. Only `NotFound` should map to empty. The hunter notes that
`load` has just parsed the file, so a read failure means something changed in
between, but the fallback is the wrong default for a writer. No test covers the
read-error path.

## BUG-003 - Concurrent first runs can mint two audit ids

Reported by: config, orchestration.

The fan-out in `app` checks `cfg.audit.id` "after lock to prevent races", but
`cfg` was loaded before the global lock was taken. Two concurrent first runs in
a project without an id both see `None`, both mint, and the second
`append_audit_id` overwrites the first. The first run's audit and sidecar rows
are then filed under an id the project no longer has, and `message_target`'s
audit-id check refuses `review message` for that session ("not the project the
session was started in"). The resume path (`run_session_resume`) mints after
dropping the global lock and ignores the write error with `let _ =`, so its
window is wider and its failure silent; it also does not update `cfg`. Two
concurrent resumes of an id-less project can each append a different id.

Fix proposed: re-read the file under the lock, or make the mint a
compare-and-set in `config_write` (`ensure_audit_id(path) -> AuditId`: read,
return an existing id, else write), called under the lock by both paths.

## BUG-004 - Weak and fallback-constant identities

Reported by: config.

`generate_short_id` is 16 bits and falls back to the literal `4242` if
`/dev/urandom` cannot be opened. `generate_uuid`'s fallback leaves `buf` zeroed
if the open fails, minting the same "v4" UUID every time. The audit id is now a
project identity that `review message` checks (`message_target`), so a
1-in-65536 collision between two projects at the same recycled path is exactly
the case that check exists to catch, and it would pass. Proposed: fail hard
when randomness is unavailable, or use a longer id.

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

## BUG-007 - `~user` expansion differs between `project` and `writable_roots`

Reported by: config.

`project::expand` refuses `~other/...` explicitly. `writable_roots::expand`
passes `~other/e` through as a literal string, which then fails the
`is_absolute` check with "ignoring relative writable_roots entry", a misleading
message. The hunter separates policy from grammar: erroring in one and warning
in the other is legitimate, while disagreeing on what `~user` means is not.
The expansion question as a whole is in `notes/consolidations.md`.

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

## BUG-009 - `find_transcript_path` gives up on the first unreadable directory

Reported by: forensics.

`let entries = std::fs::read_dir(&dir).ok()?;` sits inside the DFS loop. One
unreadable or concurrently removed subdirectory anywhere under `sessions/`
makes the whole lookup return `None`. Silent fallout: the watchdog never fires;
a resume gets `rollout_baseline = None`, so recovery and the watchdog are
disabled with a misleading "could not read the rollout" warning; `review
sessions <id>` reports "not found". Proposed: `continue`, keeping `None` only
for a missing root.

## BUG-010 - The watchdog re-walks the whole sessions tree on every poll

Reported by: forensics.

`wait_for_stranded_completion` calls `find_transcript_path` every
`poll_interval` for the whole run, even after the path is known: an unbounded
recursive `read_dir` over every session codex has written on the host, so the
cost grows with the operator's history. The rollout name is stable for a
session, and the baseline lookup in `run_codex_json` already did the same walk
before spawn and could hand the path in. Proposed: cache the path once found.

## BUG-011 - `incident::scan_transcript` is unscoped, phantom-blind, and a third definition of "answered"

Reported by: forensics.

- It reads the whole rollout (`read_to_string`, no `slice_from_offset`), where
  the rest of the forensics scope reads to the run's bytes.
- It resets `final_answer_present` on `task_started` but knows nothing of the
  phantom shutdown turn that `transcript::parse` rolls back. A clean session
  followed by codex 0.147.0's teardown phantom reads as
  `final_answer_present = None` ("suspicious") in `review incidents`, where
  `transcript::parse` says complete.
- It defines "answered" as `task_complete.last_agent_message != null`,
  alongside `transcript::final_answer_text` (phase-tagged) and the `-o` file.
  A commentary-only turn whose `last_agent_message` is the commentary reads
  "completed" here and "no final answer" everywhere else.
- `read_to_string` fails outright on invalid UTF-8 where the other readers are
  lossy, so a bundle can lack the transcript tail for a rollout the run parsed
  fine.
- `transcript.tail.jsonl` is unscoped too; on a resume the tail can be
  entirely previous turns.

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

## BUG-013 - `transcript::parse` collides id-less tool calls and mislabels tool names

Reported by: forensics.

A `_call` without `call_id` is stored under `""`, so every later id-less output
retires all of them, and two id-less calls collide. `name` falls back to the
payload type (`pt`), so "the tool name" is sometimes a tool name and sometimes
an event type.

## BUG-014 - `incident::write_bundle` stamps two different times

Reported by: forensics.

`write_bundle` calls `chrono_now()` twice, for the directory stamp and for
`meta.timestamp`. They can straddle a second boundary, so the directory name
and `meta.json` disagree.

## BUG-015 - `incident::list_recent` trusts directory names and lists foreign directories

Reported by: forensics.

`list_recent` relies on directory names sorting chronologically and lists any
directory under `incidents/`, including foreign ones. It always uses the real
location, which the hunter calls fine for the operator view but untestable
without touching the operator's home.

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

## BUG-019 - `review`'s own diagnoses are reported as provider turn failures

Reported by: providers.

`wake_failure_digest` and the post-answer status digests put `review`'s own
diagnosis into `turn_error`: "claude exited non-zero after producing an
answer", "grok's exit status is unknown", "could not verify grok's background
results", "wake-up turn ... failed", "still had unread background results".
These print as `turn failed: ...`, `review incidents` lists them as `turn
failed:`, and `app::stated_failure` reads them as "the provider stated a
reason, do not retry". Separately, `wake_failure_digest` turns a run that had
an answer into `captured: false`, so the run exits 1 while its answer is
printed beneath "note: no conclusion was produced; the text below is not an
answer", which is false: it is an answer whose background results were not
verified. `wake_failure_digest` also asserts `exit_code: Some(1)`, an exit
nothing produced; the comment says so, but the field cannot tell asserted from
observed. The typed-outcome proposal that would separate these is in
`notes/structural.md`.

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

## BUG-022 - `claude_usage` reads only the last result line

Reported by: providers.

With multiple result objects in one stream (the background-subagent case the
comments describe), earlier turns' usage is lost. Moot while background tasks
are disabled, but a trap for maki, whose stream may differ.

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

## BUG-025 - An uncanonical `GROK_HOME` bypasses the grok-worktree refusal

Reported by: grok-man.

`check_trust` canonicalizes `dir` but compares it with
`self.home.join("worktrees")` uncanonicalized. `Grok::resolve` only requires
`home` to be absolute. `GROK_HOME=/home/u/./.grok`, a trailing `..`, or a home
under a symlinked ancestor (allowed: the symlink test is only on the trust
file path, and only on existing components) makes `starts_with` false, and the
run proceeds to compute a `workspace_key` the docstring says cannot be
modelled: exactly the case where `--trust` could land on a declined source
repo. Proposed: canonicalize `home` once in `resolve` (refusing if that fails
for an existing path), as `user_home` already is.

## BUG-026 - The wake prompt points at subagent results instead of delivering them

Reported by: grok-man.

`wake_prompt`'s doc says results are "in the prompt, not pointed at", because
a model may end its turn without reading them. The `Unread::Subagent` arm does
the opposite: "Read its result with get_command_or_subagent_output." After that
wake turn ends cleanly, the subagent completion precedes a real turn end, so
`unread_background_results` reports nothing and the guarantee closes without
the model having seen anything. Either `updates.jsonl`'s `subagent_finished`
carries the result text (then put it in the prompt), or the guarantee for
subagents is "told", not "delivered", and the docs should say so. The hunter
suggests checking `research/grok-build`.

## BUG-027 - A grok write run announces a trust grant grok will refuse

Reported by: grok-man.

`decision` returns `Undecided` for an unsafe root key (for example a project at
`$HOME` with no repo), so `prepare_grok_write` announces a permanent trust
grant that grok (per `is_unsafe_trust_root`) will refuse. The post-run landed
check catches it, but only after a write run whose edits were all cancelled.
Proposed: refuse up front ("grok never trusts $HOME").

## BUG-028 - `cancel_cause` blames an early refusal for a later turn end

Reported by: grok-man. Called low impact.

`cancel_cause` keeps the last refusal before `turn_ended` but does not reset
`refused_tool` at a later allowed permission, so a refusal early in a turn that
later ends for `max_turns` is blamed on the tool.

## BUG-029 - The global lock lives in `TMPDIR`

Reported by: orchestration. Called the most likely real-world bug in that
scope.

`std::env::temp_dir().join("review.lock")` is computed twice in `app.rs` and
depends on `TMPDIR`. Two `review` processes with different `TMPDIR` (a
sandboxed agent often has one) do not serialise at all, contrary to the lock's
purpose. The launch locks already live under the data dir. Proposed:
`lock::global()` beside `inflight`'s locks.

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

Reported by: orchestration, grok-man, providers.

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

## BUG-033 - Grok submodule checkouts are untested in `main_checkout_of_worktree`

Reported by: grok-man.

`main_checkout_of_worktree` handles `.git` files without the `/worktrees/`
shape (submodules: `gitdir: ../.git/modules/x`) by returning `None`, keying on
the submodule root. The hunter thinks this probably matches grok and calls it
worth one test.
