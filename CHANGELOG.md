# Changelog

Changes that affect how you use `review`: flags, commands, config format and
output. Each entry says what to do if you relied on the old behaviour.

## Unreleased

### Removed

- **Fan-out.** A run starts one session on one harness. `-a` takes one
  archetype, so a comma-separated list, `-a all` and `[_groups]` groups are
  gone; a `[_groups]` table is a load error. For several opinions, start
  several `review` calls with the same stdin.
- **`--stagger`.** Runs on one host are spaced by a fixed 30s through the
  host launch lock, released early when a run ends sooner.
- **`--provider`.** It is refused with an error pointing at `-p` and
  `--harness`.
- **Legacy host-scoped profile tables** (`[HOST.HARNESS.NAME]`) and
  **harness-scoped tables** (`[HARNESS.NAME]`). Both are load errors. Move
  the settings to `[profiles.NAME.HARNESS]`.
- **`[_defaults].providers` and `[_defaults].provider`.** Both are load
  errors. Use `[_defaults].profile` and `[_defaults].prefer`.
- **Profile shadowing.** A project config can no longer redefine a profile
  the global config defines; that is a load error naming both files. Give
  the project's profile its own name.
- **`all` as a reserved archetype name.** An archetype may now be named
  `all`.

### Changed

- **Profiles are tiers keyed by harness.** `[profiles.NAME]` holds an
  optional `comment` and one entry per harness (`codex`, `claude`, `grok`,
  `maki`), each with that harness's model, effort, sandbox, env, config and
  writable roots. An empty entry runs that harness on its defaults. The old
  `provider = "..."` key is refused; move the settings under
  `[profiles.NAME.HARNESS]`.
- **The harness for a run** comes from `--harness`, else the first harness in
  `[_defaults].prefer` that the profile defines and this host has installed,
  else the profile's only installed entry. Every run needs a profile: `-p`
  or `[_defaults].profile`.
- **`review config` is `review list`** (alias `ls`). By default it shows the
  archetypes and the profiles whose harness is installed, with the harness a
  plain `-p NAME` would pick starred. `review list --all` shows the full
  resolved config with sources; `review list --projects` the `-C` registry.
  `review config` still works as a warning alias of `review list --all`.
- **"Provider" is now "harness"** in output and messages: the program
  `review` drives (claude, codex, grok, maki). `review message` prints
  `harness: NAME (from the session record)`, and `review list --all` lists
  `harnesses:`. Stored session, audit and incident rows keep the keys
  `provider` and `provider_sandbox`, so existing logs and `jq` queries keep
  working.
- **Exit status** describes one run: `1` when the run produced no answer,
  `3` when something it started was left running.

### Added

- `--harness NAME` to run a specific harness entry of the profile.
- `[_defaults].prefer`, an ordered list of preferred harnesses.
