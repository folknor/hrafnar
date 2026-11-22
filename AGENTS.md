# review

A Rust CLI (`review`) that hands code reviews and work to a fresh, headless AI
session on one of several harnesses (Claude Code, Codex, Grok, maki), chosen
by a named profile. It is a prompt builder: the agents fetch code themselves.
Each run starts a clean session, optionally primed with an archetype's prompt,
and is recorded so it can be messaged, interrupted or resumed while its cache
is warm.

Config is layered: the command line, then the project's `.review.toml`, then
the operator's global `~/.config/review/config.toml`. `review list` prints the
archetypes and runnable profiles; `review list --all` the effective result with
each value's source.

Where to read further:

- `src/lib.rs` - the module layers and the rules that hold across them; each
  module's `//!` comment says what it owns, and each design rule and its
  reason sit on the code that enforces it.
- [reference/steering.md](reference/steering.md) - how `review message` and
  `review interrupt` coordinate on one session.
- How each harness is driven: [reference/codex.md](reference/codex.md),
  [reference/claude.md](reference/claude.md), [reference/grok.md](reference/grok.md),
  [reference/maki.md](reference/maki.md).
- [reference/sandbox.md](reference/sandbox.md) - permissions and writable roots.
- [reference/teardown.md](reference/teardown.md) - the keeper every run runs under.
- `docs/` - the `review man` topics: observable behaviour, and the binding
  config format (`docs/config.md`, `docs/profiles.md`, `docs/projects.md`).
  Change the behaviour, change the doc - and the `--help` text, which is what
  orchestrating agents read first.

## Build and run

`brokkr` is the only entry point - never raw `cargo` (see Bash rules).

```
brokkr check                    # the gate: gremlins + clippy + tests
brokkr test <name>              # one test by substring; -N <n> to repeat
brokkr run -- <args>            # run the binary
brokkr fmt                      # before every commit
brokkr man check                # bundled docs for the pipeline
```

One crate: a library (`src/lib.rs`) and a thin binary (`src/main.rs`). The
split exists for the runner tests, which need the built binary as their
keeper; nothing in the library is a public API.

## Issue tracking

Bugs, follow-ups and anything worth doing later go to `bz`, the operator's
cross-project tracker - not into `notes/todo.md`. This repo's project is named
from its `origin` remote; `-p <owner/name>` targets another. `bz help` is the
full reference; the common moves:

```
bz ls                                  open, unblocked issues here
bz show 12                             one issue with its comments
bz new "title" -t tag -b - <<'EOF'     file one, body from stdin
...
EOF
bz comment 12 "text"
bz close 12 -m "fixed by ..."
```

## Rules

### General rules

- Don't use gremlins! Em-dash, en-dash, strange quotes, whatever - they're all verboten.
- Don't remind the user of the rules. They wrote them, so they know them.
- The user can exempt you from any rule at any time.

### Bash rules

- Never read or write from `/tmp`. All data lives in the project.
- Never run raw `cargo`, `curl`, `pkill`. Use `brokkr`. **This means never, including
  during iteration.** `cargo build` / `cargo test` while working, "just to check
  quickly", is the same violation as shipping with them - `brokkr` is the only
  supported entry point, and reaching past it means running a different build
  configuration from the one that gates the commit. The mapping is total; there
  is no case that needs raw cargo:

  | instead of | run |
  |---|---|
  | `cargo build` / `cargo clippy` / `cargo test` | `brokkr check` (gremlins + clippy + tests, the full gate) |
  | `cargo test <name>` | `brokkr test <name>` |
  | `cargo test <name>` repeatedly (flaky hunt) | `brokkr test <name> -N <count>` |
  | `cargo run -- <args>` | `brokkr run -- <args>` |
  | `cargo fmt` | `brokkr fmt` |
  | `cargo install --path .` | `brokkr install` |

- `brokkr check` is the gate; run it before every commit, not just at the end.
- `brokkr test <NAME>` is a substring filter over the package's unit *and*
  integration tests, release profile by default (`--debug` for dev). It always
  passes `--include-ignored --nocapture --test-threads=1`.
- **There is a 20-second per-test watchdog**, shared by `brokkr check`'s test
  phase and `brokkr test`. A test that runs longer is killed and reported as
  hung. `brokkr test --timeout <SECS>` raises it to at most 280s, and only for a
  name that matches exactly one test. This is why fixture timings in
  `tests/provider_runs.rs` are injectable rather than real-world durations.
- `brokkr man` lists bundled docs (`man check`, `man config`, `man clippy`,
  `man run`, `man results`, ...). Read those rather than guessing at flags.

### Where notes go

- Harness-specific notes go in `reference/<harness>.md`; permission machinery
  shared across harnesses in `reference/sandbox.md`. A harness-agnostic design
  rule and its reason go on the code that enforces it; only behaviour that
  needs several modules read at once to understand gets a `reference/` page,
  like `reference/steering.md`. This file keeps the rules and pointers only.

## Document folders

| Folder | Contents | Rule |
|---|---|---|
| `reference/` | How the thing is built and why: architecture, design decisions, per-harness mechanism and evidence, invariants, protocol contracts | Citable from source as a source of truth. What it says must be true. |
| `docs/` | How the thing is used: the `review man` topics, one file each, flat | Same must-be-true rule. |
| `notes/` | Transient: plans, research, analysis. Things that will die | No truth guarantee. Nothing durable cites it. |
| `plans/` | Retired | Plan documents go in `notes/`. |

- The dependency direction is one-way: `notes/` may cite `docs/` and
  `reference/`; nothing durable may cite `notes/` - not a code comment, not
  `docs/`, not `reference/`. A code comment must carry its full context, because
  it outlives the note.
- `docs/` never links into `reference/` (an installed binary has no checkout to
  follow it into) and states policy, not measurements. `--help` stays the
  complete short form; a topic adds depth, never a requirement to read it.
- `scripts/` holds maintained diagnostics that keep a claim in `reference/`
  re-testable against a harness that upgrades underneath us. They cost real
  turns, so they are run deliberately after a harness upgrade, never from
  `brokkr check`. A one-off probe belongs in `notes/`.
- `research/` holds gitignored checkouts of harness source (`research/codex`,
  `research/grok-build`): not ours, not guaranteed present, but the authority on
  harness behaviour when they are.
- Root-level convention files (`AGENTS.md`, `CLAUDE.md`, `README.md`,
  `LICENSE`, ...) stay at the root.
- Avoid citing source line numbers in `notes/`, `docs/` and `reference/` - they
  drift fast.
