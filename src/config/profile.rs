//! Provider-specific semantic checks after the shared profile schema is read.
use super::{Profile, Provider};
use crate::provider::{ModelSpec, Permissions};
use anyhow::{Result, bail};

impl Profile {
    pub(super) fn validate(&self, provider: Provider) -> Result<()> {
        match provider {
            Provider::Claude => {
                if self.sandbox.is_some() {
                    bail!(
                        "claude does not honour 'sandbox'; remove sandbox from this profile and configure permissions in Claude Code instead"
                    );
                }
            }
            Provider::Maki => {
                if self.sandbox.is_some() {
                    bail!(
                        "maki does not honour 'sandbox'; remove sandbox from this profile and configure permissions in maki's permissions.toml instead"
                    );
                }
            }
            Provider::Codex | Provider::Grok => {}
        }
        if !self.writable_roots.is_empty()
            && !Permissions::profile(provider, self.sandbox.as_ref()).accepts_profile_roots()
        {
            match provider {
                Provider::Claude => bail!(
                    "claude does not honour 'writable_roots'; remove writable_roots from this profile and configure permissions in Claude Code instead"
                ),
                Provider::Maki => bail!(
                    "maki does not honour 'writable_roots'; remove writable_roots from this profile and configure permissions in maki's permissions.toml instead"
                ),
                Provider::Codex | Provider::Grok => bail!(
                    "writable_roots are not honoured at this sandbox level; remove writable_roots or set sandbox = \"workspace-write\""
                ),
            }
        }
        ModelSpec::new(
            provider,
            self.model.as_deref(),
            self.effort.as_deref(),
            &self.config,
        )
        .validate(provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Layer, parse_file};

    #[test]
    fn unsupported_profile_fields_fail_in_every_table_and_layer() {
        for layer in [Layer::Local, Layer::Global] {
            for host in ["", "inactive."] {
                for (provider, field) in [
                    ("claude", "sandbox = 'read-only'"),
                    ("claude", "writable_roots = ['/srv/cache']"),
                    ("claude", "config = ['a=b']"),
                    ("grok", "config = ['a=b']"),
                    ("maki", "sandbox = 'read-only'"),
                    ("maki", "writable_roots = ['/srv/cache']"),
                    ("maki", "config = ['a=b']"),
                    ("maki", "effort = 'high'"),
                    ("codex", "writable_roots = ['/srv/cache']"),
                    (
                        "grok",
                        "sandbox = 'custom'\nwritable_roots = ['/srv/cache']",
                    ),
                ] {
                    let table = format!("{host}{provider}.test");
                    let error = parse_file(&format!("[{table}]\n{field}"), "source.toml", layer)
                        .expect_err("ignored field must fail")
                        .to_string();
                    assert!(error.contains(&format!("[{table}]")), "{error}");
                    assert!(error.contains("source.toml"), "{error}");
                    assert!(error.contains("remove"), "{error}");
                }
            }
        }
    }

    #[test]
    fn supported_profiles_and_harmless_empty_lists_still_load() {
        for provider in Provider::all() {
            // maki honours a model but not an effort (see ModelSpec::validate).
            let effort = match provider {
                Provider::Maki => "",
                Provider::Claude | Provider::Codex | Provider::Grok => "\neffort='high'",
            };
            parse_file(
                &format!("[{provider}.test]\nmodel='m'{effort}\nconfig=[]\nwritable_roots=[]"),
                "source",
                Layer::Local,
            )
            .expect("every provider applies its supported selectors");
        }
        for (provider, level) in [
            ("codex", "workspace-write"),
            ("grok", "workspace-write"),
            ("grok", "workspace"),
        ] {
            parse_file(
                &format!("[{provider}.test]\nsandbox='{level}'\nwritable_roots=['/srv/cache']"),
                "source",
                Layer::Local,
            )
            .expect("honoured roots remain supported");
        }
    }

    #[test]
    fn invalid_selector_types_fail_with_repair_instructions() {
        let error = parse_file(
            "[codex.test]\nconfig=['model_reasoning_effort=123']",
            "source",
            Layer::Local,
        )
        .expect_err("invalid scalar")
        .to_string();
        assert!(error.contains("model_reasoning_effort"), "{error}");
        assert!(error.contains("string"), "{error}");
    }
}
