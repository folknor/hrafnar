# review

A Rust CLI that sends a prompt to a fresh AI session on one of several providers (Claude Code, Codex, Grok, maki) - for reviews, and with a write profile for fixes.

Built with LLMs. See [LLM.md](LLM.md).

## How it works

You pipe your instructions via stdin. The tool starts one **fresh session** on one provider and lets the agent fetch code itself; it bakes in nothing. Optionally, an **archetype** -- a reviewer perspective like `security`, `bugs`, or any custom name, mapped to a priming prompt -- is prepended, and the archetype prompt carries its own grounding (role, whether it may modify files, "inspect current state").

Every run is a clean session by design. Reviving a long-lived session on a cold prompt cache means reprocessing its entire accumulated history - which only grows - whereas a fresh session costs roughly one review's worth of tokens each time. Every provider prints the new session ID above the response, so you can follow up while the cache is still warm with `review resume <ID>`.

## Quick start

### 1. Initialize

```
cd /path/to/your/project
review init
```

### 2. Configure

Settings shared by every project - the profiles naming which provider and
model serve which tier, and the default one - go once in the global
`~/.config/review/config.toml`:

```toml
[_defaults]
profile = "deep"

[profiles.deep]
provider = "codex"
model = "gpt-6-luna"
effort = "max"
sandbox = "read-only"
```

Project-specific archetypes - a name mapped to a priming prompt - go in the
project's `.review.toml`:

```toml
[archetypes]
security = "You are a security expert for this project. Read the codebase."
bugs = "You hunt for edge cases and correctness bugs."
```

`review config` shows the effective result and which file each value came from.

### 3. Run

```
echo "what does the retry loop guarantee?" | review -p deep
echo "look for auth boundary violations" | review -a security
echo "check for edge cases in the parsing module" | review -a bugs -p deep
echo "second opinion please" | review -a bugs -p opus
```

## Usage

```
echo "<instructions>" | review [-a ARCHETYPE] [-p PROFILE] [--dry-run]
echo "<instructions>" | review resume <SESSION_ID> [--dry-run]
review config
review sessions [ID] [--all] [--limit N]
review incidents [--limit N]
review init
```

Instructions are piped via stdin (required, 20KB limit). Without `-a`, stdin is sent unchanged - no priming prompt.

One run is one session. For several perspectives or providers, launch several `review` calls side by side; launches on one host are spaced apart automatically (see [Rate limits](#rate-limits)).

All output is plain text, for people and agents alike. There is no JSON mode.

### Archetypes

Archetypes are optional named priming prompts defined under `[archetypes]` (name = priming prompt), in the project's `.review.toml` or the global config; the project wins on a name clash. Any name works except reserved names, which fail to parse: the subcommand names (`message`, `resume`, `interrupt`, `config`, `sessions`, `incidents`, `init`, `man`, `help`), and `bare` with any non-empty prompt. `bare` is the special no-prime selector: `-a bare` works even without a config entry, and an archetype definition is allowed only as `bare = ""`.

### Options

| Flag | Description |
|------|-------------|
| `-a, --archetype <name>` | Archetype to prime with, or `bare` (an unprimed run, even without a config entry). Omit to send stdin unchanged. |
| `-p, --profile <name>` | The profile to run: `[profiles.<name>]`, naming the provider and its `model`/`effort`/`sandbox`/`env`. Project config first, then global. Overrides `[_defaults].profile`. |
| `--dry-run` | Print what would be sent instead of sending it |

Each run starts a fresh session, prepends the archetype's priming prompt (if any) to your stdin, and lets the agent fetch code itself. The profile, from `-p` or `[_defaults].profile`, decides the provider. A profile whose provider is not installed on this machine fails the run before anything launches - `review config` shows which are installed.

How each provider is launched, and why, is in [reference/claude.md](reference/claude.md), [reference/codex.md](reference/codex.md), [reference/grok.md](reference/grok.md) and [reference/maki.md](reference/maki.md). Every provider captures the new session ID: claude, grok and maki get one generated up front, codex's is parsed from its `thread.started` event. Grok is the one provider that takes no prompt on stdin, so `review` writes the assembled prompt to a temp file and passes `--prompt-file`.

### Upgrading from the positional archetype

The archetype used to be positional (`review security`) and resuming used `--session <ID>`. Both still work, with a warning, so scripts and orchestration procedures keep running until they are updated: `review security` behaves like `review -a security`, `review bare` like plain `review`, and `--session <ID>` like `review resume <ID>` (any `--profile` given with it is ignored).

### Profiles

A profile is a provider plus its `model`, `effort`, `sandbox`, and `env`, selected with `-p`/`--profile` or `[_defaults].profile`. They are `[profiles.<name>]` tables, and the natural home for them is the global config, so a new model release is one edit rather than one per project:

```toml
[profiles.opus]
provider = "claude"
model = "Opus 4.8"
effort = "medium"
env = { ANTHROPIC_BASE_URL = "http://localhost:8787" }

[profiles.implement]
provider = "codex"
model = "gpt-5.6-terra"
effort = "high"
sandbox = "workspace-write"
```

```
echo "audit the auth flow" | review -a security -p opus
```

`-p opus` resolves `[profiles.opus]`: the project's `.review.toml` first, then the global config. A project profile replaces the global one of the same name **entirely** - nothing is merged field by field, so a project that overrides only `model` also drops the global profile's `sandbox` - but it may not name a different provider. If no file defines the profile, the run errors naming every file it searched.

`sandbox` takes one of `review`'s three levels -- `read-only`, `workspace-write`, `danger-full-access` -- and defaults to `read-only` when unset, so a bare codex or grok run can never modify files. **Claude and maki profiles must omit it** -- each runs on its own permission configuration, a tool-approval policy on a different axis with no honest mapping. maki's default permissions let it edit the project; see `review man sandbox`.

The levels are `review`'s own vocabulary, translated per provider at launch, because the providers do not agree on the names and grok *refuses to start* on one it cannot resolve:

| `review` level | codex `--sandbox` | grok `--sandbox` |
|---|---|---|
| `read-only` | `read-only` | `read-only` |
| `workspace-write` | `workspace-write` | `workspace` |
| `danger-full-access` | `danger-full-access` | `none` |

A value `review` does not recognise is passed through to the provider verbatim, so a custom grok profile defined in `~/.grok/sandbox.toml` still works; the provider validates its own vocabulary and fails before running a turn if the name is wrong.

### Follow-up with `review resume`

`review resume <id>` continues a specific provider session and sends raw stdin - no prime prepended. The grounding is already in the session's history from the run that created it.

```
echo "what's the worst of those for a single-account user?" | \
  review resume 019deabc-0def-7000-8000-abcdef012345
```

There is no `-p`: the sidecar records which provider owns each session, and `resume` uses that. A session this host has no record of is refused - `review sessions` lists the ones it knows:

```
$ echo "follow up" | review resume 019deabc-...
Error: no record of session 019deabc-... on this host
  `review sessions` lists the sessions this host's review runs created.
```

Other constraints:

- No archetype and no profile - no prime and no profile overrides are applied. The sandbox level, writable roots, model and effort are inherited from the session's own recorded run instead.
- Validation of the session ID itself is delegated to the provider.

### Sessions sidecar log

Each run that captures a session ID and each `review resume` appends a JSONL row to `~/.local/share/review/sessions.jsonl` (or `sessions-private.jsonl` when `audit.private = true`). Rows carry:

- `timestamp` (UTC), `epoch_secs`, `project` (root path), `hostname`
- `audit_id`, `provider`, `archetype`, `session_id`
- `kind` - `"run"` for fresh-session creation events, `"session"` for resume touches
- `model`, `env_keys` (env-var *names* only - values are not recorded so secrets don't leak through the sidecar)
- `operator_prompt` (raw stdin), `assembled_prompt` (what the provider actually saw)
- `response` or `error`
- `review_version`

The sidecar drives two things:

**1. Cache-age gate on `review resume`.** When you resume, `review` looks up the last touch and prints how long it's been:

```
$ echo "follow up" | review resume 019deabc-...
provider: claude (from the session record)
session last touched 14m ago
--- claude ---
<response>
```

Resuming is the *warm* follow-up path. If the session last ended over 55
minutes ago - past the longest realistic prompt-cache TTL - the cache is cold,
and resuming would reprocess the whole session prefix at full cost. So `review`
**refuses** it and tells you to do a fresh run with restated context instead:

```
$ echo "follow up" | review resume 019deabc-...
Error: session last touched 1h17m ago - its prompt cache is cold.
  Resuming would reprocess the whole session prefix at full cost.
  Start a fresh run with restated context instead.
```

**2. `review sessions` listing.** Aggregates by `session_id` and shows recent sessions for the current project (or `--all` projects), most recent first:

A turn that is running right now appears first, so a long run is
distinguishable from an idle session:

```
$ review sessions
[in flight] codex / turn in flight since 6m
       session: 019f5f70-b2ca-7590-8a61-be66d9d7cf07

[14m ago] claude / bugs (run) / 3 touches
       session: 019deabc-0def-7000-8000-abcdef012345
       opened:  review the new sync code

[1h12m ago] codex / security (run) / 1 touch
       session: 019d0123-...
       opened:  check OAuth handling on the IMAP path
```

Each block shows the age since the last touch, the provider/archetype/touch count, the session ID (copy-paste into `review resume <id>`), and the operator prompt that opened the session. `--limit <N>` caps the row count (default 20).

For ad-hoc queries beyond what `review sessions` exposes, the JSONL works directly with `jq` and `grep`.

### Output format

```
--- claude ---
<review content>

--- codex ---
<review content>
```

Codex runs (both fresh and resumed) also print a digest above
the message, distilled from codex's own `--json` event stream plus the
`-o`/`--output-last-message` backstop:

```
--- codex ---
session: 019f5f70-b2ca-7590-8a61-be66d9d7cf07
exit: 0
captured: true
turns: 1
usage: input=12244 cached=10112 output=5 reasoning=0
<review content>
```

`captured: true` means the final message came from the authoritative `-o` file
(which survives a frozen or halted stream); `false` means we fell back to the
last streamed message. Non-JSON log lines codex interleaves (ERROR/WARN,
apply_patch dumps) are printed between the digest and the message.

When a run looks wrong (`captured: false`, non-zero exit, or a signal), the
digest also reads codex's on-disk transcript and appends a post-mortem: whether
the turn reached `task_complete`, whether a `stream_error` occurred, the last
event, and the last in-flight tool call (what was running when it stopped).
Clean runs skip this.

### When codex hangs

Some codex versions can stop making progress without exiting - either after
finishing the work, or partway through it. `review` watches the rollout
transcript while a run is in flight and handles the two cases differently.

**Finished but stuck.** The final answer is already on disk and codex simply
will not exit. `review` recovers the answer from the rollout, terminates codex,
and reports normally:

```
--- codex ---
exit: -
signal: SIGTERM
captured: false
terminated by review: watchdog: stranded completion
  rollout silent for: 183s
  last rollout event: event_msg/task_complete
recovered: final answer restored from transcript (stream/-o truncated)
<the real review content>
```

This is not a timeout. It cannot fire on a run that has not already produced
its answer, however long that run takes, and it cannot fire while the rollout
is still growing.

**Wedged with nothing produced.** No answer was ever written and the rollout
has gone silent. This *is* a timeout, defaulting to 15 minutes:

```
--- codex ---
terminated by review: watchdog: stall timeout (900s silent)
  rollout silent for: 900s
  last rollout event: response_item/custom_tool_call
note: review terminated the run; the text below is whatever codex had produced by then
```

A stalled run writes a forensic bundle (see `review incidents`) and exits
non-zero, so scripts and CI see a failure rather than a silent success.

(Both digests above are abridged - the usual `turns`, `usage`, `transcript` and
`incident` lines still print alongside.)

The 15 minutes rests on codex waking itself every few minutes even while
waiting on a long tool call - not on a documented guarantee. Tune or disable it
if that ever stops holding:

```toml
[_defaults]
stall_timeout_secs = 900   # 0 disables the check entirely
```

Both are codex-only; claude has no rollout to watch, and legitimately goes
quiet while waiting on a backgrounded task.

Killing `review` (Ctrl-C or `SIGTERM`) now takes codex and everything it
spawned with it, rather than leaving it running detached.

### `review incidents`

Any codex run that looks wrong - no capture, non-zero exit, killed by a signal
- writes a forensic bundle to `~/.local/share/review/incidents/`, holding
codex's stderr (with backtraces), the raw NDJSON stream, a transcript tail, the
exact prompt, and a `meta.json` with a copy-pasteable command that replays the
run verbatim. Clean runs write nothing.

```
$ review incidents --limit 2
2026-07-31T10:22:25Z  codex 019fb7b0-cac2-7a02-b121-08af9e2bf626  exit=1  no final answer (died)  [codex-cli 0.146.0]
       /home/folk/.local/share/review/incidents/2026-07-31T10-22-25Z-codex-019fb7b0-…-f008
```

Newest first, `--limit <N>` capping the count (default 20). The verdict is one
of `recovered from transcript`, `no final answer (died)`, `completed (stream/-o
truncated)`, or `suspicious`, and the codex version is recorded because deaths
have been version-specific. The bundle path is also printed on the digest of
the run that produced it.

## Configuration

Settings resolve like a standard command-line tool: the command line, then the project's `.review.toml`, then the global config. Nothing is built in.

| File | Where | Required |
|---|---|---|
| Project | `.review.toml` in the project root, discovered by walking up to the git root. `review init` creates a starter. | Yes - it holds `[_audit]`, the project's audit id. A file with only `[_audit]` is enough. |
| Global | `$XDG_CONFIG_HOME/review/config.toml`, else `~/.config/review/config.toml` | No |

Both files take the same format; `[_audit]` is allowed only in the project file.

```toml
[archetypes]
security = "You are a security expert for this project. Read the codebase."
bugs = "You hunt for edge cases and correctness bugs."
tilemaker = "You are a tilemaker maintainer weighing tradeoffs."
tippecanoe = "You are a tippecanoe maintainer weighing tradeoffs."

[_defaults]
profile = "high"                   # used when -p is omitted
stall_timeout_secs = 900           # codex-only; 0 disables. See "When codex hangs".

# Named profiles: a provider and its model/effort/sandbox/env, selected with
# -p/--profile.
[profiles.opus]
provider = "claude"
model = "Opus 4.8"
effort = "medium"
env = { ANTHROPIC_BASE_URL = "http://localhost:8787" }

[profiles.high]
provider = "codex"
model = "gpt-6-luna"
effort = "high"
```

How the layers combine:

- **Archetypes** are the union of both files; the project wins on a name clash.
- **`[_defaults]` keys** come from the first file that sets them.
- **Profiles** come from the first definition found, project file first, and win whole. A project profile may not name a different provider than the global one of the same name.

### `review config`

Prints the effective configuration for the current directory, computed by the same resolver a run uses: the files consulted, whether each provider is installed on this machine, every archetype with its source, the default profile, and every profile with its provider, the table it came from and any definitions it shadows. Env vars show by name only.

Anything that used to read `.review.toml` to find out what is available should call this instead - the project file no longer tells the whole story. Outside a project it shows the global layer and provider availability on their own.

### Upgrading existing files

These now fail to parse, each with an error saying what replaced it:

- `[<provider>.<name>]` profile tables, and legacy `[<host>.<provider>.<name>]` ones: move each to `[profiles.<name>]` with `provider = "<provider>"`.
- `[_defaults].providers` or `[_defaults].provider`: write `profile = "<name>"` instead.
- `[_groups]`: remove it. Launch one `review -a <name>` per member.
- A profile containing a key `review` does not know - usually a typo. A project profile replaces the global one whole, so a typo that parsed as an empty profile would silently discard the global settings.

### Providers

| Provider | Binary | Non-interactive | Resume | Model flag |
|----------|--------|----------------|--------|------------|
| claude | `claude` | `--print` | `--resume <id>` | `--model <name>` |
| codex | `codex` | `exec` | `exec resume <id>` | `-m <model>` |
| grok | `grok` | `--prompt-file` | `--resume <id>` | `-m <model>` |
| maki | `maki` | `-p` | `--resume <id>` | `--model <provider/model-id>` |

To run another provider, pick a profile that names it:

```
echo "ask claude" | review -a bugs -p opus
```

## Rate limits

Provider APIs enforce rate limits across multiple dimensions - requests per minute (RPM), input tokens per minute (ITPM), and rolling usage quotas. The exact limits are not publicly documented for subscription plans, but in practice, firing multiple provider sessions simultaneously (e.g. 5 claude sessions) will trigger RPM limits.

A single Claude Code invocation generates 8-12 internal API calls through its tool-use architecture. Five concurrent sessions means 40-60 API calls hitting at once - enough to blow past most RPM budgets.

To avoid this, `review` calls fired together on one host are spaced apart automatically: each fresh run holds the host launch lock for a fixed spacing after launching (or until it ends, if sooner), so the next queued call launches that much later. All run concurrently once launched.

## Concurrency

A global file lock (`/tmp/review.lock`) serializes provider **launches**, not
whole runs. A fresh run holds it for its launch spacing, a `review message`
until its single spawn, then releases it and lets the runs proceed
concurrently. Additional invocations queue and wait for the launch window only.

That distinction is deliberate. Holding the lock for a run's full duration
meant one wedged provider froze every other `review` on the machine
indefinitely - a hung run should cost you that run, not the tool.

Note: the lock is shared across all users on the machine. On shared dev
machines, one user's launch will briefly block another's.

## License

[MIT](LICENSE)
