//! Protected zones — loaded from company/config/protected-zones.json (single source of truth).
//! Call `reload()` when the file changes (triggered by watcher, not polling).

use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::constants;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProtectedZones {
    pub prefixes: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
}

static CACHE: Mutex<Option<(PathBuf, ProtectedZones)>> = Mutex::new(None);

fn read_from_disk(root: &Path) -> ProtectedZones {
    let json_path = root.join(constants::PROTECTED_ZONES_FILE);
    std::fs::read_to_string(&json_path)
        .ok()
        .and_then(|s| serde_json::from_str::<ProtectedZones>(&s).ok())
        .unwrap_or_default()
}

/// Load protected zones (cached per root path). Call `reload()` to refresh.
pub fn load(root: &Path) -> ProtectedZones {
    let mut cache = CACHE.lock().unwrap();
    if let Some((ref cached_root, ref zones)) = *cache
        && cached_root == root
    {
        return zones.clone();
    }
    let zones = read_from_disk(root);
    *cache = Some((root.to_path_buf(), zones.clone()));
    zones
}

/// Invalidate cache — next `load()` will re-read from disk.
pub fn reload() {
    let mut cache = CACHE.lock().unwrap();
    *cache = None;
}

/// Check if a relative path is in a protected zone.
pub fn is_protected(root: &Path, rel_path: &str) -> bool {
    let zones = load(root);
    let normalized = rel_path.replace('\\', "/");
    zones
        .prefixes
        .iter()
        .any(|prefix| normalized.starts_with(prefix.as_str()))
        || zones.files.contains(&normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_zones(dir: &std::path::Path, json: &str) {
        let zones_dir = dir.join("company/config");
        fs::create_dir_all(&zones_dir).unwrap();
        fs::write(zones_dir.join("protected-zones.json"), json).unwrap();
    }

    #[test]
    fn test_is_protected_prefix_match() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write_zones(root, r#"{"prefixes": ["src/"], "files": []}"#);
        reload();
        assert!(is_protected(root, "src/main.rs"));
    }

    #[test]
    fn test_is_protected_file_match() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write_zones(root, r#"{"prefixes": [], "files": ["Makefile"]}"#);
        reload();
        assert!(is_protected(root, "Makefile"));
    }

    #[test]
    fn test_is_protected_no_match() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write_zones(root, r#"{"prefixes": ["src/"], "files": []}"#);
        reload();
        assert!(!is_protected(root, "README.md"));
    }

    #[test]
    fn test_is_protected_backslash_normalization() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write_zones(root, r#"{"prefixes": ["src/"], "files": []}"#);
        reload();
        assert!(is_protected(root, "src\\main.rs"));
    }

    #[test]
    fn test_load_missing_file_returns_default() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        reload();
        let zones = load(root);
        assert!(zones.prefixes.is_empty());
        assert!(zones.files.is_empty());
    }
}
