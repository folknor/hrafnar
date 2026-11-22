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

    fn layered(local_raw: &str, global_raw: &str) -> Result<ReviewConfig> {
        Ok(resolve(
            local(local_raw),
            Some(global(global_raw)),
            "h",
            ConfigFiles::default(),
        )?)
    }

    fn entry<'a>(cfg: &'a ReviewConfig, profile: &str) -> &'a ProfileEntry {
        &cfg.profiles[profile]
    }

    // ---- profiles -------------------------------------------------------

    #[test]
    fn a_global_profile_applies_when_the_project_defines_none() {
        // The whole point of the global layer: which model serves a tier is
        // stated once per operator, not restated in every project.
        let cfg = layered("", "[profiles.deep]\nprovider = \"codex\"\nmodel = \"g\"\n").unwrap();
        let e = entry(&cfg, "deep");
        assert_eq!(e.effective.provider, Provider::Codex);
        assert_eq!(e.effective.profile.model.as_deref(), Some("g"));
        assert_eq!(e.effective.layer, Layer::Global);
        assert_eq!(e.effective.table, "[profiles.deep]");
        assert!(e.shadowed.is_empty());
    }

    #[test]
    fn a_local_profile_replaces_the_global_one_whole() {
        // No field-level merge: the global profile opts up to workspace-write,
        // the local one only names a model. If fields merged, the project's
        // profile would silently inherit write access it never asked for.
        let cfg = layered(
            "[profiles.deep]\nprovider = \"codex\"\nmodel = \"local-model\"\n",
            "[profiles.deep]\nprovider = \"codex\"\nmodel = \"global-model\"\n\
             effort = \"high\"\nsandbox = \"workspace-write\"\n\
             writable_roots = [\"/srv\"]\nconfig = ['a=\"b\"']\nenv = { K = \"v\" }\n",
        )
        .unwrap();
        let p = &cfg.resolve_profile("deep").unwrap().profile;
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
            "[profiles.deep]\nprovider = \"codex\"\nmodel = \"l\"\n",
            "[profiles.deep]\nprovider = \"codex\"\nmodel = \"g\"\n",
        )
        .unwrap();
        let e = entry(&cfg, "deep");
        assert_eq!(e.effective.layer, Layer::Local);
        assert_eq!(e.shadowed.len(), 1);
        assert_eq!(e.shadowed[0].layer, Layer::Global);
        assert_eq!(e.shadowed[0].table, "[profiles.deep]");
        assert_eq!(e.shadowed[0].profile.model.as_deref(), Some("g"));
    }

    #[test]
    fn a_project_profile_may_not_switch_the_provider() {
        // `-p deep` must launch the same provider in every project; a project
        // that redefined it on another provider would change that silently.
        let err = layered(
            "[profiles.deep]\nprovider = \"claude\"\n",
            "[profiles.deep]\nprovider = \"codex\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("runs on claude in the local config"), "{err}");
        assert!(err.contains("codex in the global config"), "{err}");
    }

    #[test]
    fn profiles_resolve_independently_per_name() {
        // A local override of one profile must not hide the global's others.
        let cfg = layered(
            "[profiles.deep]\nprovider = \"codex\"\nmodel = \"l\"\n",
            "[profiles.deep]\nprovider = \"codex\"\nmodel = \"g\"\n\
             [profiles.fast]\nprovider = \"claude\"\nmodel = \"gf\"\n",
        )
        .unwrap();
        assert_eq!(
            cfg.resolve_profile("deep")
                .unwrap()
                .profile
                .model
                .as_deref(),
            Some("l")
        );
        let fast = entry(&cfg, "fast");
        assert_eq!(fast.effective.provider, Provider::Claude);
        assert_eq!(fast.effective.profile.model.as_deref(), Some("gf"));
        assert!(fast.shadowed.is_empty());
    }

    #[test]
    fn a_dotted_profile_name_is_quoted_in_the_recorded_table() {
        // The table string is shown to the operator as where to go and edit;
        // unquoted, `[profiles.a.b]` names a different table.
        let raw = "[profiles.\"a.b\"]\nprovider = \"grok\"\n";
        let cfg = layered(raw, "").unwrap();
        assert_eq!(entry(&cfg, "a.b").effective.table, "[profiles.\"a.b\"]");
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
        // a value `review config` would have to attribute to no file.
        let cfg = layered("[_defaults]\n", "[_defaults]\n").unwrap();
        assert!(cfg.profile.is_none());
        assert!(cfg.stall_timeout_secs.is_none());
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
[profiles.deep]
provider = \"codex\"
model = \"m\"
";
        let without = resolve(local(raw), None, "h", ConfigFiles::default()).unwrap();
        let with_empty =
            resolve(local(raw), Some(global("")), "h", ConfigFiles::default()).unwrap();
        for cfg in [&without, &with_empty] {
            assert_eq!(cfg.archetypes["bugs"].layer, Layer::Local);
            assert_eq!(cfg.profile.as_ref().unwrap().layer, Layer::Local);
            let e = entry(cfg, "deep");
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
        let cfg = resolve(local(""), None, "box.lan", ConfigFiles::default()).unwrap();
        assert_eq!(cfg.hostname, "box.lan");
    }
}
