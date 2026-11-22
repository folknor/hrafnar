# Teardown: nothing a run started outlives it

Every harness process `review` starts runs under a keeper (`src/keeper/`),
launched through `contained::spawn`, the only spawn site for a harness. This
page holds what the code can't show on its own: the protocol that spans
`review`, the keeper and the harness, and the harness and kernel facts the
design rests on. The rules for each piece live in the `//!` docs of
`src/keeper/mod.rs`, `src/contained.rs` and `src/supervisor.rs`.

## Harness behaviour the keeper exists for

A process group can't hold a codex run's descendants. In the codex source
(`research/codex`):

- `linux-sandbox/src/bwrap.rs` passes `--new-session` to bwrap.
- `linux-sandbox/src/linux_run_main.rs` forks bwrap's child with
  `setpgid(0, 0)`. Its `PR_SET_PDEATHSIG` ties that child to
  `codex-linux-sandbox`, and nothing ties `codex-linux-sandbox` to codex.
- `utils/pty` calls `setsid()` for every unified-exec PTY.

An agent's shell is therefore in a session and group of its own, and when
codex exits it is orphaned to init with write access to the project. Hung
scripts left this way have run for hours after `review` exited 0.

The escalation order follows two observed behaviours:

- `codex-linux-sandbox` forwards a SIGTERM to its bwrap child and waits for
  it, then removes the empty placeholder files and directories it created in
  the workspace as mount targets. Killed before its child, it leaves them in
  the tree, so the kill rounds take leaves first.
- A parent that forks a new child whenever its last one dies, such as a shell
  loop around `sleep`, is never a leaf. Leaves first alone kills its children
  round after round, so leaves first lasts only half the kill phase.

## Kernel facts the proof rests on

The keeper's verdict depends on these Linux behaviours:

- **Subreaper**: with `PR_SET_CHILD_SUBREAPER`, an orphan in the tree
  reparents to the keeper, whatever it did to its session or group.
- **`ECHILD` is the only proof of emptiness**: the keeper is the nearest
  living ancestor of every process in its tree, so it has a child while any
  of them lives.
- **`/proc/<pid>/task/<tid>/children` is unreliable**: the kernel documents
  it as racy while siblings exit, a leader that called `pthread_exit` has no
  readable task list, and without `CONFIG_PROC_CHILDREN` the file is absent
  (the keeper scans `/proc` instead). Discovery only finds signal targets.
- **pidfds pin identity**: a process whose pidfd shows it alive after its
  parent was read from `/proc/<pid>/stat` held its pid throughout, so the
  read named the right parent. pidfd liveness is also correct for a zombie
  leader whose other threads still run, where `/proc` state is not.
- **Linux 5.3 or later** is required, for `pidfd_open` and
  `pidfd_send_signal`. A keeper that can't open a pidfd for the harness it
  forked kills it and fails the launch.
- **A `/proc` read can block** on a process in uninterruptible sleep holding
  its memory lock, such as one on a hung FUSE or NFS mount. Deadlines are
  checked between reads, so such a read can hold the keeper past them.
- **SIGKILL can't end everything**: a task in uninterruptible sleep, or one
  the keeper may not signal, is reported as surviving.

## Designs the environment rules out

Two alternatives fail on facts outside the code:

- **A cgroup per run**: `cgroup.kill` is the stronger primitive, but it needs
  a writable, delegated cgroup. On the operator's hosts `review` runs in a
  logind session scope (`/user.slice/user-UID.slice/session-N.scope`), which
  is root-owned and not delegated, and a systemd user scope needs a reachable
  user manager. A backend that never runs where it matters is not
  containment.
- **A pre-main keeper hook**: an `.init_array` constructor dispatching before
  `main` would let the unit-test binary act as keeper. Rust documents `std`
  before `main` as unsupported, the default panic hook can block on a full
  stderr before `catch_unwind` runs, and other constructors can start
  threads first. That is why the crate is a library plus a thin binary: the
  runner tests are integration tests using `CARGO_BIN_EXE_review` as their
  keeper, since a bin crate's unit tests don't get that path.

## The protocol across the three processes

`review` spawns `review __keep SPEC_FD` with SIGINT, SIGTERM, SIGCHLD and
SIGPIPE blocked; the mask survives exec, so no signal takes its default
action before the keeper reads them from a signalfd. The control and events
pipes are private to the keeper once it starts; the harness inherits
neither.

A launch proceeds in this order:

1. `review` records the keeper's pid in the in-flight marker, then writes
   `g`. The keeper forks nothing before `g`, so a keeper whose owner is gone
   first launches nothing.
2. The keeper forks the harness with its exec status on a close-on-exec pipe
   and sends `spawned` when that pipe closes with no errno. This is an
   acknowledgement, not proof of exec: a child killed before its exec closes
   the pipe too. A startup deadline fails a harness that never execs, so a
   stuck exec can't hold the host-wide launch lock.
3. `exited` means the harness itself was reaped. It can arrive before or
   after `spawned`.
4. The teardown runs, then the keeper sends one `survivor` per live process,
   one `killed` per ended process, and `done` with the verdict and both
   counts. Events are queued and written on writability, so a slow reader
   never blocks a teardown.
5. The keeper exits; `review` reaps it.

`exited` and the keeper's exit are separate moments, and the difference is
load-bearing. The codex watchdog judges whether codex refuses to exit, so it
waits on `exited` (`Contained::wait_harness`) and only then the teardown.
Waiting on the keeper alone makes a leftover that needs the SIGKILL rounds
look like a stranded codex. The in-flight marker stays until the keeper is
reaped and recording is done, so a `review message` arriving mid-teardown
finds the session in flight instead of launching beside its leftovers.

An interrupt reaches the harness two ways: `i` on the control pipe from the
owning `review`, or SIGINT to the keeper from `review interrupt`, which reads
the keeper's pid from the marker. Either way the keeper forwards one SIGINT
through the harness's pidfd, only while the harness runs. `done` reports
whether that delivery succeeded, not whether the harness handled it.

If `review` dies, the control pipe's EOF (or EPIPE on events) tells the
keeper to tear down. It appends its outcome to `teardowns.jsonl`
(`teardowns-private.jsonl` for a private project) beside the sidecar logs,
best-effort; nothing reads that log back. An outcome that reached the events
pipe in the moment before `review` died is in neither place. While such a
keeper is still tearing down, the marker reads as cleanup pending and
`review message` and `review interrupt` refuse the session; once the keeper
is gone that protection ends.

## What stays outside every tree

Only descendants are contained:

- Processes started through `systemd-run`, D-Bus activation, or anything
  else that asks another process to spawn them are outside every keeper's
  tree.
- Daemons an agent starts, such as a build cache server, are its leftovers:
  they end with the run, and the next run that wants one starts it again.
