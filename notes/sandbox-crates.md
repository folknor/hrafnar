# Sandboxing a whole harness: survey of Linux sandbox crates

Goal: run a harness (Claude Code, Codex, Grok, maki) with its own permission
gates fully off, inside a sandbox that keeps the host safe and leaves the
project checkout writable in place. Linux only. The question was whether
anything on crates.io gets us meaningfully beyond bubblewrap.

Judged on mechanism and code quality only. Age, stars and download counts are
deliberately left out: they say nothing about whether the isolation holds.

Evidence came from crates.io, docs.rs and the source repos read via `gh api`.
Nothing was built or run. Treat claims as leads to verify, not facts.

## What the 20 crates actually are

| Mechanism | Crates |
|---|---|
| Not a sandbox | embacle (env clearing only), milo-guards (command classifier), agentforge-guard (argv policy, routes to Docker) |
| Shell out to bwrap | cocoon-sandbox, astrid-workspace, heimdall-sandbox, crabpot, ai-jail, koda-sandbox, ag-harness, ultron-sandbox |
| Raw namespaces / Landlock / seccomp, no bwrap | zandbox, cantainer, seabox |
| KVM without a guest kernel | landstrip (syscall broker to host kernel), nixvm (userspace kernel, gVisor-style) |
| Full microVM | kumoba / kumobad (libkrun), kombuse-microvm (Firecracker or libkrun) |
| Harness drivers, i.e. hrafnar's siblings | harnessmux (via ACP), embacle |

## Per crate

### Not sandboxes

- **embacle**: `src/sandbox.rs` is `env_clear`, re-add allowlisted keys,
  `current_dir`. No kernel isolation despite SECURITY.md calling it that. Really
  an LLM runner over 12 agent CLIs.
- **milo-guards**: says "Not a sandbox" itself. Pure classifier: tree-sitter
  shell parsing (POSIX, PowerShell, cmd), WHATWG URL checks (catches
  `http://2130706433/`), path and secret checks. Unreadable input is Risky,
  never Safe. Careful code; could feed the digest with risky-command flags.
- **agentforge-guard**: argv normalisation and deny / approve / route / allow
  rules; routed commands go to `docker run --network none --read-only
  --cap-drop ALL`. Per command, advisory, needs Docker. Ideas: policy-file
  trust pin (any edit denies all until re-trusted), single-use approval tokens
  bound to exact argv, refuses to run itself.
- **harnessmux**: drives harnesses over the Agent Client Protocol, maps
  `--permissions` onto each harness's own modes, `--deny-command` is
  best-effort parsing. Ideas: ACP as a uniform driver, `run --record` /
  `replay` as a fake agent for tests, failure classes (`rate_limited`, `quota`,
  `auth_required`, `overloaded`) with `resets_at` and `retryable`.

### bwrap wrappers

- **cocoon-sandbox**: `bwrap --unshare-all --die-with-parent` per mount, plus
  rlimits only, "GPU isolation" via env vars, post-exit symlink checks, a
  10ms poll loop for timeouts. Nothing to take.
- **astrid-workspace**: `--ro-bind / /`, bind the workspace, tmpfs `/tmp`,
  `--unshare-all --share-net`, hidden paths covered by empty tmpfs. Competent;
  close to what we already do.
- **koda-sandbox**: `--ro-bind / /` and the non-proxied path has no
  `--unshare-*`, no `--cap-drop`, no `--die-with-parent`. Fails closed when bwrap
  is missing. Stage-2 helper brings `lo` up and bridges TCP to a Unix socket.
  Shallow.
- **ultron-sandbox**: tries bwrap, then `unshare --user --map-root-user`, then
  Docker; in `Mode::Auto` silently runs unsandboxed if all fail. The failure
  mode to avoid. One trick: ties a Docker container's life to its client via a
  `setpriv --pdeathsig` stdin pipe and an EOF watcher inside.
- **heimdall-sandbox**: bwrap for namespaces and mounts, then bwrap re-execs
  `heimdall-sandbox __heimdall-inner-exec`, which applies Landlock read grants
  and execs the payload. Also `PR_SET_DUMPABLE 0`, strips `LD_*`, exposes agent
  sockets (SSH, GPG) only on request, write-protects `.git` and agent config
  dirs by default. Bundled ONNX models are scope creep.
- **ag-harness**: sandbox for its own Bash tool only, not separable. bwrap
  `--unshare-all --uid 0 --cap-drop ALL --as-pid-1 --die-with-parent`, `--tmpfs
  /` unless full-disk read, seccomp passed via memfd `--seccomp <fd>`, re-entered
  init helper applies Landlock by raw syscalls. Refuses Landlock below ABI 3
  (no truncate control). Checks each write grant's dev/ino against host
  validation to close a path-swap race. Git-aware grants: objects, refs, logs
  writable; hooks, config, `commondir`, `rebase-merge` protected.
- **ai-jail**: CLI only, GPL-3.0. bwrap plus Landlock (applied before exec'ing
  bwrap, inherited) plus seccomp plus rlimits. tmpfs `$HOME` per launch;
  network off / full / filtered via a CONNECT proxy over an in-netns bridge;
  phantom secrets; egress log. Has escape tests with a C helper. Trusts
  `BWRAP_BIN` only if root-owned and not world-writable. README says it is
  not a boundary against hostile code.
- **crabpot**: Rust rewrite of Anthropic's `sandbox-runtime` (Claude Code's
  `/sandbox`), with a differential harness running upstream beside it. bwrap
  plus its own binary re-entered as PID 1 in a nested pid and mount namespace:
  reaps, forwards signals, installs seccomp, relays TCP to a host Unix socket.
  Seccomp: `socket(AF_UNIX/AF_VSOCK)` EPERM; io_uring refused; `unshare`/`clone`
  with NEWUSER or NEWNS, `setns`, mount family EPERM; `clone3` ENOSYS so libc
  falls back; x32 and foreign arches killed. Domain-filtering HTTP and SOCKS
  proxy, mandatory denies for shell rc files, git hooks, nested git config.
  The only real embeddable library of the bwrap group, but its policy model is
  srt's per-command settings JSON, not whole-process.

### Raw kernel primitives

- **zandbox**: embeddable, deps only `libc` and `caps`. `pre_exec`: user, mount,
  pid, net, ipc, uts namespaces with double fork and reaper, cap drop,
  hand-rolled Landlock and seccomp (handles x32), cgroup v2 pids / memory / cpu.
  Allocations happen before fork; child issues only syscalls. No mount
  reshaping: can deny `$HOME` but not hide it. Network all-or-nothing; no
  Landlock ABI 4 net or ABI 5 ioctl rules. Ideas: `strict(true)` errors when the
  kernel lacks a primitive instead of skipping; `Report` / `Applied` lists what
  was and was not applied; race-free cgroup join by the child writing `0` to
  `cgroup.procs` before exec.
- **cantainer**: Landlock plus seccomp (`landlock`, `seccompiler`), no namespaces.
  Per-program TOML manifests with per-path R / RW / RX. Below Landlock ABI 4,
  network blocked by seccomp only. No private `/proc`. Ideas: nested policy
  intersection (child gets at most parent's rights, passed via env); ELF
  resolution grants exactly the interpreter and needed `.so` files; writable
  files pre-created with `O_EXCL` because Landlock cannot grant a path that
  does not exist yet.
- **seabox**: Landlock, raw namespaces and mounts, hand-written netlink for
  loopback, seccomp denylist of 13 syscalls. Two bugs: the filter checks
  `AUDIT_ARCH_X86_64` but never rejects the x32 bit, so denied calls are likely
  reachable via x32 numbers; loopback is set to 1.0.0.127 on little-endian
  because a host-order u32 is copied as if it were network order. Not
  trustworthy as a boundary. Idea: `SECCOMP_RET_USER_NOTIF` with the listener fd
  sent to the parent over `SCM_RIGHTS`, so the parent logs each blocked syscall
  and replies EPERM.

### KVM tier

- **landstrip**: payload ELF loaded into a KVM guest with no kernel; the
  syscall instruction enters a stub that exits to the VMM, which resolves
  paths against policy, checks socket endpoints, translates guest addresses and
  forwards the call to the host kernel. Refuses io_uring, mount family, x32.
  Payload also runs in its own user, pid and mount namespaces. rust-vmm crates
  (`kvm-ioctls`, `vm-memory`), `goblin`. Per-path and per-endpoint rules on every
  syscall: per-host network allow-lists without an HTTP proxy, which bwrap
  cannot do. Ships a `selftest` that probes the sandbox against the real
  kernel. CLI, LGPL-2.1+. Needs `/dev/kvm` (no nested-virt-less VMs). Risk: a
  hand-written broker, every missed syscall is an escape or an
  incompatibility. Has opencode and pi plugins, so JS agents may run; node
  compatibility unverified. The one crate here that goes truly beyond bwrap.
- **nixvm**: userspace Rust kernel; guest syscalls trap into its own dispatch
  table. CPU via software interpreter, KVM, or HVF. TOCTOU-safe host passthrough
  (dirfd walk with `O_NOFOLLOW` per component). Embeddable builder API, no
  privileges on the interpreter path. Too incomplete for node today: no custom
  signal handlers, dynamic linking and multi-process on hardware backends
  pending, big kernel lock. Ideas: the passthrough confinement algorithm; an
  ENOSYS ledger reporting what was unsupported after a run.
- **kumoba / kumobad**: libkrun microVM per sandbox, one worker process per VM,
  vsock JSON protocol. Root is a copied ext4 image; no live host-directory
  share documented, which rules it out for in-place checkout edits. Guest has
  no NIC; egress via an allowlisting proxy that refuses private addresses and
  logs every connection. Idea: secrets never enter the VM; the proxy
  terminates TLS with a per-daemon CA and injects credentials for named hosts.
- **kombuse-microvm**: Firecracker via `api.sock`, or libkrun `dlopen`ed in a
  worker that confines itself with Landlock and seccomp first. Pinned
  `allnoconfig` guest kernel, static musl init. Job drive built with `mke2fs -d`,
  never mounted on host. No guest NIC unless egress via a vsock proxy. README
  says most of it is not built yet.

## ai-jail's `docs/sandbox-alternatives.md`

Its decision: stay on bwrap, maybe add Landlock as a second layer. Landlock
cannot replace bwrap (no namespaces, bind mounts, hostname) but is about 50
lines as a second barrier. Birdcage restricts the caller, not a launched child.
Extrasafe dismissed as small marginal benefit. A Rust reimplementation of bwrap
rejected: 500+ lines of security-sensitive edge cases without upstream fixes.
firejail (setuid, CVEs), nsjail, minijail, systemd-nspawn rejected.

Where it falls short for us: it never considers the VM tier, which is the only
real "beyond bwrap" option. The seccomp dismissal is wrong: bwrap does not stop
nested user namespaces, io_uring or x32 inside the sandbox, which is what
crabpot's filter exists for; ai-jail later added seccomp itself. The
no-rewrite argument holds.

## Where this points

Without privileges, keep our own bwrap argv and add:

1. Re-exec hrafnar inside bwrap as PID 1: reap, forward signals, apply Landlock
   and seccomp last, exec the harness. Fits the keeper model; PID 1 exiting
   kills the tree.
2. crabpot's seccomp list as the baseline, passed via memfd `--seccomp <fd>`.
3. Landlock as a second layer so denials are loud EACCES rather than hidden.
   Require ABI 3 or newer. Strict mode: a missing primitive is an error.
4. Filtered egress: unshared netns, helper brings up `lo` and bridges to a
   bind-mounted host Unix socket where a proxy filters by domain.
5. Phantom secrets: the sandbox sees a placeholder API key; the host proxy
   swaps in the real one for the bound host only.
6. Seccomp user-notif so the digest can name what the sandbox refused,
   alongside the permission-gate denials it already reports.
7. Default write protection for `.git` hooks and config and the harness config
   dirs. Trust only a root-owned, non-world-writable bwrap. Never fall back to
   unsandboxed.

Truly beyond bwrap means KVM: landstrip (syscall broker) or libkrun (full
microVM, virtio-fs for a live checkout share; muvm and krunvm already do host
dir sharing). Next step there is a hands-on probe: does a node harness run
under landstrip at all.
