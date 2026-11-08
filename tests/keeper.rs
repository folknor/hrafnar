//! The keeper driven directly over its protocol, for what no runner test can
//! reach: `review` itself disappearing mid-run. Its control pipe then closes,
//! and the keeper must tear the run down on its own and leave its outcome in
//! the orphan log, since nobody is left to read its events.

use review::keeper::{self, Control, Event, Spec};
use std::io::{BufRead, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;

fn scratch() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-scratch")
        .join(review::config::generate_uuid());
    std::fs::create_dir_all(&root).expect("scratch dir");
    root
}

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: fills two descriptors we then own.
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    // SAFETY: fresh descriptors.
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

#[test]
fn a_keeper_whose_owner_vanishes_tears_down_and_logs_the_outcome() {
    let root = scratch();
    let pidfile = root.join("detached.pid");
    let orphan_log = root.join("teardowns.jsonl");
    let (control_r, control_w) = pipe();
    let (events_r, events_w) = pipe();
    let spec = Spec {
        program: "/bin/sh".to_string(),
        args: vec![
            "-c".to_string(),
            format!(
                "setsid sh -c 'echo $$ > {}; exec sleep 300' & sleep 300",
                pidfile.display()
            ),
        ],
        control_fd: control_r.as_raw_fd(),
        events_fd: events_w.as_raw_fd(),
        leftover_grace_ms: 200,
        terminate_grace_ms: 300,
        kill_phase_ms: 500,
        beat_ms: 20,
        startup_ms: 10_000,
        flush_ms: 500,
        orphan_log: Some(orphan_log.clone()),
        label: "a test run".to_string(),
    };
    let keep = [control_r.as_raw_fd(), events_w.as_raw_fd()];
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_review"));
    cmd.arg(keeper::ARG)
        .arg(serde_json::to_string(&spec).expect("spec"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    // SAFETY: async-signal-safe calls only, on descriptors that exist.
    unsafe {
        cmd.pre_exec(move || {
            keeper::block_handled_signals()?;
            for fd in keep {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("spawn keeper");
    drop(control_r);
    drop(events_w);

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
    assert_eq!(entry["outcome"]["clean"], true);
    let _ = std::fs::remove_dir_all(&root);
}

/// Start a keeper for `sh -c <script>` and hand `control` to it; return once it
/// has exited.
fn keeper_given(script: &str, control: &[Control]) -> std::process::ExitStatus {
    let (control_r, control_w) = pipe();
    let (events_r, events_w) = pipe();
    let spec = Spec {
        program: "/bin/sh".to_string(),
        args: vec!["-c".to_string(), script.to_string()],
        control_fd: control_r.as_raw_fd(),
        events_fd: events_w.as_raw_fd(),
        leftover_grace_ms: 100,
        terminate_grace_ms: 100,
        kill_phase_ms: 100,
        beat_ms: 10,
        startup_ms: 10_000,
        flush_ms: 100,
        orphan_log: None,
        label: "never launched".to_string(),
    };
    let keep = [control_r.as_raw_fd(), events_w.as_raw_fd()];
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_review"));
    cmd.arg(keeper::ARG)
        .arg(serde_json::to_string(&spec).expect("spec"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: async-signal-safe calls only, on descriptors that exist.
    unsafe {
        cmd.pre_exec(move || {
            for fd in keep {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("spawn keeper");
    drop(control_r);
    drop(events_w);
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
