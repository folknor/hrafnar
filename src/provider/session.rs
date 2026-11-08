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

    // No adapter imposes syntax on an id it is handed to resume. Do not impose
    // UUID syntax on historical opaque IDs; maki's own ids are base58, and a
    // session started outside `review` may be resumed by its id. New adapters
    // must decide their own shape here.
    fn validate_for(&self, provider: Provider) -> Result<()> {
        match provider {
            Provider::Claude | Provider::Codex | Provider::Grok | Provider::Maki => Ok(()),
        }
    }

    /// A hyphenated UUID, `8-4-4-4-12` hex digits: the form `review` mints,
    /// and the one maki's `--session-id` accepts besides its own base58.
    fn is_uuid(&self) -> bool {
        let groups: Vec<&str> = self.0.split('-').collect();
        groups.len() == 5
            && groups
                .iter()
                .zip([8, 4, 4, 4, 12])
                .all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()))
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
                Provider::Claude | Provider::Grok | Provider::Maki => {
                    if id.is_none() {
                        *id = Some(SessionId::parse(&crate::ids::generate_uuid())?);
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
            (Provider::Claude | Provider::Grok | Provider::Maki, Self::Fresh { id: None }) => {
                bail!("{provider} fresh launch requires a caller-minted session id")
            }
            (Provider::Codex, Self::Fresh { id: Some(_) }) => {
                bail!("codex reports fresh session ids from its stream")
            }
            // maki rejects a `--session-id` it cannot parse as a UUID or its
            // own base58, after `review` has already recorded the launch.
            (Provider::Maki, Self::Fresh { id: Some(id) }) if !id.is_uuid() => {
                bail!(
                    "maki fresh launch requires a UUID session id, got '{}'",
                    id.as_str()
                )
            }
            (Provider::Claude | Provider::Grok | Provider::Maki, Self::Fresh { id: Some(_) })
            | (Provider::Codex, Self::Fresh { id: None })
            | (
                Provider::Claude | Provider::Codex | Provider::Grok | Provider::Maki,
                Self::Resume(_),
            ) => Ok(()),
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
        for provider in [Provider::Claude, Provider::Grok, Provider::Maki] {
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
    fn maki_fresh_ids_must_be_uuids_but_resumed_ids_stay_opaque() {
        let minted = SessionStart::Fresh {
            id: Some(SessionId::parse("0b6c1f3e-4a2d-4e8b-9c51-7d2f8a6e3b10").expect("id")),
        };
        minted.validate(Provider::Maki).expect("a minted UUID");
        for bad in [
            "opaque",
            "0b6c1f3e-4a2d-4e8b-9c51-7d2f8a6e3b1",
            "0b6c1f3e4a2d4e8b9c517d2f8a6e3b10",
        ] {
            let start = SessionStart::Fresh {
                id: Some(SessionId::parse(bad).expect("safe id")),
            };
            assert!(start.validate(Provider::Maki).is_err(), "{bad}");
        }
        // maki's own ids are base58; resuming one started outside review works.
        SessionStart::Resume(SessionId::parse("1111114bZ6BZRUqUqZeo").expect("id"))
            .validate(Provider::Maki)
            .expect("resume ids are opaque");
    }

    #[test]
    fn resume_never_mints_or_adopts_an_echoed_identity() {
        for provider in Provider::all().iter().copied() {
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
