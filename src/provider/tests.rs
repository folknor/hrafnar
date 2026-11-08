//! Unit tests of `invoke`'s pure parts: session-id selection, the writable-roots
//! override and the root announcement. Provider-specific tests live beside their
//! runners. The tests that run a real
//! child process are in
//! `tests/provider_runs.rs`, an integration test, because their keeper is the
//! built `review` binary (see `keeper`).

#[test]
fn a_resume_result_retains_the_session_id_known_to_its_caller() {
    assert_eq!(
        super::SessionStart::Resume(super::SessionId::parse("known-session").expect("test id"))
            .result_id(None),
        Some("known-session".to_string())
    );
    assert_eq!(
        super::SessionStart::Fresh {
            id: Some(super::SessionId::parse("requested-session").expect("test id"))
        }
        .result_id(Some("provider-session".to_string())),
        Some("provider-session".to_string())
    );
}

/// A profile `config` entry restating the writable-roots key wins over the
/// derived one, because codex takes the last `-c`. The sidecar has to follow
/// the run rather than the derivation, or it makes a confident false claim
/// about what a run could write.
#[test]
fn a_profile_config_override_of_writable_roots_is_what_gets_recorded() {
    let config = vec![r#"sandbox_workspace_write.writable_roots=["/secret"]"#.to_string()];
    assert_eq!(
        super::codex::config_writable_roots_override(&config),
        Some(vec!["/secret".to_string()])
    );
}

/// An explicit empty override is a real state - the profile narrowed the run
/// back to codex's defaults - and must not read as "no override".
#[test]
fn an_empty_writable_roots_override_is_recorded_as_empty_not_absent() {
    let config = vec!["sandbox_workspace_write.writable_roots=[]".to_string()];
    assert_eq!(
        super::codex::config_writable_roots_override(&config),
        Some(vec![])
    );
}

#[test]
fn unrelated_profile_config_entries_leave_the_derived_roots_recorded() {
    let config = vec![
        r#"model_provider="openai-http""#.to_string(),
        r#"sandbox_workspace_write.network_access=true"#.to_string(),
    ];
    assert_eq!(super::codex::config_writable_roots_override(&config), None);
}

#[test]
fn a_longer_config_key_does_not_hide_the_writable_roots_override() {
    let config = vec![
        r#"sandbox_workspace_write.writable_roots=["/first"]"#.to_string(),
        r#"sandbox_workspace_write.writable_roots_extra=["/ignored"]"#.to_string(),
    ];
    assert_eq!(
        super::codex::config_writable_roots_override(&config),
        Some(vec!["/first".to_string()])
    );
    assert_eq!(
        super::codex::config_writable_roots_override(&config[1..]),
        None,
        "a key that only shares the prefix is not the override"
    );
}

/// Codex takes the last `-c` for a repeated key, so the record must too.
#[test]
fn the_last_writable_roots_override_wins() {
    let config = vec![
        r#"sandbox_workspace_write.writable_roots=["/first"]"#.to_string(),
        r#"sandbox_workspace_write.writable_roots=["/second"]"#.to_string(),
    ];
    assert_eq!(
        super::codex::config_writable_roots_override(&config),
        Some(vec!["/second".to_string()])
    );
}

fn grant(path: &str, why: &'static str) -> crate::writable_roots::GrantedRoot {
    crate::writable_roots::GrantedRoot {
        path: path.to_string(),
        why,
    }
}

/// The ordinary case: no override, so every effective root is a derived one and
/// keeps the reason a build needs it.
#[test]
fn announcement_reports_derived_roots_with_their_reason() {
    let granted = vec![grant("/run/user/1000", "build lock ($XDG_RUNTIME_DIR)")];
    let lines = super::announcement_lines(
        super::Provider::Codex,
        "profile config override",
        &granted,
        &["/run/user/1000".to_string()],
    );
    assert_eq!(
        lines,
        vec!["codex: writable root /run/user/1000 (build lock ($XDG_RUNTIME_DIR))"]
    );
}

/// The defect this function exists to prevent. A profile `config` override
/// replaces the roots the run launches with, and codex takes the last `-c`, so
/// announcing the derived set would print paths the run did not receive while
/// withholding the one it did.
#[test]
fn announcement_follows_a_config_override_rather_than_the_derivation() {
    let granted = vec![grant("/run/user/1000", "build lock ($XDG_RUNTIME_DIR)")];
    let lines = super::announcement_lines(
        super::Provider::Codex,
        "profile config override",
        &granted,
        &["/secret".to_string()],
    );
    assert_eq!(
        lines,
        vec!["codex: writable root /secret (profile config override)"]
    );
    assert!(
        !lines.iter().any(|l| l.contains("/run/user/1000")),
        "a replaced derived root must not be announced as granted: {lines:?}"
    );
}

/// An override that *adds* to an empty derivation still has to be announced -
/// the old guard keyed on the derived set being non-empty and printed nothing
/// here, which is the silent-widening case the fence exists to catch.
#[test]
fn announcement_covers_roots_added_to_an_empty_derivation() {
    let lines = super::announcement_lines(
        super::Provider::Codex,
        "profile config override",
        &[],
        &["/srv/cache".to_string()],
    );
    assert_eq!(
        lines,
        vec!["codex: writable root /srv/cache (profile config override)"]
    );
}

/// A reason belongs to a path, not to a position: an override that keeps one
/// derived root and adds another must label each correctly rather than pairing
/// them off in order.
#[test]
fn announcement_matches_reasons_by_path_not_by_position() {
    let granted = vec![
        grant("/media/disk/cargo", "cargo target ($CARGO_TARGET_DIR)"),
        grant("/run/user/1000", "build lock ($XDG_RUNTIME_DIR)"),
    ];
    let effective = vec!["/run/user/1000".to_string(), "/srv/extra".to_string()];
    let lines = super::announcement_lines(
        super::Provider::Codex,
        "profile config override",
        &granted,
        &effective,
    );
    assert_eq!(
        lines,
        vec![
            "codex: writable root /run/user/1000 (build lock ($XDG_RUNTIME_DIR))",
            "codex: writable root /srv/extra (profile config override)",
        ]
    );
}

/// An explicit `[]` override widens nothing, so there is nothing to announce -
/// distinct from having derived nothing, which the separate zero-root warning
/// covers.
#[test]
fn announcement_is_empty_when_an_override_grants_nothing() {
    let granted = vec![grant("/run/user/1000", "build lock ($XDG_RUNTIME_DIR)")];
    assert!(
        super::announcement_lines(
            super::Provider::Codex,
            "profile config override",
            &granted,
            &[]
        )
        .is_empty()
    );
}

#[test]
fn persisted_sandbox_levels_normalize_known_grok_names() {
    for (native, level) in [
        ("workspace", "workspace-write"),
        ("review-ws-cbf29ce484222325", "workspace-write"),
        // Only the generated shape is ours; another prefixed name is native.
        ("review-ws-abc", "review-ws-abc"),
        ("none", "danger-full-access"),
        ("read-only", "read-only"),
        ("custom", "custom"),
    ] {
        assert_eq!(
            super::Permissions::restore(super::Provider::Grok, Some(native), None, &[])
                .recorded_level(),
            Some(level)
        );
        assert_eq!(
            super::Permissions::restore(super::Provider::Codex, Some(native), None, &[])
                .recorded_level(),
            Some(native)
        );
    }
    assert_eq!(
        super::Permissions::restore(super::Provider::Grok, None, None, &[]).recorded_level(),
        None
    );
}

#[test]
fn codex_preparation_replaces_effective_paths_without_changing_root_provenance() {
    use super::*;
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut launch = LaunchSpec {
        provider: Provider::Codex,
        start: SessionStart::Fresh { id: None },
        settings: RunSettings::from_profile(Provider::Codex, None),
        prompt: "test".into(),
        root: root.clone(),
    };
    launch.settings.permissions = Permissions::profile(
        Provider::Codex,
        Some(&crate::config::SandboxLevel::WorkspaceWrite),
    );
    for paths in [vec!["/raw-override".to_string()], Vec::new()] {
        launch.settings.config = vec![format!("sandbox_workspace_write.writable_roots={paths:?}")];
        let mut plan =
            permissions::PermissionPlan::new(launch.settings.permissions.clone(), &root, &[]);
        let derived: Vec<_> = plan
            .granted
            .iter()
            .map(|grant| grant.path.clone())
            .collect();
        codex::prepare(&launch, &mut plan);
        assert_eq!(plan.paths, paths);
        assert!(plan.derives());
        assert_eq!(
            plan.granted
                .iter()
                .map(|grant| grant.path.clone())
                .collect::<Vec<_>>(),
            derived
        );
        assert!(
            launch.settings.writable_roots.is_empty(),
            "raw effective paths never become requested roots"
        );
    }
}
