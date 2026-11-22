//! Prepared adapter context, distinct from requested launch policy and shared
//! effective permission facts. Execution follows this variant, never a second
//! harness selection that could mismatch the prepared context.
use super::*;

pub(super) enum Prepared {
    /// claude or maki, which share one runner.
    ClaudeCompat(Dialect),
    Codex,
    Grok(grok::Prepared),
}

impl Prepared {
    pub async fn run(
        &mut self,
        launch: &LaunchSpec,
        plan: &permissions::PermissionPlan,
        launched: Option<LaunchSignal>,
        runtime: &HarnessRuntime,
    ) -> Result<RunOutput> {
        let sandbox = plan.permissions.selector();
        match self {
            Self::ClaudeCompat(dialect) => {
                run_claude_compat(*dialect, launch, launched, runtime).await
            }
            Self::Codex => {
                run_codex(
                    launch,
                    sandbox.expect("codex permission plan always resolves a selector"),
                    &plan.paths,
                    launched,
                    runtime,
                )
                .await
            }
            Self::Grok(prepared) => prepared.run(launch, sandbox, launched, runtime).await,
        }
    }

    pub fn record(&self, result: &mut HarnessResult) {
        match self {
            Self::ClaudeCompat(_) | Self::Codex => {}
            Self::Grok(prepared) => prepared.record(result),
        }
    }
}
