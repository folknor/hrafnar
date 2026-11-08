//! Filesystem update mechanics. Callers own path and content admission policy.
use std::path::{Path, PathBuf};

/// Read, transform and atomically replace a UTF-8 file while holding a separate
/// `<name>.lock` flock throughout. `None` leaves it untouched. Replacement uses
/// a unique 0600 temporary file, removed on every returned error. The lock inode
/// survives replacement. Callers must supply a single filename component.
///
/// This replaces the destination directory entry, not a symlink's target;
/// reading the old body follows symlinks. Callers decide whether to admit them.
pub(crate) fn locked_update(
    directory: &Path,
    name: &str,
    update: impl FnOnce(&str) -> anyhow::Result<Option<String>>,
) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    let path = directory.join(name);
    std::fs::create_dir_all(directory)?;
    let dir = directory.to_path_buf();
    let file_name = name.to_string();

    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.join(format!("{file_name}.lock")))?;
    // SAFETY: a valid fd owned by `lock`, which outlives the lock; the kernel
    // releases the flock when the fd closes.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        anyhow::bail!(
            "cannot lock {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }

    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let Some(new_text) = update(&text).map_err(|e| anyhow::anyhow!("{e} ({})", path.display()))?
    else {
        return Ok(());
    };
    let tmp = dir.join(format!(
        "{file_name}.review-{}",
        crate::ids::generate_uuid()
    ));
    let mut f = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    let _cleanup = TempFile(tmp.clone());
    f.write_all(new_text.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locked_updates_serialize_the_whole_read_transform_replace_cycle() {
        use std::os::unix::fs::PermissionsExt;
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(crate::ids::generate_uuid());
        std::fs::create_dir_all(&dir).expect("scratch");
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let dir = &dir;
                scope.spawn(move || {
                    for _ in 0..8 {
                        locked_update(dir, "count", |text| {
                            let count = if text.is_empty() {
                                0
                            } else {
                                text.parse::<u32>()?
                            };
                            Ok(Some((count + 1).to_string()))
                        })
                        .expect("serialized update");
                    }
                });
            }
        });
        assert_eq!(
            std::fs::read_to_string(dir.join("count")).expect("count"),
            "32"
        );
        assert_eq!(
            std::fs::metadata(dir.join("count"))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        locked_update(&dir, "count", |_| Ok(None)).expect("no-op");
        assert!(locked_update(&dir, "count", |_| anyhow::bail!("refuse")).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join("count")).expect("unchanged"),
            "32"
        );
        // A dangling destination symlink reads as missing, but replacement of
        // an existing directory fails after the temporary has been written.
        std::os::unix::fs::symlink(dir.join("missing"), dir.join("blocked")).expect("link");
        assert!(
            locked_update(&dir, "blocked", |_| {
                std::fs::remove_file(dir.join("blocked"))?;
                std::fs::create_dir(dir.join("blocked"))?;
                Ok(Some("new".into()))
            })
            .is_err()
        );
        assert!(!std::fs::read_dir(&dir).expect("entries").any(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .contains(".review-")
        }));
        std::fs::remove_dir_all(dir).expect("cleanup");
    }
}
