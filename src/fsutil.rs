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
        "{file_name}.hrafnar-{}",
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

/// Serialize cooperating log readers/writers on the log inode itself. JSONL
/// logs are never replaced by this mechanism; frozen launch locks are separate.
fn log_lock(file: &std::fs::File, mode: libc::c_int) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    loop {
        // SAFETY: a descriptor owned through the operation; close releases it.
        if unsafe { libc::flock(file.as_raw_fd(), mode) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Append one compact encoded JSON value, with no physical newline. Preserve
/// all existing bytes, even an incomplete historical tail: a separator isolates
/// it before the next row. The lock spans inspection, repair and every short
/// write. Success means a complete row was appended, not fsync durability.
pub(crate) fn append_jsonl(path: &Path, row: &[u8]) -> std::io::Result<()> {
    append_jsonl_with(path, row, std::io::Write::write_all)
}
fn append_jsonl_with(
    path: &Path,
    row: &[u8],
    write: impl FnOnce(&mut std::fs::File, &[u8]) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    if row.is_empty() || row.contains(&b'\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "expected one encoded JSONL value",
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)?;
    log_lock(&file, libc::LOCK_EX)?;
    if file.seek(SeekFrom::End(0))? != 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            file.write_all(b"\n")?;
        }
    }
    let mut encoded = Vec::with_capacity(row.len() + 1);
    encoded.extend_from_slice(row);
    encoded.push(b'\n');
    write(&mut file, &encoded)
}

/// Snapshot bytes, not UTF-8: a failed append may stop inside a character.
/// Content readers split physical lines so one damaged fragment cannot hide
/// the earlier or later valid records. Old unlocked writers cannot participate
/// in this cooperative protocol; no cross-version atomicity is claimed.
pub(crate) fn read_jsonl(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    log_lock(&file, libc::LOCK_SH)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_utf8_tail_is_isolated_and_short_concurrent_writes_do_not_interleave() {
        use std::io::Write;
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(crate::ids::generate_uuid());
        let data = crate::data::DataDir::at(root.clone());
        let path = data
            .sessions(crate::data::LogFamily::Public)
            .expect("log path");
        let row = |prompt: &str| {
            serde_json::to_vec(&serde_json::json!({
            "timestamp": "t", "project": "/p", "hostname": "h", "audit_id": "a", "provider": "codex",
            "archetype": "bare", "session_id": "session", "operator_prompt": prompt,
            "assembled_prompt": prompt, "hrafnar_version": "0",
        })).expect("encode")
        };
        let first = row("first");
        append_jsonl(&path, &first).expect("earlier complete row");
        let broken = row("é partial");
        let cut = broken.iter().position(|b| *b == 0xc3).expect("UTF-8 lead") + 1;
        assert!(
            append_jsonl_with(&path, &broken, |file, bytes| {
                file.write_all(&bytes[..cut])?;
                Err(std::io::Error::other(
                    "injected failure after partial write",
                ))
            })
            .is_err()
        );
        let before = std::fs::read(&path).expect("partial tail");
        append_jsonl(&path, &row("after")).expect("repair separator and append");
        assert!(
            std::fs::read(&path)
                .expect("preserved bytes")
                .starts_with(&before)
        );
        let rows = crate::sessions::read_all_in(&data);
        assert_eq!(
            rows.iter()
                .map(|r| r.operator_prompt.as_str())
                .collect::<Vec<_>>(),
            ["first", "after"]
        );
        std::thread::scope(|scope| {
            for worker in 0..4 {
                let path = &path;
                let row = &row;
                scope.spawn(move || {
                    for index in 0..8 {
                        let encoded = row(&format!("worker-{worker}-{index}"));
                        append_jsonl_with(path, &encoded, |file, bytes| {
                            for byte in bytes {
                                file.write_all(&[*byte])?;
                                std::thread::yield_now();
                            }
                            Ok(())
                        })
                        .expect("serialized byte writes");
                    }
                });
            }
        });
        let rows = crate::sessions::read_all_in(&data);
        assert_eq!(rows.len(), 34);
        assert_eq!(
            rows.iter()
                .map(|r| r.operator_prompt.as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            34
        );
        let _ = std::fs::remove_dir_all(root);
    }

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
                .contains(".hrafnar-")
        }));
        std::fs::remove_dir_all(dir).expect("cleanup");
    }
}
