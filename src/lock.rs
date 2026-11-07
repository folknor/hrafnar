use anyhow::{Result, bail};
use std::fs::File;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// Open (or create) the host-wide launch lock at `inflight::global_lock_path`,
/// creating its parent directory first. Every path that spaces out provider
/// launches (the fan-out and `review message`) takes the lock through here, so
/// they cannot disagree about where it lives.
pub fn open_global_lock() -> Result<File> {
    let path = crate::inflight::global_lock_path(None);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            anyhow::anyhow!(
                "failed to create launch lock directory {}: {e}",
                parent.display()
            )
        })?;
    }
    open_lock_file(&path)
}

/// Open (or create) a lock file safely. The lock may live on a shared,
/// world-writable path (the `/tmp` fallback when no data directory can be
/// resolved), and a data directory can itself be shared, so:
/// - `O_NOFOLLOW` refuses to open through a symlink, defeating a planted-symlink
///   clobber that `File::create` (which follows symlinks and truncates) allowed;
/// - mode `0666` lets a second user open the pre-existing lock for the flock
///   (subject to umask), instead of failing on a first user's `0644`;
/// - no truncation - the file's contents are irrelevant to advisory locking, and
///   truncating would needlessly rewrite whatever is there.
pub fn open_lock_file(path: &Path) -> Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o666)
        .open(path)
        .map_err(|e| anyhow::anyhow!("failed to open lock file {}: {e}", path.display()))
}

/// Acquire an exclusive advisory lock on the given file.
/// If the lock is contested, prints a message and reports wait time.
/// Released when the file is dropped.
pub fn acquire_blocking(file: &File) -> Result<()> {
    // Try non-blocking first
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret == 0 {
        return Ok(());
    }

    // Check if it's actually contention
    let err = std::io::Error::last_os_error();
    if err.kind() != std::io::ErrorKind::WouldBlock {
        bail!("failed to acquire lock: {err}");
    }

    // Lock is held by another process - wait
    eprintln!("Waiting for the launch lock...");
    let start = std::time::Instant::now();

    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if ret != 0 {
        bail!(
            "failed to acquire lock: {}",
            std::io::Error::last_os_error()
        );
    }

    let elapsed = start.elapsed();
    eprintln!("Lock acquired after {:.1}s", elapsed.as_secs_f64());
    Ok(())
}
