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

/// Execution context for all provider runners. Production supplies its command
/// lifecycle; tests construct isolated domains and inject commands/timings.
#[derive(Clone)]
pub struct ProviderRuntime {
    pub commands: ProviderCommands,
    /// Codex watchdog polling/patience; other adapters do not inherit this policy.
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
    /// Shared command domain; default constructs an isolated one for tests.
    pub lifecycle: crate::supervisor::Lifecycle,
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
    pub fn from_config(
        lifecycle: crate::supervisor::Lifecycle,
        stall_timeout_secs: Option<u64>,
        private: bool,
    ) -> Self {
        let mut rt = Self::with_lifecycle(lifecycle);
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

impl ProviderRuntime {
    fn with_lifecycle(lifecycle: crate::supervisor::Lifecycle) -> Self {
        Self {
            commands: ProviderCommands::default(),
            timings: super::codex::watchdog_timings(),
            drain_grace: crate::timings::DRAIN_GRACE,
            data_root: None,
            lifecycle,
            teardown: crate::contained::Settings::default(),
            orphan_log: orphan_log_path(None, false),
        }
    }
}

impl Default for ProviderRuntime {
    fn default() -> Self {
        Self::with_lifecycle(crate::supervisor::Lifecycle::default())
    }
}
