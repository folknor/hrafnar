//! Unit tests of codex harness behavior.

/// Every level but `read-only` selects the sandbox the plain way.
#[test]
fn a_writable_level_is_passed_as_the_sandbox_flag() {
    assert_eq!(
        super::codex_sandbox_args("workspace-write"),
        vec!["--sandbox".to_string(), "workspace-write".to_string()]
    );
    assert_eq!(
        super::codex_sandbox_args("danger-full-access"),
        vec!["--sandbox".to_string(), "danger-full-access".to_string()]
    );
}

/// An unrecognised level is passed through verbatim rather than rejected -
/// grok resolves custom profile names, and the harness validates its own
/// vocabulary with a better error than we could write.
#[test]
fn an_unknown_level_is_passed_through_as_a_sandbox_flag() {
    assert_eq!(
        super::codex_sandbox_args("custom-profile"),
        vec!["--sandbox".to_string(), "custom-profile".to_string()]
    );
}

/// `read-only` must NOT pass `--sandbox`: a `sandbox_mode` override makes
/// `resolve_permission_config_syntax` return `Legacy`, which renders the
/// `[permissions]` profile inert and silently drops the network grant.
#[test]
fn read_only_selects_a_permissions_profile_instead_of_the_sandbox_flag() {
    let args = super::codex_sandbox_args("read-only");
    assert!(
        !args.iter().any(|a| a == "--sandbox"),
        "--sandbox would force the legacy pipeline and void the profile: {args:?}"
    );
    assert!(
        args.iter()
            .any(|a| a == r#"permissions.review-read-only.extends=":read-only""#)
    );
    assert!(
        args.iter()
            .any(|a| a == "permissions.review-read-only.network.enabled=true")
    );
    assert!(
        args.iter()
            .any(|a| a == r#"default_permissions="review-read-only""#)
    );
}

/// The profile must extend the built-in `:read-only`, since that is what
/// carries the filesystem restriction the level advertises. Defining the
/// profile without a parent would grant network over an unrestricted
/// filesystem.
#[test]
fn the_read_only_profile_extends_the_builtin_read_only_filesystem() {
    let args = super::codex_sandbox_args("read-only");
    let extends = args
        .iter()
        .find(|a| a.contains(".extends="))
        .expect("profile must declare a parent");
    assert!(
        extends.ends_with(r#"=":read-only""#),
        "expected the built-in read-only parent, got {extends}"
    );
}
