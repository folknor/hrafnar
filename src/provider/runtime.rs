/// Set once by the signal supervisor: codex runs end their turn the way `review
/// interrupt` ends one, and nothing new launches. See `install_signal_supervisor`.
static STOP: std::sync::OnceLock<tokio::sync::watch::Sender<bool>> = std::sync::OnceLock::new();

fn stop_channel() -> &'static tokio::sync::watch::Sender<bool> {
    STOP.get_or_init(|| tokio::sync::watch::channel(false).0)
}

/// Whether `review` has been signalled to stop.
pub fn stop_requested() -> bool {
    *stop_channel().borrow()
}

/// codex and claude runs not yet recorded in the sidecar - what a signalled
/// `review` waits for before it exits. See `Unrecorded`.
static UNRECORDED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Held by a caller from before it invokes the provider until after it has
/// recorded the result, so the signal supervisor does not exit between the
/// provider being reaped and the sidecar row that makes the session resumable
/// being written.
pub struct Unrecorded(());

impl Unrecorded {
    /// A guard for a run of `provider`; only interruptible providers' runs are
    /// waited for, since only their turns can be ended in a way that leaves
    /// them resumable (`Provider::interruptible`, the answer `review interrupt`
    /// reads too).
    pub fn for_provider(provider: super::Provider) -> Option<Self> {
        provider.interruptible().then(|| {
            UNRECORDED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Self(())
        })
    }
}

impl Drop for Unrecorded {
    fn drop(&mut self) {
        UNRECORDED.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Install the one process-wide handler for `SIGINT`/`SIGTERM`.
///
/// This is deliberately a single global supervisor rather than a `select!` arm
/// inside each run. Tokio's signal registration is process-wide and permanent -
/// once a `SignalKind::terminate()` stream has been created, the default
/// "terminate the process" behaviour is gone for the rest of the run, even after
/// the stream is dropped. Installing it per-run would therefore have left
/// `review` quietly ignoring every `SIGTERM` after its first codex invocation,
/// i.e. *harder* to kill than before - the opposite of the intent.
///
/// With codex or claude runs in flight the first signal is a graceful stop: each run ends
/// its turn as `review interrupt` would, records its session, and only then does
/// `review` exit. The signal is most often an orchestrating agent stopping the
/// background task it launched `review` as - asked to "interrupt the agent",
/// Claude reaches for its own task-stop tool, not for `review interrupt` - and
/// the old kill-and-exit left a fresh session with no sidecar row, so it could
/// never be resumed. A second signal, or `GRACEFUL_STOP` passing, falls through
/// to the kill below, which forfeits the digest and the row; the rollout
/// transcript is on disk regardless, for `review sessions <id>`.
///
/// Either way `review` then terminates every keeper still running (grok runs,
/// and whatever the graceful stop did not end), waits for their teardowns up to
/// `contained::Settings::terminate_bound`, and reports anything left running or
/// unaccounted for before it exits. A keeper still tearing down at that bound
/// is named as such; it finishes on its own, writing to the orphan log.
///
/// `SIGKILL` cannot be caught, so a stop that sends only that still loses the
/// row. The keepers see their control pipes close and tear down regardless.
pub fn install_signal_supervisor() {
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    tokio::spawn(async move {
        let mut next_signal = async || match sigterm.as_mut() {
            Some(term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => "SIGINT",
                    _ = term.recv() => "SIGTERM",
                }
            }
            // SIGTERM handler unavailable: still forward Ctrl-C, and leave
            // SIGTERM on its OS default (which kills us, orphaning codex - the
            // old behaviour, and better than not handling anything).
            None => {
                let _ = tokio::signal::ctrl_c().await;
                "SIGINT"
            }
        };
        let name = next_signal().await;
        let exit_code = if name == "SIGINT" { 130 } else { 143 };
        let unrecorded = || UNRECORDED.load(std::sync::atomic::Ordering::SeqCst);
        if unrecorded() > 0 {
            eprintln!(
                "\n{name}: ending {} turn(s) so their sessions stay resumable \
                 (signal again to kill instead)",
                unrecorded()
            );
            stop_channel().send_replace(true);
            let graceful = async {
                while unrecorded() > 0 {
                    tokio::time::sleep(crate::timings::GRACEFUL_STOP_POLL).await;
                }
            };
            tokio::select! {
                () = graceful => {}
                () = tokio::time::sleep(crate::timings::GRACEFUL_STOP) => {
                    eprintln!("a turn did not end in time; tearing down what is left");
                }
                _ = next_signal() => {}
            }
        }
        // No keeper spawns from here, and every live one tears down its run.
        let told = crate::contained::terminate_all();
        if told > 0 {
            eprintln!("\n{name}: tearing down {told} running provider(s)");
        }
        let bound = crate::contained::Settings::default().terminate_bound();
        let drained = async {
            while !crate::contained::live_keepers().is_empty() {
                tokio::time::sleep(crate::timings::GRACEFUL_STOP_POLL).await;
            }
        };
        tokio::select! {
            () = drained => {}
            () = tokio::time::sleep(bound) => {}
            _ = next_signal() => {}
        }
        for line in crate::contained::report_lines(&crate::contained::unresolved()) {
            eprintln!("{line}");
        }
        for pid in crate::contained::live_keepers() {
            eprintln!(
                "teardown still in progress under keeper pid {pid}; its outcome is unknown, \
                 and processes it started may still be running"
            );
        }
        // 128 + signal number, the conventional shell encoding.
        std::process::exit(exit_code);
    });
}

/// A trusted executable and its leading arguments. Adapters append their own
/// arguments and retain ownership of cwd, environment, stdio and containment.
/// An empty program fails to spawn; it never selects a real provider instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderCommand {
    pub program: String,
    pub leading_args: Vec<String>,
}

impl ProviderCommand {
    pub fn build(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.program);
        command.args(&self.leading_args);
        command
    }
}

/// Total command configuration for the closed provider set. Private storage
/// cannot lose slots; Provider::all is generated from the enum variants.
#[derive(Clone)]
pub struct ProviderCommands(Vec<(super::Provider, ProviderCommand)>);

impl Default for ProviderCommands {
    fn default() -> Self {
        Self(
            super::Provider::all()
                .iter()
                .map(|&provider| {
                    (
                        provider,
                        ProviderCommand {
                            program: provider.binary().to_string(),
                            leading_args: Vec::new(),
                        },
                    )
                })
                .collect(),
        )
    }
}

impl ProviderCommands {
    pub fn get(&self, provider: super::Provider) -> &ProviderCommand {
        &self
            .0
            .iter()
            .find(|(p, _)| *p == provider)
            .expect("every provider has a command slot")
            .1
    }

    /// Replace one prefix, without changing the table's totality or other slots.
    pub fn set(&mut self, provider: super::Provider, command: ProviderCommand) {
        self.0
            .iter_mut()
            .find(|(p, _)| *p == provider)
            .expect("every provider has a command slot")
            .1 = command;
    }
}

/// Execution knobs for all provider runners. Production uses Default; tests
/// inject command prefixes and short timings while keeping real containment.
#[derive(Clone)]
pub struct ProviderRuntime {
    pub commands: ProviderCommands,
    /// Watchdog polling/patience.
    pub timings: crate::watchdog::Timings,
    /// How long a pipe reader gets to finish after the child is reaped before
    /// it is aborted and we take whatever it buffered.
    pub drain_grace: std::time::Duration,
    /// Overrides the XDG data root used for in-flight markers and incident
    /// bundles. `None` in production. Tests set it because redirecting
    /// `CODEX_HOME` only redirects the *child* - `review`'s own paths are
    /// resolved from the test process's real `HOME`/`XDG_DATA_HOME`, so without
    /// this the suite wrote stub incident bundles into the operator's real
    /// `~/.local/share/review/incidents`.
    /// Audit logs are written by the app layer after provider calls return;
    /// direct provider-runner tests do not reach that logging path.
    pub data_root: Option<std::path::PathBuf>,
    /// Flips when `review` is signalled to stop (see `install_signal_supervisor`).
    /// The process-wide channel in production; tests pass their own so a stop
    /// cannot leak into the rest of the suite.
    pub stop: tokio::sync::watch::Receiver<bool>,
    /// The keeper binary and teardown pacing (see `contained`). Tests point the
    /// keeper at the built `review` and shorten the graces.
    pub teardown: crate::contained::Settings,
    /// Where a keeper writes its outcome if `review` is gone before reading it:
    /// `teardowns.jsonl`, or `teardowns-private.jsonl` for a private project.
    pub orphan_log: Option<std::path::PathBuf>,
}

impl ProviderRuntime {
    /// Build from project config. `stall_timeout_secs` comes from
    /// `[_defaults].stall_timeout_secs`: `None` keeps the built-in default,
    /// `Some(0)` disables the stall branch, anything else sets it. `private`
    /// routes the orphan log the way the sidecar is routed.
    pub fn from_config(stall_timeout_secs: Option<u64>, private: bool) -> Self {
        let mut rt = Self::default();
        if let Some(secs) = stall_timeout_secs {
            rt.timings.stall_grace = (secs > 0).then(|| std::time::Duration::from_secs(secs));
        }
        rt.orphan_log = orphan_log_path(None, private);
        rt
    }
}

/// The orphan log beside the sidecar logs. See `ProviderRuntime::orphan_log`.
pub fn orphan_log_path(
    data_root: Option<&std::path::Path>,
    private: bool,
) -> Option<std::path::PathBuf> {
    let base = match data_root {
        Some(root) => root.to_path_buf(),
        None => std::env::var("XDG_DATA_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|_| {
                std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))
            })
            .ok()?,
    };
    let name = if private {
        "teardowns-private.jsonl"
    } else {
        "teardowns.jsonl"
    };
    Some(base.join("review").join(name))
}

impl Default for ProviderRuntime {
    fn default() -> Self {
        Self {
            commands: ProviderCommands::default(),
            timings: crate::watchdog::Timings::default(),
            drain_grace: crate::timings::DRAIN_GRACE,
            data_root: None,
            stop: stop_channel().subscribe(),
            teardown: crate::contained::Settings::default(),
            orphan_log: orphan_log_path(None, false),
        }
    }
}
