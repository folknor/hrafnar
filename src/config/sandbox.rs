use super::Provider;

/// Translate `review`'s sandbox vocabulary into a provider's own.
///
/// `review`'s levels, in increasing order of access, are `read-only`,
/// `workspace-write` and `danger-full-access`. The names are codex's, because
/// they were `review`'s documented config surface before any translation
/// existed - adopting a fresh set would have silently changed the meaning of
/// every profile already written.
///
/// The problem this solves is that the names are not interchangeable: codex's
/// writable profile is `workspace-write`, grok's is `workspace`, and grok
/// *hard-errors* on a name it cannot resolve rather than falling back. So a
/// profile written for one provider broke the moment it was pointed at the
/// other, at launch time, for a reason that reads as a missing config file.
///
/// An unrecognised value is passed through verbatim rather than rejected. Grok
/// resolves sandbox names against user-defined profiles in `~/.grok/sandbox.toml`
/// (and codex has its own config surface), so a name `review` does not know may
/// still be one the operator legitimately defined. Rejecting it here would make
/// `review` the reason a valid provider config could not be used; passing it on
/// leaves the provider to validate its own vocabulary, which it does with a
/// better error than we could write. The cost is that a typo reaches the
/// provider - acceptable, because it fails before any turn runs and quotes the
/// offending name.
///
/// The three level names are consequently **reserved**: they always mean
/// `review`'s level, so a custom grok profile named `workspace-write` or
/// `danger-full-access` cannot be selected through a `.review.toml` profile.
/// That is the deliberate trade - the whole point is that these three names mean
/// the same thing whichever provider a profile is pointed at, which fails the
/// moment one provider can reinterpret them. Custom profiles have the entire
/// rest of the namespace, and `read-only` is unaffected either way because every
/// provider already spells it that way.
pub fn sandbox_for(provider: Provider, sandbox: &str) -> String {
    match provider {
        // Grok's built-ins are `read-only`, `workspace` and `none`.
        Provider::Grok => match sandbox {
            "workspace-write" => "workspace".to_string(),
            "danger-full-access" => "none".to_string(),
            _ => sandbox.to_string(),
        },
        // codex's names are `review`'s. `read-only` happens to be spelled the
        // same everywhere, which is why the default path was portable across
        // providers by luck rather than design. Claude has no filesystem
        // sandbox; `invoke` never hands it the value.
        Provider::Codex | Provider::Claude => sandbox.to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    const CODEX: Provider = Provider::Codex;
    const GROK: Provider = Provider::Grok;

    #[test]
    fn sandbox_levels_translate_per_provider() {
        // The default has to stay portable: it is what every bare run gets and
        // it is the invariant that a run cannot write. Claude is deliberately
        // not in this list - it has no filesystem sandbox to translate to, and
        // `invoke` drops the value rather than mapping it, so asserting a level
        // survives translation for claude would imply an enforcement claude does
        // not make.
        for provider in [CODEX, GROK] {
            assert_eq!(
                sandbox_for(provider, "read-only"),
                "read-only",
                "{provider} must understand the default level"
            );
        }

        // The levels that actually differ. Passing codex's spelling to grok is
        // not a soft failure - grok cannot resolve the profile and refuses to
        // start - so this mapping is what makes one profile vocabulary usable
        // across providers.
        assert_eq!(sandbox_for(CODEX, "workspace-write"), "workspace-write");
        assert_eq!(sandbox_for(GROK, "workspace-write"), "workspace");
        assert_eq!(
            sandbox_for(CODEX, "danger-full-access"),
            "danger-full-access"
        );
        assert_eq!(sandbox_for(GROK, "danger-full-access"), "none");
    }

    #[test]
    fn unknown_sandbox_names_reach_the_provider_unchanged() {
        // Grok resolves names against user-defined profiles in
        // `~/.grok/sandbox.toml`, so a name `review` does not recognise may be
        // perfectly valid. Rewriting or rejecting it would make `review` the
        // reason a working provider config could not be used.
        assert_eq!(sandbox_for(GROK, "my-custom-profile"), "my-custom-profile");
        assert_eq!(sandbox_for(CODEX, "my-custom-profile"), "my-custom-profile");
    }

    /// `review`'s documented sandbox vocabulary. Kept here rather than in the
    /// module so it cannot drift into looking like a runtime allowlist - the
    /// translation deliberately passes unknown names through.
    const SANDBOX_LEVELS: &[&str] = &["read-only", "workspace-write", "danger-full-access"];

    #[test]
    fn every_documented_level_maps_somewhere() {
        // Guards the pairing between the documented vocabulary and the match
        // arms: adding a level here without teaching grok about it would
        // otherwise pass it through verbatim and fail at launch.
        for level in SANDBOX_LEVELS {
            let mapped = sandbox_for(GROK, level);
            assert!(
                ["read-only", "workspace", "none"].contains(&mapped.as_str()),
                "{level} maps to {mapped}, which is not one of grok's built-in profiles"
            );
        }
    }
}
