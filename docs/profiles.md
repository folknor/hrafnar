# Profiles

A profile is a named harness and its launch settings, written as a
`[profiles.<name>]` table and selected with `-p <name>`:

```toml
[profiles.implement]
provider = "codex"
comment = "Implementation work that must build and test."
model = "gpt-5.6-terra"
effort = "high"
sandbox = "workspace-write"
writable_roots = ["/srv/fixtures", "$XDG_CACHE_HOME/assets", "~/corpora"]
config = ['model_reasoning_summary="concise"']
env = { RUST_LOG = "info" }

[profiles.opus]
provider = "claude"
model = "Opus 4.8"
effort = "medium"

[profiles.qwen]
provider = "maki"
model = "openrouter/qwen/qwen3-coder"

# A harness alone is a profile: that harness on its own defaults.
[profiles.grok]
provider = "grok"
```

`review list` lists the profiles a run here can use, without their harness,
and names any hidden because their harness is not installed; ask it before
choosing a profile for a role. `review list --all` adds each profile's
harness, where it comes from, and any definition it hides.
Session rows record the selected definition as `profile_origin` (layer and
table), including automatic retries. This describes the requested launch
source, not proof of successful execution or that the profile still exists.
Only runs with a session ID get a session row. Manual messages have no
profile origin.

## Selecting one

Every run runs a profile, and the profile decides the harness. `-p implement`
selects one; without `-p`, the run uses `[_defaults].profile`. A run with
neither, or naming a profile no file defines, fails before anything launches,
naming every file searched and the profiles that exist.

A resume (`review message`) takes no profile. The session keeps the sandbox,
model and effort recorded for it, but **not** the profile's `env` or `config`,
which are not stored in the session record and so are not restored on a
message (see `review man message`).

## Precedence: the first definition wins whole

A profile in the project file beats one of the same name in the global file,
and the winner replaces it **entirely**. Fields are never merged, so a project
profile that leaves out `model` does not inherit the global profile's `model`:
it runs on the harness's default. Every field of a run therefore comes from
one table you can point at; `review list --all` shows each profile next to what it
hides.

The one thing a project profile may not change is the harness: naming a
different one than the global profile of the same name is an error, since
`-p <name>` would otherwise launch a different harness in that project than
everywhere else. Rename one of them.

## Fields

`provider` is required. Every other field is optional. An unknown key is an
error, because a misspelled key would otherwise parse as an empty profile and
silently replace the global one. Fields the profile's harness does not honour
are errors too. Empty `config` and `writable_roots` lists are accepted
everywhere.

- `provider`: the harness the profile runs on.
- `comment`: free text for whoever picks the profile, never sent to a harness.
  `review list` shows its first line, `review list --all` all of it.
- `model`: the model to request. For maki, `provider/model-id`; without one,
  maki picks its last used model, then its configured default.
- `effort`: the reasoning effort to request. Maki profiles must omit it: maki
  accepts an effort flag and ignores it, so set its thinking level in maki's
  own config instead. A custom OpenAI-compatible model in maki only receives a
  thinking level if its maki config declares the model's thinking fields.
- `sandbox`: what the run may modify; defaults to `read-only` for codex and
  grok. Claude and maki profiles must omit it; configure the harness's own
  permissions instead. See `review man sandbox` for what each level means per
  harness, and why a maki run can write unless its permissions deny it.
- `writable_roots`: extra paths a `workspace-write` run may write, added to the
  ones `review` derives for builds, never replacing them. `~`, `$VAR` and
  `${VAR}` expand; an entry naming an unset variable is dropped with a warning.
  A non-empty list requires `workspace-write` on codex or grok (grok
  also accepts its native `workspace` alias); otherwise parsing fails.
  Remove the field or select that level. Claude and maki profiles must omit it.
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
it stays unset and the harness chooses. Codex precedence is explicit: a
profile's `model` beats raw `config` entries for `model`; otherwise the last
raw `model` entry is recorded. The last raw `model_reasoning_effort` entry
beats `effort`. Those effective selectors are inherited on a message even
though raw config is not stored or replayed. Final non-string TOML values for
these two raw overrides are errors: change them to strings or remove them.
Other raw config is still validated by codex. Existing session rows remain
readable; historical rows retain the selectors originally recorded.
