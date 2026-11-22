//! Pure decoding of the Claude-compatible stream-json format, which both
//! Claude Code and maki print under `-p --output-format stream-json`.
//! Adapters own argv, readiness publication and harness-specific termination
//! policy; what differs in *reading* the stream is the `Dialect`.
use super::*;

const CLAUDE_DETAIL_MAX: usize = 2000;

/// Which program wrote a Claude-compatible stream. The wire shape is shared;
/// the meaning of some fields is not, so every reading that differs is an
/// exhaustive match here rather than a harness-name check at the call site.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Dialect {
    #[default]
    Claude,
    Maki,
}

impl Dialect {
    pub(super) fn harness(self) -> Harness {
        match self {
            Dialect::Claude => Harness::Claude,
            Dialect::Maki => Harness::Maki,
        }
    }

    /// The reported run cost, as `Served::cost_usd` means it: what the
    /// harness reported. maki always writes `total_cost_usd` and writes `0.0`
    /// for a model it has no price for, so its zero cannot be told from a free
    /// run and is recorded as unknown. A positive maki total is kept as
    /// reported; it omits any turn maki could not price, subagents included.
    fn reported_cost(self, cost: Option<f64>) -> Option<f64> {
        match self {
            Dialect::Claude => cost,
            Dialect::Maki => cost.filter(|c| *c != 0.0),
        }
    }
}

#[derive(Default, Clone)]
pub(super) struct ClaudeMetrics {
    dialect: Dialect,
    pub(super) turns: u32,
    pub(super) usage: Usage,
    models: std::collections::BTreeSet<String>,
    cost_usd: Option<f64>,
    auth: AuthEvidence,
}

/// What the stream showed about a credential failure, for deciding whether the
/// prompt may be resent. Observed on claude 2.1.330 for an expired, unrefreshable
/// login: `system` hook and `init` events, then one main-agent `assistant`
/// event with top-level `error: "authentication_failed"`, then a `result`
/// with `is_error: true`, `terminal_reason: "api_error"`.
#[derive(Default, Clone)]
struct AuthEvidence {
    /// A main-agent assistant event carried `error: "authentication_failed"`
    /// since the last result.
    pending: bool,
    /// The last result closed a turn whose auth error came since the result
    /// before it, and ended with `terminal_reason: "api_error"`: the failure
    /// belongs to the result the adapter interprets, the last one.
    ended_on_auth: bool,
    /// Anywhere in the attempt: model output, tool activity, or anything from
    /// a subagent. An auth error that came before work and was followed by it
    /// is still an attempt that did work.
    work: bool,
    /// A line was discarded or unreadable.
    skipped: bool,
    /// The stream reached end of file.
    eof: bool,
}

/// The fields of an event that say whether work happened, or how a result
/// ended.
#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct ActivityEvent {
    #[serde(rename = "type")]
    kind: String,
    subtype: Option<String>,
    error: Option<String>,
    parent_tool_use_id: Option<serde_json::Value>,
    terminal_reason: Option<String>,
    message: Option<ActivityMessage>,
}

#[derive(Default, serde::Deserialize)]
#[serde(default)]
struct ActivityMessage {
    content: Vec<serde_json::Value>,
}

impl ActivityEvent {
    /// Content beyond plain text: a tool call or anything else that acts.
    fn acts(&self) -> bool {
        self.message.as_ref().is_some_and(|m| {
            m.content
                .iter()
                .any(|c| c.get("type").and_then(|t| t.as_str()) != Some("text"))
        })
    }
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
    pub(super) fn new(dialect: Dialect) -> Self {
        Self {
            dialect,
            ..Self::default()
        }
    }

    pub(super) fn skipped(&mut self) {
        self.auth.skipped = true;
    }

    pub(super) fn eof(&mut self) {
        self.auth.eof = true;
    }

    /// The credential failure this stream showed, if any. Replay is offered
    /// only for a failure seen in a fully observed stream before any work.
    pub(super) fn auth_failure(&self) -> Option<AuthFailure> {
        let auth = &self.auth;
        if !auth.ended_on_auth {
            // An auth error the last result does not close is not what
            // ended the run; a partial stream may have hidden that result.
            return auth.pending.then_some(AuthFailure::Unknown);
        }
        Some(if auth.work || auth.skipped || !auth.eof {
            AuthFailure::Unknown
        } else {
            AuthFailure::BeforeWork
        })
    }

    fn record_activity(&mut self, line: &[u8]) {
        let Ok(event) = serde_json::from_slice::<ActivityEvent>(line) else {
            // An event that could not be read might have been work.
            if !line.iter().all(u8::is_ascii_whitespace) {
                self.auth.skipped = true;
            }
            return;
        };
        let main_agent = event
            .parent_tool_use_id
            .as_ref()
            .is_none_or(serde_json::Value::is_null);
        match event.kind.as_str() {
            // Startup, named: the main agent's init and hook events run before
            // the model is reached. Any other system event - task events
            // (`task_started`, `task_progress`, `task_notification`, which carry
            // `tool_use_id` rather than `parent_tool_use_id`), a subagent's, or
            // one this list does not know - is treated as work.
            "system"
                if main_agent
                    && matches!(
                        event.subtype.as_deref(),
                        Some("init" | "hook_started" | "hook_response")
                    ) => {}
            "result" => {
                self.auth.ended_on_auth =
                    self.auth.pending && event.terminal_reason.as_deref() == Some("api_error");
                self.auth.pending = false;
            }
            "assistant"
                if main_agent && event.error.as_deref() == Some("authentication_failed") =>
            {
                self.auth.pending = true;
                if event.acts() {
                    self.auth.work = true;
                }
            }
            // Model output, a tool result, a subagent: anything else is work.
            _ => self.auth.work = true,
        }
    }

    pub(super) fn record_line(&mut self, line: &[u8]) {
        if self.dialect == Dialect::Claude {
            self.record_activity(line);
        }
        let Ok(event) = serde_json::from_slice::<ClaudeMetricEvent>(line) else {
            return;
        };
        if event.kind != "result" {
            return;
        }
        self.turns += event.num_turns;
        self.usage += &Usage::from_parts(
            event.usage.input_tokens,
            event.usage.cache_creation_input_tokens,
            event.usage.cache_read_input_tokens,
            event.usage.output_tokens,
            event
                .usage
                .output_tokens_details
                .map_or(0, |details| details.thinking_tokens),
        );
        // maki writes no `modelUsage`; the model ID in its other events is the
        // one it requested, not one the backend reported, so nothing else is
        // read as the served model.
        if let Some(model_usage) = event.model_usage {
            self.models
                .extend(model_usage.into_iter().map(|(name, _)| name));
        }
        if let Some(cost) = self.dialect.reported_cost(event.total_cost_usd) {
            self.cost_usd = Some(self.cost_usd.unwrap_or_default() + cost);
        }
    }

    pub(super) fn served(&self) -> Served {
        Served::new(self.models.iter().cloned(), self.cost_usd)
    }
}

/// Whether `line` is the stream's `init` event: `{"type":"system","subtype":"init",...}`.
pub(super) fn is_claude_init(line: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(line).is_ok_and(|v| {
        v.get("type").and_then(|t| t.as_str()) == Some("system")
            && v.get("subtype").and_then(|s| s.as_str()) == Some("init")
    })
}

/// The last result object in the stream, as its own line. Every event is one
/// line; a run ends with a `result`, and a run in which claude woke the model
/// for a background subagent carries one per turn, the last being the answer.
fn last_claude_result(stdout: &str) -> Option<&str> {
    stdout.lines().rev().find(|line| {
        serde_json::from_str::<ClaudeResult>(line)
            .is_ok_and(|r| r.kind.as_deref() == Some("result"))
    })
}

/// The result object that ends the stream: one per run, whatever the outcome.
/// Only the fields that decide the outcome are modelled here; `ClaudeMetrics`
/// accumulates usage and served details while the full stream is read.
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
    /// claude: `"completed"` on a normal turn, `"api_error"` on the unknown model.
    #[serde(default)]
    terminal_reason: Option<String>,
    #[serde(default)]
    api_error_status: Option<u32>,
    /// maki's turn ending, kept as raw JSON so that a missing key, `null` and
    /// an unfamiliar value stay distinguishable: decoding into a closed type
    /// would fail the whole result on a new value and lose its text.
    #[serde(default, deserialize_with = "present")]
    stop_reason: Option<serde_json::Value>,
}

/// `Some` for a key that is present, `null` included; `None` only when absent.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<serde_json::Value>, D::Error> {
    serde::Deserialize::deserialize(d).map(Some)
}

/// What a finished run amounts to. An `Err` alongside these means no result
/// object came back at all.
#[derive(Debug)]
pub(super) enum ClaudeOutcome {
    Answered {
        text: String,
        session_id: Option<String>,
    },
    /// A result object that is not an answer. The session exists, so this is
    /// `Ok` with its ID kept. `stated` is the harness's own account; a field
    /// review needed and did not get is `unverified`. Either or both are set.
    NoAnswer {
        stated: Option<String>,
        unverified: Option<String>,
        text: String,
        session_id: Option<String>,
    },
}

/// maki's `stop_reason` values for a turn that ended without finishing its
/// answer (`DoneReason` in maki-agent, `snake_case`). `end_turn` is the only
/// answer; maki sets `is_error` for `cancelled` alone, so the rest would pass
/// as answers if `is_error` decided.
const MAKI_UNFINISHED: [&str; 5] = ["max_tokens", "max_turns", "cancelled", "compact", "dropped"];

/// Decide whether a finished run produced an answer, from its streams alone.
/// Pure, so each shape is tested without a harness binary.
///
/// Only `is_error: false` can be an answer. A result missing the flag is
/// treated as a failure rather than a success: a harness that stopped
/// reporting it should fail loudly, not pass every run off as answered. maki
/// additionally needs `stop_reason: "end_turn"`, for the same reason.
pub(super) fn interpret_claude_output(
    dialect: Dialect,
    stdout: &str,
    stderr: &str,
) -> Result<ClaudeOutcome> {
    let harness = dialect.harness();
    // Why there is no result, for the error message - which is printed and
    // persisted in `audit.jsonl`, so it is bounded and free of terminal
    // escapes (maki colours its error banner unconditionally). stderr is the
    // harness's own account when it gives one. Otherwise only the last line
    // of the stream: the whole stream is every event of the run, tool output
    // included, up to `STDOUT_CAPTURE_CAP`.
    let detail = || {
        let stderr = strip_ansi(stderr);
        let source = if stderr.trim().is_empty() {
            stdout
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("(no output)")
                .to_string()
        } else {
            stderr.trim().to_string()
        };
        truncate(&source, CLAUDE_DETAIL_MAX)
    };
    let Some(result) =
        last_claude_result(stdout).and_then(|line| serde_json::from_str::<ClaudeResult>(line).ok())
    else {
        anyhow::bail!("{harness} produced no result object: {}", detail());
    };
    let text = result.result.clone().unwrap_or_default();
    let session_id = result.session_id.clone();
    let (stated, unverified) = match dialect {
        Dialect::Claude => claude_verdict(&result, &text),
        Dialect::Maki => maki_verdict(&result, &text),
    };
    if stated.is_none() && unverified.is_none() {
        return Ok(ClaudeOutcome::Answered { text, session_id });
    }
    Ok(ClaudeOutcome::NoAnswer {
        stated,
        unverified,
        text,
        session_id,
    })
}

/// The first non-blank line of the result text: on an error it is the
/// harness's own explanation (claude's unknown model: "There's an issue with
/// the selected model ..."; maki's "API error (402): ...").
fn explanation(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|l| !l.is_empty())
}

fn with_facts(mut reason: String, facts: &[String], text: &str) -> String {
    if !facts.is_empty() {
        reason.push_str(&format!(" ({})", facts.join(", ")));
    }
    if let Some(line) = explanation(text) {
        reason.push_str(&format!(": {line}"));
    }
    reason
}

/// claude: `is_error` alone decides; `subtype`, `terminal_reason` and the API
/// status are named in the reason.
fn claude_verdict(result: &ClaudeResult, text: &str) -> (Option<String>, Option<String>) {
    if result.is_error == Some(false) {
        return (None, None);
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
    if result.is_error.is_none() {
        let reason = with_facts(
            "review could not classify claude result".into(),
            &facts,
            text,
        );
        (None, Some(reason))
    } else {
        let reason = with_facts("claude reported an error".into(), &facts, text);
        (Some(reason), None)
    }
}

/// maki: an answer is `is_error: false` with `stop_reason: "end_turn"`.
///
/// | is_error | stop_reason             | verdict                                 |
/// |----------|-------------------------|-----------------------------------------|
/// | false    | end_turn                | answer                                  |
/// | false    | a known unfinished one  | stated: maki stopped (reason)           |
/// | true     | anything present        | stated: maki reported an error          |
/// | any      | key missing             | unverified, beside any stated error     |
/// | false    | null or unknown         | unverified                              |
/// | missing  | any                     | unverified                              |
///
/// `is_error: true` with `stop_reason: null` is maki's harness failure (the
/// API refused the request before any turn ended), so it is stated.
fn maki_verdict(result: &ClaudeResult, text: &str) -> (Option<String>, Option<String>) {
    let reason_str = match &result.stop_reason {
        Some(serde_json::Value::String(s)) => Some(s.as_str()),
        _ => None,
    };
    let reason_fact = match &result.stop_reason {
        None => None,
        Some(serde_json::Value::Null) => Some("stop_reason: null".to_string()),
        Some(serde_json::Value::String(s)) => Some(format!("stop_reason: {s}")),
        Some(other) => Some(format!("stop_reason: {other}")),
    };
    let missing_reason = result.stop_reason.is_none().then(|| {
        with_facts(
            "review could not classify maki result".into(),
            &["no stop_reason".to_string()],
            text,
        )
    });
    match result.is_error {
        None => {
            let mut facts = vec!["no is_error flag".to_string()];
            facts.extend(reason_fact);
            let reason = with_facts("review could not classify maki result".into(), &facts, text);
            (None, Some(reason))
        }
        Some(true) => {
            let facts: Vec<String> = reason_fact.into_iter().collect();
            let stated = with_facts("maki reported an error".into(), &facts, text);
            (Some(stated), missing_reason)
        }
        Some(false) => match reason_str {
            Some("end_turn") => (None, None),
            Some(s) if MAKI_UNFINISHED.contains(&s) => {
                let stated = with_facts(format!("maki stopped before finishing ({s})"), &[], "");
                (Some(stated), None)
            }
            _ => match missing_reason {
                Some(missing) => (None, Some(missing)),
                None => {
                    let facts: Vec<String> = reason_fact.into_iter().collect();
                    let reason = with_facts(
                        "review could not classify maki result (is_error false, unrecognised ending)"
                            .into(),
                        &facts,
                        "",
                    );
                    (None, Some(reason))
                }
            },
        },
    }
}

/// `s` without ANSI CSI escape sequences (`ESC [ ... final-byte`), which a
/// harness's terminal-coloured stderr would otherwise carry into the audit log.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
