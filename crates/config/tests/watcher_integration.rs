//! Regression test for the file watcher disjoint-capture bug.
//!
//! See RFC 062ebaa8 and diagnostic-report 9534dd33: in Rust 2024, async
//! closures only capture fields that are textually referenced. A field
//! prefixed with `_` is by convention "unused" and is NOT captured by
//! an `async move { ... }`, even when the surrounding struct must be
//! kept alive (e.g. a `RecommendedWatcher` holding inotify fds).
//!
//! This test reproduces the exact pattern used in
//! `crates/mcp-servers/orchestrator/src/main.rs` (destructure
//! `FileWatcherHandle`, spawn an `async move`, capture `_guard`
//! explicitly via `let _keep = _guard;`) and verifies that a file
//! written under a watched directory does deliver an event through
//! `rx` within a generous timeout. If a future refactor re-introduces
//! the disjoint-capture bug (e.g. by removing the `let _keep` binding),
//! this test will deadlock and fail.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use companyos_config::watcher::{FileWatcherHandle, spawn_watcher};
use tempfile::TempDir;

#[tokio::test]
async fn watcher_survives_async_move_destructure() {
    // (F4.a from RFC review) Reset the static protected_zones cache
    // BEFORE configuring a new root, otherwise a previous test (or a
    // previous run in the same integration binary) may have cached a
    // different PathBuf and `load()` would return that instead of
    // reading our tempdir.
    companyos_config::protected_zones::reload();

    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();

    // (F4.b) Create the minimum subdir layout that spawn_watcher
    // expects. Including `projects/` exercises the best-effort branch
    // introduced in this RFC.
    for d in [
        "company",
        "company/config",
        "company/personas",
        "company/schemas",
        "company/plugins",
        "crates",
        "projects",
    ] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(
        root.join("company/config/protected-zones.json"),
        r#"{"prefixes":["company/config/","company/personas/","company/schemas/","company/plugins/","crates/"],"files":[]}"#,
    )
    .unwrap();
    // Invalidate cache again now that the file exists, so the upcoming
    // `spawn_watcher` reads the real prefixes.
    companyos_config::protected_zones::reload();

    let is_shutting_down = Arc::new(AtomicBool::new(false));
    let handle = spawn_watcher(
        root.to_str().unwrap(),
        Duration::from_millis(500),
        is_shutting_down.clone(),
    )
    .expect("spawn_watcher ok");

    // Reproduce EXACTLY the pattern used in main.rs: destructure the
    // handle, move `_guard` into the async block, and capture it
    // explicitly to defeat disjoint capture.
    let FileWatcherHandle { mut rx, _guard } = handle;
    let task = tokio::spawn(async move {
        let _keep = _guard;
        rx.recv().await
    });

    // Let inotify finish attaching (typically < 50ms; 200ms is a
    // comfortable margin for slow CI).
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Touch a YAML in a watched dir (company/config/ is classified
    // ConfigChangeKind::Config by the watcher's `classify()`).
    let probe = root.join("company/config/__probe.yml");
    std::fs::write(&probe, "x").unwrap();

    // (F4.c) Timeout 2.5s = debounce 500ms + kernel propagation +
    // tokio scheduling + slack for a loaded CI runner. Anything beyond
    // that is a deadlock, not a flake.
    let received = tokio::time::timeout(Duration::from_millis(2500), task)
        .await
        .expect(
            "watcher did not deliver any event within 2.5s — \
             regression of the disjoint-capture bug (cf. diagnostic 9534dd33)",
        );
    assert!(
        matches!(received, Ok(Some(_))),
        "expected Ok(Some(_)) from the watcher consumer, got: {received:?}"
    );

    // Avoid the parasitic error log when `_keep` is dropped at end of
    // scope: semantically, the test ending IS a clean shutdown.
    is_shutting_down.store(true, Ordering::Release);
}
