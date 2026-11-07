# Hunt: grok-man (`src/grok_home.rs`, `src/man.rs`, `tests/man.rs`)

Read from code only. Callers followed into `src/provider.rs` (`invoke`,
`grok_write_launch`, `prepare_grok_write`, `run_grok`, `run_grok_turn`,
`effective_sandbox`), `src/config.rs` (`sandbox_for`, `KNOWN_PROVIDERS`),
`src/timings.rs` (`stale_session`) and `src/inflight.rs`.

## Headline findings

1. **The sidecar `sandbox` field holds a grok-native string, not `review`'s
   level.** `invoke` translates first (`sandbox_for` shadows `sandbox`), then
   swaps in the profile name (`grok_profile.or(sandbox)`), then records
   `effective_sandbox(provider, sandbox)`. So a grok write run is recorded as
   `review-ws-<hash>` (or `workspace` for an old session), and grok
   `danger-full-access` as `none`, while codex records `workspace-write`. A
   `jq 'select(.sandbox=="workspace-write")'`, the very query the
   `effective_sandbox` doc comment cites as its reason to exist, misses every
   grok write run. The resume path works *because* of this: the recorded
   profile name flows back in, passes through `sandbox_for` unchanged (unknown
   name passthrough) and `grok_write_launch` recognises it by
   `PROFILE_PREFIX`. One string field carries three different things: a
   `review` level, a provider-native level and a grok profile name, and which
   one it is depends on the provider and on whether the row is a fresh run or a
   resume. This is the clearest "axis that should be a type" in scope (see
   below).

2. **Two session-ID allowlists that disagree.** `inflight` accepts
   `[A-Za-z0-9_-]`; `grok_home::session_file` accepts `[A-Za-z0-9-]` and
   silently returns `None` otherwise. The question "is this a safe bare
   session-id path component" is answered twice. They already disagree on `_`,
   and opencode's IDs are `ses_<...>`: if an opencode-side reader is modelled on
   `session_file`, every opencode lookup fails, and fails as "no log" rather
   than "invalid id". In grok's case `None` from `session_updates` is turned
   into a wake-verification failure ("no update log for grok session"), so a
   rejected id reads as a missing file. Owner: one `SessionId` newtype
   (validated at construction, `as_path_component()`), minted where an id
   enters `review` (CLI arg, provider stream, `generate_uuid`), used by
   `inflight`, `grok_home`, `transcript`, `sessions`.

3. **The grok-managed-worktree refusal can be bypassed by an uncanonical
   `GROK_HOME`.** `check_trust` canonicalizes `dir` but compares it with
   `self.home.join("worktrees")` uncanonicalized. `Grok::resolve` only requires
   `home` to be absolute; `GROK_HOME=/home/u/./.grok`, a trailing `..`
   segment, or a home under a symlinked ancestor (allowed here: the symlink
   test is only on the trust file path, and only on the components that
   exist) makes `starts_with` false, and the run proceeds to compute a
   `workspace_key` the docstring says cannot be modelled, which is exactly the
   case where `--trust` could land on a declined source repo. Fix: canonicalize
   `home` once in `resolve` (and refuse if that fails for an existing path), so
   every comparison against it is canonical. `user_home` is already
   canonicalized there; `home` should be too.

4. **Two readers of the trust store with different rules for "readable".**
   `check_trust` refuses a symlinked store and errors on a malformed one;
   `untrusted_hint` reads through a symlink and treats malformed as "no hint".
   Grok refuses the symlinked store, so for grok it trusts nothing, which is
   exactly when `untrusted_hint` should blame trust and does not... but it does
   happen to blame it via `decision` if the symlinked file parses. The two
   functions each re-decide "what does grok think of this store". Owner: one
   `fn load_store(&self) -> StoreState` (`Missing`, `Unloadable(reason)`,
   `Loaded(TrustDocument)`) with `check_trust` and `untrusted_hint` both
   mapping from it. The post-run "did the grant land" check in `invoke` is a
   third caller (it calls `check_trust` and pattern-matches `Ok(Trusted)`),
   which also means a store that became Declined between launch and the
   re-check prints "did not durably trust" instead of the truth.

5. **The wake prompt does not deliver subagent results, contrary to its own
   contract.** `wake_prompt`'s doc says results are "in the prompt, not pointed
   at", because a model may end its turn without reading them. The
   `Unread::Subagent` arm does the opposite: "Read its result with
   get_command_or_subagent_output." After that wake turn ends cleanly, the
   subagent completion precedes a real turn end, so `unread_background_results`
   reports nothing and the guarantee closes without the model having seen
   anything. Either the `updates.jsonl` `subagent_finished` event carries the
   result text (then put it in), or the guarantee for subagents is "told",
   not "delivered", and the docs should say so. Worth checking against
   `research/grok-build`.

## 1. Axes that should be types

- **Sandbox level / native sandbox / grok profile name** (finding 1). Today:
  `Option<&str>` in, `String` out of `sandbox_for`, then string compares on
  native values downstream (`grok_write_launch` matches `"workspace"`,
  `invoke`'s `derives` matches `"workspace-write"` for codex,
  `effective_sandbox` defaults to `"read-only"`). Proposed:
  `enum Level { ReadOnly, WorkspaceWrite, DangerFullAccess }` parsed at config
  load; `enum Sandbox { Level(Level), Custom(String) }` for the passthrough;
  each provider adapter owns `fn native(&Sandbox) -> NativeSandbox`; and
  `enum GrokSandbox { Builtin(Level), ReviewProfile(ProfileName), Custom(String) }`.
  The record should carry `level: Level` plus an optional
  `provider_sandbox: String` so the jq query works and resume still has the
  profile name. `grok_write_launch` then pattern-matches a type rather than a
  prefix: today any operator-defined grok profile that happens to start with
  `review-ws-` is taken for one of ours and the resume path demands recorded
  roots hash to it.
- **Root set.** `Vec<String>` everywhere, canonicalized by calling
  `canonical_roots` in three places (`invoke`, `profile_name`, `merge_profile`).
  The comment on `canonical_roots` says the name and the record "both use it,
  so a resume can check one against the other" - a property held only because
  every site remembers. A `RootSet` (sorted, deduped by construction, maybe
  `PathBuf`s) would make `profile_name(&RootSet)` total and drop the re-sort
  in `merge_profile`. `ProfileName` as a newtype minted only by
  `RootSet::profile_name()` (or by parsing a recorded name that has the prefix)
  would remove the `String` that `ensure_profile(name, roots)` takes alongside
  the roots it should be derived from: today a caller can pass a name for a
  different set and `merge_profile` would happily create it.
- **`Trust` vs grant.** `GrokPrepared.trust_grant: Option<String>` is the
  workspace key lossily stringified; the same key is computed by
  `workspace_key` again in `check_trust`'s error, in `untrusted_hint`, and in
  `prepare_grok_write`. A `TrustKey(PathBuf)` returned with the decision
  (`check_trust -> (TrustKey, Trust)`) would compute it once.
- **Cancel cause** is returned as a formatted `String`
  (`"permission_cancelled: permission for `x` was refused"`) and then fed into
  `explain_grok_no_answer`. A struct `{ category: String, refused_tool:
  Option<String> }` would let the digest carry the category as a field
  (greppable, branchable: `permission_cancelled` vs `max_turns` call for
  different moves) instead of prose.
- **`unread_background_results` returns `Result<_, String>`** while the rest of
  the module is `anyhow`; and errors from `session_updates` (`None`) are
  folded into a different string in `run_grok`. A typed
  `enum WakeCheck { Clean, Unread(Vec<Unread>), Unverifiable(Reason) }` would
  replace three ad-hoc `wake_failure_digest(reason)` sites.
- **`Unread::id()` allocates a `String` and encodes the kind in a
  `subagent:` prefix**; the same prefix is minted independently in the
  `subagent_spawned` arm (`format!("subagent:{id}")`) so started/finished
  matching depends on two format strings agreeing. An `enum WorkId { Task(String),
  Subagent(String) }` with `Ord` removes that.
- **Positional bags in `Unread::Finished`**: `exit_code: Option<i64>, signal:
  Option<String>` encode "exit status" with an illegal both-set state; the
  wake prompt resolves it with exit-code-wins. `enum Exit { Code(i64),
  Signal(String), Unknown }`.
- **man topics: `name` is a `&str` looked up by string** and the doc tests
  reach into topics by literal name (`topic("message")`). Fine at eight topics;
  not worth a type.

## 2. Decisions made in more than one place

- **"Does this provider have a filesystem sandbox?"** `effective_sandbox`
  (`"codex" | "grok"`), the `match` arms in `invoke` (comment: claude ignores
  it), `sandbox_for` (only grok translates), the writable-roots applicability
  rule in `invoke` (`derives`: codex workspace-write or grok fresh) and its
  warning text, `config_cmd.rs` (a hard-coded `[("claude", true), ("codex",
  codex), ("grok", false)]` table, worth checking what it decides), and the
  man sandbox-table test, which hard-codes its column set
  `| codex | grok | claude |` and tests claude by `effective_sandbox(...).is_none()`.
  maki and opencode both have no filesystem sandbox, so each will need an edit
  in every one of these, and a missed one (e.g. `effective_sandbox` defaulting
  `_ => None`) passes silently. Owner: a per-provider capability descriptor
  (`struct ProviderSpec { fs_sandbox: bool, native_sandbox: fn(Level) -> ...,
  stale_cutoff, interruptible, ... }`) in one registry that `KNOWN_PROVIDERS`
  becomes a projection of. The man test should then iterate the registry and
  build the expected column set from it, rather than hard-coding three columns.
- **Stale cutoff per provider**: `timings::stale_session` matches on a string
  with `_ => STALE_SESSION`; the man test iterates `KNOWN_PROVIDERS` and calls
  it, so it is a genuine single owner, but the catch-all means a new provider
  silently inherits 55 min. Moving it onto the provider spec forces a decision.
- **Provider list in the docs vs code**: the stale-cutoff test derives from
  `KNOWN_PROVIDERS` (good: one owner, test is a check); the sandbox test does
  not (bad: adding maki to `KNOWN_PROVIDERS` does not fail it, because it only
  checks rows it finds). Same for the level list: the test hard-codes
  `["read-only", "workspace-write", "danger-full-access"]`, a third copy of the
  level vocabulary next to `sandbox_for`'s match arms and the
  "reserved level names" prose; a `Level` enum with `ALL` would own it.
- **Session-id safety** (headline 2).
- **What grok thinks of its trust store** (headline 4).
- **Whether a grok sandbox string is one of our profiles**: decided by prefix
  in `grok_write_launch`, by construction in `profile_name`, and by content
  match in `merge_profile` and `prepare_grok_write` (`profile_name(roots) !=
  *name`). The prefix check is the weakest and is the one that routes.
- **Synthetic-turn classification** (`SYNTHETIC_TURN_PREFIXES`) is
  grok-specific and single-owned; fine.
- **`RESERVED_NAMES` table**: the man test checks the doc table against the
  const - one owner (the const), doc is a checked rendering. The const's own
  doc comment says "Keep in step with ... the Reserved words table in
  CLAUDE.md", which is stale: the table is in `docs/config.md` and is tested.

## 3. Structure

- **`grok_home.rs` is four modules in one file**: (a) trust store mirror and
  decision, (b) sandbox profile naming + TOML merge + locked atomic write,
  (c) session log location, (d) background-result analysis and the wake
  prompt. (d) is not about grok's home at all; it is turn-outcome logic that
  belongs with the grok runner (`provider/grok/wake.rs`). (a)+(b) are
  "provider-local permission setup", the same job maki and opencode will need
  (maki: its permissions config; opencode: a custom read-only agent definition
  and a private config/db dir via env). That suggests a per-provider
  `prepare(&LaunchPlan) -> Prepared` hook, with `grok_home`'s trust/profile
  code as grok's implementation, rather than `invoke` special-casing
  `provider == "grok"` for `grok_write`, `derives`, `resumes_profile`,
  `canonical_roots`, `trust_grant`, the post-run landed check and
  `grok_trust` on `ProviderResult`. Today `invoke` is the grok adapter's
  orchestration layer, and `ProviderResult` has a grok-only field.
- **`locked_update` is a generic "flock + atomic replace" primitive** living in
  a grok file and used once. opencode's isolated config (agent definition
  file) will want the same; move it to a small `fsutil` module. Note it
  derives the tmp name from `config::generate_uuid` - `grok_home` depending on
  `config` for a uuid is an edge pointing the wrong way; uuid generation
  belongs in a leaf util.
- **`provider.rs` depends on grok internals by path** (`crate::grok_home::...`
  at a dozen sites, plus `provider_tests.rs` hashing a profile name). Once the
  grok runner lives in its own module, `grok_home` becomes its private child
  and nothing else needs to see `PROFILE_PREFIX`.
- **`Grok::resolve` is called four times per run** (`prepare_grok_write`, the
  landed check, `run_grok`, `run_grok_turn`) from the same `env`, each
  re-reading the process environment and re-canonicalizing. Resolve once per
  launch and pass it; it also makes the "child's view" a value rather than a
  re-derivation that could see a changed environment.
- **`man.rs`** is small and well-shaped. Its tests are where the cross-checks
  live, which is right in principle, but two of them (sandbox table, level
  list) check docs against hand-maintained copies instead of a registry, so
  they will not catch a provider being added. If a provider registry lands,
  the sandbox table test should require one column per registered provider.
  The `include_str!` catalog plus the test that `docs/` equals it is fine;
  a `build.rs` generating `TOPICS` from `docs/*.md` would remove the catalog
  test entirely but lose the hand-written summaries, so not obviously better.
- **`tests/man.rs`** checks dispatch order (good, an integration property).
  Minor: it writes to `CARGO_TARGET_TMPDIR`, and every test rewrites the same
  `man-broken-config` dir; harmless under `--test-threads=1`, racy otherwise.

## Lateral notes

- `decision` returns `Undecided` for an unsafe root key (e.g. a project at
  `$HOME` with no repo); `prepare_grok_write` will then announce a permanent
  trust grant that grok (per `is_unsafe_trust_root`) will refuse. The post-run
  landed check catches it, but only after a write run whose edits were all
  cancelled. Refusing up front ("grok never trusts $HOME") would save the run.
- `main_checkout_of_worktree` handles `.git` files without the `/worktrees/`
  shape (submodules: `gitdir: ../.git/modules/x`) by returning `None`, keying
  on the submodule root. Probably matches grok, worth one test.
- `cancel_cause` keeps the last refusal before `turn_ended` but does not reset
  `refused_tool` at a later allowed permission, so a refusal early in a turn
  that later ends for `max_turns` is blamed on the tool. Low impact.
- `merge_profile` compares `canonical_roots(&p.read_write)` against ours, so a
  hand-edit that only reorders or duplicates entries is accepted; fine.
- `RESERVED_NAMES` doc comment points at CLAUDE.md (stale, see section 2).
