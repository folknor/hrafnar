//! The session log: one append-only JSONL sidecar per log family (public and
//! private), one `SessionRow` per invocation. It is what `hrafnar message`
//! inherits a session's harness and settings from, what `hrafnar sessions`
//! lists, and what the stale-cache gate reads. Rows are historical evidence:
//! they keep their fields as strings and defaults, so old rows, and rows
//! naming a removed harness, still read. `query` resolves a session's state
//! from markers and rows; `handoff` passes one exact run's result between
//! processes. Appends and tail repair go through `fsutil` under file locks.

pub mod handoff;
pub mod query;
use serde::{Deserialize, Serialize, Serializer};
use std::fmt;

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
        Self::at_epoch(epoch_secs, crate::harness::now_epoch_secs())
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

#[derive(Serialize, Deserialize, Clone)]
pub struct SessionRow {
    pub timestamp: String,
    /// Missing and legacy zero epochs mean the time is unknown.
    #[serde(default, deserialize_with = "deserialize_epoch_secs")]
    pub epoch_secs: Option<u64>,
    pub project: String,
    pub hostname: String,
    pub audit_id: String,
    /// On disk as `provider`, the key every existing row and query uses.
    #[serde(rename = "provider")]
    pub harness: String,
    pub archetype: String,
    pub session_id: String,
    /// The row kind, preserving historical and future string values.
    #[serde(default)]
    pub kind: SessionKind,
    /// Selected launch source, absent on legacy rows and manual messages.
    #[serde(default)]
    pub profile_origin: Option<crate::harness::ProfileOrigin>,
    /// Restored by InheritedSettings so a message uses the session's model.
    pub model: Option<String>,
    /// Absent on rows written before this was recorded, and on any run that
    /// passed no effort at all.
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub env_keys: Vec<String>,
    /// Sandbox level the run launched with; absent on rows written before this
    /// was recorded, and on harnesses without a filesystem sandbox.
    #[serde(default)]
    pub sandbox: Option<String>,
    #[serde(default, rename = "provider_sandbox")]
    pub harness_sandbox: Option<String>,
    /// Writable roots granted beyond the harness's defaults.
    #[serde(default)]
    pub writable_roots: Vec<String>,
    pub operator_prompt: String,
    pub assembled_prompt: String,
    pub response: Option<String>,
    pub error: Option<String>,
    #[serde(alias = "review_version")]
    pub hrafnar_version: String,
    /// Served facts are evidence only. InheritedSettings selects request fields
    /// explicitly; these must never become model selectors or permissions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grok_trust: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leftovers: Option<crate::contained::Leftovers>,
    /// Present on codex rows written after digest persistence landed; `None` for
    /// claude rows and for older rows.
    #[serde(default)]
    pub digest: Option<crate::harness::DigestSummary>,
    /// Absent on rows written before Codex home persistence was added.
    #[serde(default)]
    pub codex_home: Option<String>,
    /// `token_command` when this attempt authenticated claude with the global
    /// `claude_token_command` (the fact, never the token). On a session's
    /// `run` row it is also the session's launch policy: messages and resumes
    /// of that session fetch the token up front. Absent on older rows, which
    /// therefore carry no such policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// Historical call sites use this name; there is only one wire representation.
pub type SessionRecord = SessionRow;

fn deserialize_epoch_secs<'de, D>(deserializer: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<u64>::deserialize(deserializer)?.filter(|epoch| *epoch != 0))
}

/// Read every well-formed entry from both the public and private sidecar logs.
/// Malformed lines are skipped with a warning; missing files are treated as empty.
pub fn read_all() -> Vec<SessionRecord> {
    read_all_in(crate::data::DataDir::process())
}

pub fn read_all_in(data: &crate::data::DataDir) -> Vec<SessionRecord> {
    let mut out = Vec::new();
    for family in [
        crate::data::LogFamily::Public,
        crate::data::LogFamily::Private,
    ] {
        let path = match data.sessions(family) {
            Some(p) => p,
            None => continue,
        };
        let content = match crate::fsutil::read_jsonl(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                eprintln!("warning: failed to read {}: {e}", path.display());
                continue;
            }
        };
        for line in content.split(|byte| *byte == b'\n') {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match serde_json::from_slice::<SessionRow>(line) {
                Ok(r) => out.push(r),
                Err(e) => eprintln!("warning: skipping malformed sessions log line: {e}"),
            }
        }
    }
    out
}

/// Most recent record matching the given session ID, if any.
pub fn latest_for_session(session_id: &str) -> Option<SessionRecord> {
    query::Snapshot::read().latest(session_id).cloned()
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

/// Append a complete row. Success means the sidecar append completed; an audit
/// append is an independent obligation and cannot stand in for this result.
pub(crate) fn append(
    data: &crate::data::DataDir,
    scope: &crate::data::LogScope,
    entry: &SessionRow,
) -> bool {
    let path = match data.sessions(scope.family()) {
        Some(p) => p,
        None => {
            eprintln!("warning: could not determine sessions log path (HOME not set)");
            return false;
        }
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("warning: failed to create sessions log dir: {e}");
        return false;
    }

    let line = match serde_json::to_string(&entry) {
        Ok(json) => json,
        Err(e) => {
            eprintln!("warning: failed to serialize session entry: {e}");
            return false;
        }
    };

    match crate::fsutil::append_jsonl(&path, line.as_bytes()) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("warning: failed to append session entry: {e}");
            false
        }
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

#[cfg(test)]
mod compatibility_tests {
    use super::*;
    #[test]
    fn historical_rows_round_trip_without_gaining_execution_authority() {
        let legacy = serde_json::json!({
            "timestamp": "t", "project": "/p", "hostname": "h", "audit_id": "opaque / id",
            "provider": "codex", "archetype": "bare", "session_id": "../historical",
            "model": "requested", "operator_prompt": "operator", "assembled_prompt": "assembled",
            "hrafnar_version": "0.1", "served_model": "served-build", "cost_usd": 1.25,
            "grok_trust": "/trusted", "kind": "future-kind",
        });
        let mut row: SessionRow = serde_json::from_value(legacy).expect("historical row");
        assert_eq!(row.epoch_secs, None);
        assert!(crate::harness::SessionId::parse(&row.session_id).is_err());
        let inherited = crate::harness::InheritedSettings::from_record(
            Some(&row),
            crate::harness::Harness::Codex,
        );
        assert_eq!(inherited.model.as_deref(), Some("requested"));
        assert_eq!(row.served_model.as_deref(), Some("served-build"));
        row.profile_origin = Some(crate::harness::ProfileOrigin {
            layer: "local".into(),
            table: "[codex.read]".into(),
        });
        let value = serde_json::to_value(&row).expect("serialize same representation");
        let restored: SessionRow = serde_json::from_value(value).expect("reread row");
        assert_eq!(restored.kind, SessionKind::Other("future-kind".into()));
        assert_eq!(restored.profile_origin, row.profile_origin);
        assert_eq!(restored.cost_usd, Some(1.25));
        assert_eq!(restored.grok_trust.as_deref(), Some("/trusted"));
        assert_eq!(restored.audit_id, "opaque / id");
        row.epoch_secs = Some(0);
        let zero: SessionRow = serde_json::from_value(serde_json::to_value(row).expect("zero row"))
            .expect("read zero");
        assert_eq!(zero.epoch_secs, None);
    }
}
