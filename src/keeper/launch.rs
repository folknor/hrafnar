//! Explicit process bytes and one keeper launcher for production and protocol tests.
use super::{Spec, TeardownSchedule};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Parent-side environment intent. No opaque Command/env_clear introspection.
pub enum Environment {
    Inherit(Vec<(OsString, Option<OsString>)>),
    Exact(Vec<(OsString, OsString)>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Launch {
    pub(super) program: Vec<u8>,
    pub(super) args: Vec<Vec<u8>>,
    pub(super) cwd: Vec<u8>,
    /// Exact environment, resolved once in the parent. The keeper itself never
    /// receives this through its own environment.
    pub(super) env: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Launch {
    pub fn new(
        program: &OsStr,
        args: impl IntoIterator<Item = OsString>,
        cwd: &Path,
        environment: Environment,
    ) -> std::io::Result<Self> {
        let cwd = if cwd.is_absolute() {
            cwd.to_path_buf()
        } else {
            std::env::current_dir()?.join(cwd)
        };
        let mut env: BTreeMap<OsString, OsString> = match environment {
            Environment::Inherit(overrides) => {
                let mut env: BTreeMap<_, _> = std::env::vars_os().collect();
                for (key, value) in overrides {
                    validate_entry(&key, value.as_deref())?;
                    if let Some(value) = value {
                        env.insert(key, value);
                    } else {
                        env.remove(&key);
                    }
                }
                env
            }
            Environment::Exact(entries) => {
                let mut env = BTreeMap::new();
                for (key, value) in entries {
                    validate_entry(&key, Some(&value))?;
                    if env.insert(key, value).is_some() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "duplicate environment key",
                        ));
                    }
                }
                env
            }
        };
        env.insert("PWD".into(), cwd.as_os_str().to_owned());
        let launch = Self {
            program: program.as_bytes().to_vec(),
            args: args.into_iter().map(OsString::into_vec).collect(),
            cwd: cwd.into_os_string().into_vec(),
            env: env
                .into_iter()
                .map(|(k, v)| (k.into_vec(), v.into_vec()))
                .collect(),
        };
        launch.validate()?;
        Ok(launch)
    }

    pub(super) fn validate(&self) -> std::io::Result<()> {
        let keys: std::collections::BTreeSet<_> = self.env.iter().map(|(k, _)| k).collect();
        let bad = !Path::new(OsStr::from_bytes(&self.cwd)).is_absolute()
            || keys.len() != self.env.len()
            || !self.env.iter().any(|(k, v)| k == b"PWD" && v == &self.cwd)
            || self.program.is_empty()
            || self.program.contains(&0)
            || self.cwd.contains(&0)
            || self.args.iter().any(|a| a.contains(&0))
            || self.env.iter().any(|(k, v)| {
                k.is_empty() || k.contains(&b'=') || k.contains(&0) || v.contains(&0)
            });
        if bad {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid keeper launch bytes",
            ));
        }
        Ok(())
    }
}

fn validate_entry(key: &OsStr, value: Option<&OsStr>) -> std::io::Result<()> {
    if key.is_empty()
        || key.as_bytes().contains(&b'=')
        || key.as_bytes().contains(&0)
        || value.is_some_and(|v| v.as_bytes().contains(&0))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid environment entry",
        ));
    }
    Ok(())
}

/// Owns the inherited descriptors until the keeper has spawned. Dropping after
/// spawn closes only the parent's copies; callers retain control/events ends.
pub struct Launcher {
    pub command: Command,
    pub control: OwnedFd,
    pub events: OwnedFd,
    pub inherited: Vec<OwnedFd>,
}

impl Launcher {
    pub fn new(
        binary: &Path,
        launch: Launch,
        schedule: TeardownSchedule,
        startup: std::time::Duration,
        label: &str,
        orphan_log: Option<PathBuf>,
        stdin: Stdio,
    ) -> anyhow::Result<Self> {
        launch.validate()?;
        schedule.validate()?;
        let (control_r, control) = pipe_pair()?;
        let (events, events_w) = pipe_pair()?;
        let spec = Spec {
            launch,
            control_fd: control_r.as_raw_fd(),
            events_fd: events_w.as_raw_fd(),
            schedule,
            startup,
            orphan_log,
            label: label.into(),
        };
        let payload = serde_json::to_vec(&spec)?;
        anyhow::ensure!(
            payload.len() <= super::protocol::SPEC_MAX,
            "keeper spec exceeds transport limit"
        );
        // SAFETY: constant NUL-terminated name; fresh anonymous descriptor.
        let fd = unsafe {
            libc::memfd_create(
                c"review-spec".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: fresh descriptor.
        let mut file = std::fs::File::from(above_stdio(unsafe { OwnedFd::from_raw_fd(fd) })?);
        use std::io::{Seek, Write};
        file.write_all(&payload)?;
        file.rewind()?;
        // SAFETY: seal the fully written payload against further mutation.
        if unsafe {
            libc::fcntl(
                file.as_raw_fd(),
                libc::F_ADD_SEALS,
                libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let spec_fd = OwnedFd::from(file);
        let mut command = Command::new(binary);
        command
            .arg(super::ARG)
            .arg(spec_fd.as_raw_fd().to_string())
            .process_group(0)
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let keep = [
            control_r.as_raw_fd(),
            events_w.as_raw_fd(),
            spec_fd.as_raw_fd(),
        ];
        // SAFETY: only async-signal-safe operations on existing descriptors.
        unsafe {
            command.pre_exec(move || {
                super::block_handled_signals()?;
                for fd in keep {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        Ok(Self {
            command,
            control,
            events,
            inherited: vec![control_r, events_w, spec_fd],
        })
    }
}

fn pipe_pair() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: fills fresh descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fresh descriptors owned from here.
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    Ok((above_stdio(r)?, above_stdio(w)?))
}
fn above_stdio(fd: OwnedFd) -> std::io::Result<OwnedFd> {
    if fd.as_raw_fd() > 2 {
        return Ok(fd);
    }
    // SAFETY: duplicate our descriptor; original closes on drop.
    let dup = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if dup < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: newly owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(dup) })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn environment_modes_and_validation_are_explicit() {
        let launch = Launch::new(
            OsStr::new("/bin/true"),
            [],
            Path::new("/"),
            Environment::Inherit(vec![
                ("HOME".into(), None),
                ("ONLY_OVERRIDE".into(), Some("value".into())),
            ]),
        )
        .expect("launch");
        assert!(!launch.env.iter().any(|(k, _)| k == b"HOME"));
        assert!(
            launch
                .env
                .iter()
                .any(|(k, v)| k == b"ONLY_OVERRIDE" && v == b"value")
        );
        let exact = Launch::new(
            OsStr::new("/bin/true"),
            [],
            Path::new("/"),
            Environment::Exact(vec![]),
        )
        .expect("exact");
        assert_eq!(exact.env, vec![(b"PWD".to_vec(), b"/".to_vec())]);
        assert!(
            Launch::new(
                OsStr::new("/bin/true"),
                [],
                Path::new("/"),
                Environment::Exact(vec![("bad=key".into(), "value".into())])
            )
            .is_err()
        );
        assert!(
            Launch::new(
                OsStr::new("/bin/true"),
                [OsString::from_vec(vec![0])],
                Path::new("/"),
                Environment::Exact(vec![])
            )
            .is_err()
        );
    }
}
