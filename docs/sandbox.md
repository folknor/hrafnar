# Sandbox levels

A profile's `sandbox` says what a run may modify. It defaults to `read-only`
for codex and grok. **Claude and maki profiles must omit the setting**:
what a claude run can touch is decided by Claude Code's own permission
configuration, what a maki run can touch by maki's own `permissions.toml`, and
`review` records no level for either.

**A maki run is not read-only by default.** maki's built-in rules allow its
file-writing tools inside the working directory, so a maki run with no
permissions file of its own can edit the project. Deny those tools in maki's
permissions to make its runs read-only. Anything that would ask for approval is
denied, since nobody is there to answer. `review` never marks the project as
trusted for maki, but maki keeps any trust it was given before - answered in an
interactive maki session, or granted by a `trust.paths` pattern in maki's own
config. In an untrusted project the project's own `.maki` configuration can
tighten maki's permissions but not loosen them; in a trusted one it can do
both.

The levels are `review`'s own names, translated per harness:

| `sandbox` | codex | grok | claude | maki |
|---|---|---|---|---|
| `read-only` | `read-only` | `read-only` | unsupported | unsupported |
| `workspace-write` | `workspace-write` | `workspace` | unsupported | unsupported |
| `danger-full-access` | `danger-full-access` | `none` | unsupported | unsupported |

Any other value is passed to the harness unchanged, for a harness that
defines its own named profiles; the harness rejects a name it does not know
before running a turn. These three names always mean the levels above.

## read-only

The run can read the filesystem and cannot write to the project or anywhere
else the operator owns. Grok's read-only level still lets it write its own
home and temporary directories. On codex this holds no matter what the
operator's own codex configuration says: `review` pins the
settings that could otherwise widen it. A read-only codex run can still make
network connections, including to sockets of services already running.

The one way to widen it is deliberate: a profile's codex `config` overrides
come after `review`'s own settings, so one that restates the approval policy,
the permission settings or network access replaces them. The guarantees on this
page assume a profile that does not.

## workspace-write

The run can write inside the project directory, plus the roots below, plus
whatever the harness itself always grants: temporary directories on both, and
on grok its own home. Codex also keeps some paths inside the project read-only
on its own account (`.git`, `.agents`, `.codex`, `.aws`); `review` lifts that
for `.git` only, so a codex write run can commit (see Git metadata below).
On codex the run has network access too, which is what lets build and test
suites use unix sockets.

On grok, a write run first marks the project as trusted in grok's folder-trust
store, because grok cancels every edit in an untrusted folder. That grant is
**permanent and host-wide**: from then on grok also runs that repo's own
MCP and LSP configuration without asking, in every grok session. The run that
grants it says so. A folder explicitly marked untrusted is never overridden;
the run is refused instead.

## Writable roots

A build needs to write outside the project, so a `workspace-write` run on codex
or grok is also granted these, when they lie outside the project:

- brokkr's build lock and worktree directory, `$HOME/.brokkr`;
- the cargo home, `$CARGO_HOME`, else `$HOME/.cargo`;
- brokkr's command history, under `$XDG_DATA_HOME`, else `$HOME/.local/share`;
- `$CARGO_TARGET_DIR`, and the target of a `./target` symlink (a relative
  link such as `target -> ../shared-target` is resolved from the project
  directory first, so it is granted by its real path);
- the `.git/lfs` store of a sibling checkout that holds a path dependency, when
  that store exists;
- on codex, the project's own `.git`, when it is a directory in the project
  root.

The first three are granted whether or not they exist yet, since a build may
create them.

Every grant is printed at launch with its reason, and recorded with the run.

A profile's `writable_roots` adds to these, never replaces them. Each entry,
after expanding bare `~`, `~/...`, `$VAR` and `${VAR}`, must be an absolute
path. `~user` expansion is unsupported. These entries use the same filters as
the derived roots: a spelling containing `..` is refused before resolution.
Otherwise, an existing entry is resolved first - symlinks followed - and the
reachability filters apply to where it actually leads, so an alias cannot reach
what its spelling would not; an entry that does not exist yet is checked, and
granted, as written. Either way it is refused if it is `/` or is the project
itself or one of its parents. A sibling of the project is allowed. An entry
with an unset variable or unsupported `~user` form is dropped with a warning
rather than expanded to an unintended path.

The exception: a codex profile whose `config` sets
`sandbox_workspace_write.writable_roots` **replaces** the whole set - derived
roots and `writable_roots` alike - with exactly what it lists, and those paths
pass through none of the filters above. The run records what it actually got.

## Git metadata

A codex `workspace-write` run can `git add` and `git commit`: by default the
project's `.git` is granted writable (a codex `config` override of
`sandbox_workspace_write.writable_roots` replaces this along with the other
roots). Only a real `.git` directory in the project root is granted
automatically. When `.git` is a pointer file (a linked worktree, a submodule, a
separate git directory), or the project sits below the repository's top level,
nothing is granted automatically and a note at launch says so; a commit then
needs a deliberate grant (a codex `config` override) or happens outside the
run.

A writable `.git` lets a run leave behind something git later executes outside
any sandbox, the next time anyone runs git in that repository: a hook, or a
config entry naming a program. Claude and maki runs could always do this. So
every run, on every harness and level, is checked: at launch `review` notes the
repository's `config`, `config.worktree` and hooks, plus any include file and
`core.hooksPath` directory that config names, and the files that say where the
git directory is (a `.git` pointer, `commondir`). At the end of the run it warns
about each one that changed, and about anything it could not check:

```
warning: codex: git metadata that git may execute from changed while this run was live, by it or by anything else, or could not be checked - inspect before running git in this repository:
  /path/to/repo/.git/hooks/pre-commit: added
```

It names the path and the kind of change - added, removed, content or mode
changed, now or no longer a symlink, the repository layout changed, or not
checked - and never the contents, since config can hold credentials. Nothing is undone; what to do is your call.
Setting a branch's upstream (`git branch -u`, `git push -u`, or creating a
branch from a remote one) writes `config` too, so a warning is not proof of
anything malicious.

This is a best-effort comparison of two moments, not a guarantee. No warning
does not mean the repository is safe. Not covered: nested git directories under
`modules/` and `worktrees/`, git's global and system config, anything run in
the repository while the run was still going, and a `review` that was killed
before the run ended.

## What a resumed session gets

`review message` keeps the session's own level, from the last run recorded for
it: a `workspace-write` session stays writable.

- **Codex**: the recorded roots are passed again as profile roots, so they go
  back through the filters, and the build roots are derived afresh for this
  host. A root that came from a `config` override is recorded like any other,
  so it carries over if it passes the filters; what is lost is the override
  itself - its replacing the whole set, and its exemption from the filters.
- **Grok**: a session started under one of `review`'s write profiles resumes
  under exactly that profile and its recorded roots, because grok fixes a
  session's sandbox for its lifetime. An older session recorded as plain
  `workspace` resumes that way, with no extra roots; one with no recorded level
  resumes under whatever grok saved for it, and records no sandbox level because
  review does not know that saved setting.

## When a write fails

A run that reports a read-only filesystem was launched `read-only`, is writing
outside its roots, or is writing a path the harness protects (such as `.codex`
on codex, or a `.git` the run was not granted - see Git metadata above).
`review sessions <ID>` shows the level and roots it
launched with. Then:

- launched `read-only`: use a profile with `workspace-write`;
- writing outside its roots: add the path to the profile's `writable_roots`;
- writing a path the harness protects: `writable_roots` does not lift that
  protection. `danger-full-access` or a deliberate codex permission override
  would, at the cost of the whole sandbox; usually do that step outside the
  agent instead.

Never work around it by moving `HOME` or `CARGO_TARGET_DIR` inside the project:
that silently bypasses the host's shared build lock.
