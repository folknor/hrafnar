# Profiles

A profile is a named tier of work, such as a standard hrafnar, an
implementation that must build, or a cheap mechanical edit. You select one
with `-p NAME`, and `hrafnar` launches one of its harness entries. This page
explains how a run picks the profile and the entry, how profiles layer across
config files, and every field an entry takes, under "Profile fields". The
table of refusals is at the end.

## What a profile holds

A profile is a `[profiles.NAME]` table with an optional `comment` and one
entry per harness that can serve the tier. Each entry is that harness's launch
settings. The example below defines three profiles and the defaults that pick
among them:

```toml
[profiles.review]
comment = "Standard reviewer."
codex  = { model = "gpt-5.6-terra", effort = "low", sandbox = "read-only" }
claude = { model = "Opus 4.8", effort = "low" }
maki   = { model = "openrouter/qwen/qwen3-coder" }

[profiles.implement]
comment = "Implementation work that must build and test."

[profiles.implement.codex]
model = "gpt-5.6-terra"
effort = "high"
sandbox = "workspace-write"
writable_roots = ["/srv/fixtures", "$XDG_CACHE_HOME/assets", "~/corpora"]
env = { RUST_LOG = "info" }

[profiles.plain]
grok = {}

[_defaults]
profile = "hrafnar"
prefer  = ["codex", "claude", "maki"]
```

An empty entry, like `grok = {}` above, runs that harness on its own
defaults.

## Finding the profiles you can use

Run `hrafnar list` before choosing a profile for a role. It lists the profiles
a run here can use, each with its installed harnesses, and marks with `*` the
harness a plain `-p NAME` picks. It also names each profile it hides because
none of that profile's harnesses is installed. `hrafnar list --all` adds the
file each profile comes from and every entry's settings.

## How a run picks the profile and harness

Every run launches one entry of one profile. The profile is the one `-p`
names, or without `-p`, `[_defaults].profile`. A run with neither fails before
anything launches, and so does a run naming a profile no file defines. The
error names every file searched and the profiles that exist.

`hrafnar` then picks the harness, in this order:

1. `--harness NAME`, if given. The profile must have an entry for that
   harness, or the run fails listing the harnesses the profile has.
2. The first harness in `[_defaults].prefer` that the profile defines and
   this host has installed.
3. The profile's one installed entry, if exactly one is installed.
4. A profile with a single entry and nothing installed resolves to that
   entry, so the run can name the missing harness.

Anything else fails: several installed entries with no preference asks for
`--harness` or `[_defaults].prefer`, and several entries with none installed
asks you to install one or pick another profile. A chosen harness that isn't
installed fails the run before anything launches; `--dry-run` only warns.

`hrafnar message` takes no profile. To learn what a resumed session keeps from
its entry, see `hrafnar man message`.

## Profiles across config files

Each profile is defined in exactly one file. A project's `.hrafnar.toml` can
add profiles of its own, but it can't define a profile name the global config
already defines: loading fails, naming both files. Give the project's profile
another name.

We refuse the redefinition so that a checked-out repository can't change what
`-p review` means for whoever runs `hrafnar` in it, for example by swapping
the model, widening the sandbox or adding env. Nothing is overridden, so
nothing is merged: every field of a run comes from the one entry
`hrafnar list --all` shows.

## Profile fields

A `[profiles.NAME]` table takes only `comment` and harness names (`claude`,
`codex`, `grok`, `maki`). Any other key is an error, and so is a profile with
no harness entry:

- **`comment`**: free text for whoever picks the profile, never sent to a
  harness. `hrafnar list` shows its first line, `hrafnar list --all` all of
  it.

### Entry fields

Every field of a harness entry is optional. An unknown key is an error,
because a misspelled key would otherwise leave an empty entry that silently
runs the harness's defaults. A field the entry's harness doesn't honour is an
error too, though empty `config` and `writable_roots` lists are accepted on
every harness:

- **`model`**: the model to request. For maki, write `provider/model-id`;
  without one, maki picks its last used model, then its configured default.
- **`effort`**: the reasoning effort to request. Maki entries must omit it,
  because maki accepts an effort flag and ignores it. Set maki's thinking
  level in maki's own config instead. A custom OpenAI-compatible model in maki
  receives a thinking level only if its maki config declares the model's
  thinking fields.
- **`sandbox`**: what the run may modify, `read-only` when omitted. Codex and
  grok entries take it; claude and maki entries must omit it, so configure
  those harnesses' own permissions instead. See `hrafnar man sandbox` for what
  each level means per harness.
- **`writable_roots`**: extra paths a `workspace-write` run may write, added
  to the roots `hrafnar` derives for builds. `~`, `$VAR` and `${VAR}` expand.
  A non-empty list needs `sandbox = "workspace-write"` on codex or grok (grok
  also accepts its native `workspace`). Claude and maki entries must omit it.
  See `hrafnar man sandbox` for which entries are dropped at launch.
- **`config`**: codex only. Each string goes to codex verbatim as a `-c`
  override, after `hrafnar`'s own, so an entry can restate one of them on
  purpose, including the settings behind the sandbox guarantees. Codex runs
  ignore your own codex `config.toml`, so anything an override refers to,
  such as a custom `model_provider`, must be defined by overrides here too.
- **`env`**: environment variables set for the harness process. They override
  the defaults `hrafnar` sets, except `PWD`, which is always the project root
  the harness runs in. `hrafnar list --all` shows only their names, since
  values can be secrets.

### Codex model and effort precedence

A codex entry can set the model and effort both through `model` and `effort`
and through raw `config` entries. `hrafnar` records the selector codex uses:

- **Model**: `model` wins over any raw `model` override; without it, the last
  raw `model` override is used.
- **Effort**: the last raw `model_reasoning_effort` override wins over
  `effort`.

The final raw override for either key must be a TOML string. If neither a
field nor a raw override sets a selector, the harness chooses. What each
session records is covered in `hrafnar man sessions`.

## Refusals and the fix

Each row below is an error a profile can produce at load or launch:

| What you see | Why | Fix |
|---|---|---|
| `no harness entry` | The profile has only a `comment`, or nothing | Add an entry, `HARNESS = {}` for defaults |
| `unknown key 'KEY'` in `[profiles.NAME]` | A key that is neither `comment` nor a harness | Move an entry field under `[profiles.NAME.HARNESS]`; fix a typo |
| `` `provider` was removed `` | The profile names its harness in a `provider` key | Move the settings under `[profiles.NAME.HARNESS]` for that harness |
| `[HARNESS.*] ... move each` | A `[HARNESS.NAME]` table defines the profile | Move it to `[profiles.NAME.HARNESS]` |
| `unknown section` naming a host | A `[HOST.HARNESS.NAME]` table | Move the profiles to `[profiles.NAME.HARNESS]` |
| `does not honour 'FIELD'` | The entry's harness ignores that field | Remove the field and configure the harness itself |
| `writable_roots are not honoured at this sandbox level` | Roots on a level other than `workspace-write` | Remove the roots or set that level |
| `config override 'KEY' must be a string` | The last raw `model` or `model_reasoning_effort` isn't a string | Quote its value or remove it |
| `defined in both` | The project redefines a global profile | Rename the project's profile |
| `has no HARNESS entry` | `--harness` names a harness the profile lacks | Pick one of the listed harnesses |
| `several installed harnesses` | Nothing orders the installed entries | Pass `--harness` or set `[_defaults].prefer` |
| `none of its harnesses ... is installed` | No entry's harness is on this host | Install one, or pick another profile with `-p` |
