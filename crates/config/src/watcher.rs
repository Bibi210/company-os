//! File watcher for automatic config/schema hot-reload.
//!
//! Watches directories from protected-zones.json and emits
//! debounced change events so MCP servers can reload without restart.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::constants;
use crate::protected_zones;

/// What category of file changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigChangeKind {
    /// Files in company/config/ or company/personas/ changed.
    Config,
    /// Files in company/schemas/ changed.
    Schemas,
    /// YAML artifacts changed (anything else under company/).
    Artifacts,
}

/// Sentinel that owns the [`RecommendedWatcher`] and emits a loud trace
/// when dropped outside a clean shutdown.
///
/// Always store this in a binding that lives at least as long as the
/// consumer task that reads [`FileWatcherHandle::rx`]. Disjoint capture
/// in Rust 2024 async closures will NOT capture a field that is never
/// read textually — see diagnostic-report 9534dd33.
///
/// `WatcherGuard` is the type that carries `Drop`, NOT [`FileWatcherHandle`]:
/// that asymmetry is what allows the handle to be destructured by move
/// at the caller site without triggering E0509.
pub struct WatcherGuard {
    // `Option` so `Drop` can take ownership and explicit-drop the
    // RecommendedWatcher BEFORE deciding the log level (otherwise the
    // drop trace would fire after the OS resources are already released,
    // racing the notify thread exit).
    watcher: Option<RecommendedWatcher>,
    is_shutting_down: Arc<AtomicBool>,
}

impl WatcherGuard {
    fn new(watcher: RecommendedWatcher, is_shutting_down: Arc<AtomicBool>) -> Self {
        Self {
            watcher: Some(watcher),
            is_shutting_down,
        }
    }
}

impl Drop for WatcherGuard {
    fn drop(&mut self) {
        // Drop the inner watcher first so inotify fds are released
        // before the log line, then trace at the right level.
        // `catch_unwind` is paranoia in case the notify v7 destructor
        // panics: we still want a log line.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.watcher.take();
        }));
        if self.is_shutting_down.load(Ordering::Acquire) {
            tracing::info!(
                "WatcherGuard dropped during shutdown — inotify watches released cleanly."
            );
        } else {
            tracing::error!(
                "WatcherGuard dropped OUTSIDE shutdown — file watcher is now DEAD; \
                 inotify watches released. Investigate immediately (cf. diagnostic 9534dd33)."
            );
        }
    }
}

/// Handle returned by [`spawn_watcher`]. Drop it (or its `_guard` field,
/// if you destructure) to stop watching.
pub struct FileWatcherHandle {
    pub rx: mpsc::Receiver<ConfigChangeKind>,

    /// HOLD this field alive for as long as you consume `rx`.
    ///
    /// Dropping it releases the inotify watches and kills the underlying
    /// file watcher. The leading underscore silences the unused-field
    /// lint; capturing it via `let _keep = _guard;` inside an
    /// `async move` block is MANDATORY in Rust 2024 (disjoint capture
    /// for async closures). Cf. diagnostic-report 9534dd33.
    pub _guard: WatcherGuard,
}

/// Classify a changed path into a [`ConfigChangeKind`].
fn classify(path: &Path, root: &Path) -> Option<ConfigChangeKind> {
    let rel = path.strip_prefix(root).ok()?;
    let rel_str = rel.to_str()?;

    let zones = protected_zones::load(root);
    // Schemas dir gets its own change kind
    if rel_str.starts_with(constants::SCHEMAS_DIR) {
        return Some(ConfigChangeKind::Schemas);
    }
    // Any other protected zone prefix → config change
    for prefix in &zones.prefixes {
        if rel_str.starts_with(prefix.as_str()) {
            return Some(ConfigChangeKind::Config);
        }
    }
    // YAML outside protected zones → artifact change
    if rel_str.ends_with(".yml") || rel_str.ends_with(".yaml") {
        return Some(ConfigChangeKind::Artifacts);
    }
    None
}

/// Spawn a file watcher on protected zone directories + company/ + projects/.
///
/// Returns a handle whose `.rx` yields debounced [`ConfigChangeKind`] values.
/// The watcher runs in a background tokio task; drop the handle (or its
/// `_guard` field) to stop it.
///
/// `is_shutting_down` is shared with the orchestrator's signal handlers
/// (SIGTERM/SIGINT) so that the [`WatcherGuard`] `Drop` impl can emit
/// `info!` on a clean shutdown and `error!` on an unexpected drop.
pub fn spawn_watcher(
    root: &str,
    debounce: Duration,
    is_shutting_down: Arc<AtomicBool>,
) -> anyhow::Result<FileWatcherHandle> {
    let root_path = std::fs::canonicalize(root)?;

    // Read watched dirs from protected-zones.json
    let zones = protected_zones::load(&root_path);
    let watch_dirs: Vec<String> = zones
        .prefixes
        .iter()
        .map(|p| p.trim_end_matches('/').to_string())
        .collect();

    // Channel from notify OS thread → tokio task
    let (notify_tx, mut notify_rx) = mpsc::channel::<Event>(64);

    let mut watcher = RecommendedWatcher::new(
        move |res: Result<Event, notify::Error>| {
            if let Ok(event) = res {
                let _ = notify_tx.blocking_send(event);
            }
        },
        notify::Config::default(),
    )?;

    // Watch protected zone directories (strict: their absence indicates
    // a malformed repo and MUST surface).
    for dir in &watch_dirs {
        let full = root_path.join(dir);
        if full.is_dir() {
            watcher.watch(&full, RecursiveMode::Recursive)?;
        }
    }

    // Also watch company/ top-level for artifact YAML changes (strict).
    let company_dir = root_path.join("company");
    if company_dir.is_dir() {
        watcher.watch(&company_dir, RecursiveMode::Recursive)?;
    }

    // Best-effort: projects/ may be absent on a fresh checkout or if all
    // projects were archived. Its absence is NOT a failure, and an
    // inotify_watch error (e.g. max_user_watches saturated) on this dir
    // alone is not a reason to sink the entire watcher — the other dirs
    // (protected zones + company/) remain watched.
    let projects_dir = root_path.join(constants::PROJECTS_DIR);
    if projects_dir.is_dir()
        && let Err(e) = watcher.watch(&projects_dir, RecursiveMode::Recursive)
    {
        tracing::warn!("Failed to watch projects/ (best-effort, other dirs still watched): {e}");
    }

    // Output channel: debounced change kinds
    let (out_tx, out_rx) = mpsc::channel::<ConfigChangeKind>(16);

    let root_for_task = root_path.clone();
    tokio::spawn(async move {
        loop {
            // Wait for the first event
            let event = match notify_rx.recv().await {
                Some(e) => e,
                None => break, // channel closed
            };

            let mut pending = HashSet::new();
            // Classify paths from the first event
            for path in &event.paths {
                if let Some(kind) = classify(path, &root_for_task) {
                    pending.insert(kind);
                }
            }

            // Drain additional events within the debounce window
            while let Ok(Some(event)) = tokio::time::timeout(debounce, notify_rx.recv()).await {
                for path in &event.paths {
                    if let Some(kind) = classify(path, &root_for_task) {
                        pending.insert(kind);
                    }
                }
            }

            // Flush accumulated changes
            for kind in pending.drain() {
                if out_tx.send(kind).await.is_err() {
                    return; // receiver dropped
                }
            }
        }
    });

    Ok(FileWatcherHandle {
        rx: out_rx,
        _guard: WatcherGuard::new(watcher, is_shutting_down),
    })
}
