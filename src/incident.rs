//! Forensic capture for suspicious harness runs.
//!
//! When a codex run looks wrong (not captured / non-zero exit / signal) the
//! live digest + transcript summary are not enough to explain *why* it died.
//! That needs what `review` would otherwise discard: codex's stderr, the raw
//! NDJSON stream, whether the prompt finished writing, and (absent
//! `RUST_BACKTRACE`) any panic trace. Only the codex adapter writes bundles.
//!
//! An answer the watchdog preserved stays affirmative evidence in the bundle
//! even when the later recovery read can no longer find the transcript.
//!
//! `write_bundle` dumps all of it to `~/.local/share/review/incidents/<dir>/`
//! so the *next* death is over-instrumented instead of a mystery. It is
//! best-effort: `write_bundle` returns failures to its caller, which reports
//! them without derailing the run. Only suspicious runs write a bundle, so
//! clean runs stay uncluttered.

mod probe;
pub(crate) use probe::version_probe;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Everything known about a finished harness run, handed to the bundle writer.
/// ProcessSpec, Outcome, Exit and the harness extension own independent rules.
/// The remaining byte buffers and measurements are observations, not interchangeable
/// policy selectors; wrapping each scalar would not add an invariant.
pub struct Incident<'a> {
    pub harness: crate::harness::Harness,
    /// The process as it was spawned: argv[0] and the args, the working
    /// directory and the env overrides (adapter defaults, profile, forced,
    /// `PWD`). The replay command and the env list are rendered from it, so
    /// nothing a runner injects can be missing from them. What it cannot
    /// carry: the environment the child inherited from `review` (only the
    /// overrides are recorded), and the contents of runner-created temp files
    /// named in argv (codex's `-o` file, grok's prompt file).
    pub process: &'a crate::harness::ProcessSpec,
    /// `<binary> <leading args> --version`, best-effort; deaths may be version-specific. It
    /// probes the binary that actually ran, not the harness name: those differ
    /// whenever the binary is overridden (the test harness runs a stub), and
    /// probing the name recorded the host's real codex for a stub run. Taken by
    /// the caller (`incident::version_probe`), so the probe is contained like
    /// any harness process.
    pub harness_version: Option<String>,
    pub session_id: Option<&'a str>,
    /// The prompt piped on the child's stdin - part of the invocation, so a
    /// replay is impossible without it.
    pub prompt: &'a str,
    pub exit: crate::process::Exit,
    /// `Some` if writing the prompt to the child's stdin failed (e.g. a broken
    /// pipe because the child exited before consuming it).
    pub stdin_write_error: Option<&'a str>,
    pub stdout: &'a [u8],
    pub stderr: &'a [u8],
    pub duration_ms: u128,
    /// The invocation's immutable storage domain, also injected by tests.
    pub data: &'a crate::data::DataDir,
    pub outcome: &'a crate::harness::Outcome,
    pub turns: u32,
    pub forensics: Option<IncidentForensics<'a>>,
}

/// Harness-owned inputs, projected explicitly into the stable bundle schema.
pub enum IncidentForensics<'a> {
    Codex(CodexIncident<'a>),
}

pub use crate::harness::codex::forensics::CodexIncident;

/// Keep the last MiB of the raw NDJSON stream - the death is at the end, and a
/// long agentic run's stream can be tens of MiB.
const STDOUT_TAIL_BYTES: usize = 1 << 20;
const INCIDENT_SCHEMA: u32 = 1;

// This is the schema-1 compatibility DTO, not the internal harness model.
// Historical writers omitted some fields; default omissions, but reject invalid
// present values and unsupported schema versions. Both read and write use it.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Meta {
    pub schema: u32,
    pub timestamp: String,
    /// Historical schema-1 key `provider`.
    #[serde(rename = "provider")]
    pub harness: String,
    pub session_id: Option<String>,
    pub review_version: String,
    // Historical schema-1 key: keep it even though Incident uses harness_version.
    pub codex_version: Option<String>,
    /// Full argv including argv[0] (the binary).
    pub argv: Vec<String>,
    /// A copy-pasteable shell command that reproduces the run: the injected env
    /// prefix, the quoted argv, and `< prompt.txt` (the prompt is piped on
    /// stdin, so it's written alongside as `prompt.txt`). Secret env values are
    /// shown as `<redacted>` - fill them back in to actually run it.
    pub command: String,
    pub cwd: String,
    pub prompt_bytes: usize,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    pub stdin_write_error: Option<String>,
    pub duration_ms: u128,
    pub captured: bool,
    pub recovered_from_transcript: bool,
    pub turns: u32,
    pub terminated_by_review: Option<String>,
    pub quiet_secs: Option<u64>,
    pub last_rollout_event: Option<String>,
    /// Codex's stated reason for the turn ending, when it gave one.
    pub turn_error: Option<String>,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
    pub stdout_truncated: bool,
    pub transcript_path: Option<String>,
    /// Whether the shared transcript summary found a final-answer phase in the
    /// scoped bytes. `Some(false)` means it found none.
    pub final_answer_present: Option<bool>,
    /// Rate-limit snapshot from the last `token_count` event (raw JSON), so a
    /// limit-driven stop is visible without re-reading the transcript.
    pub rate_limits: Option<serde_json::Value>,
    /// Env var names configured for the run; values only when non-secret.
    pub env: Vec<EnvVar>,
}

#[derive(Serialize, Deserialize)]
pub struct EnvVar {
    name: String,
    value: Option<String>,
}

/// A name that shouldn't have its value recorded (heuristic, errs toward hiding).
/// Bundles keep nonsecret values for replay and redact likely secrets. Sidecars
/// record env names only, so omitting values there is compatible with this rule.
pub(crate) fn looks_secret(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    [
        "KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "AUTH",
        "COOKIE",
        "CREDENTIAL",
    ]
    .iter()
    .any(|needle| n.contains(needle))
}

#[derive(Debug)]
pub struct IncidentFailure {
    pub path: Option<PathBuf>,
    pub message: String,
}

#[derive(Debug)]
pub enum BundleWriteOutcome {
    Written(PathBuf),
    Unavailable(String),
    Incomplete {
        directory: Option<PathBuf>,
        failures: Vec<IncidentFailure>,
    },
}

/// Write a forensic bundle for a suspicious run. A directory is reported as
/// written only when every artifact, including its index, was written.
pub fn write_bundle(inc: &Incident) -> BundleWriteOutcome {
    let Some(base) = inc.data.incidents() else {
        return BundleWriteOutcome::Unavailable(
            "could not determine the incident data directory".to_string(),
        );
    };
    let timestamp = crate::audit::chrono_now();
    let stamp = timestamp.replace(':', "-");
    let sid = inc.session_id.unwrap_or("no-session");
    // A random suffix keeps two same-second failures (e.g. two concurrent
    // deaths before `thread.started`, both "no-session") from colliding on the
    // same dir and overwriting each other's artifacts. It is the audit id's
    // generator (32 hex characters), but this path is best-effort: without
    // system randomness it falls back to pid and clock nanoseconds, which are
    // still distinct between two failures of one `review`, rather than
    // panicking or dropping the bundle.
    let nonce = crate::config::try_generate_short_id().unwrap_or_else(|_| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or_default();
        format!("{}-{nanos:09}", std::process::id())
    });
    let dir = base.join(format!("{stamp}-{}-{sid}-{nonce}", inc.harness));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return BundleWriteOutcome::Incomplete {
            directory: Some(dir.clone()),
            failures: vec![IncidentFailure {
                path: Some(dir),
                message: e.to_string(),
            }],
        };
    }
    let mut failures = Vec::new();

    // Full stderr, the highest-value channel in the bundle. With
    // RUST_BACKTRACE set on the child, a panic lands here with its trace.
    if let Err(error) = write_file(&dir, "stderr.txt", inc.stderr) {
        failures.push(error);
    }

    // The prompt piped on stdin, and the runnable command that consumes it.
    if let Err(error) = write_file(&dir, "prompt.txt", inc.prompt.as_bytes()) {
        failures.push(error);
    }
    let mut full_argv = vec![inc.process.program.clone()];
    full_argv.extend(inc.process.args.iter().cloned());
    let command = replay_command(
        &full_argv,
        &inc.process.env,
        &inc.process.cwd,
        &dir.join("prompt.txt"),
    );

    // Raw NDJSON stream, tail-capped. The interesting part is where it stops.
    let (stdout_slice, stdout_truncated) = tail_bytes(inc.stdout, STDOUT_TAIL_BYTES);
    if let Err(error) = write_file(&dir, "stdout.jsonl", stdout_slice) {
        failures.push(error);
    }

    let codex = inc.forensics.as_ref().map(|evidence| match evidence {
        IncidentForensics::Codex(codex) => codex,
    });
    let rate_limits = match codex {
        Some(codex) => {
            let (rate_limits, errors) = codex.write_artifacts(&dir);
            failures.extend(errors);
            rate_limits
        }
        None => None,
    };
    let final_answer_present = codex.and_then(|c| c.final_answer_present);

    let env = inc
        .process
        .env
        .iter()
        .map(|(k, v)| EnvVar {
            name: k.clone(),
            value: (!looks_secret(k)).then(|| v.clone()),
        })
        .collect();

    let meta = Meta {
        schema: INCIDENT_SCHEMA,
        timestamp,
        harness: inc.harness.to_string(),
        session_id: inc.session_id.map(str::to_string),
        review_version: env!("CARGO_PKG_VERSION").to_string(),
        codex_version: inc.harness_version.clone(),
        argv: full_argv,
        command,
        cwd: inc.process.cwd.to_string_lossy().into_owned(),
        prompt_bytes: inc.prompt.len(),
        exit_code: inc.exit.code(),
        signal: inc.exit.signal_name(),
        stdin_write_error: inc.stdin_write_error.map(str::to_string),
        duration_ms: inc.duration_ms,
        captured: inc.outcome.answer == crate::harness::Answer::Captured,
        recovered_from_transcript: inc.outcome.answer == crate::harness::Answer::Recovered,
        turns: inc.turns,
        terminated_by_review: inc.outcome.termination().map(ToString::to_string),
        quiet_secs: codex.and_then(|c| c.quiet_secs),
        last_rollout_event: codex
            .and_then(|c| c.last_rollout_event)
            .map(ToString::to_string),
        turn_error: inc.outcome.stated.clone(),
        stdout_bytes: inc.stdout.len(),
        stderr_bytes: inc.stderr.len(),
        stdout_truncated,
        transcript_path: codex
            .and_then(|c| c.transcript_path)
            .map(|path| path.to_string_lossy().into_owned()),
        final_answer_present,
        rate_limits,
        env,
    };
    // meta.json is the index `review incidents` reads and the file that makes
    // the bundle self-describing. If it can't be written (quota, unwritable
    // dir), don't advertise a path that points at a broken bundle.
    match serde_json::to_vec_pretty(&meta) {
        Ok(bytes) => {
            if let Err(error) = write_file(&dir, "meta.json", &bytes) {
                failures.push(error);
            }
        }
        Err(error) => failures.push(IncidentFailure {
            path: Some(dir.join("meta.json")),
            message: format!("failed to serialize incident metadata: {error}"),
        }),
    }

    if failures.is_empty() {
        BundleWriteOutcome::Written(dir)
    } else {
        BundleWriteOutcome::Incomplete {
            directory: Some(dir),
            failures,
        }
    }
}

/// Write `bytes` to `dir/name`, retaining the failed path and cause.
pub(crate) fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), IncidentFailure> {
    let path = dir.join(name);
    match std::fs::write(&path, bytes) {
        Ok(()) => Ok(()),
        Err(error) => Err(IncidentFailure {
            path: Some(path),
            message: error.to_string(),
        }),
    }
}

/// Last `max` bytes of `data`, plus whether it was truncated.
fn tail_bytes(data: &[u8], max: usize) -> (&[u8], bool) {
    if data.len() <= max {
        (data, false)
    } else {
        (&data[data.len() - max..], true)
    }
}

/// Last `max` non-empty lines, rejoined.
pub(crate) fn tail_lines(content: &str, max: usize) -> String {
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(max);
    lines[start..].join("\n")
}

/// Build a copy-pasteable shell command that reproduces the run: `cd` to the
/// recorded cwd (codex resolves rules/sandbox root from it), every env
/// override the spawn applied (the runner's own and the profile's alike), the
/// quoted argv, and stdin redirected from the bundle's absolute `prompt.txt`.
/// Secret env values become `<redacted>` so the string is safe to keep.
/// Self-contained: runnable from anywhere, and it can't pick up the wrong repo
/// or the wrong prompt.
fn replay_command(
    full_argv: &[String],
    env: &std::collections::BTreeMap<String, String>,
    cwd: &Path,
    prompt_path: &Path,
) -> String {
    let mut parts = Vec::new();
    for (k, v) in env {
        let val = if looks_secret(k) {
            "<redacted>"
        } else {
            v.as_str()
        };
        parts.push(format!("{k}={}", shell_quote(val)));
    }
    parts.extend(full_argv.iter().map(|a| shell_quote(a)));
    format!(
        "cd {} && {} < {}",
        shell_quote(&cwd.to_string_lossy()),
        parts.join(" "),
        shell_quote(&prompt_path.to_string_lossy()),
    )
}

/// Minimal POSIX shell quoting: bare when safe, single-quoted otherwise.
fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,@".contains(&b));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

impl Meta {
    /// Bundle evidence is tri-state, unlike Outcome: no readable transcript is
    /// not proof of no answer. Preserve that distinction for historical bundles.
    pub fn verdict(&self) -> String {
        if let Some(reason) = &self.turn_error {
            let mut chars = reason.lines().next().unwrap_or("").chars();
            let text: String = chars.by_ref().take(90).collect();
            let suffix = if chars.next().is_some() { "..." } else { "" };
            format!("turn failed: {text}{suffix}")
        } else if self.recovered_from_transcript {
            "recovered from transcript".into()
        } else {
            match self.final_answer_present {
                Some(false) => "no final answer (died)",
                Some(true) => "completed (stream/-o truncated)",
                None => "suspicious",
            }
            .into()
        }
    }
}

pub struct ListedIncident {
    pub dir: PathBuf,
    pub meta: Meta,
}

pub struct IncidentListing {
    pub incidents: Vec<ListedIncident>,
    pub failures: Vec<IncidentFailure>,
}

/// Recent incident bundles, newest first, ordered by their recorded UTC time.
/// Read failures are returned alongside the bundles that could be read.
pub fn list_recent(limit: usize) -> IncidentListing {
    list_recent_in(crate::data::DataDir::process(), limit)
}

pub fn list_recent_in(data: &crate::data::DataDir, limit: usize) -> IncidentListing {
    let base = match data.incidents() {
        Some(b) => b,
        None => {
            return IncidentListing {
                incidents: Vec::new(),
                failures: vec![IncidentFailure {
                    path: None,
                    message: "could not determine the incident data directory".to_string(),
                }],
            };
        }
    };
    list_recent_from(&base, limit)
}

/// Read incident bundles from an explicit directory, so listing can be
/// exercised without consulting the operator's data directory.
fn list_recent_from(base: &Path, limit: usize) -> IncidentListing {
    if limit == 0 {
        return IncidentListing {
            incidents: Vec::new(),
            failures: Vec::new(),
        };
    }
    let entries = match std::fs::read_dir(base) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return IncidentListing {
                incidents: Vec::new(),
                failures: Vec::new(),
            };
        }
        Err(error) => {
            return IncidentListing {
                incidents: Vec::new(),
                failures: vec![IncidentFailure {
                    path: Some(base.to_path_buf()),
                    message: format!("failed to read incident directory: {error}"),
                }],
            };
        }
    };
    let mut incidents = Vec::new();
    let mut failures = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                failures.push(IncidentFailure {
                    path: Some(base.to_path_buf()),
                    message: format!("failed to read incident directory entry: {error}"),
                });
                continue;
            }
        };
        let dir = entry.path();
        match entry.file_type() {
            Ok(kind) if !kind.is_dir() => continue,
            Ok(_) => {}
            Err(error) => {
                failures.push(IncidentFailure {
                    path: Some(dir),
                    message: format!("failed to inspect incident entry: {error}"),
                });
                continue;
            }
        }
        match read_incident_summary(&dir) {
            Ok(meta) => incidents.push(ListedIncident { dir, meta }),
            Err(error) => failures.push(error),
        }
    }
    incidents.sort_by(|a, b| {
        b.meta
            .timestamp
            .cmp(&a.meta.timestamp)
            .then_with(|| b.dir.file_name().cmp(&a.dir.file_name()))
    });
    incidents.truncate(limit);
    IncidentListing {
        incidents,
        failures,
    }
}

fn read_incident_summary(dir: &Path) -> Result<Meta, IncidentFailure> {
    let path = dir.join("meta.json");
    let content = std::fs::read_to_string(&path).map_err(|error| IncidentFailure {
        path: Some(path.clone()),
        message: format!("failed to read incident metadata: {error}"),
    })?;
    let meta: Meta = serde_json::from_str(&content).map_err(|error| IncidentFailure {
        path: Some(path.clone()),
        message: format!("failed to decode incident metadata: {error}"),
    })?;
    // A default schema of zero is deliberately unsupported, never a silent
    // interpretation of an unversioned or future bundle as today's schema.
    if meta.schema != INCIDENT_SCHEMA {
        return Err(IncidentFailure {
            path: Some(path),
            message: "unsupported incident metadata schema".to_string(),
        });
    }
    if meta.harness.is_empty() || !is_utc_timestamp(&meta.timestamp) {
        return Err(IncidentFailure {
            path: Some(path),
            message: "incident metadata has no harness or a bad UTC timestamp".to_string(),
        });
    }
    Ok(meta)
}

fn is_utc_timestamp(timestamp: &str) -> bool {
    let bytes = timestamp.as_bytes();
    let shape_is_valid = bytes.len() == 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'Z'
        && bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit)
        && bytes[11..13].iter().all(u8::is_ascii_digit)
        && bytes[14..16].iter().all(u8::is_ascii_digit)
        && bytes[17..19].iter().all(u8::is_ascii_digit);
    if !shape_is_valid {
        return false;
    }

    let year = decimal(&bytes[0..4]);
    let month = decimal(&bytes[5..7]);
    let day = decimal(&bytes[8..10]);
    let hour = decimal(&bytes[11..13]);
    let minute = decimal(&bytes[14..16]);
    let second = decimal(&bytes[17..19]);
    let leap_year = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year => 29,
        2 => 28,
        _ => return false,
    };

    (1..=days_in_month).contains(&day) && hour <= 23 && minute <= 59 && second <= 59
}

/// Convert ASCII decimal digits after `is_utc_timestamp` has checked the shape.
fn decimal(digits: &[u8]) -> u32 {
    digits
        .iter()
        .fold(0, |value, digit| value * 10 + u32::from(digit - b'0'))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn scratch_incident_dir() -> PathBuf {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(format!(
                "incidents-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn write_test_meta(base: &Path, name: &str, content: &str) {
        let dir = base.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta.json"), content).unwrap();
    }

    #[test]
    fn secret_env_names_are_redacted() {
        assert!(looks_secret("ANTHROPIC_API_KEY"));
        assert!(looks_secret("OPENAI_TOKEN"));
        assert!(looks_secret("DB_PASSWORD"));
        assert!(!looks_secret("CODEX_HOME"));
        assert!(!looks_secret("CARGO_TARGET_DIR"));
    }

    #[test]
    fn replay_command_is_runnable_and_redacts_secrets() {
        let argv = vec![
            "codex".to_string(),
            "exec".to_string(),
            "--sandbox".to_string(),
            "read-only".to_string(),
        ];
        let mut env = std::collections::BTreeMap::new();
        env.insert("RUST_BACKTRACE".to_string(), "1".to_string());
        env.insert("CODEX_HOME".to_string(), "/home/x/.codex".to_string());
        env.insert(
            "ANTHROPIC_API_KEY".to_string(),
            "sk-super-secret".to_string(),
        );
        let cmd = replay_command(
            &argv,
            &env,
            Path::new("/home/x/proj"),
            Path::new("/data/incidents/x/prompt.txt"),
        );
        assert!(cmd.starts_with("cd /home/x/proj && "));
        assert!(cmd.contains("RUST_BACKTRACE=1"), "cmd was: {cmd}");
        assert!(cmd.contains("CODEX_HOME=/home/x/.codex"));
        assert!(cmd.contains("ANTHROPIC_API_KEY='<redacted>'"));
        assert!(!cmd.contains("sk-super-secret"), "secret leaked: {cmd}");
        assert!(
            cmd.ends_with("codex exec --sandbox read-only < /data/incidents/x/prompt.txt"),
            "cmd was: {cmd}"
        );
    }

    #[test]
    fn shell_quote_handles_spaces_and_quotes() {
        assert_eq!(shell_quote("plain-value"), "plain-value");
        assert_eq!(shell_quote("has space"), "'has space'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn recent_incidents_filter_metadata_and_sort_by_recorded_time() {
        let base = scratch_incident_dir();
        write_test_meta(
            &base,
            "older",
            r#"{"schema":1,"timestamp":"2026-01-01T00:00:00Z","provider":"codex"}"#,
        );
        write_test_meta(
            &base,
            "newer",
            r#"{"schema":1,"timestamp":"2026-02-01T00:00:00Z","provider":"claude"}"#,
        );
        write_test_meta(
            &base,
            "wrong_schema",
            r#"{"schema":2,"timestamp":"2026-03-01T00:00:00Z","provider":"codex"}"#,
        );
        write_test_meta(
            &base,
            "missing_provider",
            r#"{"schema":1,"timestamp":"2026-03-01T00:00:00Z"}"#,
        );
        write_test_meta(
            &base,
            "bad_timestamp",
            r#"{"schema":1,"timestamp":"2026-03-01","provider":"codex"}"#,
        );

        let recent = list_recent_from(&base, 10);
        let names: Vec<_> = recent
            .incidents
            .iter()
            .map(|item| item.dir.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["newer".to_string(), "older".to_string()]);
        assert_eq!(recent.failures.len(), 3);

        let limited = list_recent_from(&base, 1);
        assert_eq!(limited.incidents.len(), 1);
        assert_eq!(
            limited.incidents[0]
                .dir
                .file_name()
                .and_then(|name| name.to_str()),
            Some("newer")
        );
        assert!(list_recent_from(&base, 0).incidents.is_empty());
        std::fs::remove_dir_all(base).unwrap();
    }

    // Shape of the first schema-1 writer (before replay/prompt/watchdog fields).
    // Missing later fields must remain readable; the old codex_version key stays.
    #[test]
    fn original_schema_one_metadata_stays_readable() {
        let base = scratch_incident_dir();
        let original = r#"{
            "schema":1,"timestamp":"2026-01-01T00:00:00Z","provider":"codex",
            "session_id":"old","review_version":"0.1.0","codex_version":"codex 0.146.0",
            "argv":["codex","exec"],"cwd":"/project","exit_code":1,"signal":null,
            "stdin_write_error":null,"duration_ms":1234,"captured":false,
            "recovered_from_transcript":false,"turns":0,"stdout_bytes":12,"stderr_bytes":4,
            "stdout_truncated":false,"transcript_path":"/rollout","final_answer_present":false,
            "rate_limits":{"primary":{"used_percent":10}},"env":[{"name":"API_KEY","value":null}]
        }"#;
        write_test_meta(&base, "original", original);
        let meta = read_incident_summary(&base.join("original")).expect("historical schema");
        assert_eq!(meta.codex_version.as_deref(), Some("codex 0.146.0"));
        assert_eq!(meta.verdict(), "no final answer (died)");
        assert!(meta.command.is_empty() && meta.quiet_secs.is_none());
        let value = serde_json::to_value(&meta).expect("same DTO writes");
        assert_eq!(value["codex_version"], "codex 0.146.0");
        assert!(value.get("forensics").is_none());
        assert_eq!(value["env"][0]["value"], serde_json::Value::Null);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn generic_bundle_and_codex_extension_both_use_the_same_schema() {
        use crate::harness::{Harness, Outcome, ProcessSpec, Stdin};
        let base = scratch_incident_dir();
        let process = ProcessSpec {
            program: "stub".into(),
            args: vec!["arg with space".into()],
            cwd: base.clone(),
            env: [("PWD".into(), base.to_string_lossy().into_owned())].into(),
            stdin: Stdin::Pipe(Vec::new()),
        };
        let outcome = Outcome::no_answer().stated("refused".into());
        use std::os::unix::ffi::OsStrExt;
        let rollout = base.join(std::ffi::OsStr::from_bytes(b"rollout-\xff.jsonl"));
        std::fs::write(&rollout, concat!(
            "{\"payload\":{\"type\":\"agent_message\",\"phase\":\"final_answer\",\"message\":\"old\"}}\n",
            "{\"payload\":{\"type\":\"token_count\",\"rate_limits\":{\"limit\":5}}}\n"
        )).unwrap();
        let baseline = std::fs::metadata(&rollout).unwrap().len();
        let path = rollout.as_path();
        let mut incident = Incident {
            harness: Harness::Codex,
            process: &process,
            harness_version: Some("stub version".into()),
            session_id: Some("session"),
            prompt: "request",
            exit: crate::process::Exit::Code(1),
            stdin_write_error: None,
            stdout: b"stream",
            stderr: b"error",
            duration_ms: 12,
            data: &crate::data::DataDir::at(base.clone()),
            outcome: &outcome,
            turns: 0,
            forensics: None,
        };
        // Current-run evidence must exclude the earlier answer and limits.
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap()
            .write_all(b"{\"payload\":{\"type\":\"token_count\",\"rate_limits\":{\"limit\":7}}}\n")
            .unwrap();
        for offset in [None, Some(baseline), Some(u64::MAX)] {
            incident.forensics = offset.map(|offset| {
                IncidentForensics::Codex(CodexIncident {
                    transcript_path: Some(path),
                    scope: if offset == u64::MAX {
                        crate::transcript::Scope::Untrusted
                    } else {
                        crate::transcript::Scope::From(crate::transcript::RolloutOffset(offset))
                    },
                    final_answer_present: Some(false),
                    quiet_secs: Some(3),
                    last_rollout_event: None,
                })
            });
            let directory = match write_bundle(&incident) {
                BundleWriteOutcome::Written(path) => path,
                other => panic!("bundle failed: {other:?}"),
            };
            let meta = read_incident_summary(&directory).expect("written schema reads");
            assert_eq!(meta.verdict(), "turn failed: refused");
            assert_eq!(meta.codex_version.as_deref(), Some("stub version"));
            assert_eq!(
                meta.command,
                replay_command(
                    &meta.argv,
                    &process.env,
                    &process.cwd,
                    &directory.join("prompt.txt")
                )
            );
            assert_eq!(
                meta.rate_limits,
                (offset == Some(baseline)).then(|| serde_json::json!({"limit":7}))
            );
            assert_eq!(meta.final_answer_present, offset.map(|_| false));
            if let Some(offset) = offset {
                let tail =
                    std::fs::read_to_string(directory.join("transcript.tail.jsonl")).unwrap();
                assert!(!tail.contains("old"));
                assert_eq!(tail.is_empty(), offset == u64::MAX);
            }
        }
        // The watchdog already licensed an answer, but recovery can no longer
        // read its file. Affirmative evidence still reaches the schema-1 bundle.
        incident.forensics = Some(IncidentForensics::Codex(CodexIncident {
            transcript_path: None,
            scope: crate::transcript::Scope::From(crate::transcript::RolloutOffset(baseline)),
            final_answer_present: crate::harness::codex::forensics::answer_presence(true, None),
            quiet_secs: Some(3),
            last_rollout_event: None,
        }));
        let directory = match write_bundle(&incident) {
            BundleWriteOutcome::Written(path) => path,
            other => panic!("preserved answer bundle failed: {other:?}"),
        };
        assert_eq!(
            read_incident_summary(&directory)
                .expect("preserved evidence reads")
                .final_answer_present,
            Some(true)
        );
        assert!(!directory.join("transcript.tail.jsonl").exists());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn tails_keep_the_end() {
        let (slice, truncated) = tail_bytes(b"abcdef", 3);
        assert_eq!(slice, b"def");
        assert!(truncated);
        let (slice, truncated) = tail_bytes(b"ab", 3);
        assert_eq!(slice, b"ab");
        assert!(!truncated);

        assert_eq!(tail_lines("a\n\nb\nc\n", 2), "b\nc");
    }
}
