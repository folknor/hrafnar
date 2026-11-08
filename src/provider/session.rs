//! Executable session identity. Persisted evidence deliberately remains strings.
use super::Provider;
use anyhow::{Result, bail};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionId(String);

impl SessionId {
    pub fn parse(id: &str) -> Result<Self> {
        if !Self::is_safe(id) {
            bail!("invalid session id: expected 1-128 ASCII letters, digits, '-' or '_'");
        }
        Ok(Self(id.to_string()))
    }

    pub(crate) fn is_safe(id: &str) -> bool {
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    // No current adapter imposes additional syntax. Do not impose UUID syntax
    // on historical opaque IDs; new adapters must decide their own shape here.
    fn validate_for(&self, provider: Provider) -> Result<()> {
        match provider {
            Provider::Claude | Provider::Codex | Provider::Grok => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionStart {
    Fresh { id: Option<SessionId> },
    Resume(SessionId),
}

impl SessionStart {
    /// Only the invocation owner mints IDs. Runners validate, never mint.
    pub fn prepare(&mut self, provider: Provider) -> Result<()> {
        if let Self::Fresh { id } = self {
            match provider {
                Provider::Claude | Provider::Grok => {
                    if id.is_none() {
                        *id = Some(SessionId::parse(&crate::config::generate_uuid())?);
                    }
                }
                Provider::Codex => {}
            }
        }
        self.validate(provider)
    }

    pub(super) fn validate(&self, provider: Provider) -> Result<()> {
        if let Some(id) = self.id() {
            id.validate_for(provider)?;
        }
        match (provider, self) {
            (Provider::Claude | Provider::Grok, Self::Fresh { id: None }) => {
                bail!("{provider} fresh launch requires a caller-minted session id")
            }
            (Provider::Codex, Self::Fresh { id: Some(_) }) => {
                bail!("codex reports fresh session ids from its stream")
            }
            (Provider::Claude | Provider::Grok, Self::Fresh { id: Some(_) })
            | (Provider::Codex, Self::Fresh { id: None })
            | (Provider::Claude | Provider::Codex | Provider::Grok, Self::Resume(_)) => Ok(()),
        }
    }

    pub fn id(&self) -> Option<&SessionId> {
        match self {
            Self::Fresh { id } => id.as_ref(),
            Self::Resume(id) => Some(id),
        }
    }

    pub(super) fn is_fresh(&self) -> bool {
        matches!(self, Self::Fresh { .. })
    }

    pub(super) fn result_id(&self, echoed: Option<String>) -> Option<String> {
        match self {
            Self::Fresh { id } => echoed.or_else(|| id.as_ref().map(|id| id.as_str().to_string())),
            Self::Resume(id) => Some(id.as_str().to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preparation_mints_only_for_caller_assigned_adapters_and_preserves_ids() {
        for provider in [Provider::Claude, Provider::Grok] {
            let mut start = SessionStart::Fresh { id: None };
            assert!(start.validate(provider).is_err());
            start.prepare(provider).expect("prepare");
            let prepared = start.clone();
            start.prepare(provider).expect("prepare again");
            assert_eq!(start, prepared);
            assert!(start.id().is_some());
        }
        let mut codex = SessionStart::Fresh { id: None };
        codex.prepare(Provider::Codex).expect("stream assigned");
        assert!(codex.id().is_none());
        let mut supplied = SessionStart::Fresh {
            id: Some(SessionId::parse("opaque").expect("id")),
        };
        assert!(supplied.prepare(Provider::Codex).is_err());
    }

    #[test]
    fn resume_never_mints_or_adopts_an_echoed_identity() {
        for provider in [Provider::Claude, Provider::Codex, Provider::Grok] {
            let mut start = SessionStart::Resume(SessionId::parse("old_opaque-ID").expect("id"));
            let saved = start.clone();
            start.prepare(provider).expect("prepare");
            assert_eq!(start, saved);
            assert_eq!(
                start.result_id(Some("different".into())),
                Some("old_opaque-ID".into())
            );
        }
    }
}
