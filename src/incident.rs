//! Forensic capture for suspicious provider runs.
//!
//! When a codex run looks wrong (not captured / non-zero exit / signal) the
//! live digest + transcript summary are not enough to explain *why* it died -
//! the investigation that motivated this module kept stalling on information
//! `review` had thrown away: codex's stderr, the raw NDJSON stream, whether the
//! prompt even finished writing, and (absent `RUST_BACKTRACE`) any panic trace.
//!
//! `write_bundle` dumps all of it to `~/.local/share/review/incidents/<dir>/`
//! so the *next* death is over-instrumented instead of a mystery. It is
//! best-effort: `write_bundle` returns failures to its caller, which reports
//! them without derailing the run. Only suspicious runs write a bundle, so
//! clean runs stay uncluttered.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Everything known about a finished provider run, handed to the bundle writer.
pub struct Incident<'a> {
    pub provider: &'a str,
    /// The binary invoked (e.g. "codex") - argv[0] of the replayable command.
    pub binary: &'a str,
    /// `<binary> <leading args> --version`, best-effort; deaths may be version-specific. It
    /// probes the binary that actually ran, not the provider name: those differ
    /// whenever the binary is overridden (the test harness runs a stub), and
    /// probing the name recorded the host's real codex for a stub run. Taken by
    /// the caller (`contained::version_probe`), so the probe is contained like
    /// any provider process.
    pub codex_version: Option<String>,
    pub session_id: Option<&'a str>,
    /// The args passed to the binary (not including argv[0]).
    pub argv: &'a [String],
    /// The prompt piped on the child's stdin - part of the invocation, so a
    /// replay is impossible without it.
    pub prompt: &'a str,
    pub cwd: &'a Path,
    /// Profile env (names always recorded; values redacted when the name looks
    /// secret, so the bundle never leaks tokens).
    pub env: Option<&'a std::collections::BTreeMap<String, String>>,
    pub exit_code: Option<i32>,
    pub signal: Option<&'a str>,
    /// `Some` if writing the prompt to the child's stdin failed (e.g. a broken
    /// pipe because the child exited before consuming it).
    pub stdin_write_error: Option<&'a str>,
    pub stdout: &'a [u8],
    pub stderr: &'a [u8],
    pub transcript_path: Option<&'a str>,
    /// The same scope passed to transcript recovery. An offset beyond EOF
    /// means no bytes can safely be attributed to this run.
    pub transcript_offset: Option<u64>,
    /// Whether the runner's scoped transcript summary found a final answer.
    pub final_answer_present: Option<bool>,
    pub duration_ms: u128,
    /// Overrides the XDG data root the bundle is written under. `None` in
    /// production; set by tests so a stub run cannot write into the operator's
    /// real incident directory.
    pub data_root: Option<&'a Path>,
    pub captured: bool,
    pub recovered_from_transcript: bool,
    pub turns: u32,
    /// Why `review` killed the run, when it did (watchdog verdict or operator
    /// signal). `None` means codex ended on its own.
    pub terminated_by_review: Option<&'a str>,
    /// How long the rollout had been unchanged when the watchdog acted, and the
    /// last event it saw. The pair that makes a stall-timeout misfire on a
    /// healthy run diagnosable after the fact.
    pub quiet_secs: Option<u64>,
    pub last_rollout_event: Option<&'a str>,
    /// Codex's own stated reason for the turn ending (stream `error` /
    /// `turn.failed`) - typically an upstream refusal. Recorded so a bundle that
    /// has an explanation says so on its face, rather than being filed alongside
    /// the genuinely unexplained deaths this directory exists for.
    pub turn_error: Option<&'a str>,
}

/// Keep the last MiB of the raw NDJSON stream - the death is at the end, and a
/// long agentic run's stream can be tens of MiB.
const STDOUT_TAIL_BYTES: usize = 1 << 20;
/// Raw transcript lines to keep in the tail (the full file stays on disk).
const TRANSCRIPT_TAIL_LINES: usize = 80;
const INCIDENT_SCHEMA: u32 = 1;

#[derive(Serialize)]
struct Meta {
    schema: u32,
    timestamp: String,
    provider: String,
    session_id: Option<String>,
    review_version: &'static str,
    codex_version: Option<String>,
    /// Full argv including argv[0] (the binary).
    argv: Vec<String>,
    /// A copy-pasteable shell command that reproduces the run: the injected env
    /// prefix, the quoted argv, and `< prompt.txt` (the prompt is piped on
    /// stdin, so it's written alongside as `prompt.txt`). Secret env values are
    /// shown as `<redacted>` - fill them back in to actually run it.
    command: String,
    cwd: String,
    prompt_bytes: usize,
    exit_code: Option<i32>,
    signal: Option<String>,
    stdin_write_error: Option<String>,
    duration_ms: u128,
    captured: bool,
    recovered_from_transcript: bool,
    turns: u32,
    terminated_by_review: Option<String>,
    quiet_secs: Option<u64>,
    last_rollout_event: Option<String>,
    /// Codex's stated reason for the turn ending, when it gave one.
    turn_error: Option<String>,
    stdout_bytes: usize,
    stderr_bytes: usize,
    stdout_truncated: bool,
    transcript_path: Option<String>,
    /// Whether the shared transcript summary found a final-answer phase in the
    /// scoped bytes. `Some(false)` means it found none.
    final_answer_present: Option<bool>,
    /// Rate-limit snapshot from the last `token_count` event (raw JSON), so a
    /// limit-driven stop is visible without re-reading the transcript.
    rate_limits: Option<serde_json::Value>,
    /// Env var names configured for the run; values only when non-secret.
    env: Vec<EnvVar>,
}

#[derive(Serialize)]
struct EnvVar {
    name: String,
    value: Option<String>,
}

/// Where bundles are written. `data_root` overrides the real XDG location and
/// exists so the test harness cannot deposit stub bundles in the operator's
/// actual `~/.local/share/review/incidents` - it did exactly that before this
/// was injectable, because redirecting `CODEX_HOME` only redirects the *child*.
fn incidents_dir(data_root: Option<&Path>) -> Option<PathBuf> {
    let data_dir = match data_root {
        Some(root) => root.to_path_buf(),
        None => std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .ok()?,
    };
    Some(data_dir.join("review").join("incidents"))
}

/// A name that shouldn't have its value recorded (heuristic, errs toward hiding).
/// Bundles keep nonsecret values for replay and redact likely secrets. Sidecars
/// record env names only, so omitting values there is compatible with this rule.
fn looks_secret(name: &str) -> bool {
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
    let Some(base) = incidents_dir(inc.data_root) else {
        return BundleWriteOutcome::Unavailable(
            "could not determine the incident data directory".to_string(),
        );
    };
    let timestamp = crate::audit::chrono_now();
    let stamp = timestamp.replace(':', "-");
    let sid = inc.session_id.unwrap_or("no-session");
    // A random suffix keeps two same-second failures (e.g. two `--stagger 0`
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
    let dir = base.join(format!("{stamp}-{}-{sid}-{nonce}", inc.provider));
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

    // Full stderr - the highest-value channel we used to discard. With
    // RUST_BACKTRACE set on the child, a panic lands here with its trace.
    if let Err(error) = write_file(&dir, "stderr.txt", inc.stderr) {
        failures.push(error);
    }

    // The prompt piped on stdin, and the runnable command that consumes it.
    if let Err(error) = write_file(&dir, "prompt.txt", inc.prompt.as_bytes()) {
        failures.push(error);
    }
    let mut full_argv = vec![inc.binary.to_string()];
    full_argv.extend(inc.argv.iter().cloned());
    let command = replay_command(&full_argv, inc.env, inc.cwd, &dir.join("prompt.txt"));

    // Raw NDJSON stream, tail-capped. The interesting part is where it stops.
    let (stdout_slice, stdout_truncated) = tail_bytes(inc.stdout, STDOUT_TAIL_BYTES);
    if let Err(error) = write_file(&dir, "stdout.jsonl", stdout_slice) {
        failures.push(error);
    }

    // The excerpt and metadata must describe this invocation, not an earlier
    // turn in the same resumed session. Use the exact byte baseline that scoped
    // recovery; an untrusted baseline is represented by an offset beyond EOF.
    let mut rate_limits = None;
    if let Some(path) = inc.transcript_path {
        match std::fs::read(path) {
            Ok(bytes) => {
                let scoped = crate::transcript::slice_from_offset(&bytes, inc.transcript_offset);
                let content = String::from_utf8_lossy(scoped);
                let tail = tail_lines(&content, TRANSCRIPT_TAIL_LINES);
                if let Err(error) = write_file(&dir, "transcript.tail.jsonl", tail.as_bytes()) {
                    failures.push(error);
                }
                rate_limits = scan_rate_limits(&content);
            }
            Err(error) => failures.push(IncidentFailure {
                path: Some(PathBuf::from(path)),
                message: format!("failed to read rollout transcript: {error}"),
            }),
        }
    }
    let final_answer_present = inc.final_answer_present;

    let env = inc
        .env
        .map(|m| {
            m.iter()
                .map(|(k, v)| EnvVar {
                    name: k.clone(),
                    value: (!looks_secret(k)).then(|| v.clone()),
                })
                .collect()
        })
        .unwrap_or_default();

    let meta = Meta {
        schema: INCIDENT_SCHEMA,
        timestamp,
        provider: inc.provider.to_string(),
        session_id: inc.session_id.map(str::to_string),
        review_version: env!("CARGO_PKG_VERSION"),
        codex_version: inc.codex_version.clone(),
        argv: full_argv,
        command,
        cwd: inc.cwd.to_string_lossy().into_owned(),
        prompt_bytes: inc.prompt.len(),
        exit_code: inc.exit_code,
        signal: inc.signal.map(str::to_string),
        stdin_write_error: inc.stdin_write_error.map(str::to_string),
        duration_ms: inc.duration_ms,
        captured: inc.captured,
        recovered_from_transcript: inc.recovered_from_transcript,
        turns: inc.turns,
        terminated_by_review: inc.terminated_by_review.map(str::to_string),
        quiet_secs: inc.quiet_secs,
        last_rollout_event: inc.last_rollout_event.map(str::to_string),
        turn_error: inc.turn_error.map(str::to_string),
        stdout_bytes: inc.stdout.len(),
        stderr_bytes: inc.stderr.len(),
        stdout_truncated,
        transcript_path: inc.transcript_path.map(str::to_string),
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
fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), IncidentFailure> {
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
fn tail_lines(content: &str, max: usize) -> String {
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(max);
    lines[start..].join("\n")
}

/// Extract the last non-null rate-limit snapshot from an already run-scoped
/// rollout excerpt. Answer recognition belongs to `transcript::summarize`.
fn scan_rate_limits(content: &str) -> Option<serde_json::Value> {
    let mut rate_limits = None;
    for line in content.lines() {
        let val: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let payload = val.get("payload");
        if payload.and_then(|p| p.get("type")).and_then(|t| t.as_str()) == Some("token_count")
            && let Some(rl) = payload.and_then(|p| p.get("rate_limits"))
            && !rl.is_null()
        {
            rate_limits = Some(rl.clone());
        }
    }
    rate_limits
}

/// Build a copy-pasteable shell command that reproduces the run: `cd` to the
/// recorded cwd (codex resolves rules/sandbox root from it), the injected
/// `RUST_BACKTRACE=1` plus profile env, the quoted argv, and stdin redirected
/// from the bundle's absolute `prompt.txt`. Secret env values become
/// `<redacted>` so the string is safe to keep. Self-contained: runnable from
/// anywhere, and it can't pick up the wrong repo or the wrong prompt.
fn replay_command(
    full_argv: &[String],
    env: Option<&std::collections::BTreeMap<String, String>>,
    cwd: &Path,
    prompt_path: &Path,
) -> String {
    let mut parts = vec!["RUST_BACKTRACE=1".to_string()];
    if let Some(m) = env {
        for (k, v) in m {
            let val = if looks_secret(k) {
                "<redacted>"
            } else {
                v.as_str()
            };
            parts.push(format!("{k}={}", shell_quote(val)));
        }
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

/// A subset of `meta.json` read back for `review incidents`. Fields default for
/// deserialization, but `read_incident_summary` lists only a bundle whose
/// `schema` matches and that carries a provider and a UTC timestamp.
#[derive(Deserialize, Default)]
pub struct IncidentSummary {
    #[serde(default)]
    pub timestamp: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub signal: Option<String>,
    #[serde(default)]
    pub recovered_from_transcript: bool,
    #[serde(default)]
    pub final_answer_present: Option<bool>,
    #[serde(default)]
    pub codex_version: Option<String>,
    /// Codex's stated reason for the turn ending. When present it *is* the
    /// verdict - the bundle is explained, not a mystery death.
    #[serde(default)]
    pub turn_error: Option<String>,
}

pub struct ListedIncident {
    pub dir: PathBuf,
    pub meta: IncidentSummary,
}

pub struct IncidentListing {
    pub incidents: Vec<ListedIncident>,
    pub failures: Vec<IncidentFailure>,
}

/// Recent incident bundles, newest first, ordered by their recorded UTC time.
/// Read failures are returned alongside the bundles that could be read.
pub fn list_recent(limit: usize) -> IncidentListing {
    // Always the real location: `review incidents` is an operator-facing view.
    let base = match incidents_dir(None) {
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

fn read_incident_summary(dir: &Path) -> Result<IncidentSummary, IncidentFailure> {
    let path = dir.join("meta.json");
    let content = std::fs::read_to_string(&path).map_err(|error| IncidentFailure {
        path: Some(path.clone()),
        message: format!("failed to read incident metadata: {error}"),
    })?;
    let value: serde_json::Value =
        serde_json::from_str(&content).map_err(|error| IncidentFailure {
            path: Some(path.clone()),
            message: format!("failed to parse incident metadata: {error}"),
        })?;
    if value.get("schema").and_then(serde_json::Value::as_u64) != Some(u64::from(INCIDENT_SCHEMA)) {
        return Err(IncidentFailure {
            path: Some(path),
            message: "unsupported incident metadata schema".to_string(),
        });
    }
    let meta: IncidentSummary = serde_json::from_value(value).map_err(|error| IncidentFailure {
        path: Some(path.clone()),
        message: format!("failed to decode incident metadata: {error}"),
    })?;
    if meta.provider.is_empty() || !is_utc_timestamp(&meta.timestamp) {
        return Err(IncidentFailure {
            path: Some(path),
            message: "incident metadata has no provider or a bad UTC timestamp".to_string(),
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
    fn scan_keeps_the_last_non_null_rate_limit_snapshot() {
        let rollout = concat!(
            r#"{"payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":31.0}}}}"#,
            "\n",
            r#"{"payload":{"type":"token_count","rate_limits":null}}"#,
        );
        assert_eq!(
            scan_rate_limits(rollout),
            Some(serde_json::json!({"primary":{"used_percent":31.0}}))
        );
    }

    #[test]
    fn scan_skips_malformed_lines() {
        let rollout = concat!(
            "not json\n",
            r#"{"payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":31.0}}}}"#,
            "\n",
        );
        assert_eq!(
            scan_rate_limits(rollout),
            Some(serde_json::json!({"primary":{"used_percent":31.0}}))
        );
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
        env.insert("CODEX_HOME".to_string(), "/home/x/.codex".to_string());
        env.insert(
            "ANTHROPIC_API_KEY".to_string(),
            "sk-super-secret".to_string(),
        );
        let cmd = replay_command(
            &argv,
            Some(&env),
            Path::new("/home/x/proj"),
            Path::new("/data/incidents/x/prompt.txt"),
        );
        assert!(cmd.starts_with("cd /home/x/proj && RUST_BACKTRACE=1 "));
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
