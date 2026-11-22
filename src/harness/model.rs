//! A model selection is a launch request, never evidence of the served model.
//! Keep rendering and restorable record projections together. New adapters must
//! decide how their own model/effort syntax composes; no fictional variants.
use super::Harness;
use anyhow::{Result, bail};

#[derive(Debug, Clone)]
pub struct ModelSpec {
    harness: Harness,
    args: Vec<String>,
    config_args: Vec<String>,
    model: Option<String>,
    effort: Option<String>,
    invalid: Option<&'static str>,
}

impl ModelSpec {
    pub fn new(
        harness: Harness,
        model: Option<&str>,
        effort: Option<&str>,
        config: &[String],
    ) -> Self {
        let mut spec = Self {
            harness,
            args: Vec::new(),
            config_args: config
                .iter()
                .flat_map(|entry| ["-c".to_owned(), entry.clone()])
                .collect(),
            model: model.map(str::to_owned),
            effort: effort.map(str::to_owned),
            invalid: None,
        };
        match harness {
            Harness::Claude => {
                spec.flag("--model", model);
                spec.flag("--effort", effort);
            }
            Harness::Grok => {
                spec.flag("-m", model);
                spec.flag("--reasoning-effort", effort);
            }
            // `harness/model-id`. maki accepts `--effort` and ignores it, and
            // a resumed headless session takes its thinking level from config
            // or the last one used anywhere, not from the session, so there is
            // no flag that would honour an effort: `validate` refuses one.
            Harness::Maki => spec.flag("--model", model),
            Harness::Codex => {
                spec.flag("-m", model);
                if let Some(effort) = effort {
                    spec.flag(
                        "-c",
                        Some(&format!(
                            "model_reasoning_effort={}",
                            toml::Value::String(effort.to_owned())
                        )),
                    );
                }
                // Codex deserializes raw config before applying ConfigOverrides:
                // explicit -m wins over cfg.model; effort has no such CLI field,
                // so its final -c assignment wins. Preserve the original argv.
                for (key, target) in [
                    ("model", &mut spec.model),
                    ("model_reasoning_effort", &mut spec.effort),
                ] {
                    if let Some(value) = override_value(config, key) {
                        match value {
                            toml::Value::String(value) => {
                                if key != "model" || model.is_none() {
                                    *target = Some(value);
                                }
                            }
                            _ => {
                                spec.invalid = Some(key);
                                *target = None;
                            }
                        }
                    }
                }
            }
        }
        spec
    }

    fn flag(&mut self, flag: &str, value: Option<&str>) {
        if let Some(value) = value {
            self.args.extend([flag.to_owned(), value.to_owned()]);
        }
    }

    pub fn validate(&self, harness: Harness) -> Result<()> {
        if self.harness != harness {
            bail!("model selection belongs to {}, not {harness}", self.harness);
        }
        match self.harness {
            Harness::Claude | Harness::Grok | Harness::Maki => {
                if !self.config_args.is_empty() {
                    bail!(
                        "{} does not honour 'config'; remove config from this profile (config is codex-only)",
                        self.harness
                    );
                }
            }
            Harness::Codex => {}
        }
        match self.harness {
            Harness::Maki => {
                if self.effort.is_some() {
                    bail!(
                        "maki does not honour 'effort'; remove effort from this profile and set the thinking level in maki's own config (always_thinking) instead"
                    );
                }
            }
            Harness::Claude | Harness::Codex | Harness::Grok => {}
        }
        if let Some(key) = self.invalid {
            bail!(
                "codex config override '{key}' must be a string; change its final config entry to {key}=\"value\" or remove that entry"
            );
        }
        Ok(())
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }
    pub fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }
    pub(super) fn args(&self) -> &[String] {
        &self.args
    }
    pub(super) fn config_args(&self) -> &[String] {
        &self.config_args
    }
}

// Match Codex's CLI value parser: only a TOML parse failure falls back to
// literal text, with its exact quote trimming. A valid non-string stays typed
// and is refused before launch, rather than becoming an absent selector.
fn override_value(config: &[String], key: &str) -> Option<toml::Value> {
    let raw = config.iter().rev().find_map(|entry| {
        let (candidate, value) = entry.split_once('=')?;
        (candidate.trim() == key).then_some(value.trim())
    })?;
    match format!("_x_ = {raw}").parse::<toml::Table>() {
        Ok(mut table) => table.remove("_x_"),
        Err(_) => Some(toml::Value::String(
            raw.trim_matches(|c| c == '"' || c == '\'').to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_config_is_rejected_for_direct_callers_too() {
        for harness in [Harness::Claude, Harness::Grok, Harness::Maki] {
            let spec = ModelSpec::new(harness, None, None, &["a=b".into()]);
            let error = spec
                .validate(harness)
                .expect_err("config is codex-only")
                .to_string();
            assert!(error.contains("remove config"), "{error}");
        }
    }

    #[test]
    fn codex_record_follows_selection_precedence() {
        let config = vec![
            "model='raw'".into(),
            "model_reasoning_effort='low'".into(),
            "model_reasoning_effort=high".into(),
        ];
        let spec = ModelSpec::new(Harness::Codex, Some("explicit"), Some("medium"), &config);
        spec.validate(Harness::Codex).expect("valid selectors");
        assert_eq!(spec.model(), Some("explicit"));
        assert_eq!(spec.effort(), Some("high"));
        assert_eq!(
            ModelSpec::new(Harness::Codex, None, None, &config).model(),
            Some("raw")
        );
        assert_eq!(
            spec.config_args(),
            [
                "-c",
                "model='raw'",
                "-c",
                "model_reasoning_effort='low'",
                "-c",
                "model_reasoning_effort=high"
            ]
        );
    }

    #[test]
    fn final_invalid_override_cannot_turn_into_a_default() {
        for key in ["model", "model_reasoning_effort"] {
            let mut config = vec![format!("{key}='valid'"), format!("{key}=123")];
            let spec = ModelSpec::new(Harness::Codex, Some("explicit"), Some("low"), &config);
            assert!(spec.validate(Harness::Codex).is_err());
            config.push(format!("{key}='repaired'"));
            ModelSpec::new(Harness::Codex, None, None, &config)
                .validate(Harness::Codex)
                .expect("later string repairs invalid type");
        }
    }

    #[test]
    fn override_parser_decodes_strings_and_keeps_raw_fallback() {
        for (raw, expected) in [
            ("'a=b'", "a=b"),
            (r#""a\u0062""#, "ab"),
            ("unquoted", "unquoted"),
            ("\"'broken'\"'", "broken"),
        ] {
            assert_eq!(
                override_value(&[format!(" model = {raw} ")], "model"),
                Some(toml::Value::String(expected.into()))
            );
        }
        assert_eq!(
            override_value(&["model_extra='other'".into()], "model"),
            None
        );
        for raw in ["123", "true", "[]", "{}"] {
            assert!(
                ModelSpec::new(Harness::Codex, None, None, &[format!("model={raw}")])
                    .validate(Harness::Codex)
                    .is_err()
            );
        }
    }

    #[test]
    fn adapter_rendering_and_identity_are_explicit() {
        for (harness, flags) in [
            (Harness::Claude, ["--model", "m", "--effort", "high"]),
            (Harness::Grok, ["-m", "m", "--reasoning-effort", "high"]),
        ] {
            let spec = ModelSpec::new(harness, Some("m"), Some("high"), &[]);
            assert_eq!(spec.args(), flags);
            assert!(spec.validate(Harness::Codex).is_err());
        }
        let maki = ModelSpec::new(Harness::Maki, Some("openrouter/qwen"), None, &[]);
        assert_eq!(maki.args(), ["--model", "openrouter/qwen"]);
        maki.validate(Harness::Maki)
            .expect("model alone is honoured");
        let error = ModelSpec::new(Harness::Maki, None, Some("high"), &[])
            .validate(Harness::Maki)
            .expect_err("maki ignores effort")
            .to_string();
        assert!(error.contains("remove effort"), "{error}");
        let effort = "a\"b\nc";
        let spec = ModelSpec::new(Harness::Codex, None, Some(effort), &[]);
        assert_eq!(
            override_value(&[spec.args()[1].clone()], "model_reasoning_effort"),
            Some(toml::Value::String(effort.into()))
        );
    }
}
