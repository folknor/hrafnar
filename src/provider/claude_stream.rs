//! Pure decoding of the Claude-compatible stream-json format.
//! Adapters own argv, readiness publication and provider-specific termination policy.
use super::*;

const CLAUDE_DETAIL_MAX: usize = 2000;

#[derive(Default, Clone)]
pub(super) struct ClaudeMetrics {
    pub(super) turns: u32,
    pub(super) usage: Usage,
    models: std::collections::BTreeSet<String>,
    cost_usd: Option<f64>,
}

#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct ClaudeMetricEvent {
    #[serde(rename = "type")]
    kind: String,
    num_turns: u32,
    usage: ClaudeMetricUsage,
    #[serde(rename = "modelUsage")]
    model_usage: Option<serde_json::Map<String, serde_json::Value>>,
    total_cost_usd: Option<f64>,
}

#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct ClaudeMetricUsage {
    input_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    output_tokens: u64,
    output_tokens_details: Option<ClaudeMetricUsageDetails>,
}

#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct ClaudeMetricUsageDetails {
    thinking_tokens: u64,
}

impl ClaudeMetrics {
    pub(super) fn record_line(&mut self, line: &[u8]) {
        let Ok(event) = serde_json::from_slice::<ClaudeMetricEvent>(line) else {
            return;
        };
        if event.kind != "result" {
            return;
        }
        self.turns += event.num_turns;
        self.usage.input_tokens += event.usage.input_tokens
            + event.usage.cache_creation_input_tokens
            + event.usage.cache_read_input_tokens;
        self.usage.cached_input_tokens += event.usage.cache_read_input_tokens;
        self.usage.output_tokens += event.usage.output_tokens;
        self.usage.reasoning_output_tokens += event
            .usage
            .output_tokens_details
            .map_or(0, |details| details.thinking_tokens);
        if let Some(model_usage) = event.model_usage {
            self.models
                .extend(model_usage.into_iter().map(|(name, _)| name));
        }
        if let Some(cost) = event.total_cost_usd {
            self.cost_usd = Some(self.cost_usd.unwrap_or_default() + cost);
        }
    }

    pub(super) fn served(&self) -> Served {
        let models: Vec<&str> = self.models.iter().map(String::as_str).collect();
        Served {
            model: (!models.is_empty()).then(|| models.join(",")),
            cost_usd: self.cost_usd,
        }
    }
}

/// Whether `line` is claude's `init` event: `{"type":"system","subtype":"init",...}`.
pub(super) fn is_claude_init(line: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(line).is_ok_and(|v| {
        v.get("type").and_then(|t| t.as_str()) == Some("system")
            && v.get("subtype").and_then(|s| s.as_str()) == Some("init")
    })
}

/// The last result object in claude's `stream-json` output, as its own line.
/// Every event is one line; a run ends with a `result`, and a run in which
/// claude woke the model for a background subagent carries one per turn, the
/// last being the answer.
fn last_claude_result(stdout: &str) -> Option<&str> {
    stdout.lines().rev().find(|line| {
        serde_json::from_str::<ClaudeResult>(line)
            .is_ok_and(|r| r.kind.as_deref() == Some("result"))
    })
}

/// The result object that ends claude's `--output-format stream-json` output:
/// one per run, whatever the outcome. Only the fields that decide the outcome
/// are modelled here; `ClaudeMetrics` accumulates usage and served details while
/// the full stream is read.
#[derive(serde::Deserialize)]
struct ClaudeResult {
    /// `"result"`. Checked so that some other JSON on stdout is not mistaken
    /// for a result - every other field here is optional.
    #[serde(rename = "type", default)]
    kind: Option<String>,
    /// `"success"` even for an API error (a model claude could not find
    /// reported `subtype: "success"`, `is_error: true`) - so `is_error`, not
    /// this, decides. Named in the reason when it says something else.
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    /// `"completed"` on a normal turn, `"api_error"` on the unknown model.
    #[serde(default)]
    terminal_reason: Option<String>,
    #[serde(default)]
    api_error_status: Option<u32>,
}

/// What a finished claude run amounts to. An `Err` alongside these means no
/// result object came back at all.
#[derive(Debug)]
pub(super) enum ClaudeOutcome {
    Answered {
        text: String,
        session_id: Option<String>,
    },
    /// Claude produced a result object that is either flagged as an error or
    /// missing the flag needed to trust it as an answer. The session exists, so
    /// this is `Ok` with its ID kept.
    NoAnswer {
        reason: String,
        /// `true` when review inferred a failure because Claude omitted the
        /// field that says whether the result is an error.
        review_error: bool,
        text: String,
        session_id: Option<String>,
    },
}

/// Decide whether a finished claude run produced an answer, from its streams
/// alone. Pure, so each shape is tested without a claude binary.
///
/// Only `is_error: false` is an answer. A result missing the flag is treated
/// as a failure rather than a success: a claude that stopped reporting it
/// should fail loudly, not pass every run off as answered.
pub(super) fn interpret_claude_output(
    provider: &str,
    stdout: &str,
    stderr: &str,
) -> Result<ClaudeOutcome> {
    // Why there is no result, for the error message - which is printed and
    // persisted in `audit.jsonl`, so it is bounded. stderr is claude's own
    // account when it gives one. Otherwise only the last line of the stream:
    // the whole stream is every event of the run, tool output included, up to
    // `STDOUT_CAPTURE_CAP`.
    let detail = || {
        let source = if stderr.trim().is_empty() {
            stdout
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("(no output)")
        } else {
            stderr.trim()
        };
        truncate(source, CLAUDE_DETAIL_MAX)
    };
    let Some(result) =
        last_claude_result(stdout).and_then(|line| serde_json::from_str::<ClaudeResult>(line).ok())
    else {
        anyhow::bail!("{provider} produced no result object: {}", detail());
    };
    let text = result.result.unwrap_or_default();
    if result.is_error == Some(false) {
        return Ok(ClaudeOutcome::Answered {
            text,
            session_id: result.session_id,
        });
    }
    let mut facts: Vec<String> = Vec::new();
    if result.is_error.is_none() {
        facts.push("no is_error flag".to_string());
    }
    if let Some(s) = result.subtype.as_deref().filter(|s| *s != "success") {
        facts.push(format!("subtype: {s}"));
    }
    if let Some(t) = result
        .terminal_reason
        .as_deref()
        .filter(|t| *t != "completed")
    {
        facts.push(format!("terminal_reason: {t}"));
    }
    if let Some(status) = result.api_error_status {
        facts.push(format!("API status {status}"));
    }
    let mut reason = if result.is_error.is_none() {
        format!("review could not classify {provider} result")
    } else {
        format!("{provider} reported an error")
    };
    if !facts.is_empty() {
        reason.push_str(&format!(" ({})", facts.join(", ")));
    }
    // On an error the result text is claude's own explanation (the unknown
    // model: "There's an issue with the selected model ..."). It is persisted
    // with the provider reason; a missing `is_error` is described as review's
    // classification failure and routes to `review_error` below.
    if let Some(line) = text.lines().find(|l| !l.trim().is_empty()) {
        reason.push_str(&format!(": {}", line.trim()));
    }
    Ok(ClaudeOutcome::NoAnswer {
        reason,
        review_error: result.is_error.is_none(),
        text,
        session_id: result.session_id,
    })
}
