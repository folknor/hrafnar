# Sandbox and permissions

This page covers what the code can't show from one module: how a sandbox
level turns into native permissions, then a record, then a resume's
permissions, and the build and harness facts behind the writable roots.
Each module's own rules are in its module docs: `config::sandbox` for the
vocabulary, `harness::permissions` for native selection and restoration,
`writable_roots` for derivation and filtering. The harness-specific halves
are in [codex.md](codex.md), [grok.md](grok.md) and [claude.md](claude.md).

## From a level to a record and back

A run's permissions pass through three modules, and the record they leave
is what the next resume launches with:

1. `config::SandboxLevel` parses the profile's `sandbox`. An omitted level
   becomes `read-only` in `Permissions::profile`, before any runner sees it.
2. `harness::Permissions` maps the level to the harness's native selector.
   `PermissionPlan` derives writable roots (codex and grok
   `workspace-write` only), adds the codex `.git` grant, and holds the
   effective paths. Codex renders them as
   `-c sandbox_workspace_write.writable_roots=[...]` before profile
   `config`. Grok gets them through a generated `review-ws-*` sandbox
   profile.
3. The sidecar row records the effective result: `sandbox` in review's
   vocabulary, `provider_sandbox` with grok's exact selector, and
   `writable_roots` only when non-empty. A row with the field is a row that
   widened something.

A profile `config` entry that restates `sandbox_workspace_write.writable_roots`
wins, because codex takes the last `-c`. `codex::config_writable_roots_override`
parses it back so the record follows the run. An explicit `[]` records as
empty, not as "no override". A failed launch records what it asked for; a
panicked task records no permission facts, because its effective grants are
unavailable.

## What a resume inherits

A `review resume` takes no profile, so permissions, model and effort come
from the session's last sidecar row (`InheritedSettings::from_record`),
because they belong to the session, not the invocation. Each resume records
what it launched with, so the chain carries forward. The parts that only
line up across modules:

- The row holds the effective permissions, not the profile's request, and
  that is what a resume must inherit.
- `Permissions::restore` reads the native record. Codex roots go back in as
  profile roots, so they pass the `writable_roots` filters again and are
  re-derived against this host: the build lock belongs to the machine the
  resume runs on. A restored grok `review-ws-*` profile keeps its recorded
  root set verbatim instead (see [grok.md](grok.md#writable-roots)).
- A row with no selector restores codex as `read-only`. Grok passes no
  override, runs on the session's saved profile, and records no level.
- Inheritance is skipped when the row names a different harness. The
  harness comes from that same row today, but a grok level fed to codex
  would widen a run nobody asked to widen.
- Env values and profile `config` aren't recorded, so a resume can't
  inherit them.

Model and effort are inherited because codex resolves the model again on
every invocation. Without `-m`, every resume ran on codex's built-in default
at its default effort, and since `--ignore-user-config` that isn't even the
operator's configured model. A field session's rollout `turn_context` showed
one turn on the profile's model and five resumed turns on another. Grok
stores the model with the session: measured on grok 1.0.40, a session
created with `-m grok-4.6` and resumed without `-m` was still served by
`grok-4.6-build`. The inherited `-m` is redundant there, and harmless.

## What a sandboxed build needs

Codex `workspace-write` grants the cwd and `/tmp`. With `--ignore-rules`
(see [codex.md](codex.md#the-sandbox-guarantee)) build commands really run
sandboxed, and a `brokkr check` fails in this order:

1. The build lock: fatal, before the compiler starts.
2. The cargo target (`$CARGO_TARGET_DIR` or a `./target` symlink onto a
   shared drive): fatal.
3. The cargo home: fatal only when a crate has to be fetched.
4. Brokkr's history database: silent.

The cargo home looked unneeded because warm builds pass. Two agents on one
tree split exactly that way: one passed `brokkr check` twice on a warm crate
set, the other failed on one missing `crc32fast` with `failed to open ...
registry/cache/...: Read-only file system (os error 30)`.

Brokkr's lock is a CPU-contention gate, taken by `check`, `test`, `clippy`
and `bench` but not `fmt`. Cargo's own target-directory locking protects
build state, so a partitioned brokkr lock costs contention and noisy
benchmarks, both recoverable. For the same reason `review` must not hold the
lock around a run: model latency dominates a run, and holding a CPU gate
across it blocks other work while the CPU is idle.

### Why the lock lives in HOME

The lock root is `$HOME/.brokkr` because a derivable path has to resolve
identically from every login. `$XDG_RUNTIME_DIR` doesn't. Tailscale SSH
builds the child environment itself, without `pam_systemd`, so on a host
reached that way every shell had it unset, while `systemctl --user
show-environment` still reported `/run/user/1000`. That command reads the
user manager's environment, not a login shell's. Brokkr then fell back under
`$HOME`, outside every granted root. Measured: 41 consecutive
`workspace-write` runs on that host recorded `writable_roots: null`, and
every build in them died at `lock: failed to open lock file: Read-only file
system`. `/run/user/<uid>` would also have been far too wide a grant: it
holds D-Bus, Wayland, keyring and agent sockets.

### Brokkr's commit worktrees

Brokkr builds another commit in a container under
`$HOME/.brokkr/worktrees/`, with the checkout and one symlink per path
dependency that leaves the repo. Cargo joins a path dependency to its
manifest's directory and normalizes lexically without following symlinks,
so `../dep` reaches the symlink and then the real checkout. The real
checkout stays read-only, because the sandbox binds the resolved path.
Nothing above `$HOME/.brokkr` has a `Cargo.toml`, so cargo's upward
workspace search can't pick up the live project.

Codex writable roots take no globs: its Linux sandbox expands globs only
for unreadable paths, against what exists at startup. A cached worktree is
writable by any `workspace-write` run, so brokkr has to check that a
worktree is clean before reusing it. Otherwise an A/B could measure edited
code under a commit's name.

## Verified behaviour

These checks were run on the installed harnesses:

- A full `brokkr check` passes inside the codex sandbox, while an ungranted
  path is still refused.
- A profile-granted directory is writable, while its sibling on the same
  drive is not.
- Under the `.git` grant, `touch .git/...` succeeds on the installed codex.
  The grant was traced through the legacy
  `-c sandbox_workspace_write.writable_roots` path to the bubblewrap mounts.
