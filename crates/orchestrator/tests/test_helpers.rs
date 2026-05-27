//! Shared test helpers for the orchestrator integration tests.
//!
//! Kept minimal on purpose — only the bits actually used by
//! `integration_lock.rs`. The LocalTempDir pattern is inspired by
//! lesson f3fc4a5d ("avoid tempfile as dev-dep when a 10-line struct
//! does the same job").
//!
//! Note: this file is conventionally compiled as part of the test
//! crate but never referenced from the lib itself, so unused symbols
//! are flagged. `#![allow(dead_code)]` keeps the helpers usable as
//! the test suite evolves without warnings.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// RAII tempdir for tests. Creates a unique directory under
/// `std::env::temp_dir()` on construction, removes it on drop.
///
/// The directory is named with a UUID so concurrent tests can run
/// without colliding.
pub struct LocalTempDir {
    path: PathBuf,
}

impl LocalTempDir {
    pub fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!("{}-{}", prefix, uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create_dir_all");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn join(&self, sub: &str) -> PathBuf {
        self.path.join(sub)
    }
}

impl Drop for LocalTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
