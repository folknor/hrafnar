//! A Claude OAuth token fetched by the operator's global
//! `[_defaults].claude_token_command`, for a claude run whose own login failed.
//!
//! Why it exists: an operator who authenticates claude by exporting
//! `CLAUDE_CODE_OAUTH_TOKEN` from a login shell has a claude that works from
//! any terminal, but Claude Code does not pass that variable to the shells it
//! runs. A `review` launched from inside a Claude Code session therefore
//! starts a claude with no token, which falls back to its saved login and, if
//! that is stale, fails after about 30s with "OAuth session expired and could
//! not be refreshed".
//!
//! The command is run lazily - only after a fresh run failed to authenticate,
//! or before continuing a session that was created with it - so a synced
//! global config is harmless on hosts that log claude in some other way. Its
//! output goes only into the environment of that claude process, and is never
//! printed, recorded, or kept past the `review` process.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The environment variable claude reads an OAuth token from.
pub const TOKEN_VAR: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// Recorded on a session row whose launch authenticated with the command.
/// Only the fact, never the token.
pub const CREDENTIAL_TOKEN_COMMAND: &str = "token_command";

/// Long enough for a password manager to answer; a helper that waits for
/// input it can never get (stdin is closed) is killed past it.
const HELPER_DEADLINE: Duration = Duration::from_secs(60);

/// Shorter than any Claude OAuth token.
const MIN_TOKEN_LEN: usize = 32;

/// A token. Its `Debug` never shows the value.
#[derive(Clone, PartialEq, Eq)]
pub struct ClaudeToken(String);

impl ClaudeToken {
    pub(super) fn value(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ClaudeToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClaudeToken(<redacted>)")
    }
}

/// The configured command and, once run, its result, shared by every claude
/// run in one `review` process so a fan-out runs it at most once.
#[derive(Debug)]
pub struct TokenSource {
    command: String,
    fetched: OnceLock<Result<ClaudeToken, String>>,
}

impl TokenSource {
    pub fn new(command: String) -> Self {
        Self {
            command,
            fetched: OnceLock::new(),
        }
    }

    /// Run the command once (blocking) and return its token, or why there is
    /// none. Later calls return the first result.
    pub fn fetch(&self) -> Result<ClaudeToken, String> {
        self.fetched
            .get_or_init(|| run_helper(&self.command))
            .clone()
    }
}

/// `sh -c <command>` from `$HOME` (never the project, whose files the command
/// is not meant to see), stdin closed, stderr passed through so a helper can
/// explain itself to the operator, stdout read as the token.
fn run_helper(command: &str) -> Result<ClaudeToken, String> {
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/".into());
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(&home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("could not run claude_token_command: {e}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or("claude_token_command stdout unavailable")?;
    let (sent, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = sent.send(stdout.read_to_end(&mut out).map(|_| out));
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < HELPER_DEADLINE => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Err(format!(
                    "claude_token_command did not finish in {}s",
                    HELPER_DEADLINE.as_secs()
                ));
            }
            Err(e) => return Err(format!("claude_token_command failed: {e}")),
        }
    };
    if !status.success() {
        return Err(format!("claude_token_command exited with {status}"));
    }
    let out = match received.recv_timeout(HELPER_DEADLINE.saturating_sub(started.elapsed())) {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(format!("reading claude_token_command output failed: {e}")),
        Err(_) => return Err("claude_token_command output did not end".into()),
    };
    parse_token(&out)
}

/// The command's stdout, trimmed. One line of printable text, or an error
/// that names the problem without echoing what was printed.
fn parse_token(out: &[u8]) -> Result<ClaudeToken, String> {
    let text = std::str::from_utf8(out)
        .map_err(|_| "claude_token_command printed something that is not text".to_string())?
        .trim();
    if text.is_empty() {
        return Err("claude_token_command printed nothing".into());
    }
    if text.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("claude_token_command printed more than one token".into());
    }
    // Claude OAuth tokens are ASCII. Requiring it also keeps redaction exact:
    // a command line cut mid-character would otherwise hide a token prefix
    // behind a replacement character.
    if !text.is_ascii() {
        return Err("claude_token_command printed non-ASCII text, which is not a token".into());
    }
    // Real tokens are long. A short value is almost certainly not one, and
    // scrubbing claude's output of it would corrupt ordinary words and fields.
    let chars = text.chars().count();
    if chars < MIN_TOKEN_LEN {
        return Err(format!(
            "claude_token_command printed {chars} characters, too short to be a token"
        ));
    }
    Ok(ClaudeToken(text.to_string()))
}

/// Whether the claude child would already have a token: inherited from this
/// process, or set by the profile. Retrying with the command then cannot help
/// - it is that token that was refused.
pub fn env_has_token(profile_env: Option<&std::collections::BTreeMap<String, String>>) -> bool {
    let set = |v: &str| !v.trim().is_empty();
    match profile_env.and_then(|env| env.get(TOKEN_VAR)) {
        Some(value) => set(value),
        None => std::env::var(TOKEN_VAR).is_ok_and(|v| set(&v)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_trimmed_and_never_debug_printed() {
        let token = parse_token(b"  sk-ant-oat01-0123456789abcdefghijklmnop\n").expect("token");
        assert_eq!(token.value(), "sk-ant-oat01-0123456789abcdefghijklmnop");
        assert_eq!(format!("{token:?}"), "ClaudeToken(<redacted>)");
    }

    #[test]
    fn empty_multi_line_and_binary_output_are_refused_without_echo() {
        for out in [
            &b""[..],
            b"  \n",
            b"sekrit\nb",
            b"sekrit b",
            b"\xff\xfe",
            b"sekrit",
        ] {
            let err = parse_token(out).expect_err("refused");
            assert!(!err.contains("sekrit"), "{err}");
        }
    }

    #[test]
    fn the_helper_runs_once_and_its_result_is_cached() {
        let tok = "tok-0123456789abcdefghijklmnopqrstuvwxyz";
        let source = TokenSource::new(format!("echo {tok}"));
        assert_eq!(source.fetch().expect("token").value(), tok);
        assert_eq!(source.fetch().expect("token").value(), tok);
    }

    #[test]
    fn a_failing_helper_reports_its_status() {
        let err = TokenSource::new("exit 3".into())
            .fetch()
            .expect_err("failed");
        assert!(err.contains("exited"), "{err}");
    }

    #[test]
    fn a_profile_token_counts_and_a_blank_one_does_not() {
        let mut env = std::collections::BTreeMap::new();
        env.insert(TOKEN_VAR.to_string(), "x".to_string());
        assert!(env_has_token(Some(&env)));
        env.insert(TOKEN_VAR.to_string(), " ".to_string());
        assert!(!env_has_token(Some(&env)));
    }
}
