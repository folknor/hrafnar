//! Resolution tests across the local and global config layers.
//!
//! `config::parse`'s own `mod tests` covers parsing a single file; everything here
//! is about how two files combine: which one wins, whether a winner replaces or
//! merges, and whether the provenance (`layer`, `table`) that `review config`
//! reports is the provenance of the value actually used.

#[allow(clippy::unwrap_used)]
mod layering {
    use crate::config::*;
    use anyhow::Result;

    fn local(raw: &str) -> ConfigFile {
        parse_file(raw, "local.toml", Layer::Local).unwrap()
    }

    fn global(raw: &str) -> ConfigFile {
        parse_file(raw, "global.toml", Layer::Global).unwrap()
    }

    fn layered(local_raw: &str, global_raw: &str, host: &str) -> Result<ReviewConfig> {
        Ok(resolve(
            local(local_raw),
            Some(global(global_raw)),
            host,
            ConfigFiles::default(),
        )?)
    }

    fn entry<'a>(cfg: &'a ReviewConfig, provider: &str, profile: &str) -> &'a ProfileEntry {
        &cfg.profiles[&provider.parse::<Provider>().unwrap()][profile]
    }

    // ---- profiles -------------------------------------------------------

    #[test]
    fn a_global_profile_applies_when_the_project_defines_none() {
        // The whole point of the global layer: which model serves a tier is
        // stated once per operator, not restated in every project.
        let cfg = layered("", "[codex.deep]\nmodel = \"g\"\n", "h").unwrap();
        let e = entry(&cfg, "codex", "deep");
        assert_eq!(e.effective.profile.model.as_deref(), Some("g"));
        assert_eq!(e.effective.layer, Layer::Global);
        assert_eq!(e.effective.table, "[codex.deep]");
        assert_eq!(e.effective.host, None);
        assert!(e.shadowed.is_empty());
    }

    #[test]
    fn a_local_profile_replaces_the_global_one_whole() {
        // No field-level merge: the global profile opts up to workspace-write,
        // the local one only names a model. If fields merged, the project's
        // profile would silently inherit write access it never asked for.
        let cfg = layered(
            "[codex.deep]\nmodel = \"local-model\"\n",
            "[codex.deep]\nmodel = \"global-model\"\neffort = \"high\"\n\
             sandbox = \"workspace-write\"\nwritable_roots = [\"/srv\"]\n\
             config = ['a=\"b\"']\nenv = { K = \"v\" }\n",
            "h",
        )
        .unwrap();
        let p = cfg.resolve_profile(Provider::Codex, "deep").unwrap();
        assert_eq!(p.model.as_deref(), Some("local-model"));
        assert_eq!(p.effort, None);
        assert_eq!(p.sandbox, None);
        assert!(p.writable_roots.is_empty());
        assert!(p.config.is_empty());
        assert!(p.env.is_none());
    }

    #[test]
    fn the_shadowed_global_profile_is_retained_for_display() {
        // `review config` shows what a stale local override is hiding; that
        // only works if the loser survives resolution intact.
        let cfg = layered(
            "[codex.deep]\nmodel = \"l\"\n",
            "[codex.deep]\nmodel = \"g\"\n",
            "h",
        )
        .unwrap();
        let e = entry(&cfg, "codex", "deep");
        assert_eq!(e.effective.layer, Layer::Local);
        assert_eq!(e.shadowed.len(), 1);
        assert_eq!(e.shadowed[0].layer, Layer::Global);
        assert_eq!(e.shadowed[0].table, "[codex.deep]");
        assert_eq!(e.shadowed[0].profile.model.as_deref(), Some("g"));
    }

    #[test]
    fn profile_precedence_is_local_host_local_hostless_global_host_global_hostless() {
        let local_raw = "\
[codex.deep]
model = \"local-hostless\"

[h.codex.deep]
model = \"local-host\"
";
        let global_raw = "\
[codex.deep]
model = \"global-hostless\"

[h.codex.deep]
model = \"global-host\"
";
        let cfg = layered(local_raw, global_raw, "h").unwrap();
        let e = entry(&cfg, "codex", "deep");

        let order: Vec<(&str, Layer, &str, Option<&str>)> = std::iter::once(&e.effective)
            .chain(&e.shadowed)
            .map(|d| {
                (
                    d.profile.model.as_deref().unwrap(),
                    d.layer,
                    d.table.as_str(),
                    d.host.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            order,
            vec![
                ("local-host", Layer::Local, "[h.codex.deep]", Some("h")),
                ("local-hostless", Layer::Local, "[codex.deep]", None),
                ("global-host", Layer::Global, "[h.codex.deep]", Some("h")),
                ("global-hostless", Layer::Global, "[codex.deep]", None),
            ]
        );
    }

    #[test]
    fn a_global_host_table_does_not_beat_a_local_hostless_one() {
        // Host-specificity only ranks definitions *within* a file. Across
        // files the layer decides, or a project could never override a host
        // table the operator wrote globally.
        let cfg = layered(
            "[codex.deep]\nmodel = \"local\"\n",
            "[h.codex.deep]\nmodel = \"global-host\"\n",
            "h",
        )
        .unwrap();
        let e = entry(&cfg, "codex", "deep");
        assert_eq!(e.effective.profile.model.as_deref(), Some("local"));
        assert_eq!(e.shadowed[0].table, "[h.codex.deep]");
        assert_eq!(e.shadowed[0].layer, Layer::Global);
    }

    #[test]
    fn host_tables_for_other_hosts_are_invisible_in_both_layers() {
        // Neither effective nor shadowed: a profile written for another
        // machine is not a candidate here at all.
        let cfg = layered(
            "[other.codex.deep]\nmodel = \"l-other\"\n",
            "[other.codex.deep]\nmodel = \"g-other\"\n[codex.deep]\nmodel = \"g\"\n",
            "h",
        )
        .unwrap();
        let e = entry(&cfg, "codex", "deep");
        assert_eq!(e.effective.profile.model.as_deref(), Some("g"));
        assert!(e.shadowed.is_empty());

        let cfg = layered(
            "[other.codex.deep]\nmodel = \"l-other\"\n",
            "[other.codex.deep]\nmodel = \"g-other\"\n",
            "h",
        )
        .unwrap();
        assert!(cfg.resolve_profile(Provider::Codex, "deep").is_none());
    }

    #[test]
    fn profiles_resolve_independently_per_provider_and_name() {
        // A local override of one profile must not hide the global's other
        // profiles, nor the same name under a different provider.
        let cfg = layered(
            "[codex.deep]\nmodel = \"l\"\n",
            "[codex.deep]\nmodel = \"g\"\n[codex.fast]\nmodel = \"gf\"\n\
             [claude.deep]\nmodel = \"gc\"\n",
            "h",
        )
        .unwrap();
        assert_eq!(
            cfg.resolve_profile(Provider::Codex, "deep")
                .unwrap()
                .model
                .as_deref(),
            Some("l")
        );
        assert_eq!(
            cfg.resolve_profile(Provider::Codex, "fast")
                .unwrap()
                .model
                .as_deref(),
            Some("gf")
        );
        let claude = entry(&cfg, "claude", "deep");
        assert_eq!(claude.effective.profile.model.as_deref(), Some("gc"));
        assert!(claude.shadowed.is_empty());
    }

    #[test]
    fn a_dotted_hostname_is_quoted_in_the_recorded_table() {
        // The table string is shown to the operator as where to go and edit;
        // unquoted, `[a.b.codex.deep]` names a different (nonexistent) table.
        let raw = "[\"box.lan\".codex.deep]\nmodel = \"m\"\n";
        let cfg = layered(raw, raw, "box.lan").unwrap();
        let e = entry(&cfg, "codex", "deep");
        assert_eq!(e.effective.table, "[\"box.lan\".codex.deep]");
        assert_eq!(e.effective.host.as_deref(), Some("box.lan"));
        assert_eq!(e.effective.layer, Layer::Local);
        assert_eq!(e.shadowed[0].table, "[\"box.lan\".codex.deep]");
        assert_eq!(e.shadowed[0].layer, Layer::Global);
    }

    #[test]
    fn a_hostless_profile_resolves_the_same_on_every_host() {
        let g = "[grok.deep]\nmodel = \"m\"\n";
        for host in ["a", "b.c", "plantasjen"] {
            let cfg = layered("", g, host).unwrap();
            assert_eq!(
                cfg.resolve_profile(Provider::Grok, "deep")
                    .unwrap()
                    .model
                    .as_deref(),
                Some("m"),
                "{host}"
            );
        }
    }

    // ---- archetypes -----------------------------------------------------

    #[test]
    fn archetypes_union_across_layers_and_local_wins_per_name() {
        let cfg = layered(
            "[archetypes]\nbugs = \"local bugs\"\nperf = \"local perf\"\n",
            "[archetypes]\nbugs = \"global bugs\"\nsecurity = \"global security\"\n",
            "h",
        )
        .unwrap();
        assert_eq!(cfg.archetypes.len(), 3);
        assert_eq!(cfg.archetype("bugs"), Some("local bugs"));
        assert_eq!(cfg.archetypes["bugs"].layer, Layer::Local);
        assert_eq!(cfg.archetypes["perf"].layer, Layer::Local);
        assert_eq!(cfg.archetype("security"), Some("global security"));
        assert_eq!(cfg.archetypes["security"].layer, Layer::Global);
        assert_eq!(cfg.archetypes["security"].table, "[archetypes]");
    }

    #[test]
    fn a_misspelled_profile_key_is_an_error() {
        // A profile wins whole, so a typo that parsed as an empty profile would
        // silently discard the global definition it shadows.
        let err = parse_file("[codex.deep]\nmodle = \"x\"\n", "local.toml", Layer::Local)
            .unwrap_err()
            .to_string();
        assert!(err.contains("modle"), "{err}");
    }

    #[test]
    fn reserved_names_error_as_archetypes_in_the_global_layer_too() {
        for name in reserved_names() {
            let raw = format!("[archetypes]\n{name} = \"x\"\n");
            let err = parse_file(&raw, "global.toml", Layer::Global)
                .unwrap_err()
                .to_string();
            assert!(err.contains("reserved"), "{name}: {err}");
            assert!(err.contains("global.toml"), "{name}: {err}");
        }
    }

    // ---- defaults -------------------------------------------------------

    #[test]
    fn a_local_provider_replaces_the_global_one() {
        let cfg = layered(
            "[_defaults]\nprovider = \"grok\"\n",
            "[_defaults]\nprovider = \"claude\"\n",
            "h",
        )
        .unwrap();
        assert_eq!(cfg.default_provider(), Some(Provider::Grok));
        let p = cfg.provider.as_ref().unwrap();
        assert_eq!(p.layer, Layer::Local);
        assert_eq!(p.table, "[_defaults]");
    }

    #[test]
    fn defaults_fall_through_key_by_key_not_table_by_table() {
        // A local `[_defaults]` that sets only stall_timeout_secs must not
        // blank the global provider, and vice versa.
        let cfg = layered(
            "[_defaults]\nstall_timeout_secs = 0\n",
            "[_defaults]\nprovider = \"codex\"\nstall_timeout_secs = 900\n",
            "h",
        )
        .unwrap();
        assert_eq!(cfg.default_provider(), Some(Provider::Codex));
        assert_eq!(cfg.provider.as_ref().unwrap().layer, Layer::Global);
        // 0 means "disabled", which is a setting, not an absence.
        assert_eq!(cfg.stall_timeout(), Some(crate::config::StallTimeout::Off));
        assert_eq!(cfg.stall_timeout_secs.as_ref().unwrap().layer, Layer::Local);

        let cfg = layered(
            "[_defaults]\nprovider = \"claude\"\n",
            "[_defaults]\nstall_timeout_secs = 600\n",
            "h",
        )
        .unwrap();
        assert_eq!(cfg.default_provider(), Some(Provider::Claude));
        assert_eq!(
            cfg.stall_timeout(),
            Some(crate::config::StallTimeout::from(600))
        );
        let s = cfg.stall_timeout_secs.as_ref().unwrap();
        assert_eq!(s.layer, Layer::Global);
        assert_eq!(s.table, "[_defaults]");
    }

    #[test]
    fn defaults_are_none_when_no_layer_sets_them() {
        // Nothing is built in: an unset key stays unset rather than acquiring
        // a value `review config` would have to attribute to no file.
        let cfg = layered("[_defaults]\n", "[_defaults]\n", "h").unwrap();
        assert!(cfg.provider.is_none());
        assert!(cfg.stall_timeout_secs.is_none());
    }

    #[test]
    fn an_unknown_provider_in_global_defaults_errors() {
        let err = parse_file(
            "[_defaults]\nprovider = \"kilo\"\n",
            "global.toml",
            Layer::Global,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown provider 'kilo'"), "{err}");
        assert!(err.contains("global.toml"), "{err}");
    }

    // ---- audit ----------------------------------------------------------

    #[test]
    fn an_audit_block_in_the_global_config_errors() {
        // An audit id identifies one project; a global one would stamp every
        // project with the same id.
        let err = parse_file("[_audit]\nid = \"abcd\"\n", "global.toml", Layer::Global)
            .unwrap_err()
            .to_string();
        assert!(err.contains("[_audit]"), "{err}");
        assert!(err.contains("global.toml"), "{err}");
    }

    // ---- project registry ----------------------------------------------

    #[test]
    fn a_project_registry_in_a_project_file_errors() {
        let err = parse_file("[_projects.h]\nbz = \"/b\"\n", "local.toml", Layer::Local)
            .unwrap_err()
            .to_string();
        assert!(err.contains("[_projects]"), "{err}");
    }

    #[test]
    fn only_this_hosts_registry_is_kept() {
        let cfg = layered(
            "",
            "[_projects.h]\nbz = \"/b\"\n[_projects.other]\nx = \"/x\"\n",
            "h",
        )
        .unwrap();
        assert_eq!(cfg.projects.len(), 1);
        assert_eq!(cfg.projects["bz"], "/b");
    }

    #[test]
    fn another_hosts_registry_clash_does_not_break_this_host() {
        let raw = "[_projects.other]\nFoo = \"/a\"\nfoo = \"/b\"\n\"./x\" = \"/x\"\n";
        assert!(layered("", raw, "h").is_ok());
        let err = layered("", raw, "other").unwrap_err().to_string();
        assert!(
            err.contains("differ only in case") || err.contains("path"),
            "{err}"
        );
    }

    #[test]
    fn the_resolved_audit_is_the_projects() {
        let cfg = layered(
            "[_audit]\nid = \"a1b2\"\nprivate = true\n",
            "[archetypes]\nbugs = \"b\"\n",
            "h",
        )
        .unwrap();
        assert_eq!(cfg.audit.id.as_deref(), Some("a1b2"));
        assert!(cfg.audit.private);
    }

    // ---- structural errors ----------------------------------------------

    #[test]
    fn an_unknown_provider_under_a_host_table_errors_in_either_layer() {
        for layer in [Layer::Local, Layer::Global] {
            let err = parse_file("[h.gpt.fast]\nmodel = \"m\"\n", "f.toml", layer)
                .unwrap_err()
                .to_string();
            assert!(err.contains("unknown provider 'gpt'"), "{err}");
        }
    }

    #[test]
    fn a_hostless_table_under_an_unknown_provider_errors() {
        // `[gpt.fast]` reads as host `gpt`, provider `fast`; it must still be
        // refused rather than silently ignored as another machine's profile.
        let err = parse_file("[gpt.fast]\nmodel = \"m\"\n", "f.toml", Layer::Global)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown provider"), "{err}");
    }

    #[test]
    fn a_non_table_top_level_value_errors() {
        for raw in ["stray = 1\n", "codex = \"deep\"\n", "h = [1]\n"] {
            let err = parse_file(raw, "f.toml", Layer::Global)
                .unwrap_err()
                .to_string();
            assert!(err.contains("expected a table"), "{raw}: {err}");
        }
    }

    // ---- layer plumbing -------------------------------------------------

    #[test]
    fn an_absent_global_resolves_like_an_empty_one() {
        let raw = "\
[archetypes]
bugs = \"b\"
[_defaults]
provider = \"codex\"
[codex.deep]
model = \"m\"
";
        let without = resolve(local(raw), None, "h", ConfigFiles::default()).unwrap();
        let with_empty =
            resolve(local(raw), Some(global("")), "h", ConfigFiles::default()).unwrap();
        for cfg in [&without, &with_empty] {
            assert_eq!(cfg.archetypes["bugs"].layer, Layer::Local);
            assert_eq!(cfg.provider.as_ref().unwrap().layer, Layer::Local);
            let e = entry(cfg, "codex", "deep");
            assert_eq!(e.effective.layer, Layer::Local);
            assert!(e.shadowed.is_empty());
        }
    }

    #[test]
    fn searched_lists_the_global_path_and_marks_it_when_absent() {
        // Error messages about an undefined archetype/profile quote this, so
        // the operator can see the global file was looked for but not found.
        let files = ConfigFiles {
            local: Some("/p/.review.toml".into()),
            global: Some("/home/u/.config/review/config.toml".into()),
            global_loaded: false,
        };
        let mut files = files;
        let cfg = resolve(local(""), None, "h", files.clone()).unwrap();
        assert_eq!(
            cfg.searched(),
            "/p/.review.toml, /home/u/.config/review/config.toml (absent)"
        );
        files.global_loaded = true;
        let cfg = resolve(local(""), None, "h", files.clone()).unwrap();
        assert_eq!(
            cfg.searched(),
            "/p/.review.toml, /home/u/.config/review/config.toml"
        );
        files.global = None;
        let cfg = resolve(local(""), None, "h", files.clone()).unwrap();
        assert_eq!(cfg.searched(), "/p/.review.toml");
        files.local = None;
        let cfg = resolve(local(""), None, "h", files).unwrap();
        assert_eq!(cfg.searched(), "(no .review.toml)");
    }

    #[test]
    fn the_resolved_config_records_the_hostname_it_was_resolved_for() {
        let cfg = layered("", "", "box.lan").unwrap();
        assert_eq!(cfg.hostname, "box.lan");
    }
}
