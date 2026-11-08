use anyhow::Result;
use serde::{Deserialize, Serialize, Serializer};
use std::fmt;
use std::path::Path;

/// How this session-log row was produced. Known legacy spellings remain
/// readable, and unrecognized strings round-trip without losing their value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionKind {
    Run,
    Resume,
    Prime,
    Oneshot,
    Other(String),
}

impl SessionKind {
    fn from_value(value: String) -> Self {
        match value.as_str() {
            "run" => Self::Run,
            "session" => Self::Resume,
            "prime" => Self::Prime,
            "oneshot" => Self::Oneshot,
            _ => Self::Other(value),
        }
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Run => "run",
            Self::Resume => "session",
            Self::Prime => "prime",
            Self::Oneshot => "oneshot",
            Self::Other(value) => value,
        }
    }
}

impl Default for SessionKind {
    fn default() -> Self {
        Self::Other(String::new())
    }
}

impl fmt::Display for SessionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for SessionKind {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SessionKind {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Self::from_value(String::deserialize(deserializer)?))
    }
}

/// Elapsed time since an optional UNIX timestamp. The display is shared by
/// session listings and the session artifact view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Age(Option<u64>);

impl Age {
    /// Build an age against a caller's shared clock snapshot.
    pub fn at_epoch(epoch_secs: Option<u64>, now_epoch_secs: u64) -> Self {
        Self(
            epoch_secs
                .filter(|epoch| *epoch != 0)
                .map(|epoch| now_epoch_secs.saturating_sub(epoch)),
        )
    }

    /// Build an age using the process-wide epoch clock.
    pub fn since_epoch(epoch_secs: Option<u64>) -> Self {
        Self::at_epoch(epoch_secs, crate::provider::now_epoch_secs())
    }

    /// The elapsed seconds, or `None` when the timestamp was absent or zero.
    pub fn elapsed_secs(self) -> Option<u64> {
        self.0
    }
}

impl fmt::Display for Age {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            None => f.write_str("?"),
            Some(secs) if secs < 60 => f.write_str("just now"),
            Some(secs) => write!(f, "{} ago", format_age(secs)),
        }
    }
}

#[derive(Serialize)]
struct SessionEntry<'a> {
    timestamp: String,
    /// Seconds since UNIX epoch - duplicates `timestamp` but lets readers do
    /// age math without parsing the ISO string back.
    epoch_secs: u64,
    project: &'a str,
    hostname: String,
    audit_id: &'a str,
    provider: &'a str,
    archetype: &'a str,
    session_id: &'a str,
    /// "run" for a fresh run, "session" for a resume.
    kind: SessionKind,
    model: Option<&'a str>,
    /// Reasoning effort the run launched with. Recorded alongside `model`
    /// because a `review resume` inherits both from this row, and a resume
    /// that restored the model but not the effort would run the right model at
    /// the provider's default depth.
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<&'a str>,
    /// Environment variable *names* configured for this provider entry; values
    /// are deliberately not recorded to avoid leaking secrets in the sidecar.
    env_keys: Vec<String>,
    /// The sandbox level in review's vocabulary for known levels. Custom names
    /// pass through; claude has no filesystem sandbox and records `None`. Makes
    /// "which runs could write?" a `jq` query rather than an inference from the
    /// profile name, which is only a claim about what was requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    sandbox: Option<&'a str>,
    /// Native selector retained separately because Grok pins it for the session.
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_sandbox: Option<&'a str>,
    /// Writable roots granted beyond the provider's defaults. Empty for every
    /// run that widened nothing, which is every read-only run.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    writable_roots: Vec<String>,
    /// The model the provider reports actually served the run, which is not
    /// the name `model` holds (grok serves `-m grok-4.7` as `grok-4.7-build`).
    /// Record-only: deliberately absent from `SessionRecord`, so no resume can
    /// inherit it into `-m`, which refuses it. See `provider::Served`.
    #[serde(skip_serializing_if = "Option::is_none")]
    served_model: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_usd: Option<f64>,
    /// The folder a grok run was launched with `--trust` for - a permanent,
    /// host-wide grant that also lets the repo's `.grok` MCP/LSP config run.
    /// See `provider::ProviderResult::grok_trust`.
    #[serde(skip_serializing_if = "Option::is_none")]
    grok_trust: Option<&'a str>,
    operator_prompt: &'a str,
    assembled_prompt: &'a str,
    response: Option<String>,
    error: Option<String>,
    review_version: &'static str,
    /// Flat run digest when available: exit/signal/captured/turns/task_complete/
    /// usage, so a truncated or errored run is searchable instead of
    /// masquerading as a clean short `response`.
    #[serde(skip_serializing_if = "Option::is_none")]
    digest: Option<crate::provider::DigestSummary>,
    /// What the run's processes left behind: the ones its keeper killed, and
    /// any it could not (see `contained::Leftovers`). Absent when nothing was
    /// left, so `jq 'select(.leftovers)'` finds the runs that left something.
    #[serde(skip_serializing_if = "Option::is_none")]
    leftovers: Option<crate::contained::Leftovers>,
    /// Resolved Codex home used for this session's rollout. Stored separately
    /// from `env_keys` because the profile value is needed for later forensics
    /// and resumes, while environment values are otherwise not persisted.
    #[serde(skip_serializing_if = "Option::is_none")]
    codex_home: Option<&'a str>,
}

#[derive(Deserialize, Clone)]
pub struct SessionRecord {
    #[allow(dead_code)]
    pub timestamp: String,
    /// Missing and legacy zero epochs mean the time is unknown.
    #[serde(default, deserialize_with = "deserialize_epoch_secs")]
    pub epoch_secs: Option<u64>,
    pub project: String,
    #[allow(dead_code)]
    pub hostname: String,
    #[allow(dead_code)]
    pub audit_id: String,
    pub provider: String,
    pub archetype: String,
    pub session_id: String,
    /// The row kind, preserving historical and future string values.
    #[serde(default)]
    pub kind: SessionKind,
    /// Read back by `app::message::inherited_settings` so a `review resume` launches
    /// on the session's own model rather than the provider's default.
    pub model: Option<String>,
    /// Absent on rows written before this was recorded, and on any run that
    /// passed no effort at all.
    #[serde(default)]
    pub effort: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub env_keys: Vec<String>,
    /// Sandbox level the run launched with; absent on rows written before this
    /// was recorded, and on providers without a filesystem sandbox.
    #[allow(dead_code)]
    #[serde(default)]
    pub sandbox: Option<String>,
    #[serde(default)]
    pub provider_sandbox: Option<String>,
    /// Writable roots granted beyond the provider's defaults.
    #[allow(dead_code)]
    #[serde(default)]
    pub writable_roots: Vec<String>,
    pub operator_prompt: String,
    #[allow(dead_code)]
    pub assembled_prompt: String,
    #[allow(dead_code)]
    pub response: Option<String>,
    pub error: Option<String>,
    #[allow(dead_code)]
    pub review_version: String,
    /// Present on codex rows written after digest persistence landed; `None` for
    /// claude rows and for older rows.
    #[serde(default)]
    pub digest: Option<crate::provider::DigestSummary>,
    /// Absent on rows written before Codex home persistence was added.
    #[serde(default)]
    pub codex_home: Option<String>,
}

fn deserialize_epoch_secs<'de, D>(deserializer: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<u64>::deserialize(deserializer)?.filter(|epoch| *epoch != 0))
}

fn sessions_path(private: bool) -> Option<std::path::PathBuf> {
    let data_dir = std::env::var("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| {
            std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))
        })
        .ok()?;
    let base = data_dir.join("review");
    let name = if private {
        "sessions-private.jsonl"
    } else {
        "sessions.jsonl"
    };
    Some(base.join(name))
}

/// Read every well-formed entry from both the public and private sidecar logs.
/// Malformed lines are skipped with a warning; missing files are treated as empty.
pub fn read_all() -> Vec<SessionRecord> {
    let mut out = Vec::new();
    for private in [false, true] {
        let path = match sessions_path(private) {
            Some(p) => p,
            None => continue,
        };
        if !path.exists() {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("warning: failed to read {}: {e}", path.display());
                continue;
            }
        };
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionRecord>(line) {
                Ok(r) => out.push(r),
                Err(e) => eprintln!("warning: skipping malformed sessions log line: {e}"),
            }
        }
    }
    out
}

/// Most recent record matching the given session ID, if any.
pub fn latest_for_session(session_id: &str) -> Option<SessionRecord> {
    read_all()
        .into_iter()
        .filter(|r| r.session_id == session_id)
        .max_by(|a, b| a.epoch_secs.cmp(&b.epoch_secs))
}

/// Compact elapsed time for session listings, with second precision below a minute.
pub fn format_age(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    let rem_mins = mins % 60;
    if hours < 24 {
        if rem_mins == 0 {
            return format!("{hours}h");
        }
        return format!("{hours}h{rem_mins}m");
    }
    let days = hours / 24;
    format!("{days}d")
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_with_kind(
    project_root: &Path,
    private: bool,
    audit_id: &str,
    archetype: &str,
    provider: crate::provider::Provider,
    session_id: &str,
    kind: SessionKind,
    // UNIX seconds when the run finished (from ProviderResult.completed_epoch),
    // so the recorded time reflects completion rather than collection order.
    epoch_secs: u64,
    model: Option<&str>,
    effort: Option<&str>,
    env_keys: Vec<String>,
    // The filesystem permissions the run launched with. Recorded because a
    // profile's advertised guarantee is worth nothing if what a run actually
    // received is only visible in the terminal that launched it.
    sandbox: Option<&crate::provider::Permissions>,
    writable_roots: Vec<String>,
    served: &crate::provider::Served,
    grok_trust: Option<&str>,
    operator_prompt: &str,
    assembled_prompt: &str,
    result: &Result<crate::provider::Turn>,
    digest: Option<&crate::provider::DigestSummary>,
    leftovers: &crate::contained::Leftovers,
    codex_home: Option<&str>,
) {
    let path = match sessions_path(private) {
        Some(p) => p,
        None => {
            eprintln!("warning: could not determine sessions log path (HOME not set)");
            return;
        }
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("warning: failed to create sessions log dir: {e}");
        return;
    }

    let project_str = project_root.to_string_lossy();
    let entry = SessionEntry {
        timestamp: crate::audit::chrono_utc(epoch_secs),
        epoch_secs,
        project: project_str.as_ref(),
        hostname: crate::config::hostname(),
        audit_id,
        provider: provider.as_str(),
        archetype,
        session_id,
        kind,
        model,
        effort,
        env_keys,
        sandbox: sandbox.and_then(crate::provider::Permissions::recorded_level),
        provider_sandbox: sandbox.and_then(crate::provider::Permissions::native_to_record),
        writable_roots,
        served_model: served.model.as_deref(),
        cost_usd: served.cost_usd,
        grok_trust,
        operator_prompt,
        assembled_prompt,
        response: result.as_ref().ok().map(|turn| turn.text.clone()),
        error: result.as_ref().err().map(ToString::to_string),
        review_version: env!("CARGO_PKG_VERSION"),
        digest: digest.cloned(),
        leftovers: (!leftovers.is_empty()).then(|| leftovers.clone()),
        codex_home,
    };

    let line = match serde_json::to_string(&entry) {
        Ok(json) => json,
        Err(e) => {
            eprintln!("warning: failed to serialize session entry: {e}");
            return;
        }
    };

    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("warning: failed to open sessions log: {e}");
            return;
        }
    };

    // One `write_all` of the whole line: `writeln!` on an unbuffered `File`
    // issues the text and the newline as separate writes, and runs that finish
    // together record from concurrent tasks, so their rows could interleave.
    use std::io::Write;
    if let Err(e) = file.write_all(format!("{line}\n").as_bytes()) {
        eprintln!("warning: failed to write session entry: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::format_age;

    #[test]
    fn subminute_age_shows_elapsed_seconds() {
        assert_eq!(format_age(0), "0s");
        assert_eq!(format_age(12), "12s");
        assert_eq!(format_age(59), "59s");
    }
}
