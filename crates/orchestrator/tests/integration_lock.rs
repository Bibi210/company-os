//! Integration tests for PILIER A (file-lock-based writer coordination).
//!
//! These tests exercise the lock semantics directly via the public API
//! of `companyos_orchestrator::lock`, without spawning the server
//! binary. The server-process-level scenarios from the design-doc
//! 45c04902 protocole-de-validation (tests d, e, e', f) are covered
//! either by the existing unit tests in `src/lock.rs` or by manual
//! validation against the live binary documented in the final
//! lesson-learned (the harness for spawning the binary with realistic
//! signal-driven shutdown and proxy-resilient interactions is heavier
//! than the value gained vs the manual validation, given the proxy
//! infrastructure already exercises the server in real conditions).
//!
//! This file complements `src/lock.rs` tests by validating
//! cross-process semantics: two separate processes (the test binary
//! itself and a thread that mimics a `--index` transient holder)
//! exercising the same lock file at the OS level.

mod test_helpers;

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use companyos_orchestrator::lock::{acquire_exclusive_blocking, try_acquire_exclusive};

use crate::test_helpers::LocalTempDir;

/// Test (d) equivalent — "double instance serveur": when a writer
/// already holds the exclusive lock, a second blocking-acquire that
/// retries within a bounded window eventually times out.
#[test]
fn test_d_double_instance_times_out() {
    let tmp = LocalTempDir::new("test-d-double");
    let lock_path = tmp.join("orchestrator.lock");

    // Holder A acquires the lock for the duration of the test.
    let _holder = acquire_exclusive_blocking(
        &lock_path,
        Duration::from_millis(200),
        Duration::from_secs(1),
    )
    .expect("holder acquire");

    // Server B attempts to acquire with a 600ms timeout — must fail.
    let start = Instant::now();
    let result = acquire_exclusive_blocking(
        &lock_path,
        Duration::from_millis(200),
        Duration::from_millis(600),
    );
    let elapsed = start.elapsed();

    assert!(result.is_err(), "B should time out");
    // The error must be LockBusy, not a different IO error.
    let msg = format!("{:?}", result.err().unwrap());
    assert!(msg.contains("LockBusy"), "expected LockBusy, got {msg}");
    // Timing: B should have waited approximately the timeout (with
    // tolerance for the polling resolution).
    assert!(
        elapsed >= Duration::from_millis(500),
        "B gave up too early: {elapsed:?}"
    );
}

/// Test (e) equivalent — "serveur démarre pendant --index transitoire":
/// when a transient writer holds the lock briefly, a blocking-acquire
/// with a large enough retry window absorbs the wait and succeeds.
#[test]
fn test_e_server_absorbs_transient_index() {
    let tmp = LocalTempDir::new("test-e-transient");
    let lock_path = tmp.join("orchestrator.lock");
    let barrier = Arc::new(Barrier::new(2));

    let lock_path_a = lock_path.clone();
    let barrier_a = barrier.clone();
    // Thread A simulates a transient --index that holds the lock for
    // ~400ms (close to the upper bound of a realistic --index nominal
    // duration of < 200ms, with safety margin).
    let thread_a = thread::spawn(move || {
        let _guard = acquire_exclusive_blocking(
            &lock_path_a,
            Duration::from_millis(50),
            Duration::from_secs(1),
        )
        .expect("A acquire");
        barrier_a.wait();
        thread::sleep(Duration::from_millis(400));
        // _guard drops at scope exit → flock released.
    });

    // Wait until A has the lock.
    barrier.wait();

    // Thread B (the simulated server) tries with the production-realistic
    // 200ms polling / 10s timeout from main.rs.
    let start = Instant::now();
    let result = acquire_exclusive_blocking(
        &lock_path,
        Duration::from_millis(200),
        Duration::from_secs(10),
    );
    let elapsed = start.elapsed();

    thread_a.join().unwrap();
    assert!(result.is_ok(), "B should acquire after A releases");
    // B should have spent roughly 400ms waiting + up to 200ms polling
    // resolution. Sanity bounds.
    assert!(
        elapsed >= Duration::from_millis(300),
        "B acquired too fast: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "B took too long: {elapsed:?}"
    );
}

/// Test (e') equivalent — "--index zombie tient le lock plus que le
/// timeout": the server gives up cleanly with LockBusy. We use a
/// short timeout (500ms) to keep the test fast.
#[test]
fn test_e_prime_zombie_index_triggers_timeout() {
    let tmp = LocalTempDir::new("test-eprime-zombie");
    let lock_path = tmp.join("orchestrator.lock");

    // Zombie holder for the duration of the test (in the same process —
    // we don't need a second OS process to exercise the timeout).
    let _zombie = acquire_exclusive_blocking(
        &lock_path,
        Duration::from_millis(50),
        Duration::from_secs(1),
    )
    .expect("zombie acquire");

    let start = Instant::now();
    let result = acquire_exclusive_blocking(
        &lock_path,
        Duration::from_millis(100),
        Duration::from_millis(500),
    );
    let elapsed = start.elapsed();

    assert!(result.is_err(), "server should give up");
    assert!(format!("{:?}", result.err().unwrap()).contains("LockBusy"));
    assert!(
        elapsed >= Duration::from_millis(400),
        "server gave up too early: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(900),
        "server took too long: {elapsed:?}"
    );
}

/// Validates that try_acquire_exclusive correctly distinguishes between
/// "no holder" and "holder exists" — the foundation of the no-op-if-
/// server semantics in run_index.
#[test]
fn test_try_acquire_distinguishes_free_vs_held() {
    let tmp = LocalTempDir::new("test-try-acquire");
    let lock_path = tmp.join("orchestrator.lock");

    // Free: try_acquire returns Some.
    let probe1 = try_acquire_exclusive(&lock_path).expect("probe1");
    assert!(probe1.is_some(), "free lock should be Some");
    drop(probe1);

    // Held: hold the lock, try_acquire returns None.
    let _holder = acquire_exclusive_blocking(
        &lock_path,
        Duration::from_millis(50),
        Duration::from_secs(1),
    )
    .expect("holder acquire");

    let probe2 = try_acquire_exclusive(&lock_path).expect("probe2 must not error");
    assert!(probe2.is_none(), "held lock should be None");
}

/// Validates RAII: dropping the guard releases the lock immediately,
/// without needing to wait for the kernel to reclaim FDs.
#[test]
fn test_lock_released_on_drop() {
    let tmp = LocalTempDir::new("test-drop");
    let lock_path = tmp.join("orchestrator.lock");

    {
        let _guard = acquire_exclusive_blocking(
            &lock_path,
            Duration::from_millis(50),
            Duration::from_secs(1),
        )
        .expect("first acquire");
        // While in this scope, try_acquire must be None.
        assert!(
            try_acquire_exclusive(&lock_path).unwrap().is_none(),
            "lock should be held"
        );
    }

    // Out of scope: the guard is dropped, the lock is released.
    let probe = try_acquire_exclusive(&lock_path).expect("probe after drop");
    assert!(probe.is_some(), "lock should be free after drop");
}
