//! Resolution tests across the local and global config layers.
//!
//! `config::parse`'s own `mod tests` covers parsing a single file; everything here
//! is about how two files combine: which one wins, whether a winner replaces or
//! merges, and whether the provenance (`layer`, `table`) that `review list --all`
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

    fn layered(local_raw: &str, global_raw: &str) -> Result<ReviewConfig> {
        Ok(resolve(
            local(local_raw),
            Some(global(global_raw)),
            "h",
            ConfigFiles::default(),
        )?)
    }

    fn entry<'a>(cfg: &'a ReviewConfig, profile: &str) -> &'a ProfileDef {
        &cfg.profiles[profile]
    }

    // ---- profiles -------------------------------------------------------

    #[test]
    fn a_global_profile_applies_when_the_project_defines_none() {
        // The whole point of the global layer: which model serves a tier is
        // stated once per operator, not restated in every project.
        let cfg = layered("", "[profiles.deep.codex]\nmodel = \"g\"\n").unwrap();
        let e = entry(&cfg, "deep");
        assert_eq!(e.entries[&Harness::Codex].model.as_deref(), Some("g"));
        assert_eq!(e.layer, Layer::Global);
        assert_eq!(e.table, "[profiles.deep]");
        let codex = e.entry(Harness::Codex).unwrap();
        assert_eq!(codex.layer, Layer::Global);
        assert_eq!(codex.table, "[profiles.deep.codex]");
    }

    #[test]
    fn a_project_may_add_profiles_beside_the_global_ones() {
        let cfg = layered(
            "[profiles.local-only.claude]\nmodel = \"l\"\n",
            "[profiles.deep.codex]\nmodel = \"g\"\n",
        )
        .unwrap();
        assert_eq!(entry(&cfg, "local-only").layer, Layer::Local);
        assert_eq!(entry(&cfg, "deep").layer, Layer::Global);
    }

    #[test]
    fn a_project_may_not_redefine_a_global_profile() {
        // A checked-out repo must not change what `-p deep` means - not its
        // model, and above all not its sandbox - whatever the harnesses.
        for local_raw in [
            "[profiles.deep.codex]\nsandbox = \"danger-full-access\"\n",
            "[profiles.deep.claude]\n",
        ] {
            let err = resolve(
                local(local_raw),
                Some(global("[profiles.deep.codex]\nmodel = \"g\"\n")),
                "h",
                ConfigFiles {
                    local: Some("/p/.review.toml".into()),
                    global: Some("/home/u/.config/review/config.toml".into()),
                    global_loaded: true,
                },
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("profile 'deep' is defined in both"), "{err}");
            assert!(err.contains("/p/.review.toml"), "{err}");
            assert!(err.contains("/home/u/.config/review/config.toml"), "{err}");
            assert!(err.contains("pick another name"), "{err}");
        }
        // Without known paths the layers are still named.
        let err = layered("[profiles.deep.codex]\n", "[profiles.deep.codex]\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("the local config"), "{err}");
        assert!(err.contains("the global config"), "{err}");
    }

    // ---- archetypes -----------------------------------------------------

    #[test]
    fn archetypes_union_across_layers_and_local_wins_per_name() {
        let cfg = layered(
            "[archetypes]\nbugs = \"local bugs\"\nperf = \"local perf\"\n",
            "[archetypes]\nbugs = \"global bugs\"\nsecurity = \"global security\"\n",
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
    fn a_local_default_profile_replaces_the_global_one() {
        let cfg = layered(
            "[_defaults]\nprofile = \"fast\"\n",
            "[_defaults]\nprofile = \"deep\"\n",
        )
        .unwrap();
        assert_eq!(cfg.default_profile(), Some("fast"));
        let p = cfg.profile.as_ref().unwrap();
        assert_eq!(p.layer, Layer::Local);
        assert_eq!(p.table, "[_defaults]");
    }

    #[test]
    fn defaults_fall_through_key_by_key_not_table_by_table() {
        // A local `[_defaults]` that sets only stall_timeout_secs must not
        // blank the global default profile, and vice versa.
        let cfg = layered(
            "[_defaults]\nstall_timeout_secs = 0\n",
            "[_defaults]\nprofile = \"deep\"\nstall_timeout_secs = 900\n",
        )
        .unwrap();
        assert_eq!(cfg.default_profile(), Some("deep"));
        assert_eq!(cfg.profile.as_ref().unwrap().layer, Layer::Global);
        // 0 means "disabled", which is a setting, not an absence.
        assert_eq!(cfg.stall_timeout(), Some(crate::config::StallTimeout::Off));
        assert_eq!(cfg.stall_timeout_secs.as_ref().unwrap().layer, Layer::Local);

        let cfg = layered(
            "[_defaults]\nprofile = \"fast\"\n",
            "[_defaults]\nstall_timeout_secs = 600\n",
        )
        .unwrap();
        assert_eq!(cfg.default_profile(), Some("fast"));
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
        // a value `review list --all` would have to attribute to no file.
        let cfg = layered("[_defaults]\n", "[_defaults]\n").unwrap();
        assert!(cfg.profile.is_none());
        assert!(cfg.stall_timeout_secs.is_none());
        assert!(cfg.prefer.is_none());
        assert!(cfg.prefer().is_empty());
    }

    #[test]
    fn prefer_comes_whole_from_the_first_layer_that_sets_it() {
        // Whole, not merged: a project that prefers claude alone must not
        // inherit the global list's tail.
        let cfg = layered(
            "[_defaults]\nprefer = [\"claude\"]\n",
            "[_defaults]\nprefer = [\"codex\", \"grok\"]\nprofile = \"deep\"\n",
        )
        .unwrap();
        assert_eq!(cfg.prefer(), [Harness::Claude]);
        let p = cfg.prefer.as_ref().unwrap();
        assert_eq!(p.layer, Layer::Local);
        assert_eq!(p.table, "[_defaults]");
        assert_eq!(cfg.default_profile(), Some("deep"));

        let cfg = layered("", "[_defaults]\nprefer = [\"codex\", \"grok\"]\n").unwrap();
        assert_eq!(cfg.prefer(), [Harness::Codex, Harness::Grok]);
        assert_eq!(cfg.prefer.as_ref().unwrap().layer, Layer::Global);

        // An empty list is a setting: it overrides the global one.
        let cfg = layered(
            "[_defaults]\nprefer = []\n",
            "[_defaults]\nprefer = [\"codex\"]\n",
        )
        .unwrap();
        assert!(cfg.prefer().is_empty());
        assert_eq!(cfg.prefer.as_ref().unwrap().layer, Layer::Local);
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
        )
        .unwrap();
        assert_eq!(cfg.projects.len(), 1);
        assert_eq!(cfg.projects["bz"], "/b");
    }

    #[test]
    fn another_hosts_registry_clash_does_not_break_this_host() {
        let raw = "[_projects.other]\nFoo = \"/a\"\nfoo = \"/b\"\n\"./x\" = \"/x\"\n";
        assert!(layered("", raw).is_ok());
        let err = resolve(
            local(""),
            Some(global(raw)),
            "other",
            ConfigFiles::default(),
        )
        .unwrap_err()
        .to_string();
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
        )
        .unwrap();
        assert_eq!(cfg.audit.id.as_deref(), Some("a1b2"));
        assert!(cfg.audit.private);
    }

    // ---- layer plumbing -------------------------------------------------

    #[test]
    fn an_absent_global_resolves_like_an_empty_one() {
        let raw = "\
[archetypes]
bugs = \"b\"
[_defaults]
profile = \"deep\"
[profiles.deep.codex]
model = \"m\"
";
        let without = resolve(local(raw), None, "h", ConfigFiles::default()).unwrap();
        let with_empty =
            resolve(local(raw), Some(global("")), "h", ConfigFiles::default()).unwrap();
        for cfg in [&without, &with_empty] {
            assert_eq!(cfg.archetypes["bugs"].layer, Layer::Local);
            assert_eq!(cfg.profile.as_ref().unwrap().layer, Layer::Local);
            assert_eq!(entry(cfg, "deep").layer, Layer::Local);
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
        let cfg = resolve(local(""), None, "box.lan", ConfigFiles::default()).unwrap();
        assert_eq!(cfg.hostname, "box.lan");
    }
}
