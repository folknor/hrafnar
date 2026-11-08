# Profiles

A profile is a named set of launch settings for one provider, written as a
`[<provider>.<profile>]` table and selected with `-p <profile>`:

```toml
[codex.implement]
comment = "Implementation work that must build and test."
model = "gpt-5.6-terra"
effort = "high"
sandbox = "workspace-write"
writable_roots = ["/srv/fixtures", "$XDG_CACHE_HOME/assets", "~/corpora"]
config = ['model_reasoning_summary="concise"']
env = { RUST_LOG = "info" }

[claude.opus]
model = "Opus 4.8"
effort = "medium"

[maki.qwen]
model = "openrouter/qwen/qwen3-coder"
```

`review config` lists every profile visible here, where each one comes from,
and any definition it hides. Ask it before choosing a profile for a role.
Session rows record the selected definition as `profile_origin` (layer and
table), including automatic retries. This describes the requested launch
source, not proof of successful execution or that the profile still exists.
Only runs with a session ID get a session row. Unprofiled runs and manual
messages have no profile origin.

## Selecting one

`-p implement` looks the name up separately for each launched provider. Every
launched provider must have it, or the run fails before anything launches,
naming every file searched. Without `-p`, each provider runs on its own
defaults with the `read-only` sandbox.

A resume (`review message`) takes no profile. The session keeps the sandbox,
model and effort recorded for it, but **not** the profile's `env` or `config`,
which are not stored in the session record and so are not restored on a
message (see `review man message`).

## Precedence: the first definition wins whole

A profile resolves to its **first** definition in this order:

1. a legacy `[<host>.<provider>.<profile>]` table for this host, in the project
   file;
2. `[<provider>.<profile>]` in the project file;
3. a legacy host table for this host, in the global file;
4. `[<provider>.<profile>]` in the global file.

The winner replaces the others **entirely**. Fields are never merged, so a
project profile that leaves out `model` does not inherit the global profile's
`model`: it runs on the provider's default. Every field of a run therefore
comes from one table you can point at.

Legacy host tables apply only on the host they name, and still beat a hostless
table in the same file there. An old project file's host tables can therefore
keep hiding the global profiles on those hosts; `review config` shows each one
next to what it hides.

## Fields

Every field is optional. An unknown key is an error, because a misspelled key
would otherwise parse as an empty profile and silently replace the global one.
Fields the selected provider does not honour are errors too, in every table
(including inactive legacy hosts). Empty `config` and `writable_roots` lists
are accepted everywhere.

- `comment`: free text shown by `review config`, never sent to a provider.
- `model`: the model to request. For maki, `provider/model-id`; without one,
  maki picks its last used model, then its configured default.
- `effort`: the reasoning effort to request. Maki profiles must omit it: maki
  accepts an effort flag and ignores it, so set its thinking level in maki's
  own config instead. A custom OpenAI-compatible model in maki only receives a
  thinking level if its maki config declares the model's thinking fields.
- `sandbox`: what the run may modify; defaults to `read-only` for codex and
  grok. Claude and maki profiles must omit it; configure the provider's own
  permissions instead. See `review man sandbox` for what each level means per
  provider, and why a maki run can write unless its permissions deny it.
- `writable_roots`: extra paths a `workspace-write` run may write, added to the
  ones `review` derives for builds, never replacing them. `~`, `$VAR` and
  `${VAR}` expand; an entry naming an unset variable is dropped with a warning.
  A non-empty list requires `workspace-write` on codex or grok (grok
  also accepts its native `workspace` alias); otherwise parsing fails.
  Remove the field or select that level. Claude and maki profiles must omit it.
  A codex `config` override of the writable roots replaces all of them,
  these included. See `review man sandbox`.
- `config`: codex only; remove a non-empty list from other providers. Each
  string is passed to codex verbatim as a `-c`
  override, after `review`'s own, so it can restate one deliberately - which
  includes the settings behind the sandbox guarantees. Codex runs ignore the
  operator's own codex `config.toml`, so anything an override refers to, such
  as a custom `model_provider`, must be defined by overrides here too.
- `env`: environment variables set for the provider process. They override
  the defaults `review` sets for a provider, but not `PWD`, which is always
  the project root the provider runs in. The session log
  and `review config` show only their names, since values can carry secrets.
  A codex incident bundle keeps values too, except for names that look
  secret, so it can replay the run.

## What a run records

Each run records its prepared permissions, effective writable roots, and model
and effort selectors. Claude and maki record no sandbox level; an old grok
session with no known selector keeps it unknown. A level is not a certificate
of filesystem enforcement: raw codex config can change permissions.

Model and effort are launch selections, separate from the provider-reported
served model. maki reports no served model, so none is recorded for it. Its
reported cost is recorded as maki gives it, which leaves out any turn maki has
no price for, subagents included; a cost of zero is recorded as unknown, since
maki reports zero for an unpriced model. If neither a dedicated field nor raw config sets a selector,
it stays unset and the provider chooses. Codex precedence is explicit: a
profile's `model` beats raw `config` entries for `model`; otherwise the last
raw `model` entry is recorded. The last raw `model_reasoning_effort` entry
beats `effort`. Those effective selectors are inherited on a message even
though raw config is not stored or replayed. Final non-string TOML values for
these two raw overrides are errors: change them to strings or remove them.
Other raw config is still validated by codex. Existing session rows remain
readable; historical rows retain the selectors originally recorded.
