# review

`review` is a command-line tool that hands a prompt to a fresh, headless AI
coding session and prints the answer. You pipe in instructions; it starts a
new session on Claude Code, Codex, Grok or maki, and the agent reads the code
itself. Use it for code reviews, second opinions, and with a write profile,
for fixes.

Built with LLMs. See [LLM.md](LLM.md).

## Why fresh sessions

Every run starts a clean session. Reviving a long-lived session on a cold
prompt cache reprocesses its whole history, which only grows; a fresh session
costs about one review's worth of tokens. Each run prints its session ID, so
you can follow up with `review message ID` while the cache is still warm.

`review` is built to be driven by other agents as much as by people:

- All output is plain text, and the exit status says whether the run
  produced an answer.
- When a run ends, everything its agent started is torn down, and anything
  that survives is named.
- A running agent is steered with `review message`, never by killing
  `review`.

## Install

`review` is a Rust crate. From a checkout:

```sh
cargo install --path .
```

It drives whichever harnesses are on your `PATH`: `claude`, `codex`, `grok`
or `maki`. `review list --all` shows which ones this host has.

## Quick start

Create the project config in your repository. It holds the project's audit
id, and can hold its archetypes:

```sh
cd /path/to/your/project
review init
```

Define profiles once, in the global config at
`~/.config/review/config.toml`. A profile is a tier of work with one entry
per harness that can serve it:

```toml
[_defaults]
profile = "review"
prefer = ["codex", "claude"]

[profiles.review]
comment = "Standard reviewer."
codex = { model = "gpt-6.2-sol", effort = "low", sandbox = "read-only" }
claude = { model = "opus" }
```

Give the project its archetypes, the priming prompts for each kind of
review, in its `.review.toml`:

```toml
[archetypes]
security = "You are a security expert for this project. Read the codebase."
bugs = "You hunt for edge cases and correctness bugs."
```

Then run it:

```sh
echo "look for auth boundary violations" | review -a security
echo "second opinion please" | review -a bugs --harness claude
echo "what's the worst of those?" | review message SESSION_ID
review list
```

`review list` shows the archetypes and profiles you can use here, with the
harness each profile would pick.

## Documentation

`review --help` is the complete short form. `review man` lists the longer
topics, which are the same pages as this repository's [docs](docs/):

- **`run`**: a fresh run, from stdin to exit status
- **`profiles`**: profile fields and how a run picks its harness
- **`config`**: the config files, how they combine, and their errors
- **`message`**: steering and continuing a session
- **`sandbox`**: what a run can touch on each harness
- **`sessions`**: what is recorded and how to query it
- **`failures`**: what a failed run tells you, and what to do
- **`projects`**: acting on another project with `-C`

Changes that affect how you use `review` are in
[CHANGELOG.md](CHANGELOG.md).

## License

[MIT](LICENSE)
