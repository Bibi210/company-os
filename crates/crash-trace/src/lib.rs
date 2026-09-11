//! Pre-unwind crash traces for the served MCP servers (RFC 5bacb08a, D3b).
//!
//! [`install`] registers a process-wide panic hook that writes one plain
//! text trace per panic under [`constants::CRASHES_DIR`] BEFORE the unwind
//! starts. That ordering is the entire point: a panic hook runs at the
//! panic site, before any destructor, so the trace exists on disk even
//! when a panicking destructor later turns the unwind into an abort and
//! kills the process instantly. That is the failure mode of the six
//! `SIGABRT` of the served orchestrator between 2026-09-09 and 2026-09-11,
//! each of which left no diagnostic material at all.
//!
//! Traces are never rotated and never purged: one file per crash, kept
//! until a human decides otherwise. They must survive the process, the
//! session and the reboot.
//!
//! # Infallibility invariant (RFC 5bacb08a, D3b)
//!
//! The hook NEVER panics and never propagates an error. Every I/O result
//! is deliberately discarded, no `?`, no `unwrap`, no `expect`, no slice
//! indexing. If the trace cannot be written, the hook simply returns and
//! the default panic behaviour proceeds untouched: a failure here degrades
//! observability, never availability. The reason is not style. A hook that
//! panicked would turn a plain panic, which the per-artifact `catch_unwind`
//! of the indexing path confines, into the very abort this crate exists to
//! document, making the instrumentation the cause of the symptom.
//!
//! Two consequences of that invariant, both deliberate:
//!
//! - the crash directory is created EAGERLY by [`install`], at boot, not
//!   by the hook. The failure surface left at panic time is `open`,
//!   `write`, `fsync` and nothing else.
//! - the previously installed hook is chained and called after ours, so
//!   the standard `thread '<name>' panicked at ...` line still reaches
//!   stderr, where the proxy captures and persists it (RFC 5bacb08a, D1).
//!   The two channels are complementary by design: when a destructor
//!   panics during cleanup, the runtime prints its abort message straight
//!   to stderr, which only the proxy channel can capture.

use std::cell::{Cell, RefCell};
use std::io::Write;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};

use companyos_config::constants;

thread_local! {
    /// Path of the artifact currently being processed on this thread, if
    /// any. Set through [`ArtifactScope`] by the indexing path so a trace
    /// can name the artifact that triggered the panic.
    static ARTIFACT_SCOPE: RefCell<Option<String>> = const { RefCell::new(None) };

    /// How many times the hook has been entered on this thread. Never
    /// decremented, so the value doubles as a re-entrancy indicator: a
    /// value above 1 means this thread has already been through the hook,
    /// which is the signature of a panic raised while a previous one was
    /// still unwinding. It also keeps two traces of the same millisecond
    /// from colliding on the same file name.
    static HOOK_ENTRIES: Cell<usize> = const { Cell::new(0) };
}

/// Record `path` as the artifact being processed on the current thread for
/// as long as the returned guard lives.
///
/// Sound on the indexing path because that path is synchronous: there is
/// no `await` between entering the scope and leaving it, so the work
/// cannot migrate to another worker thread of the async runtime and the
/// thread local stays the right one.
///
/// The guard clears the scope on drop, including while unwinding. That is
/// harmless and correct: the hook has already run and read the scope by
/// the time any destructor executes.
pub struct ArtifactScope;

impl ArtifactScope {
    /// Enter a scope naming `path` as the artifact under work.
    pub fn enter(path: &str) -> Self {
        let _ = ARTIFACT_SCOPE.try_with(|slot| {
            *slot.borrow_mut() = Some(path.to_string());
        });
        ArtifactScope
    }
}

impl Drop for ArtifactScope {
    fn drop(&mut self) {
        let _ = ARTIFACT_SCOPE.try_with(|slot| {
            *slot.borrow_mut() = None;
        });
    }
}

/// Install the crash trace hook for a server rooted at `root`, labelling
/// every trace with `binary`.
///
/// Creates `<root>/company/data/crashes` eagerly, then chains a panic hook
/// in front of the current one. Call it as the very first statement of
/// `main`, before any argument parsing, so that no code path of the
/// process is left uninstrumented.
///
/// Calling it twice stacks two hooks and writes two traces per panic;
/// callers are expected to call it once.
pub fn install(root: &str, binary: &'static str) {
    install_at(Path::new(root).join(constants::CRASHES_DIR), binary);
}

/// Same as [`install`] but with an explicit trace directory.
///
/// Exposed for tests, which need to point the hook at a temporary
/// directory, and for any caller that does not lay its data out under a
/// CompanyOS root.
pub fn install_at(dir: PathBuf, binary: &'static str) {
    // EAGER creation, on purpose: see the infallibility invariant in the
    // module documentation. A failure here is ignored, exactly like a
    // failure inside the hook: it costs observability, not availability.
    let _ = std::fs::create_dir_all(&dir);

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // `Result::unwrap_or_default`, never the panicking `unwrap`: when
        // the thread locals are already gone (thread teardown) we carry on
        // with a neutral counter rather than give up on the trace.
        let entry = HOOK_ENTRIES
            .try_with(|cell| {
                let next = cell.get().saturating_add(1);
                cell.set(next);
                next
            })
            .unwrap_or_default();
        write_trace(&dir, binary, info, entry);
        previous(info);
    }));
}

/// Write one trace file for `info` into `dir`. Best effort from start to
/// finish: every fallible step is discarded and the function always
/// returns normally.
fn write_trace(dir: &Path, binary: &str, info: &PanicHookInfo<'_>, entry: usize) {
    let now = chrono::Utc::now();
    let pid = std::process::id();

    // `<compact timestamp>-<pid>-<hook entry>.txt`: the entry number keeps
    // two panics of the same millisecond on the same thread apart, which
    // is precisely the double panic case we want both halves of.
    let name = format!(
        "{}-{}-{}.{}",
        now.format("%Y%m%dT%H%M%S%3f"),
        pid,
        entry,
        constants::CRASH_TRACE_EXT
    );

    let message = payload_message(info);
    let location = match info.location() {
        Some(loc) => format!("{}:{}:{}", loc.file(), loc.line(), loc.column()),
        None => "<unknown>".to_string(),
    };
    let artifact = match ARTIFACT_SCOPE.try_with(|slot| slot.borrow().clone()) {
        Ok(Some(path)) => path,
        Ok(None) | Err(_) => "<none>".to_string(),
    };
    // Non panicking in current std: when thread locals are unavailable it
    // yields an unnamed handle instead of failing.
    let thread = std::thread::current();
    let thread_name = match thread.name() {
        Some(name) => name.to_string(),
        None => "<unnamed>".to_string(),
    };
    // Forced: a trace without a backtrace would not be worth writing, and
    // RUST_BACKTRACE is not set on the served binaries.
    let backtrace = std::backtrace::Backtrace::force_capture();

    let mut body = String::new();
    body.push_str("=== companyos crash trace ===\n");
    body.push_str(&format!("timestamp: {}\n", now.to_rfc3339()));
    body.push_str(&format!("binary: {binary}\n"));
    body.push_str(&format!("pid: {pid}\n"));
    body.push_str(&format!("incarnation: {}\n", incarnation()));
    body.push_str(&format!("thread: {thread_name}\n"));
    body.push_str(&format!("hook_entry: {entry}\n"));
    body.push_str(&format!("reentered_hook: {}\n", entry > 1));
    body.push_str(&format!("artifact: {artifact}\n"));
    body.push_str(&format!("location: {location}\n"));
    body.push_str(&format!("message: {message}\n"));
    body.push_str("--- backtrace ---\n");
    body.push_str(&format!("{backtrace}\n"));

    // No `create_dir_all` here: the directory is created at install time
    // so that the panic path is limited to open, write and fsync.
    if let Ok(mut file) = std::fs::File::create(dir.join(name)) {
        let _ = file.write_all(body.as_bytes());
        let _ = file.flush();
        // Durability is the reason this crate exists: an abort follows the
        // hook immediately in the double panic case, so the bytes must be
        // on the device before we return.
        let _ = file.sync_all();
    }
}

/// Best effort extraction of the panic message.
fn payload_message(info: &PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "<non string panic payload>".to_string()
    }
}

/// Incarnation number of this process, as counted by the supervising
/// proxy and handed over through the environment at spawn.
///
/// Falls back to a self-describing marker when the variable is absent,
/// which happens whenever a server is started outside the proxy (a CLI
/// invocation, a test, a manual run): the PID in the trace and its file
/// name still identify the process.
fn incarnation() -> String {
    match std::env::var(constants::ENV_MCP_INCARNATION) {
        Ok(value) if !value.trim().is_empty() => value,
        _ => "<unset, not spawned by the proxy>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// The panic hook is process wide, so tests that install one must not
    /// overlap. `cargo test` runs them on parallel threads of a single
    /// process.
    fn hook_guard() -> &'static Mutex<()> {
        static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
        GUARD.get_or_init(|| Mutex::new(()))
    }

    /// Install the hook on `dir`, run `body`, then restore the default
    /// hook. `take_hook` resets the hook to the standard one.
    fn with_hook<F: FnOnce()>(dir: PathBuf, body: F) {
        let lock = match hook_guard().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        install_at(dir, "companyos-test-binary");
        body();
        let _ = std::panic::take_hook();
        drop(lock);
    }

    fn traces_in(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                found.push(entry.path());
            }
        }
        found.sort();
        found
    }

    #[test]
    fn nominal_writes_a_trace_naming_the_artifact() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("crashes");

        with_hook(dir.clone(), || {
            let outcome = std::panic::catch_unwind(|| {
                let _scope = ArtifactScope::enter("projects/company-os/design-docs/fake.yml");
                panic!("boom from the indexing path");
            });
            assert!(outcome.is_err(), "the panic must still propagate");
        });

        let traces = traces_in(&dir);
        assert_eq!(traces.len(), 1, "exactly one trace expected: {traces:?}");
        let body = std::fs::read_to_string(&traces[0]).expect("trace readable");
        assert!(body.contains("=== companyos crash trace ==="));
        assert!(body.contains("boom from the indexing path"));
        assert!(body.contains("binary: companyos-test-binary"));
        assert!(body.contains(&format!("pid: {}", std::process::id())));
        assert!(
            body.contains("artifact: projects/company-os/design-docs/fake.yml"),
            "the artifact scope must appear in the trace"
        );
        assert!(body.contains("--- backtrace ---"));
        assert!(
            body.lines().count() > 12,
            "a captured backtrace should make the trace substantial"
        );
        assert!(
            traces[0].to_string_lossy().ends_with(".txt"),
            "a trace must never carry a YAML extension, it would wake the file watcher"
        );
    }

    #[test]
    fn negative_unwritable_directory_never_panics_and_never_blocks() {
        // Neither creatable nor writable: /proc rejects directory creation.
        let dir = PathBuf::from("/proc/companyos-crash-trace-must-not-exist/crashes");
        assert!(!dir.exists(), "precondition: the directory cannot exist");

        with_hook(dir.clone(), || {
            let outcome = std::panic::catch_unwind(|| panic!("boom with nowhere to write"));
            assert!(
                outcome.is_err(),
                "the panic must propagate exactly as it would without the hook"
            );
        });

        // Reaching this line IS the assertion: the hook swallowed the I/O
        // failure instead of panicking inside a panic.
        assert!(!dir.exists(), "nothing must have been created");
    }

    #[test]
    fn edge_two_panics_on_one_thread_produce_two_distinct_traces() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("crashes");

        with_hook(dir.clone(), || {
            let first = std::panic::catch_unwind(|| panic!("first"));
            let second = std::panic::catch_unwind(|| panic!("second"));
            assert!(first.is_err() && second.is_err());
        });

        let traces = traces_in(&dir);
        assert_eq!(traces.len(), 2, "no file name collision: {traces:?}");
        let second_body =
            std::fs::read_to_string(&traces[1]).unwrap_or_else(|_| String::from("<unreadable>"));
        assert!(
            second_body.contains("reentered_hook: true"),
            "the second entry on the same thread must be flagged"
        );
    }

    #[test]
    fn artifact_scope_clears_itself_on_drop() {
        {
            let _scope = ArtifactScope::enter("company/rfcs/fake.yml");
            let seen = ARTIFACT_SCOPE
                .try_with(|slot| slot.borrow().clone())
                .unwrap_or(None);
            assert_eq!(seen.as_deref(), Some("company/rfcs/fake.yml"));
        }
        let after = ARTIFACT_SCOPE
            .try_with(|slot| slot.borrow().clone())
            .unwrap_or(None);
        assert_eq!(after, None, "the guard must clear the scope");
    }
}
