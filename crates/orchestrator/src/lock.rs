//! File-lock based coordination of DB writers (PILIER A).
//!
//! Provides RAII guards around a POSIX `flock(2)` exclusive lock on a
//! sentinel file (typically `company/data/orchestrator.lock`). Used by:
//!
//! - The orchestrator server (`run_server`): acquires the exclusive lock
//!   for its entire lifetime via [`acquire_exclusive_blocking`] with a
//!   bounded retry to absorb transient `--index` holders. The lock is
//!   released when the [`DbLockGuard`] is dropped, including via SIGKILL
//!   (the kernel releases POSIX flocks on process death).
//!
//! - The orchestrator CLI `--index` (`run_index`): tries
//!   [`try_acquire_exclusive`] non-blocking. If no other writer holds the
//!   lock the guard is kept for the duration of the indexing work. If
//!   another writer (the server) holds it, `--index` becomes no-op and
//!   delegates to the server file watcher.
//!
//! Rationale for OS flock over PID files or BEGIN EXCLUSIVE
//! transactions: the kernel releases the lock automatically on process
//! death (including SIGKILL), there is no stale-lock cleanup race, and
//! it remains functional even when the DB is corrupted.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use crate::error::OrchestratorError;

/// RAII guard around a file-level exclusive lock.
///
/// The lock is released when the guard is dropped. The underlying file
/// stays on disk (it is a sentinel; its content is not significant).
///
/// Drop is best-effort: if `unlock` errors at drop time, the error is
/// silently ignored because the kernel will release the lock on FD close
/// anyway.
#[derive(Debug)]
pub struct DbLockGuard {
    file: File,
    path: String,
}

impl DbLockGuard {
    /// Path of the sentinel file backing this lock guard, for logging.
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl Drop for DbLockGuard {
    fn drop(&mut self) {
        // Explicit unlock is documentation; closing the FD (via File drop)
        // also releases the lock. Errors here are unrecoverable and
        // ignored.
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

/// Open (creating if necessary) the sentinel file at `path` with read +
/// write permissions. The file's content is irrelevant; only its
/// associated flock matters.
fn open_sentinel(path: &Path) -> Result<File, OrchestratorError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(OrchestratorError::Io)
}

/// Attempt to acquire the exclusive lock once, non-blocking.
///
/// Returns:
/// - `Ok(Some(guard))` if the lock was acquired.
/// - `Ok(None)` if another writer holds the lock (the OS reported
///   `EWOULDBLOCK` / `EAGAIN`).
/// - `Err(_)` for any other failure (file permission denied, parent
///   directory missing, etc.).
///
/// This is the entry point used by `--index` to detect whether the
/// server is alive without blocking.
pub fn try_acquire_exclusive(path: &Path) -> Result<Option<DbLockGuard>, OrchestratorError> {
    let file = open_sentinel(path)?;
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(Some(DbLockGuard {
            file,
            path: path.display().to_string(),
        })),
        Err(e) if would_block(&e) => Ok(None),
        Err(e) => Err(OrchestratorError::Io(e)),
    }
}

/// Acquire the exclusive lock, retrying with `retry_interval` between
/// attempts until `timeout` elapses.
///
/// Returns [`OrchestratorError::LockBusy`] if the timeout expires before
/// the lock can be acquired.
///
/// This is the entry point used by `run_server` to absorb transient
/// `--index` holders during a proxy-driven restart (see RFC cdbfee72
/// PROPOSITION 1 and finding A round 2). A typical `--index` holds the
/// lock for < 200ms, so the recommended timeout of 10s leaves a 50x
/// margin without making the server tolerant to a true double instance.
pub fn acquire_exclusive_blocking(
    path: &Path,
    retry_interval: Duration,
    timeout: Duration,
) -> Result<DbLockGuard, OrchestratorError> {
    let start = Instant::now();
    let timeout_ms = timeout.as_millis() as u64;
    let path_display = path.display().to_string();

    loop {
        // Re-open the file at every retry because the previous handle was
        // dropped when the previous try failed (the OpenOptions builder
        // doesn't expose a way to keep the FD alive across try_lock
        // failures portably).
        let file = open_sentinel(path)?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => {
                return Ok(DbLockGuard {
                    file,
                    path: path_display,
                });
            }
            Err(e) if would_block(&e) => {
                if start.elapsed() >= timeout {
                    return Err(OrchestratorError::LockBusy {
                        path: path_display,
                        timeout_ms,
                    });
                }
                thread::sleep(retry_interval);
            }
            Err(e) => return Err(OrchestratorError::Io(e)),
        }
    }
}

/// True if the IO error reports EWOULDBLOCK or EAGAIN, i.e. the lock is
/// already held by another process. Maps both error kinds since fs2
/// returns one or the other depending on the platform.
fn would_block(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::WouldBlock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    /// Local Drop-based tempdir, inspired by lesson f3fc4a5d (avoid adding
    /// tempfile as a dev-dependency just for this).
    struct LocalTempDir {
        path: std::path::PathBuf,
    }

    impl LocalTempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("orch-lock-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn lock_path(&self) -> std::path::PathBuf {
            self.path.join("orchestrator.lock")
        }
    }

    impl Drop for LocalTempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn test_acquire_then_release() {
        let tmp = LocalTempDir::new();
        let path = tmp.lock_path();

        {
            let guard = acquire_exclusive_blocking(
                &path,
                Duration::from_millis(50),
                Duration::from_secs(1),
            )
            .expect("first acquire should succeed");
            assert_eq!(guard.path(), path.display().to_string());
        }

        // After drop, a fresh acquire must succeed.
        let _guard2 =
            acquire_exclusive_blocking(&path, Duration::from_millis(50), Duration::from_secs(1))
                .expect("second acquire after release should succeed");
    }

    #[test]
    fn test_try_acquire_when_held() {
        let tmp = LocalTempDir::new();
        let path = tmp.lock_path();

        let _holder =
            acquire_exclusive_blocking(&path, Duration::from_millis(50), Duration::from_secs(1))
                .expect("holder acquire should succeed");

        // While the holder lives, try_acquire must return Ok(None).
        let result = try_acquire_exclusive(&path).expect("try_acquire must not error");
        assert!(result.is_none(), "lock is held, try_acquire should be None");
    }

    #[test]
    fn test_try_acquire_when_free() {
        let tmp = LocalTempDir::new();
        let path = tmp.lock_path();

        let result = try_acquire_exclusive(&path).expect("try_acquire must not error");
        assert!(result.is_some(), "free lock, try_acquire should be Some");
    }

    #[test]
    fn test_blocking_acquire_succeeds_after_release() {
        // Thread A holds the lock for ~300ms then releases.
        // Thread B calls acquire_blocking with a 2s timeout — must succeed.
        let tmp = LocalTempDir::new();
        let path = tmp.lock_path();
        let barrier = Arc::new(Barrier::new(2));

        let path_a = path.clone();
        let barrier_a = barrier.clone();
        let thread_a = thread::spawn(move || {
            let _guard = acquire_exclusive_blocking(
                &path_a,
                Duration::from_millis(50),
                Duration::from_secs(1),
            )
            .expect("A acquire");
            // Signal B to start trying
            barrier_a.wait();
            thread::sleep(Duration::from_millis(300));
            // guard drops here
        });

        // Wait for A to have the lock
        barrier.wait();
        // B tries to acquire with bounded retry
        let start = Instant::now();
        let _guard_b =
            acquire_exclusive_blocking(&path, Duration::from_millis(50), Duration::from_secs(2))
                .expect("B should eventually acquire after A releases");
        let elapsed = start.elapsed();

        thread_a.join().unwrap();
        // B should have waited roughly 300ms (within tolerance)
        assert!(
            elapsed >= Duration::from_millis(200),
            "B acquired too fast: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(800),
            "B took too long: {elapsed:?}"
        );
    }

    #[test]
    fn test_blocking_acquire_times_out() {
        // Thread A holds the lock for the duration of the test.
        // Thread B calls acquire_blocking with 500ms timeout — must fail.
        let tmp = LocalTempDir::new();
        let path = tmp.lock_path();

        let _holder =
            acquire_exclusive_blocking(&path, Duration::from_millis(50), Duration::from_secs(1))
                .expect("holder acquire");

        let result = acquire_exclusive_blocking(
            &path,
            Duration::from_millis(50),
            Duration::from_millis(500),
        );
        match result {
            Err(OrchestratorError::LockBusy {
                path: p,
                timeout_ms,
            }) => {
                assert!(p.contains("orchestrator.lock"));
                assert_eq!(timeout_ms, 500);
            }
            other => panic!("expected LockBusy, got {other:?}"),
        }
    }
}
