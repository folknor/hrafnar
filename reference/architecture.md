# Architecture

How `review` is put together: the layers, how a run moves through them, and the
rules that hold across modules. What each module owns in detail is in its own
`//!` doc comment; this document is the map that tells you which one to open.
Why the behaviour is what it is lives in [design.md](design.md), and the
per-provider mechanism in [codex.md](codex.md), [claude.md](claude.md) and
[grok.md](grok.md).

## Shape

One crate. `src/main.rs` is a thin binary with two personalities: invoked as
`review __keep <fd>` it becomes a keeper (before any tokio runtime exists), and
otherwise it hands everything to `app::run`. The library (`src/lib.rs`) exists
so the integration tests can drive the provider runners against stub providers
with the real binary as their keeper; nothing in it is a public API.

## Layers

From the operator's command line down to the process a provider runs in:

| Layer | Modules | Owns |
|---|---|---|
| Command | `cli`, `app` (`launch`, `message`, `history`), `interrupt_cmd`, `config_cmd`, `man`, `exit` | Parsing, the fan-out, the verbs, exit status |
| Config | `config` (`parse`, `resolve`, `load`, `profile`, `sandbox`, `error`), `project`, `path_expansion`, `config_write` | Layered config, `-C` project selection, the audit id |
| Provider | `provider` (`kind`, `launch`, `prepare`, `permissions`, `model`, `session`, `lifecycle`, `stream`, `outcome`, `result`, `output`), one adapter per provider: `claude`, `codex/`, `grok/` | Turning a launch request into a classified result |
| Containment | `contained`, `keeper/`, `supervisor`, `process`, `report` | Running each provider under its own keeper and proving its tree empty |
| State | `data`, `sessions/`, `inflight`, `audit`, `incident`, `transcript`, `lock`, `fsutil` | Everything `review` writes to disk and reads back |
| Shared policy | `writable_roots/`, `watchdog`, `timings`, `prompt`, `input`, `ids` | Pieces several layers use |

Dependencies point downward. The command layer picks what to run, the provider
layer runs it, containment holds it, and state records it. The provider layer
never reads the command line, and the keeper and `contained` hold any process
the same way: no provider's behaviour reaches them, only a `ProcessSpec`.

## A fresh run

`echo ... | review [-a archetype] [-p profile] [--provider p]`:

1. `app::run` parses (`Cli::parse_checked`, which also refuses run flags beside
   a subcommand), installs the process-wide signal supervisor around one
   `supervisor::Lifecycle`, and resolves the project (`project::select`) and
   config (`config::load`). Archetypes, providers and the profile are validated
   and every provider must be installed before anything launches.
2. Under the host launch lock (`lock`), `config_write::ensure_audit_id` fixes
   the project's audit id, which selects the log family (`data::LogScope`).
3. For each archetype and provider, `prompt::assemble` builds the prompt and
   `app::launch::run_launch` is spawned with a `provider::LaunchSpec`: the
   provider, a fresh `SessionStart`, the profile's `RunSettings`, the prompt
   and the root. Launches are staggered, and the lock is released once they
   have all fired.
4. `provider::invoke` prepares the adapter (`prepare::Prepared`: permissions,
   writable roots, model arguments, a caller-minted session id where the
   provider accepts one) and calls the adapter's runner.
5. The runner builds a `lifecycle::ProcessSpec` and drives `lifecycle::Run`:
   spawn the keeper (`contained::spawn`) and record it on the in-flight marker,
   launch, write the prompt, read the provider's stream, wait (with the
   adapter's kill arm, such as codex's watchdog, and the graceful-stop arm),
   then collect the keeper's teardown verdict. The adapter turns what it
   observed into a `provider::Outcome` inside a `ProviderResult`.
6. Back in `run_launch`, `after_run` decides whether a codex death is resumed
   once, and `RecordCtx` writes the audit row (`audit`) and the session row
   (`sessions::append`) as soon as the task ends, so a follow-up can find the
   session before the slowest run finishes.
7. `app::run` prints each result (`provider::print_result`), the leftover
   report (`report`) and the runtime, and exits with a `exit::CommandExit`.

## Messages and interrupts

`review message <ID>` (`app::message::run_message`) works on a session, not a
project. It takes the per-session launch lock (`inflight::LaunchLock`), reads
the session's state from markers and rows (`sessions::query`), and checks that
the recorded project still exists with the same audit id. If a codex or claude
turn is in flight it interrupts it (`interrupt_cmd`), waits for that run to
record, then resumes with the session's inherited settings through the same
`provider::invoke`. `review interrupt <ID>` is the interrupt half alone.

## Central types

- `provider::Provider` is the closed set of providers, parsed once wherever a
  name enters and matched exhaustively everywhere else, so adding a variant
  makes the compiler name every site that must decide something. Persisted
  rows keep the provider as a string, so a row naming a removed provider still
  reads.
- `provider::LaunchSpec` is the complete request; `ProcessSpec` is the exact
  process, and the same value an incident bundle replays.
- `lifecycle::Run` owns the order of the load-bearing steps; `lifecycle::Lease`
  carries facts the stream readers learn (readiness for SIGINT, a session id
  learned mid-stream) apart from the marker they also update.
- `provider::Outcome` is how a turn ended, built once by the adapter that saw
  it and read by every later decision: exit status, auto-resume, incident
  capture, printing.
- `sessions::SessionRow` is the one persisted row, written to both the sidecar
  log and (projected) the audit log.
- `data::DataDir` is every review-owned path, resolved once, so tests can
  isolate all stores by pointing it elsewhere.

## Rules that hold across modules

- **Every provider process runs under its own keeper,** launched through
  `contained::spawn`: runs, auto-resumes, grok wakes, messages and the incident
  version probe alike. A run is over only once its keeper has proved its tree
  empty. See [teardown.md](teardown.md).
- **Every runner drives the one lifecycle.** An adapter keeps its own control
  flow (argv, stream decoding, recovery, classification) and calls
  `lifecycle` for the steps whose order matters.
- **Provider-specific decisions are exhaustive matches** in the provider
  layer (`invoke`, `Permissions`, `provider::recovery`), never a check of a
  provider's name elsewhere.
- **Config loads strictly** wherever its result decides something: a file that
  exists but does not parse is an error, never treated as absent.
- **Plain text everywhere,** no JSON output.

## Tests

Unit tests live beside the code they cover, in `#[cfg(test)]` modules or a
sibling `*_tests.rs` / `tests.rs` file. Integration tests in `tests/` drive the
built binary: `tests/provider_runs.rs` runs each adapter against stub provider
scripts with the real keeper, `tests/keeper.rs` speaks the keeper protocol
directly, and `tests/man.rs` checks the bundled docs against the code.
