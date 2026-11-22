# What a run can touch

A profile entry's `sandbox` sets what a codex or grok run may modify, and
its `writable_roots` adds paths outside the project to a write run. This
page covers each level per harness, the roots a write run is granted, the
git metadata check every run gets, what a resumed session keeps, and what
to do when a write fails. The profile fields themselves are in
`review man profiles`.

## Levels per harness

The levels are `review`'s own names, and each harness gets its own
spelling of them. An entry that omits `sandbox` runs `read-only`:

| `sandbox` | codex | grok | claude | maki |
|---|---|---|---|---|
| `read-only` | `read-only` | `read-only` | unsupported | unsupported |
| `workspace-write` | `workspace-write` | `workspace` | unsupported | unsupported |
| `danger-full-access` | `danger-full-access` | `none` | unsupported | unsupported |

`danger-full-access` turns the harness's sandbox off: the run can write
anything you can.

Any other value goes to the harness unchanged, so a grok entry can name
one of your own profiles in grok's `sandbox.toml`. The harness rejects a
name it doesn't know before running a turn. The three level names above
always mean the levels in the table, so a grok profile of your own named
`workspace-write` can't be selected. On grok, `workspace` and `none` are
read as `workspace-write` and `danger-full-access`.

A codex or grok profile entry fails to load when it sets `writable_roots`
at any level but `workspace-write`.

## Claude and maki runs

Claude and maki have no filesystem sandbox that `review` controls, so
their entries must omit `sandbox` and `writable_roots`, and their runs
record no level. A profile that sets either fails to load.

- **Claude**: the run uses Claude Code's own permission settings in
  `dontAsk` mode. Pre-approved tools run, and anything that would prompt
  is denied.
- **Maki**: the run uses maki's own `permissions.toml`. Anything that would
  ask for approval is denied, since nobody is there to answer.

A maki run isn't read-only by default. Maki's built-in rules allow its
file-writing tools inside the working directory, so a maki run with no
permissions file of its own can edit the project. Deny those tools in
maki's permissions to make its runs read-only.

`review` never marks a project as trusted for maki, but maki keeps any
trust you gave it: an answer in an interactive maki session, or a
`trust.paths` pattern in maki's own config. In an untrusted project the
project's `.maki` configuration can tighten maki's permissions but not
loosen them. In a trusted one it can do both.

## Read-only

A `read-only` run can read the filesystem, and can't write to the project
or anywhere else you own. Grok's `read-only` still lets it write grok's own
home and the temporary directories.

On codex the level holds whatever your own codex setup says. `review`
pins the approval policy to never, and ignores your codex `config.toml` and
your user and project `.rules` files, either of which could otherwise let
a command run outside the sandbox. A read-only codex run can still make
network connections, including to sockets of services already running.

A profile's codex `config` overrides are applied after `review`'s own
settings. One that restates the approval policy, the permission settings
or network access replaces them, and the guarantees on this page assume a
profile that doesn't.

## Workspace-write

A `workspace-write` run can write inside the project directory, plus the
roots in Writable roots below, plus what the harness always grants:
temporary directories on both, and grok's own home on grok. A codex write
run also has network access, which lets build and test suites use unix
sockets.

Codex keeps some paths inside the project read-only on its own account:
`.git`, `.agents`, `.codex` and `.aws`. `review` lifts that for `.git`
only, as Git metadata below describes.

### Grok write runs

Grok's built-in `workspace` profile can't add writable paths, so a grok
write run uses a profile `review` creates in grok's global `sandbox.toml`. Each profile is named `review-ws-` plus a hash of its root
set, so a run gets exactly its own roots. `review` creates a profile once
and never modifies it.

A grok write run first marks the project as trusted in grok's folder trust
store, because grok cancels every edit in an untrusted folder. That grant
is **permanent and host-wide**: grok also runs that repository's own MCP
and LSP configuration without asking, in every grok session. The run that
grants it says so. The run is refused instead when the project:

- is marked untrusted in grok's store; `review` never overrides that
- is the filesystem root or your home directory, which grok never trusts
- is a grok-managed worktree, whose trust key `review` can't model
- has a trust store grok can't load

## Writable roots

A build needs to write outside the project, so a `workspace-write` run on
codex or grok is also granted these paths, when they lie outside the
project:

- brokkr's build lock and worktree directory, `$HOME/.brokkr`
- the cargo home, `$CARGO_HOME`, else `$HOME/.cargo`
- brokkr's command history, `brokkr` under `$XDG_DATA_HOME`, else under
  `$HOME/.local/share`
- `$CARGO_TARGET_DIR`, and the target of a `./target` symlink, granted by
  its real path
- the `.git/lfs` store of a sibling checkout that holds a path dependency,
  when that store exists

A path that doesn't exist yet is granted as written, since a build may
create it. Every grant is printed at launch with its reason, and recorded
with the run.

A profile's `writable_roots` adds to these and never replaces them. Each
entry may use bare `~`, `~/`, `$VAR` and `${VAR}`, and must be absolute
after expansion; `~user` isn't supported. An entry with an unset variable,
an unsupported form or a relative path is dropped with a warning.

Every root, derived or from the profile, passes the same filters. A root
spelled with `..` is refused. An existing root is resolved through its
symlinks first, and the rest of the filters apply to where it leads. A
root is refused when it is `/`, or is the project or one of its parents.
A root inside the project is skipped, since the project is writable
already. A sibling of the project is allowed.

A codex profile whose `config` sets `sandbox_workspace_write.writable_roots`
**replaces** the whole set, project `.git` included, with exactly what it
lists. Those paths pass none of the filters. The run records what it got.

## Git metadata

A codex `workspace-write` run can `git add` and `git commit`, because the
project's `.git` is granted writable. Only a real `.git` directory in the
project root is granted. Nothing is granted, and a note at launch says so,
when:

- `.git` is a pointer file: a linked worktree, a submodule or a separate
  git directory
- the project sits below the repository's top level

A commit then needs a codex `config` override that grants it, or has to
happen outside the run. Grok gets no automatic `.git` grant.

### What a run changed in git metadata

A writable `.git` lets a run leave behind something git executes later,
outside any sandbox, the next time anyone runs git in that repository: a
hook, or a config entry naming a program. Claude and maki runs can always
write `.git`. So every run, on every harness and level, is checked.

At launch `review` notes these files, in both the git directory and the
shared one:

- `config` and `config.worktree`
- the hooks, including a `core.hooksPath` directory that config names
- any include file that config names
- the files that say where the git directory is: a `.git` pointer and
  `commondir`

At the end of the run `review` warns about each one that changed, and
about anything it couldn't check:

```text
warning: codex: git metadata that git may execute from changed while this
run was live, by it or by anything else, or could not be checked - inspect
before running git in this repository:
  /path/to/repo/.git/hooks/pre-commit: added
```

The warning is one line; it's wrapped here. Each entry names the path and
the kind of change, never the contents, since config can hold credentials.
The kinds are: added, removed, content or mode changed, symlink changes,
repository layout changed, and not checked. `review` undoes nothing, so
inspect the change yourself. Setting a branch's upstream writes `config`
too (`git branch -u`, `git push -u`, or creating a branch from a remote
one), so a warning isn't proof of anything malicious.

The check compares two moments and guarantees nothing: no warning doesn't
mean the repository is safe. It doesn't cover:

- nested git directories under `modules/` and `worktrees/`
- git's global and system config
- anything run in the repository while the run was still going
- a `review` killed before the run ended

## What a resumed session gets

`review message` keeps the session's own level, from the last run recorded
for it, so a `workspace-write` session stays writable.

- **Codex**: the recorded roots are passed again as profile roots and go
  back through the filters, and the build roots are derived afresh for
  this host. A root that came from a `config` override is recorded like any
  other, so it carries over if it passes the filters. The override itself
  doesn't: its replacing the whole set, and its exemption from the filters.
- **Grok**: grok fixes a session's sandbox for its lifetime. A session
  started under a `review-ws-` profile resumes under exactly that profile
  and its recorded roots. A session recorded under grok's plain `workspace`
  resumes that way, with no extra roots. A session with no recorded level
  resumes under whatever grok saved for it, and the run records no level,
  because `review` doesn't know that saved setting.

## When a write fails

A run that reports a read-only filesystem was launched `read-only`, is
writing outside its roots, or is writing a path the harness protects, such
as `.codex` on codex or a `.git` the run wasn't granted. Run
`review sessions ID` to see the level and roots it launched with, then:

- **Launched read-only**: use a profile with `workspace-write`.
- **Writing outside its roots**: add the path to the profile's
  `writable_roots`.
- **Writing a protected path**: `writable_roots` doesn't lift that
  protection. `danger-full-access` or a deliberate codex permission
  override would, at the cost of the whole sandbox; do that step outside
  the agent instead.

Never work around a failed write by moving `HOME` or `CARGO_TARGET_DIR`
inside the project: that bypasses the host's shared build lock without
any error.
