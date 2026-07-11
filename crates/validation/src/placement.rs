//! Placement validation: enforces the kind → path convention documented in
//! shared-rules `file_placement`. Mechanism 9 of RFC a4ee8b6a (lot 2 process
//! hardening).
//!
//! The rule maps every artifact `kind` to the directory it must live under:
//!   - Project kinds live DIRECTLY under `projects/<slug>/<folder>/` (no
//!     nested sub-folder).
//!   - Global kinds live under a fixed `company/<folder>/`.
//!
//! One exact exception: `company/config/shared-rules.yml` is a `project-config`
//! that legitimately lives in `company/config/` (CompanyOS's own system config).
//!
//! Filename validity is NOT this module's concern — that stays with
//! `check-artifact-naming.sh` (separation of responsibilities unchanged).

use std::path::{Component, Path};

use companyos_config::ArtifactKind;

/// The exact path (relative to repo root) of the sole project-config that
/// legitimately lives outside `projects/`.
const SHARED_RULES_EXCEPTION: &str = "company/config/shared-rules.yml";

/// Check that `rel_path` (relative to the repo root, using `/` separators)
/// is a valid location for an artifact of the given `kind`.
///
/// Returns `Some(message)` describing the mismatch when the placement is
/// wrong, or `None` when the placement is correct. The message names the
/// kind, the observed path, and the expected location.
pub fn check_placement(kind: ArtifactKind, rel_path: &Path) -> Option<String> {
    let rel = normalize(rel_path);

    // Exact exception: shared-rules.yml as a project-config in company/config/.
    if kind == ArtifactKind::ProjectConfig && rel == SHARED_RULES_EXCEPTION {
        return None;
    }

    match expected_location(kind) {
        Location::Project { folder } => check_project(kind, &rel, folder),
        Location::Global { dir } => check_global(kind, &rel, dir),
    }
}

enum Location {
    /// projects/<slug>/<folder>/<file>.yml
    Project { folder: &'static str },
    /// <dir>/<file>.yml  (dir already ends with '/')
    Global { dir: &'static str },
}

/// Exhaustive table over the 13 kinds.
fn expected_location(kind: ArtifactKind) -> Location {
    match kind {
        // Project kinds.
        ArtifactKind::TaskRequest => Location::Project {
            folder: "task-requests",
        },
        ArtifactKind::DesignDoc => Location::Project {
            folder: "design-docs",
        },
        ArtifactKind::ImplementationPlan => Location::Project {
            folder: "implementation-plans",
        },
        ArtifactKind::ReviewReport => Location::Project {
            folder: "review-reports",
        },
        ArtifactKind::DiagnosticReport => Location::Project {
            folder: "diagnostic-reports",
        },
        ArtifactKind::ProjectConfig => Location::Project { folder: "config" },
        // Global kinds.
        ArtifactKind::LessonLearned => Location::Global {
            dir: "company/lessons/",
        },
        ArtifactKind::Persona => Location::Global {
            dir: "company/personas/",
        },
        ArtifactKind::Rfc => Location::Global {
            dir: "company/rfcs/",
        },
        ArtifactKind::Roadmap => Location::Global {
            dir: "company/roadmaps/",
        },
        ArtifactKind::FlowControl
        | ArtifactKind::ReviewProtocol
        | ArtifactKind::HumanReviewTriggers => Location::Global {
            dir: "company/config/",
        },
    }
}

/// Validate a project-kind path: projects/<slug>/<folder>/<file>.yml, where
/// <slug> is a single non-empty segment (no slash) and the file sits DIRECTLY
/// under <folder> (no further nesting).
fn check_project(kind: ArtifactKind, rel: &str, folder: &str) -> Option<String> {
    let segments: Vec<&str> = rel.split('/').collect();
    // Expect exactly: ["projects", <slug>, <folder>, <file>.yml]
    let ok = segments.len() == 4
        && segments[0] == "projects"
        && !segments[1].is_empty()
        && segments[2] == folder
        && !segments[3].is_empty();
    if ok {
        None
    } else {
        Some(format!(
            "placement error: {} at '{}' — expected projects/<slug>/{}/<file>.yml",
            kind.as_str(),
            rel,
            folder
        ))
    }
}

/// Validate a global-kind path: file DIRECTLY under `dir` (which ends with '/').
fn check_global(kind: ArtifactKind, rel: &str, dir: &str) -> Option<String> {
    let ok = rel
        .strip_prefix(dir)
        .map(|tail| !tail.is_empty() && !tail.contains('/'))
        .unwrap_or(false);
    if ok {
        None
    } else {
        Some(format!(
            "placement error: {} at '{}' — expected {}<file>.yml",
            kind.as_str(),
            rel,
            dir
        ))
    }
}

/// Normalize a path to a `/`-joined relative string, dropping any leading
/// `./` and current-dir components so comparisons are stable across platforms.
fn normalize(path: &Path) -> String {
    let parts: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    // --- Nominal: each of the 13 kinds at its correct place ---

    #[test]
    fn nominal_all_kinds_correct_placement() {
        let cases = [
            (
                ArtifactKind::TaskRequest,
                "projects/company-os/task-requests/x-12345678.yml",
            ),
            (
                ArtifactKind::DesignDoc,
                "projects/company-os/design-docs/x-12345678.yml",
            ),
            (
                ArtifactKind::ImplementationPlan,
                "projects/company-os/implementation-plans/x-12345678.yml",
            ),
            (
                ArtifactKind::ReviewReport,
                "projects/company-os/review-reports/x-12345678.yml",
            ),
            (
                ArtifactKind::DiagnosticReport,
                "projects/company-os/diagnostic-reports/x-12345678.yml",
            ),
            (
                ArtifactKind::ProjectConfig,
                "projects/company-os/config/x-12345678.yml",
            ),
            (
                ArtifactKind::LessonLearned,
                "company/lessons/x-12345678.yml",
            ),
            (ArtifactKind::Persona, "company/personas/x-12345678.yml"),
            (ArtifactKind::Rfc, "company/rfcs/x-12345678.yml"),
            (ArtifactKind::Roadmap, "company/roadmaps/x-12345678.yml"),
            (ArtifactKind::FlowControl, "company/config/x-12345678.yml"),
            (
                ArtifactKind::ReviewProtocol,
                "company/config/x-12345678.yml",
            ),
            (
                ArtifactKind::HumanReviewTriggers,
                "company/config/x-12345678.yml",
            ),
        ];
        for (kind, path) in cases {
            assert!(
                check_placement(kind, &p(path)).is_none(),
                "expected {} at '{}' to be valid",
                kind.as_str(),
                path
            );
        }
    }

    #[test]
    fn nominal_project_config_in_projects() {
        assert!(
            check_placement(
                ArtifactKind::ProjectConfig,
                &p("projects/puissance4-verus/config/x-12345678.yml")
            )
            .is_none()
        );
    }

    // --- Negative: wrong directory for the kind ---

    #[test]
    fn negative_design_doc_in_lessons() {
        assert!(
            check_placement(
                ArtifactKind::DesignDoc,
                &p("company/lessons/x-12345678.yml")
            )
            .is_some()
        );
    }

    #[test]
    fn negative_rfc_in_projects() {
        assert!(
            check_placement(
                ArtifactKind::Rfc,
                &p("projects/company-os/rfcs/x-12345678.yml")
            )
            .is_some()
        );
    }

    #[test]
    fn negative_lesson_in_rfcs() {
        assert!(
            check_placement(
                ArtifactKind::LessonLearned,
                &p("company/rfcs/x-12345678.yml")
            )
            .is_some()
        );
    }

    // --- Edge cases ---

    #[test]
    fn edge_shared_rules_exception_accepted() {
        assert!(
            check_placement(
                ArtifactKind::ProjectConfig,
                &p("company/config/shared-rules.yml")
            )
            .is_none()
        );
    }

    #[test]
    fn edge_nested_subfolder_under_kind_folder_rejected() {
        // A file nested one level deeper than the kind folder must fail.
        assert!(
            check_placement(
                ArtifactKind::DesignDoc,
                &p("projects/company-os/design-docs/sub/x-12345678.yml")
            )
            .is_some()
        );
    }

    #[test]
    fn edge_global_nested_rejected() {
        assert!(
            check_placement(
                ArtifactKind::LessonLearned,
                &p("company/lessons/sub/x-12345678.yml")
            )
            .is_some()
        );
    }

    #[test]
    fn edge_empty_slug_rejected() {
        // projects//design-docs/... has an empty slug segment.
        assert!(
            check_placement(
                ArtifactKind::DesignDoc,
                &p("projects//design-docs/x-12345678.yml")
            )
            .is_some()
        );
    }

    #[test]
    fn edge_leading_dot_slash_normalized() {
        // ./company/rfcs/x.yml should normalize and be accepted.
        assert!(check_placement(ArtifactKind::Rfc, &p("./company/rfcs/x-12345678.yml")).is_none());
    }

    #[test]
    fn edge_shared_rules_path_for_wrong_kind_still_checked() {
        // The exception is scoped to project-config only. A rfc at that exact
        // path is NOT the exception and must be judged by the rfc rule.
        assert!(
            check_placement(ArtifactKind::Rfc, &p("company/config/shared-rules.yml")).is_some()
        );
    }
}
