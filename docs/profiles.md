# Profiles

A profile is a tier of work - "standard review", "implement", "cheap
mechanical edit" - written as a `[profiles.<name>]` table and selected with
`-p <name>`. It holds an optional `comment` and one entry per harness that can
serve that tier, each with that harness's launch settings:

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
config = ['model_reasoning_summary="concise"']
env = { RUST_LOG = "info" }

# An empty entry is that harness on its own defaults.
[profiles.plain]
grok = {}

[_defaults]
profile = "review"
prefer  = ["codex", "claude", "maki"]
```

`review list` lists the profiles a run here can use, each with its installed
harnesses and the one a plain `-p <name>` would pick marked `*`, and names any
profile hidden because none of its harnesses is installed; ask it before
choosing a profile for a role. `review list --all` adds where each profile
comes from and every entry's settings.

Session rows record the selected entry as `profile_origin` (layer and table,
`[profiles.<name>.<harness>]`), including automatic retries. This describes
the requested launch source, not proof of successful execution or that the
profile still exists. Only runs with a session ID get a session row. Manual
messages have no profile origin.

## Selecting one

Every run runs one entry of one profile. The profile is `-p implement`, or
without `-p`, `[_defaults].profile`. A run with neither, or naming a profile no
file defines, fails before anything launches, naming every file searched and
the profiles that exist.

The harness is then:

1. `--harness <name>` if given. The profile must have an entry for it;
   otherwise the run fails listing the harnesses it has.
2. Otherwise the first harness in `[_defaults].prefer` that the profile
   defines and this host has installed.
3. Otherwise, if exactly one of the profile's entries is installed, that one.
   A profile with a single entry resolves to it even when it is not
   installed, so the run can say which harness is missing.
4. Otherwise the run fails, asking for `--harness` or `[_defaults].prefer`.

A chosen harness that is not installed fails the run before anything launches;
`--dry-run` only warns.

A resume (`review message`) takes no profile. The session keeps the sandbox,
model and effort recorded for it, but **not** the entry's `env` or `config`,
which are not stored in the session record and so are not restored on a
message (see `review man message`).

## Layers: add-only

A profile is defined in exactly one file. A project's `.review.toml` may add
profiles of its own, but a profile name the global config already defines may
not be defined again in the project: that is a load error naming both files.
Pick another name for the project's profile.

The reason is the same one that keeps `claude_token_command` global-only: a
checked-out repository's config must not be able to change what `-p review`
means - swap its model, widen its sandbox, add env - for whoever runs `review`
in it. Since nothing is overridden, nothing is merged either: every field of a
run comes from the one entry `review list --all` shows.

## Fields

In `[profiles.<name>]` the only keys are `comment` and harness names. An
unknown key is an error, and so is a profile with no harness entry. The old
`provider = "..."` key is refused: move the settings under
`[profiles.<name>.<harness>]`.

- `comment`: free text for whoever picks the profile, never sent to a
  harness. `review list` shows its first line, `review list --all` all of it.

Every entry field is optional. An unknown key in an entry is an error, because
a misspelled key would otherwise parse as an empty entry and silently run the
harness's defaults. Fields the entry's harness does not honour are errors too.
Empty `config` and `writable_roots` lists are accepted everywhere.

- `model`: the model to request. For maki, `provider/model-id`; without one,
  maki picks its last used model, then its configured default.
- `effort`: the reasoning effort to request. Maki entries must omit it: maki
  accepts an effort flag and ignores it, so set its thinking level in maki's
  own config instead. A custom OpenAI-compatible model in maki only receives a
  thinking level if its maki config declares the model's thinking fields.
- `sandbox`: what the run may modify; defaults to `read-only` for codex and
  grok. Claude and maki entries must omit it; configure the harness's own
  permissions instead. See `review man sandbox` for what each level means per
  harness, and why a maki run can write unless its permissions deny it.
- `writable_roots`: extra paths a `workspace-write` run may write, added to the
  ones `review` derives for builds, never replacing them. `~`, `$VAR` and
  `${VAR}` expand; an entry naming an unset variable is dropped with a warning.
  A non-empty list requires `workspace-write` on codex or grok (grok
  also accepts its native `workspace` alias); otherwise parsing fails.
  Remove the field or select that level. Claude and maki entries must omit it.
  A codex `config` override of the writable roots replaces all of them,
  these included. See `review man sandbox`.
- `config`: codex only; remove a non-empty list from other harnesses. Each
  string is passed to codex verbatim as a `-c`
  override, after `review`'s own, so it can restate one deliberately - which
  includes the settings behind the sandbox guarantees. Codex runs ignore the
  operator's own codex `config.toml`, so anything an override refers to, such
  as a custom `model_provider`, must be defined by overrides here too.
- `env`: environment variables set for the harness process. They override
  the defaults `review` sets for a harness, but not `PWD`, which is always
  the project root the harness runs in. The session log
  and `review list --all` show only their names, since values can carry secrets.
  A codex incident bundle keeps values too, except for names that look
  secret, so it can replay the run.

## What a run records

Each run records its prepared permissions, effective writable roots, and model
and effort selectors. Claude and maki record no sandbox level; an old grok
session with no known selector keeps it unknown. A level is not a certificate
of filesystem enforcement: raw codex config can change permissions.

Model and effort are launch selections, separate from the harness-reported
served model. maki reports no served model, so none is recorded for it. Its
reported cost is recorded as maki gives it, which leaves out any turn maki has
no price for, subagents included; a cost of zero is recorded as unknown, since
maki reports zero for an unpriced model. If neither a dedicated field nor raw config sets a selector,
it stays unset and the harness chooses. Codex precedence is explicit: an
entry's `model` beats raw `config` entries for `model`; otherwise the last
raw `model` entry is recorded. The last raw `model_reasoning_effort` entry
beats `effort`. Those effective selectors are inherited on a message even
though raw config is not stored or replayed. Final non-string TOML values for
these two raw overrides are errors: change them to strings or remove them.
Other raw config is still validated by codex. Existing session rows remain
readable; historical rows retain the selectors originally recorded.
