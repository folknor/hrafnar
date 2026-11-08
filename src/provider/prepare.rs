//! Prepared adapter context, distinct from requested launch policy and shared
//! effective permission facts. Execution follows this variant, never a second
//! provider selection that could mismatch the prepared context.
use super::*;

pub(super) enum Prepared {
    Claude,
    Codex,
    Grok(grok::Prepared),
}

impl Prepared {
    pub async fn run(
        &mut self,
        launch: &LaunchSpec,
        plan: &permissions::PermissionPlan,
        launched: Option<LaunchSignal>,
        runtime: &ProviderRuntime,
    ) -> Result<RunOutput> {
        let sandbox = plan.permissions.selector();
        match self {
            Self::Claude => run_claude(launch, launched, runtime).await,
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

    pub fn record(&self, result: &mut ProviderResult) {
        match self {
            Self::Claude | Self::Codex => {}
            Self::Grok(prepared) => prepared.record(result),
        }
    }
}
