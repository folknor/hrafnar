//! Bounded, contained provider version capture for incident bundles.

use crate::contained::{Leftovers, Settings, spawn};
use tokio::process::Command;

/// `binary <leading_args> --version` under a keeper, for incident bundles: a wrapper or a
/// stub can fork too. Its leftovers join the run's.
pub async fn version_probe(
    lifecycle: &crate::supervisor::Lifecycle,
    binary: &str,
    leading_args: &[String],
    settings: &Settings,
) -> (Option<String>, Leftovers) {
    let mut cmd = Command::new(binary);
    cmd.args(leading_args).arg("--version");
    let label = format!("{binary} --version probe");
    // Never inherit our stdin: a binary that reads stdin before parsing its
    // arguments would block forever, taking the incident write with it.
    let mut contained = match spawn(
        lifecycle,
        &cmd,
        std::process::Stdio::null(),
        settings,
        &label,
        None,
    ) {
        Ok(c) => c,
        Err(_) => return (None, Leftovers::default()),
    };
    let stdout = contained.child.stdout.take();
    let stderr = contained.child.stderr.take();
    // Both pipes drained to EOF, keeping only a version's worth: a probe left
    // blocked on a full pipe would never exit, and its keeper never start the
    // teardown.
    async fn drain<R: tokio::io::AsyncRead + Unpin>(pipe: Option<R>) -> Vec<u8> {
        use tokio::io::AsyncReadExt;
        let mut kept = Vec::new();
        let Some(mut pipe) = pipe else {
            return kept;
        };
        let mut chunk = [0u8; 4096];
        while let Ok(n) = pipe.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            let room = (64usize << 10).saturating_sub(kept.len()).min(n);
            kept.extend_from_slice(&chunk[..room]);
        }
        kept
    }
    let run = async {
        let launched = contained.go().await.is_ok();
        let (buf, _, status) = tokio::join!(drain(stdout), drain(stderr), contained.wait());
        (launched, buf, status)
    };
    // Bounded end to end, launch included: a probe that has not finished
    // within the startup bound is terminated, and given the keeper's own
    // teardown bound. Past that it is abandoned, which `Drop` records as an
    // unknown outcome; an incident bundle is not worth a hung run.
    let outcome = tokio::time::timeout(settings.startup, run).await;
    let (launched, buf, status) = match outcome {
        Ok(done) => done,
        Err(_) => {
            contained.terminate();
            match tokio::time::timeout(settings.terminate_bound(), contained.wait()).await {
                Ok(status) => (false, Vec::new(), status),
                Err(_) => {
                    drop(contained);
                    return (
                        None,
                        Leftovers {
                            unknown: Some(
                                "the version probe's keeper did not finish in time".to_string(),
                            ),
                            ..Leftovers::default()
                        },
                    );
                }
            }
        }
    };
    let finished = contained.finish(&status).await;
    let text = String::from_utf8_lossy(&buf).trim().to_string();
    let version = (launched && !text.is_empty()).then_some(text);
    (version, finished.leftovers)
}
