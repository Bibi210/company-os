//! File watcher for automatic config/schema hot-reload.
//!
//! Watches directories from protected-zones.json and emits
//! debounced change events so MCP servers can reload without restart.

use std::collections::HashSet;
use std::path::Path;
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

/// Handle returned by [`spawn_watcher`]. Drop it to stop watching.
pub struct FileWatcherHandle {
    pub rx: mpsc::Receiver<ConfigChangeKind>,
    _watcher: RecommendedWatcher,
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

/// Spawn a file watcher on protected zone directories + company/.
///
/// Returns a handle whose `.rx` yields debounced [`ConfigChangeKind`] values.
/// The watcher runs in a background tokio task; drop the handle to stop it.
pub fn spawn_watcher(root: &str, debounce: Duration) -> anyhow::Result<FileWatcherHandle> {
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

    // Watch protected zone directories
    for dir in &watch_dirs {
        let full = root_path.join(dir);
        if full.is_dir() {
            watcher.watch(&full, RecursiveMode::Recursive)?;
        }
    }

    // Also watch company/ top-level for artifact YAML changes
    let company_dir = root_path.join("company");
    if company_dir.is_dir() {
        watcher.watch(&company_dir, RecursiveMode::Recursive)?;
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
        _watcher: watcher,
    })
}
