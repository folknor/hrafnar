//! Shared grammar for paths configured with `~`, `$VAR` or `${VAR}`.
//!
//! The callers decide what a malformed path means to their operation: project
//! selection fails, while an unusable writable root is dropped with a warning.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExpandError {
    HomeNotSet,
    UnsupportedUserTilde,
    UnclosedVariable,
    EmptyVariableName,
    VariableNotSet(String),
}

impl fmt::Display for ExpandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HomeNotSet => f.write_str("HOME is not set"),
            Self::UnsupportedUserTilde => f.write_str("only `~` and `~/...` expand"),
            Self::UnclosedVariable => f.write_str("unclosed '${'"),
            Self::EmptyVariableName => f.write_str("empty variable name"),
            Self::VariableNotSet(name) => write!(f, "${name} is not set"),
        }
    }
}

impl std::error::Error for ExpandError {}

/// Expand a configured path using one grammar for both project selection and
/// writable roots. Empty values count as unset, matching environment lookup
/// behavior elsewhere in `hrafnar`.
pub(crate) fn expand(
    raw: &str,
    var: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<String, ExpandError> {
    let get = |name: &str| var(name).filter(|value| !value.is_empty());
    let home = || get("HOME").ok_or(ExpandError::HomeNotSet);
    let raw = if raw == "~" {
        home()?
    } else if let Some(rest) = raw.strip_prefix("~/") {
        format!("{}/{rest}", home()?.trim_end_matches('/'))
    } else if raw.starts_with('~') {
        return Err(ExpandError::UnsupportedUserTilde);
    } else {
        raw.to_string()
    };

    let mut out = String::with_capacity(raw.len());
    let mut rest = raw.as_str();
    while let Some(idx) = rest.find('$') {
        out.push_str(&rest[..idx]);
        let after = &rest[idx + 1..];
        let (name, tail) = if let Some(braced) = after.strip_prefix('{') {
            match braced.find('}') {
                Some(end) => (&braced[..end], &braced[end + 1..]),
                None => return Err(ExpandError::UnclosedVariable),
            }
        } else {
            let end = after
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(after.len());
            (&after[..end], &after[end..])
        };
        if name.is_empty() {
            return Err(ExpandError::EmptyVariableName);
        }
        match get(name) {
            Some(value) => out.push_str(&value),
            None => return Err(ExpandError::VariableNotSet(name.to_string())),
        }
        rest = tail;
    }
    out.push_str(rest);
    Ok(out)
}
