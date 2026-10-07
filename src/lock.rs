//! flock on `$XDG_RUNTIME_DIR/lgtv-wake.lock` around TV actions, so two watchers (or a
//! watcher and a manual `on`/`off`) can't interleave their requests.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const LOCK_FILE: &str = "lgtv-wake.lock";

/// Held exclusive lock; released on drop (closing the file releases the flock).
#[derive(Debug)]
pub struct TvLock {
    _file: File,
}

/// `$XDG_RUNTIME_DIR`, or the temp dir when it's unset (e.g. on macOS).
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// `$XDG_RUNTIME_DIR/lgtv-wake.lock`.
pub fn lock_path() -> PathBuf {
    runtime_dir().join(LOCK_FILE)
}

/// Block (without blocking the runtime) until the exclusive lock is held.
pub async fn acquire() -> Result<TvLock> {
    acquire_at(lock_path()).await
}

/// Like [`acquire`] with an explicit lock file path.
pub async fn acquire_at(path: PathBuf) -> Result<TvLock> {
    tokio::task::spawn_blocking(move || lock_blocking(&path))
        .await
        .context("lock task panicked")?
}

fn lock_blocking(path: &Path) -> Result<TvLock> {
    let file = open(path)?;
    if file.try_lock().is_err() {
        tracing::info!(
            "waiting for another lgtv-wake to finish ({})",
            path.display()
        );
        file.lock()
            .with_context(|| format!("locking {}", path.display()))?;
    }
    tracing::debug!("holding {}", path.display());
    Ok(TvLock { _file: file })
}

fn open(path: &Path) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
        .with_context(|| format!("opening lock file {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn exclusive_until_dropped() {
        let path = std::env::temp_dir().join(format!("lgtv-wake-test-lock-{}", std::process::id()));
        let held = acquire_at(path.clone()).await.unwrap();

        // A second open file description can't take it while it's held.
        let other = open(&path).unwrap();
        assert!(other.try_lock().is_err());

        // A second acquire waits until the first is dropped.
        let waiter = tokio::spawn(acquire_at(path.clone()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!waiter.is_finished());
        drop(held);
        let second = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("second acquire should finish after the first is dropped")
            .unwrap()
            .unwrap();
        assert!(other.try_lock().is_err());
        drop(second);
        other.try_lock().unwrap();

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn path_name() {
        assert_eq!(lock_path().file_name().unwrap(), LOCK_FILE);
    }
}
