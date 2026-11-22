//! The runtime an invocation runs in, as opposed to what it asks for: the
//! command `Lifecycle`, the data directory and log family, the run's identity
//! and reservation, watchdog and teardown timings, and the executable each
//! harness is launched with (`HarnessCommand`, which tests point at stub
//! scripts).

/// A trusted executable and its leading arguments. Adapters append their own
/// arguments and retain ownership of cwd, environment, stdio and containment.
/// An empty program fails to spawn; it never selects a real harness instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessCommand {
    pub program: String,
    pub leading_args: Vec<String>,
}

impl HarnessCommand {
    pub fn build(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.program);
        command.args(&self.leading_args);
        command
    }
}

/// Total command configuration for the closed harness set. Private storage
/// cannot lose slots; Harness::all is generated from the enum variants.
#[derive(Clone)]
pub struct HarnessCommands(Vec<(super::Harness, HarnessCommand)>);

impl Default for HarnessCommands {
    fn default() -> Self {
        Self(
            super::Harness::all()
                .iter()
                .map(|&harness| {
                    (
                        harness,
                        HarnessCommand {
                            program: harness.binary().to_string(),
                            leading_args: Vec::new(),
                        },
                    )
                })
                .collect(),
        )
    }
}

impl HarnessCommands {
    pub fn get(&self, harness: super::Harness) -> &HarnessCommand {
        &self
            .0
            .iter()
            .find(|(p, _)| *p == harness)
            .expect("every harness has a command slot")
            .1
    }

    /// Replace one prefix, without changing the table's totality or other slots.
    pub fn set(&mut self, harness: super::Harness, command: HarnessCommand) {
        self.0
            .iter_mut()
            .find(|(p, _)| *p == harness)
            .expect("every harness has a command slot")
            .1 = command;
    }
}

/// Execution context for all harness runners. Production supplies its command
/// lifecycle; tests construct isolated domains and inject commands/timings.
#[derive(Clone)]
pub struct HarnessRuntime {
    pub commands: HarnessCommands,
    pub run_id: crate::sessions::handoff::RunId,
    pub predecessor: Option<crate::sessions::handoff::RunId>,
    pub reservation: Option<std::sync::Arc<crate::inflight::Reservation>>,
    /// Codex watchdog polling/patience; other adapters do not inherit this policy.
    pub timings: crate::watchdog::Timings,
    /// How long a pipe reader gets to finish after the child is reaped before
    /// it is aborted and we take whatever it buffered.
    pub drain_grace: std::time::Duration,
    /// Hrafnar-owned storage, shared by markers, incidents and orphan outcomes.
    pub data: crate::data::DataDir,
    pub log_family: crate::data::LogFamily,
    /// Shared command domain; default constructs an isolated one for tests.
    pub lifecycle: crate::supervisor::Lifecycle,
    /// The keeper binary and teardown pacing (see `contained`). Tests point the
    /// keeper at the built `hrafnar` and shorten the graces.
    pub teardown: crate::contained::Settings,
    /// The global `claude_token_command`, shared by every claude run in this
    /// process so it is run at most once. `None` when unconfigured.
    pub claude_token: Option<std::sync::Arc<super::claude_token::TokenSource>>,
}

impl HarnessRuntime {
    /// Build from config. `stall_timeout` comes from
    /// `[_defaults].stall_timeout_secs`: `None` keeps the built-in default,
    /// otherwise it replaces it (off included). Scope routes the orphan log
    /// to the same family as the sidecar.
    pub fn from_config(
        lifecycle: crate::supervisor::Lifecycle,
        stall_timeout: Option<crate::config::StallTimeout>,
        claude_token_command: Option<&str>,
        data: crate::data::DataDir,
        scope: &crate::data::LogScope,
    ) -> Self {
        let mut rt = Self::with_data(lifecycle, data, scope.family());
        if let Some(timeout) = stall_timeout {
            rt.timings.stall_grace = timeout.grace();
        }
        rt.claude_token = claude_token_command
            .map(str::to_string)
            .map(|command| std::sync::Arc::new(super::claude_token::TokenSource::new(command)));
        rt
    }

    /// The configured command's token, run off the async workers. `None` when
    /// no command is configured.
    pub async fn fetch_claude_token(
        &self,
    ) -> Option<Result<super::claude_token::ClaudeToken, String>> {
        let source = std::sync::Arc::clone(self.claude_token.as_ref()?);
        Some(
            tokio::task::spawn_blocking(move || source.fetch())
                .await
                .unwrap_or_else(|e| Err(format!("claude_token_command task failed: {e}"))),
        )
    }
}

impl HarnessRuntime {
    fn with_data(
        lifecycle: crate::supervisor::Lifecycle,
        data: crate::data::DataDir,
        log_family: crate::data::LogFamily,
    ) -> Self {
        Self {
            commands: HarnessCommands::default(),
            run_id: crate::sessions::handoff::RunId::default(),
            predecessor: None,
            reservation: None,
            timings: super::codex::watchdog_timings(),
            drain_grace: crate::timings::DRAIN_GRACE,
            data,
            log_family,
            lifecycle,
            teardown: crate::contained::Settings::default(),
            claude_token: None,
        }
    }
}

impl Default for HarnessRuntime {
    fn default() -> Self {
        Self::with_data(
            crate::supervisor::Lifecycle::default(),
            crate::data::DataDir::process().clone(),
            crate::data::LogFamily::Public,
        )
    }
}
