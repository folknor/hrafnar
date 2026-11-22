# A fresh run

`echo "<instructions>" | review [-a ARCHETYPE] [-p PROFILE]`

Every run starts one **fresh session** on one provider. Nothing
carries over from an earlier run: if the new session needs context, the
instructions have to state it. To continue a session instead, use
`review message <ID>` while its cache is warm (see `review man message`).

## Input

Instructions come on stdin and are required, up to 20 KB. A pipe and a heredoc
are the same thing to `review`:

```
review -a security <<'EOF'
Audit the session handling in src/auth.
EOF
```

Quote the heredoc delimiter (`'EOF'`) so the shell leaves `$`, backticks and
backslashes in the text alone. With no arguments and nothing on stdin, `review`
prints its help and exits 2.

## Archetypes

`-a` names what to prime the session with. An archetype is a name mapped to a
prompt in the config, prepended to stdin with a blank line between.

- no `-a`: stdin is sent unchanged, recorded under the archetype label `bare`.
- `-a bare`: an unprimed run, whether or not `bare` is in the config.
- `-a security`: the `security` archetype.

`-a` takes one name; a comma-separated list is refused. An unknown name is an
error listing what is configured. The old positional form (`review security`)
still works with a warning.

## Profile

The profile names the provider and its settings: `-p deep`, else
`[_defaults].profile` in the config (see `review man profiles`). A run with
neither, or naming a profile no file defines, errors before launching. So does
a profile whose provider is not installed on this host; `review list --all` shows
which are installed. `--provider` was removed and is refused with that advice.

## Several sessions

One run is one session. For several archetypes or profiles, launch
several `review` calls side by side, each with the same stdin. Runs on one host
queue behind a host-wide launch lock, which each holds for a fixed spacing
after launching (or until it ends, if sooner), so calls fired together reach
their providers spaced apart rather than all at once. A long run never blocks
other `review` calls beyond that spacing.

## Dry run

`--dry-run` prints the assembled prompt and launches nothing. It warns, rather
than fails, when the provider is not installed.

## Output

A `--- <provider> ---` header, the `session: <ID>` line, any digest, then the
response. `review --help` lists the digest fields. The output ends with a
`runtime:` line, which is wall clock including the wait for stdin and the
launch lock.

## Auto-resume

A codex run that dies mid-turn without a final answer is resumed once,
immediately, on the same profile and session, with a short nudge to finish. If
that produces an answer it is reported, prefixed
`(auto-resumed after the initial run died without a final answer)`; if not,
the original death is reported - unless the resume was itself interrupted, in
which case the resume is reported, so its continue command is shown. A run whose provider stated why it failed, and
a run that was interrupted, are never auto-resumed (see `review man failures`).

## Exit status

`0` when the run produced an answer and nothing it started is still running,
and for a `--dry-run`. `1` when it produced no answer - including a run that
died, stalled, stated a failure or was interrupted - and on any error before
launch. `3` when anything a run started was left running,
or its teardown could not be confirmed, whatever the answers were (see
`review man failures`): it outranks `1`, because those processes may still be
changing the tree and need checking first. `2` for invalid arguments, and
for bare `review` with nothing on stdin. A run that failed still prints its session ID when the
provider created one, so it can be inspected (`review sessions <ID>`) or
continued.

## Long runs

Runs can take hours. A harness that stops background commands after a timeout
signals `review`, which tries to end its codex and claude turns gracefully and
record them within a bounded wait, so the sessions can be continued with
`review message`. A second signal, SIGKILL, or a turn that does not stop in
time loses the record. Launch with the largest
timeout the harness allows, and never stop or kill `review` to steer an agent:
use `review message` (see `review man message`).

When no response text was produced, the placeholder describes the settled
outcome: a stated failure, interruption, termination, verification failure, or
no final answer. Process status and diagnostic stderr remain in the output.
