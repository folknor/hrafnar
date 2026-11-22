# Acting on another project

This page covers `-C`, which makes a command act on a project other than the
one the current directory is in. You use it when you work in one checkout and
want a review of another. The selector grammar is under "Names and paths",
the registry format under "Registering a project", and which commands accept
`-C` under "Which commands take -C".

## What -C changes

`-C PROJECT` picks the project a command uses: its `.review.toml`, its audit
id and its project root. The harness runs in that project root. The current
directory of `review` itself never changes. The flag goes before or after the
subcommand, so both of these work:

```sh
echo "check this" | review -C bz -a bugs
review list -C bz
```

## Names and paths

A selector is either a path or a registered name, decided by its spelling
alone:

- **Path**: it is `.`, `..` or `~`, or starts with `/`, `./`, `../` or `~/`.
  It resolves against the current directory, must be a directory, and is
  searched upward for `.review.toml` the way the current directory is.
- **Registered name**: anything else. It is looked up in this host's
  registry, described under "Registering a project".

Any other selector starting with `~`, such as `~alice/src`, is refused as an
invalid path: `~user` expansion isn't supported.

There is no fallback between the two kinds. `-C bz` never means a directory
named `bz`; write `-C ./bz` for that. A name that doesn't resolve is an error
listing what this host has registered. This way, registering a project or
losing a checkout can't silently change what an existing command acts on.

## Registering a project

You register projects in the global config, in a table named after this
machine's hostname. A project's `.review.toml` can't hold the registry, since
a checkout's path is a fact about one machine. This example registers two
projects on a host named `myhostname`:

```toml
[_projects.myhostname]
bz = "~/Programs/bz"
"folknor/piners" = "$HOME/src/piners"
```

Run `review list --projects` to see the projects registered for this host.
Tables for other hosts are ignored, and their mistakes don't stop this host
from loading.

### How a name matches

`review` first compares the name with each key, ignoring ASCII case. If no key
matches, it compares the name with the part after each key's last `/`, so
`-C piners` finds `folknor/piners`. A short name that matches several keys is
an error; use the full key.

### Rules for keys and paths

A registry key must be usable as a name. These keys are refused when the
config loads:

- an empty key
- a key spelled like a path, which `-C` would never look up
- a key with a trailing `/` or any whitespace
- two keys that differ only in case

A registered path may use bare `~`, `~/...`, `$VAR` and `${VAR}`. After
expansion it must be an absolute path to an existing directory. An unset
variable, a relative path or a missing directory is an error.

A registered path is the project root itself. Its `.review.toml` must be
directly in that directory: `review` doesn't search a parent for it.

## Project identity

`review` identifies a project by its root, resolved through symlinks to a
canonical path when it can be. A root that can't be resolved, such as a
deleted checkout, keeps the spelling it was recorded with. A recorded session
belongs to the selected project when the two paths are equal as written or
resolve to the same canonical path. Sessions of a deleted project still list
under the path they were recorded with.

A matching path doesn't prove the checkout is the same project. Before
`review message` continues a session, it checks that the recorded project
still exists and that its audit id matches the one recorded with the session.
See `review man message` for the full sequence.

## Which commands take -C

Each command treats `-C` in one of three ways:

- **Selects the project**: a run, `review list`, `review init` and
  `review sessions` without an ID. `review init -C PROJECT` creates
  `.review.toml` in the selected directory itself, never in an ancestor.
- **Asserts the project**: `review message ID` and `review sessions ID` take
  the project from the session's own record. With `-C`, the selected project
  must be that one, or the command fails.
- **Refuses -C**: `review interrupt`, `review incidents`,
  `review sessions --all` and `review man`, which act on the whole host.
