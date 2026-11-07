# Hunt: config scope

Scope: `src/config.rs`, `src/config_tests.rs`, `src/config_cmd.rs`,
`src/config_write.rs`, `src/project.rs`, `src/writable_roots.rs`, followed
into `src/app.rs` and `src/provider.rs` where the config's answers are consumed.

Headline: the config layer is careful about *where values come from* (Sourced,
Layer, shadowing) and careless about *what the values are*. Provider, sandbox
level, model, effort, audit id, project root and archetype name all travel as
`String`, and the questions "which providers exist", "what does this sandbox
level mean for this provider", "does this provider honour this field" are
answered by string matches spread across config, config_cmd, app and provider.
Adding maki and opencode under the current shape means editing every one of
those matches, and the compiler will point at none of them.

## Bugs found on the way

1. **`config_write::load_doc` can wipe a project config.** It does
   `read_to_string(..).unwrap_or_default()`, so any read error (permissions,
   EIO, a non-UTF-8 byte) becomes an empty document, and `append_audit_id` then
   writes a file holding only `[_audit] id = ...` over the original. The comment
   says "or an empty doc if missing", but it treats every failure as missing.
   Only `NotFound` should map to empty; everything else must error. (In practice
   `load` just parsed that file, so a read failure means something changed
   between, but the fallback is the wrong default for a writer.)

2. **The audit-id mint race is not prevented by the lock that claims to prevent
   it.** `app` (fan-out, "after lock to prevent races") checks `cfg.audit.id`,
   but `cfg` was loaded *before* the global lock was taken. Two concurrent first
   runs in a project without an id both see `None`, both mint, and the second
   `append_audit_id` overwrites the first; the first run's audit and sidecar
   rows are filed under an id the project no longer has, and `message_target`'s
   audit-id check will then refuse `review message` for that session ("not the
   project the session was started in"). The fix is to re-read the file under
   the lock (or make the mint a compare-and-set in `config_write`: read, if an
   id is present return it, else write). The resume path (`run_session_resume`)
   mints *after* dropping the global lock and ignores the write error with
   `let _ =`, so its race window is wider and its failure silent.

3. **Weak, fallback-constant identity.** `generate_short_id` is 16 bits and
   falls back to the literal `4242` if `/dev/urandom` cannot be opened;
   `generate_uuid`'s fallback leaves `buf` zeroed if the open fails, minting the
   same "v4" UUID every time. The audit id is now a project identity that
   `review message` checks against (`message_target`), so a 1-in-65536 collision
   between two projects at the same recycled path is exactly the case that check
   exists to catch, and it would pass. Either fail hard when randomness is
   unavailable or use a longer id. These functions also do not belong in
   `config.rs` (see Structure).

4. **`-a all` includes `bare`.** `all` expands to every archetype, and most
   configs carry `bare = ""`, so `-a all` launches an extra unprimed run per
   provider. Probably unintended; `all` should mean "every priming archetype".

5. **`bare` is asymmetric.** The legacy positional `review bare` means "no
   archetype" whatever the config holds, but `-a bare` only works when the
   config defines `bare = ""`; without it, `-a bare` is "not found". The parser
   forbids a non-empty `bare` precisely to keep these in step, so the cleaner
   answer is to resolve `bare` in one place (the archetype resolver) regardless
   of config, and stop needing the config entry at all.

6. **`expand` in `project.rs` and `writable_roots.rs` disagree on `~user`.**
   `project::expand` refuses `~other/...` explicitly; `writable_roots::expand`
   passes `~other/e` through as a literal string, which then fails the
   `is_absolute` check with "ignoring relative writable_roots entry" - a
   misleading message. Also, `project::env_var` and `RealHost::var` both filter
   empty values, so `$HOME=""` behaves the same; that agreement is by copy.

7. **The `--profile` "not defined" error hard-codes the resolution order** in
   `app` (`looked for [{prov}.{profile}] and [{host}.{prov}.{profile}]`) instead
   of asking the resolver what it searched. It is right today; it is a second
   statement of `resolve`'s table order.

8. **`.review.toml` is spelled out in `app.rs`** (`project_root.join(".review.toml")`
   twice, plus the warning text) while `config::CONFIG_FILENAME` is private. The
   path is already known: `ReviewConfig.files.local`. App should write to that,
   not reconstruct it.

## 1. Axes that should be types

### Provider (`String` / `&str` everywhere)

`KNOWN_PROVIDERS: &[&str]` plus `contains` checks in `parse_file` (three
times), `app` (fan-out validation and resume-record validation),
`config_cmd::probe_availability` and the `man` test. Every consumer then
re-matches the string: `sandbox_for` (`"grok"`), `config_cmd::sandbox_text`
(`"claude"`), `provider::effective_sandbox` (`"codex" | "grok"`),
`provider::invoke` (`provider == "grok"`, `provider == "codex"`), the
writable-roots-ignored warning, `interrupt_cmd::interruptible`,
`timings::stale_session`, `Unrecorded::for_provider`.

A `enum Provider { Claude, Codex, Grok }` (then `Maki`, `Opencode`) with
`FromStr`/`Deserialize` parsed once at the config boundary removes the
validation calls entirely (the parse *is* the validation) and turns every
`match provider` into an exhaustive match, so adding maki fails to compile at
each site that must decide something about it. Today adding `"maki"` to
`KNOWN_PROVIDERS` compiles and silently falls into every `_ =>` arm: it would
record no sandbox (claude's arm via `_ => None` in `effective_sandbox`), show
"unset (runner default: read-only)" in `review config` (the codex/grok arm in
`sandbox_text`, false for maki which has no filesystem sandbox), and
`sandbox_for` passes it through. Three different wrong defaults from one
omission.

Better still: a `ProviderSpec`/capabilities table owned by one module
(`provider::spec(Provider) -> &'static Capabilities`) carrying the facts each of
those sites decides: has a filesystem sandbox, native spelling of each level,
honours `effort` flag vs config-only, honours `model`, accepts codex-style
`config` overrides, interruptible, stale cutoff, session id is caller-chosen
(claude, maki, opencode) vs provider-minted, session id format constraint
(opencode's `ses` prefix), binary name for `is_available`. `config_cmd` then
renders "ignored by maki" from the same fact the runner acts on.

### Sandbox level (`Option<String>`)

The profile carries `sandbox: Option<String>`; `sandbox_for` maps it to a
provider string; then `provider` compares the *translated* string to
`"workspace-write"` (codex) and the grok helpers compare to grok's names,
`effective_sandbox` defaults with `"read-only"`, `codex_sandbox_args`
compares `!= "read-only"`, and `config_cmd` defaults to "read-only" in prose.
The default level is written as a literal in at least four places
(`provider::effective_sandbox`, the runner's `unwrap_or("read-only")` in the
codex arg builder, the warning in `invoke`, `config_cmd::sandbox_text`).

Proposed:

```rust
enum SandboxLevel { ReadOnly, WorkspaceWrite, DangerFullAccess, Custom(String) }
impl Default for SandboxLevel { ReadOnly }
```

parsed at `Profile` deserialisation. The `Custom` arm keeps the deliberate
pass-through; the three named levels become reserved by construction rather
than by a doc comment. Translation to native spelling moves to the provider
spec (`fn native(level) -> Option<NativeSandbox>`, `None` for providers with
no sandbox). The `SANDBOX_LEVELS` test constant and the `man` test's
`["read-only", "workspace-write", "danger-full-access"]` list collapse into
`SandboxLevel::NAMED`. Note the man test is a pairwise-agreement test of
exactly the kind the brief calls a finding.

This also clarifies a real ambiguity: the recorded sidecar `sandbox` is in the
*provider's native* vocabulary (grok rows say `workspace`), and the resume path
feeds it back through `sandbox_for`, which is safe only because grok's native
names happen not to collide with `review`'s. `app::inherited_settings`'s doc
comment argues this at length; a `NativeSandbox` newtype distinct from
`SandboxLevel` would make "a grok level fed to codex" a type error rather than
a guarded-against case.

For maki and opencode "read-only" is not a sandbox at all: maki's permissions
live in its own config, opencode's in a custom agent definition. So the type
the runner needs is not a level but a *permission plan*:
`enum Permissions { Sandbox(NativeSandbox, Vec<GrantedRoot>), AgentPolicy(..),
None }`. Deciding that per provider belongs in one function, not in `invoke`'s
`provider == "codex" && sandbox == Some("workspace-write")`.

### Model and effort (`Option<String>`)

For opencode, effort is not a separate value: it is the `#variant` suffix of
the model string. For maki, effort is config-only and the CLI flag is silently
ignored. For claude and codex it is a flag. The profile type cannot express
"this profile's effort will be ignored by this provider", so `review config`
will cheerfully print `effort high` on a maki profile that runs at whatever
maki's config says, and the sidecar will record `effort: high` for a run that
did not use it. A per-provider validation at resolve time ("maki: effort is not
settable from review; set it in maki's config" as an error or at least a
`review config` annotation) needs the capability table above. Model should
probably be a `ModelSpec` the provider spec renders, so opencode's
`provider/model#variant` composition happens in one place.

### Profile fields that only some providers honour

`config` (codex `-c` only), `writable_roots` (codex/grok write only), `sandbox`
(not claude). These are checked at *run* time with an `eprintln!` warning
(writable_roots) or not at all (`config` on a grok/claude profile is silently
dropped; `sandbox` on claude is only annotated in `review config`). Since
profiles are already keyed by provider (`[codex.deep]`), the parse knows the
provider and could reject `config` under `[grok.*]` or `[claude.*]` at load,
the same way `deny_unknown_fields` rejects typos and for the same reason. A
per-provider profile type (or a `Profile` validated against capabilities in
`parse_profiles`) makes the wrong field impossible to write. This is the
cheapest way to stop maki users writing `effort =` and believing it.

### Audit id (`Option<String>`), project root (`PathBuf`), recorded project (`&str`)

- `AuditConfig.id: Option<String>` is an identity compared against sidecar
  rows (`message_target`, `run_session_resume`'s `expected` tuple). A
  `AuditId` newtype with a validated format would stop `""` (rows use an empty
  string as "none", `!rec.audit_id.is_empty()`) and `None` being two spellings
  of absence that the comparisons have to reconcile by hand.
- The project root is "canonicalized" by `project_root_of` (with a silent
  fallback to the raw parent on canonicalize failure), and separately by
  `project::canonical` (same fallback). A `ProjectRoot` newtype only mintable
  by those constructors would let `same_project` and the `root !=
  project_root` comparison in `run_session_resume` rely on canonical form
  instead of re-canonicalizing; right now `run_session_resume` compares two
  `PathBuf`s with `!=`, which is correct only because both came from the same
  `project_root_of`.
- `expected: Option<(PathBuf, Option<String>)>` in `run_session_resume` is a
  bare tuple carrying a domain fact (the validated target). Name it.

### Archetype selection

`archetype_arg` is a comma string split in `app`; `"all"` and group/archetype
lookup are matched by string there. A resolver in config returning
`Vec<ArchetypeRun { label, prime }>` (or a typed error) would own `all`,
`bare`, groups and dedupe together. See Decisions.

### Failures as prose

Every config error is `anyhow::bail!` prose. Callers do branch on some of them
by kind already, just indirectly: `load_optional` branches on
`Located::Missing` vs `Found` but `load` flattens `Missing` into a string;
`message_target` needs "config missing" vs "config broken" vs "audit mismatch"
vs "root gone", and orchestrating agents would benefit from distinguishing
"not in a project" from "project config broken" (different fixes). A
`ConfigError` enum (`NotFound { searched_to }`, `Parse { file, .. }`,
`Reserved { name, kind }`, `UnknownProvider { name, where_ }`,
`GroupMember { .. }`, `ProfileUndefined { provider, profile, searched }`,
`Registry(..)`) costs little and is the place to hang exit codes later. The
`project::lookup` already returns `Result<PathBuf, String>` - a typed
`LookupError { Unknown { registered }, Ambiguous(Vec<String>), Expand(..),
NotAbsolute }` is the natural shape there.

## 2. Decisions made in more than one place

| Question | Sites | Disagree today? | Owner |
|---|---|---|---|
| Is this a known provider? | `config::parse_file` x3, `app` fan-out check, `app` resume-record check, `config_cmd::probe_availability` iterates the list | No, all read `KNOWN_PROVIDERS` | A `Provider` enum; parse is validation. The app fan-out check is a re-check of `--provider` input that never went through config, fine once it is `Provider::from_str`. |
| Does provider P have a filesystem sandbox / what is the default level? | `config::sandbox_for`, `config_cmd::sandbox_text` (claude special-cased), `provider::effective_sandbox` (`codex|grok`), `provider::invoke` warning, codex arg builder `unwrap_or("read-only")` | Will disagree the moment a provider is added: `sandbox_text` defaults non-claude to "read-only", `effective_sandbox` defaults non-codex/grok to none | Provider capability table |
| Which sandbox names are `review`'s levels? | `sandbox_for` match arms, test `SANDBOX_LEVELS`, `man` test list, doc table | No; held by pairwise tests | `SandboxLevel` enum |
| Do writable roots apply to this run? | `provider::invoke` (`derives`), its warning branch, `Profile.writable_roots` doc ("ignored unless workspace-write"), `writable_roots` module doc | Subtly: the Profile doc says "unless sandbox is workspace-write", code also grants for grok fresh write and for `danger-full-access`? (grok_write_launch decides) | Permission plan built once per launch |
| Which name wins, group or archetype? | `config::resolve` (removes clashes by layer), `app` (checks groups before archetypes) | No, because resolve removed the loser | Archetype resolver in config, so app does no lookup ordering |
| What does `bare` mean? | `config::BARE` (parse rule), `app::BARE` (label, positional alias), `-a all` expansion | Yes, see bugs 4 and 5 | One resolver |
| What is a reserved name? | `RESERVED_NAMES`, `cli` test that every subcommand is in it, `man` test that the doc table equals it | No; pairwise tests | Derive from clap's `Command` at startup (`Cli::command().get_subcommands()` names + aliases + `all` + `help`) and drop the hand list. The tests then become unnecessary rather than guarding a copy. |
| Where does the project config live / what is its filename? | `config::CONFIG_FILENAME`, `app` literal `.review.toml` x2, error strings | No | `ReviewConfig.files.local` |
| Is the audit id present, and mint one if not? | fan-out in `app`, resume in `app` | Yes in error handling: fan-out warns on write failure, resume ignores it; fan-out mints under lock, resume after release | `config_write::ensure_audit_id(path) -> AuditId`, read-check-write under one lock |
| How do `~`/`$VAR` expand? | `project::expand` (strict, errors), `writable_roots::expand` (lenient, warns) | Yes on `~user` (bug 6) and in error vs warn by design | One expander returning a typed error; callers choose to bail or warn. The divergence in policy is legitimate, the divergence in grammar is not. |
| Canonical identity of a project root | `config::project_root_of`, `project::canonical`, `project::same_project` | No | `ProjectRoot` type |
| What table order does a profile lookup search? | `config::resolve`, `app` error message | No | Resolver reports what it searched |
| Is a provider installed? | `config_cmd::probe_availability`, `app` fan-out, `app` resume | No, all call `provider::is_available` | Fine; one answer re-checked. But `is_available` maps a provider name to a binary name, which for opencode (`opencode`) and maki (`maki`) is trivial now but is a capability-table fact. |
| `[_projects]` is global-only / `[_audit]` is local-only | `parse_file` (per layer), `resolve` comment "only the global layer can carry one", `global_projects` re-parses the global file and calls `check_projects` again | No; `resolve` and `global_projects` each run `check_projects` independently | `global_projects` should be `load_layers(None)` reused, or `parse_file` should run `check_projects` for this host so it is checked once. |

## 3. Structure

### `config.rs` does six jobs

It is the file format (parse_file), the layered resolver, filesystem
discovery (`Selection`, `locate_config`, `load*`), the `init` command (template
and stdout), random id generation (`generate_uuid`, `generate_short_id`), and
the provider sandbox vocabulary (`sandbox_for`). Proposed split:

- `config/file.rs` - `ConfigFile`, `Profile`, `parse_file`. Pure.
- `config/resolve.rs` - `ReviewConfig`, `Sourced`, `ProfileEntry`, `resolve`,
  and the archetype-selection resolver now living in `app`.
- `config/locate.rs` - `Selection`, `locate_config`, `load`, `load_optional`,
  `project_root`, `global_config_path`. `Selection` belongs with `project.rs`
  arguably, since `project::select` is its only interesting constructor; the
  two modules together are "which project", and `config.rs` should only consume
  a located path.
- `init` moves next to `config_write` (both are the only writers of
  `.review.toml`); `config_write` becomes `config/write.rs` with `init`,
  `ensure_audit_id`.
- `generate_uuid` is a session-id concern (claude, and soon maki and opencode,
  take caller-chosen ids; opencode needs a `ses` prefix). It belongs with the
  provider spec's "mint a session id" capability, not in config.
  `generate_short_id` belongs with the audit id type.
- `sandbox_for` belongs to the provider layer; it is imported by config only
  because `Profile` holds the level. The dependency arrow config -> provider
  vocabulary is backwards: config should hold `SandboxLevel` and know nothing
  about grok.

### Dependency edges

- `config_cmd` imports `crate::provider::is_available` and hand-encodes claude's
  sandbox semantics; with a capability table it imports the table and nothing
  else from provider.
- `config::check_projects` calls `crate::project::check_name`, and `project`
  calls `crate::config::global_projects` and `crate::config::hostname`. A cycle
  between the two modules; merge the registry (parse, validate, lookup) into
  `project.rs` and have config hand it the raw table.
- `writable_roots` is codex-shaped in name and output (`config_override` emits
  `sandbox_workspace_write.writable_roots=`), yet its derivation is generic and
  grok reuses it. Split derivation (generic, "what a build on this host needs")
  from rendering (codex's `-c`, grok's profile hash). Maki and opencode have no
  sandbox, so they will never call it; that is fine, but `provider::invoke`'s
  `derives` boolean should come from the capability table.
- `writable_roots` hard-codes brokkr (`$HOME/.brokkr`, `$XDG_DATA_HOME/brokkr`)
  and cargo/LFS facts. It is "the operator's toolchain as a sandbox policy",
  compiled in. The module doc argues why these are deterministic, and they are,
  but they are one operator's tools. A global-config `[_sandbox].derive = [...]`
  or a named set of derivers selected in config would move a machine-and-toolchain
  fact out of the binary. Not urgent; flag it as the one place config-ish policy
  is in code.

### State split from behaviour

`ReviewConfig.files` is filled in after `resolve` returns (`cfg.files = ...`),
so `resolve` produces a value with `ConfigFiles::default()` that is wrong until
the caller patches it, and tests build configs with no files at all. Pass the
file metadata into `resolve`, or return it alongside.

`load` returns `(ReviewConfig, PathBuf)` and `load_optional` `(ReviewConfig,
Option<PathBuf>)`; the root is derivable from `files.local`. Put
`project_root: Option<ProjectRoot>` on `ReviewConfig` and stop returning
tuples.

`app` copies six fields out of `Profile` into `Launch` field by field
(`model`, `effort`, `sandbox`, `env`, `config`, `writable_roots`, plus env keys
separately). `Launch` should carry the resolved `&ProfileDef` (or an owned
clone) and `Sourced` origin, so the sidecar can also record *which profile
table* a run used - currently lost, though `review config` knows it.

### `config_cmd`

Rendering is pure and well tested, but `Availability = BTreeMap<String, bool>`
keyed by stringly provider with `unwrap_or(false)` for missing keys; with an
enum it is a total function. The STEERING block names "codex or claude" in
text; it is a statement of `interrupt_cmd::interruptible` in prose and will be
wrong the day maki (whose first SIGINT ends a turn cleanly) and opencode
(same, exit 130) become interruptible. Generate the provider list from the
capability.

### Tests

- `config_tests.rs` included as a `#[path]` module of `config.rs`, plus a
  second `mod tests` inside `config.rs` that also exercises resolution
  (`local_only` helper, profile parsing). The split is "layering" vs "single
  file", but both inline-test the same `resolve`. After the file split above,
  each new file owns its tests and the `#[path]` trick goes.
- `config_write` tests write into `target/test-scratch` with a pid+thread name
  and best-effort remove; fine, but a test for the read-error path (bug 1) is
  missing and would have caught it.
- The pairwise-agreement tests (`man` sandbox table vs `sandbox_for`, `man`
  reserved-words table vs `RESERVED_NAMES`, `cli` subcommands vs
  `RESERVED_NAMES`, `every_documented_level_maps_somewhere`) are each guarding
  a copy that a type or a derived list would remove.

## What adding maki and opencode would touch today

Concretely, in this scope and its consumers, with the current shape:
`KNOWN_PROVIDERS`; `sandbox_for` (must decide pass-through vs none for
providers with no sandbox; today it would pass `read-only` through as if it
meant something); `config_cmd::sandbox_text` (would claim "runner default:
read-only" for both, false); `provider::effective_sandbox` (would record none,
arguably right, by accident of `_`); `invoke`'s grok/codex conditions;
`interruptible`; `stale_session`; the STEERING text; the man sandbox table and
its test; `generate_uuid` callers (opencode needs `ses`-prefixed ids); `Profile`
would accept `effort` and `sandbox` for both and nothing would warn that maki
ignores them; `writable_roots` warning text ("apply only to a codex or grok
workspace-write profile"). None of these fail to compile. With a `Provider`
enum and a capabilities struct, all of them do, and the profile parse can
refuse fields a provider does not honour.

Recommendation: do the `Provider` enum, `SandboxLevel`, and a per-provider
capability table before adding maki, as one change; then split `config.rs`
along file/resolve/locate/write lines and fold the archetype selection out of
`app` into the resolver. Fix bugs 1 and 2 independently and first; they are
small and real.
