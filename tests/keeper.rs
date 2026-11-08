//! The keeper driven directly over its protocol, for what no runner test can
//! reach: `review` itself disappearing mid-run. Its control pipe then closes,
//! and the keeper must tear the run down on its own and leave its outcome in
//! the orphan log, since nobody is left to read its events.

use review::keeper::{self, Control, Event};
use std::io::{BufRead, Write};
use std::os::fd::OwnedFd;
use std::path::PathBuf;

fn scratch() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-scratch")
        .join(review::config::generate_uuid());
    std::fs::create_dir_all(&root).expect("scratch dir");
    root
}

fn launcher(script: &str, orphan_log: Option<PathBuf>) -> keeper::Launcher {
    let launch = keeper::Launch::new(
        std::ffi::OsStr::new("/bin/sh"),
        ["-c".into(), script.into()],
        &std::env::current_dir().expect("cwd"),
        keeper::Environment::Inherit(Vec::new()),
    )
    .expect("launch");
    let schedule = keeper::TeardownSchedule {
        leftover_grace: std::time::Duration::from_millis(200),
        terminate_grace: std::time::Duration::from_millis(300),
        kill_phase: std::time::Duration::from_millis(500),
        beat: std::time::Duration::from_millis(20),
        flush: std::time::Duration::from_millis(500),
    };
    keeper::Launcher::new(
        std::path::Path::new(env!("CARGO_BIN_EXE_review")),
        launch,
        schedule,
        std::time::Duration::from_secs(10),
        "a test run",
        orphan_log,
        std::process::Stdio::null(),
    )
    .expect("launcher")
}

#[test]
fn a_keeper_whose_owner_vanishes_tears_down_and_logs_the_outcome() {
    let root = scratch();
    let pidfile = root.join("detached.pid");
    let orphan_log = root.join("teardowns.jsonl");
    let keeper::Launcher {
        mut command,
        control: control_w,
        events: events_r,
        inherited,
    } = launcher(
        &format!(
            "setsid sh -c 'echo $$ > {}; exec sleep 300' & sleep 300",
            pidfile.display()
        ),
        Some(orphan_log.clone()),
    );
    let mut child = command.spawn().expect("spawn keeper");
    drop(inherited);

    let mut control = std::fs::File::from(control_w);
    control.write_all(&[Control::Go.byte()]).expect("go");
    let mut events = std::io::BufReader::new(std::fs::File::from(events_r));
    let mut line = String::new();
    events.read_line(&mut line).expect("spawned event");
    assert!(
        matches!(
            serde_json::from_str::<Event>(&line),
            Ok(Event::Spawned { .. })
        ),
        "{line}"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let (detached, detached_pidfd): (u32, Option<OwnedFd>) = loop {
        if let Some(pid) = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            let pidfd = match review::process::pidfd_open(pid) {
                Ok(pidfd) => Some(pidfd),
                Err(err) if err.raw_os_error() == Some(libc::ESRCH) => None,
                Err(err) => panic!("open detached pidfd: {err}"),
            };
            break (pid, pidfd);
        }
        assert!(std::time::Instant::now() < deadline, "no detached pid");
        std::thread::sleep(std::time::Duration::from_millis(10));
    };

    // `review` vanishes: both its pipe ends close.
    drop(control);
    drop(events);
    let status = child.wait().expect("keeper exits");
    assert!(status.success(), "{status}");
    let gone = detached_pidfd
        .as_ref()
        .is_none_or(review::process::pidfd_exited);
    if let Some(pidfd) = detached_pidfd
        .as_ref()
        .filter(|pidfd| !review::process::pidfd_exited(pidfd))
    {
        let _ = review::process::pidfd_signal(pidfd, libc::SIGKILL);
    }
    assert!(
        gone,
        "detached process {detached} must be torn down without its owner"
    );
    let log = std::fs::read_to_string(&orphan_log).expect("orphan log written");
    let entry: serde_json::Value =
        serde_json::from_str(log.lines().next().expect("a line")).expect("json");
    assert_eq!(entry["label"], "a test run");
    assert!(entry["outcome"]["teardown"].get("Clean").is_some());
    let _ = std::fs::remove_dir_all(&root);
}

/// Start a keeper for `sh -c <script>` and hand `control` to it; return once it
/// has exited.
fn keeper_given(script: &str, control: &[Control]) -> std::process::ExitStatus {
    let keeper::Launcher {
        mut command,
        control: control_w,
        events: events_r,
        inherited,
    } = launcher(script, None);
    let mut child = command.spawn().expect("spawn keeper");
    drop(inherited);
    let mut control_w = std::fs::File::from(control_w);
    if !control.is_empty() {
        let bytes: Vec<_> = control.iter().map(|control| control.byte()).collect();
        control_w.write_all(&bytes).expect("control");
    }
    drop(control_w);
    let status = child.wait().expect("keeper exits");
    drop(events_r);
    status
}

#[test]
fn a_keeper_whose_owner_is_gone_before_go_launches_nothing() {
    let root = scratch();
    let marker = root.join("ran");
    let status = keeper_given(&format!("touch {}", marker.display()), &[]);
    assert!(status.success(), "{status}");
    assert!(!marker.exists(), "nothing may launch without `go`");
    let _ = std::fs::remove_dir_all(&root);
}

/// Shutdown closes admission by terminating every registered keeper, which
/// can reach a keeper before its run sends `go`: the terminate wins.
#[test]
fn a_terminate_ahead_of_go_launches_nothing() {
    let root = scratch();
    let marker = root.join("ran");
    let status = keeper_given(
        &format!("touch {}", marker.display()),
        &[Control::Terminate, Control::Go],
    );
    assert!(status.success(), "{status}");
    assert!(!marker.exists(), "a `g` after a `t` must not launch");
    let _ = std::fs::remove_dir_all(&root);
}

fn run_launch(launch: keeper::Launch) -> (std::process::Output, Vec<Event>) {
    let keeper::Launcher {
        mut command,
        control,
        events,
        inherited,
    } = keeper::Launcher::new(
        std::path::Path::new(env!("CARGO_BIN_EXE_review")),
        launch,
        keeper::TeardownSchedule::default(),
        std::time::Duration::from_secs(5),
        "bytes test",
        None,
        std::process::Stdio::null(),
    )
    .expect("launcher");
    // Launch data, especially environment values, must never be argv.
    assert_eq!(command.get_args().count(), 2);
    assert!(
        command
            .get_args()
            .nth(1)
            .expect("fd")
            .to_string_lossy()
            .parse::<i32>()
            .is_ok()
    );
    let child = command.spawn().expect("keeper");
    drop(inherited);
    let mut control = std::fs::File::from(control);
    control.write_all(&[Control::Go.byte()]).expect("go");
    let output = child.wait_with_output().expect("wait");
    let events = std::io::BufReader::new(std::fs::File::from(events))
        .lines()
        .map(|s| serde_json::from_str(&s.expect("event line")).expect("event"))
        .collect();
    assert!(output.status.success(), "{output:?}");
    (output, events)
}

#[test]
fn launch_preserves_non_utf8_program_cwd_argv_and_environment_bytes() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::PermissionsExt;
    let root = scratch();
    let cwd = root.join(std::ffi::OsString::from_vec(b"cwd-\xff".to_vec()));
    std::fs::create_dir(&cwd).expect("cwd");
    let program = cwd.join(std::ffi::OsString::from_vec(b"program-\xfe".to_vec()));
    std::fs::write(
        &program,
        b"#!/bin/sh\nprintf '%s\n' \"$1\" \"$VALUE\" \"$PWD\" \"${HOME-unset}\"\n",
    )
    .expect("script");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).expect("mode");
    let launch = keeper::Launch::new(
        program.as_os_str(),
        [std::ffi::OsString::from_vec(b"arg-\xfd".to_vec())],
        &cwd,
        keeper::Environment::Exact(vec![(
            "VALUE".into(),
            std::ffi::OsString::from_vec(b"env-\xfc".to_vec()),
        )]),
    )
    .expect("launch");
    let (out, events) = run_launch(launch);
    let mut expected = b"arg-\xfd\nenv-\xfc\n".to_vec();
    expected.extend(cwd.as_os_str().as_bytes());
    expected.extend(b"\nunset\n");
    assert_eq!(out.stdout, expected);
    assert!(events.iter().any(|e| matches!(e, Event::Done(o) if !o.interrupt_delivered && matches!(o.teardown, keeper::Teardown::Clean { .. }))));
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn relative_path_lookup_uses_child_cwd_and_large_environment_uses_fd_transport() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch();
    let bin = root.join("bin");
    std::fs::create_dir(&bin).expect("bin");
    let program = bin.join("probe");
    std::fs::write(&program, b"#!/bin/sh\nprintf '%s' \"${#VALUE}\"\n").expect("script");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).expect("mode");
    let launch = keeper::Launch::new(
        std::ffi::OsStr::new("probe"),
        [],
        &root,
        keeper::Environment::Exact(vec![
            ("PATH".into(), "bin".into()),
            ("VALUE".into(), "a".repeat(60_000).into()),
        ]),
    )
    .expect("launch");
    assert!(serde_json::to_vec(&launch).expect("json").len() > 128 * 1024);
    let (out, _) = run_launch(launch);
    assert_eq!(out.stdout, b"60000");
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn empty_and_literal_tilde_path_components_use_child_cwd() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch();
    for component in ["", "~/bin"] {
        let dir = root.join(component);
        std::fs::create_dir_all(&dir).expect("path dir");
        let program = dir.join("probe");
        std::fs::write(&program, b"#!/bin/sh\nprintf expected\n").expect("script");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).expect("mode");
        let launch = keeper::Launch::new(
            std::ffi::OsStr::new("probe"),
            [],
            &root,
            keeper::Environment::Exact(vec![("PATH".into(), component.into())]),
        )
        .expect("launch");
        assert_eq!(run_launch(launch).0.stdout, b"expected");
        std::fs::remove_file(program).expect("remove probe");
    }
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn keeper_reports_successful_interrupt_delivery() {
    let root = scratch();
    let ready = root.join("ready");
    let keeper::Launcher {
        mut command,
        control,
        events,
        inherited,
    } = launcher(
        &format!(
            "trap 'exit 0' INT; echo ready > {}; while :; do :; done",
            ready.display()
        ),
        None,
    );
    let mut child = command.spawn().expect("keeper");
    drop(inherited);
    let mut control = std::fs::File::from(control);
    control.write_all(&[Control::Go.byte()]).expect("go");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !ready.exists() {
        assert!(std::time::Instant::now() < deadline, "no readiness");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    control
        .write_all(&[Control::Interrupt.byte(), Control::Interrupt.byte()])
        .expect("interrupt");
    assert!(child.wait().expect("wait").success());
    let done = std::io::BufReader::new(std::fs::File::from(events))
        .lines()
        .find_map(|line| {
            match serde_json::from_str::<Event>(&line.expect("line")).expect("event") {
                Event::Done(outcome) => Some(outcome),
                _ => None,
            }
        })
        .expect("done");
    assert!(done.interrupt_delivered);
    std::fs::remove_dir_all(root).expect("cleanup");
}
