//! Timestamps shared between watcher processes, as the mtime of marker files in
//! `$XDG_RUNTIME_DIR` (like the lock file): when a keyboard or mouse was last used, and when
//! the TV was last turned off. A missing or unreadable mark counts as "never".

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

/// Touched on keyboard and mouse input.
const DESK_FILE: &str = "lgtv-wake.desk";
/// Touched when `off` sends `turnOff`.
const OFF_FILE: &str = "lgtv-wake.off";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// A keyboard or mouse was used.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))] // used by `watch`
    Desk,
    /// The TV was turned off.
    Off,
}

impl Mark {
    fn path(self) -> PathBuf {
        crate::lock::runtime_dir().join(match self {
            Mark::Desk => DESK_FILE,
            Mark::Off => OFF_FILE,
        })
    }

    /// Record that this happened now.
    pub fn touch(self) -> Result<()> {
        touch_at(&self.path(), SystemTime::now())
    }

    /// How long ago this last happened, if within `window`.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))] // used by `watch`
    pub fn within(self, window: Duration) -> Option<Duration> {
        recent(read_at(&self.path()), SystemTime::now(), window)
    }
}

/// Set the mtime of `path` (creating it if needed) to `t`. A single metadata update, so
/// readers never see a partial write.
fn touch_at(path: &Path, t: SystemTime) -> Result<()> {
    File::options()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|f| f.set_modified(t))
        .with_context(|| format!("updating {}", path.display()))
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // used by `watch`
fn read_at(path: &Path) -> Option<SystemTime> {
    path.metadata().and_then(|m| m.modified()).ok()
}

/// Age of `mark` at `now` if it's at most `window`. A mark in the future (the clock went
/// back) doesn't count, so it can't suppress anything for long.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // used by `watch`
fn recent(mark: Option<SystemTime>, now: SystemTime, window: Duration) -> Option<Duration> {
    let age = now.duration_since(mark?).ok()?;
    (age <= window).then_some(age)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(30);

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + secs)
    }

    #[test]
    fn recent_within_window() {
        assert_eq!(
            recent(Some(at(0)), at(10), WINDOW),
            Some(Duration::from_secs(10))
        );
        assert_eq!(recent(Some(at(0)), at(30), WINDOW), Some(WINDOW));
        assert_eq!(recent(Some(at(0)), at(0), WINDOW), Some(Duration::ZERO));
    }

    #[test]
    fn old_missing_or_future_marks_dont_count() {
        assert_eq!(recent(Some(at(0)), at(31), WINDOW), None);
        assert_eq!(recent(None, at(10), WINDOW), None);
        assert_eq!(recent(Some(at(10)), at(0), WINDOW), None);
    }

    #[test]
    fn touch_then_read() {
        let path = std::env::temp_dir().join(format!("lgtv-wake-test-mark-{}", std::process::id()));
        assert_eq!(read_at(&path), None);
        touch_at(&path, at(5)).unwrap();
        assert_eq!(read_at(&path), Some(at(5)));
        // Touching again moves the time, keeping the file.
        touch_at(&path, at(9)).unwrap();
        assert_eq!(read_at(&path), Some(at(9)));
        std::fs::remove_file(&path).unwrap();
    }
}
