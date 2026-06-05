//! Types and parsing helpers for the roadmap MCP tools (list_roadmaps,
//! summarize_roadmap). Lives in its own module because it bundles 5 metier
//! types + parsing/grouping helpers; keeping it out of `types.rs` (which is
//! reserved for DB primitives) preserves cohesion.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// --- Public selector for summarize_roadmap ---

/// Selects which roadmap to summarize. Built by the MCP tool layer from
/// mutually exclusive `id` / `domain` parameters.
#[derive(Debug, Clone)]
pub enum RoadmapSelector {
    /// Resolve by metadata.id (UUID).
    ById(String),
    /// Resolve by spec.domain; must match exactly ONE active roadmap.
    ByDomain(String),
}

// --- Item types (typed parsing of spec.items[]) ---

/// A single roadmap item, typed from the YAML.
///
/// `target_date`, `depends_on`, `rationale` are optional per the schema.
/// `timeframe` and `status` are kept as `String` because they are used as
/// keys in the output `BTreeMap`s; an extra enum layer would not add value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoadmapItem {
    pub key: String,
    pub title: String,
    #[serde(rename = "ref")]
    pub ref_: RoadmapItemRef,
    pub timeframe: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
}

/// A roadmap item reference. Tagged enum matching the JSON Schema `oneOf`.
///
/// `category` on `Loose` is kept as `String` (not a dedicated Rust enum): the
/// strict enum is enforced by the JSON Schema at validation time, consistent
/// with the existing choice of keeping `status`/`timeframe` as `String`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RoadmapItemRef {
    Project { project_slug: String },
    Rfc { id: String },
    Loose { category: String, label: String },
}

/// Resolved status of the source artifact referenced by a roadmap item.
///
/// Built by the engine (which performs the FS/DB lookups) and fed to the PURE
/// [`effective_status`] mapping function. Keeping the lookups out of the
/// mapping makes it trivially unit-testable on each combination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceStatus {
    /// The referenced RFC was found; carries its raw `metadata.status`.
    Rfc(String),
    /// The referenced project: `true` if `projects/<slug>/` exists.
    Project(bool),
    /// No source could be resolved (RFC not indexed / unreadable / orphan).
    None,
}

/// PURE mapping from (item ref, YAML status, resolved source status) to the
/// effective status displayed by the roadmap tools, plus a `drift` flag
/// signalling that the YAML status diverged from the computed one.
///
/// Invariants (order is non-negotiable):
/// 1. `blocked` short-circuits BEFORE any mapping (absolute PM right).
/// 2. `loose` items keep their manual YAML status (no source, no drift).
/// 3. `rfc` / `project` items are computed from the source; drift = mapped != yaml.
pub fn effective_status(
    item_ref: &RoadmapItemRef,
    status_yaml: &str,
    source: &SourceStatus,
) -> (String, bool) {
    // 1. Short-circuit: the PM's explicit `blocked` always wins.
    if status_yaml == "blocked" {
        return ("blocked".to_string(), false);
    }
    match item_ref {
        // 2. Loose items have no source artifact: manual status respected.
        RoadmapItemRef::Loose { .. } => (status_yaml.to_string(), false),
        // 3. RFC / project items are computed from the source.
        RoadmapItemRef::Rfc { .. } => {
            let mapped = map_rfc_status(source);
            let drift = mapped != status_yaml;
            (mapped, drift)
        }
        RoadmapItemRef::Project { .. } => {
            let mapped = map_project_status(source);
            let drift = mapped != status_yaml;
            (mapped, drift)
        }
    }
}

/// Deterministic mapping table from an RFC's `metadata.status` to a roadmap
/// item status. Source absent or status outside the known enum falls back to
/// `planned` (neutral). No best-effort "round open" refinement (the design-doc
/// OQ2 was resolved by abandoning it: review_rounds/file_path matching is
/// fragile, and the mapping is correct without it).
pub fn map_rfc_status(source: &SourceStatus) -> String {
    match source {
        SourceStatus::Rfc(s) => match s.as_str() {
            "draft" => "planned",
            "approved" => "in_progress",
            "implemented" => "done",
            "rejected" => "cancelled",
            _ => "planned",
        },
        _ => "planned",
    }
    .to_string()
}

/// Deterministic binary mapping for project refs: the `projects/<slug>/`
/// directory exists -> `in_progress`, absent (or wrong source variant) ->
/// `planned`. No activity deduction (design decision 5).
pub fn map_project_status(source: &SourceStatus) -> String {
    match source {
        SourceStatus::Project(true) => "in_progress",
        _ => "planned",
    }
    .to_string()
}

/// A drift warning emitted by `summarize_roadmap` when an item's YAML status
/// diverges from the computed status. Blocked and loose items are excluded by
/// construction (they never drift). Non blocking: the displayed status remains
/// the computed one; the warning flags a YAML written outside the pipeline
/// (manual edit or stale pre-auto-sync roadmap) for the PM to clean up.
#[derive(Debug, Clone, Serialize)]
pub struct DriftWarning {
    pub key: String,
    pub yaml_status: String,
    pub mapped_status: String,
    #[serde(rename = "ref")]
    pub ref_: RoadmapItemRef,
    /// Optional cause when the source could not be resolved (RFC not indexed /
    /// unreadable). `None` for an ordinary status divergence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
}

// --- list_roadmaps output ---

/// Lightweight entry returned by `list_roadmaps`. No items detail, just the
/// counters needed to decide where to look first.
#[derive(Debug, Clone, Serialize)]
pub struct RoadmapListEntry {
    pub id: String,
    pub title: String,
    pub domain: String,
    pub status: String,
    pub items_count: usize,
    pub blocked_count: usize,
    pub in_progress_count: usize,
    pub file_path: String,
}

// --- summarize_roadmap output ---

/// Full summary of a single roadmap: narrative + items grouped both ways +
/// blocked items highlighted.
#[derive(Debug, Clone, Serialize)]
pub struct RoadmapSummary {
    pub roadmap: RoadmapHeader,
    pub summary: RoadmapCounters,
    pub blocked_items: Vec<RoadmapItem>,
    pub items_by_timeframe: BTreeMap<String, Vec<RoadmapItem>>,
    pub items_by_status: BTreeMap<String, Vec<RoadmapItem>>,
    /// Items whose YAML status diverged from the computed status (excludes
    /// blocked and loose items by construction). Filet de sécurité for the PM.
    pub drift_warnings: Vec<DriftWarning>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoadmapHeader {
    pub id: String,
    pub title: String,
    pub domain: String,
    pub status: String,
    pub narrative: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct RoadmapCounters {
    pub items_total: usize,
    pub blocked_count: usize,
    pub by_status: BTreeMap<String, usize>,
    pub by_timeframe: BTreeMap<String, usize>,
}

// --- Intermediate YAML parsing structures (crate-private) ---

#[derive(Debug, Deserialize)]
pub(crate) struct RoadmapYaml {
    pub metadata: RoadmapYamlMetadata,
    pub spec: RoadmapYamlSpec,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RoadmapYamlMetadata {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RoadmapYamlSpec {
    pub domain: String,
    pub status: String,
    pub narrative: String,
    pub items: Vec<RoadmapItem>,
}

/// Parse a roadmap YAML content into a typed structure.
pub(crate) fn parse_roadmap_yaml(content: &str) -> Result<RoadmapYaml, serde_yaml::Error> {
    serde_yaml::from_str(content)
}

/// Count items by their `status` field.
pub(crate) fn count_by_status(items: &[RoadmapItem]) -> BTreeMap<String, usize> {
    let mut map = BTreeMap::new();
    for item in items {
        *map.entry(item.status.clone()).or_insert(0) += 1;
    }
    map
}

/// Count items by their `timeframe` field.
pub(crate) fn count_by_timeframe(items: &[RoadmapItem]) -> BTreeMap<String, usize> {
    let mut map = BTreeMap::new();
    for item in items {
        *map.entry(item.timeframe.clone()).or_insert(0) += 1;
    }
    map
}

/// Group items by an arbitrary key extractor. Insertion order within each
/// group preserves the YAML order.
pub(crate) fn group_items_by<F>(items: &[RoadmapItem], key: F) -> BTreeMap<String, Vec<RoadmapItem>>
where
    F: Fn(&RoadmapItem) -> String,
{
    let mut map: BTreeMap<String, Vec<RoadmapItem>> = BTreeMap::new();
    for item in items {
        map.entry(key(item)).or_default().push(item.clone());
    }
    map
}

/// Canonical list of status values (matches the JSON Schema enum).
/// Used to zero-initialize maps so the output JSON shape is stable.
pub(crate) const ALL_STATUSES: &[&str] =
    &["planned", "in_progress", "done", "blocked", "cancelled"];

/// Canonical list of timeframe values (matches the JSON Schema enum).
pub(crate) const ALL_TIMEFRAMES: &[&str] = &["past", "present", "future"];

#[cfg(test)]
mod tests {
    use super::*;

    fn make_item(key: &str, status: &str, timeframe: &str) -> RoadmapItem {
        RoadmapItem {
            key: key.into(),
            title: format!("Item {key}"),
            ref_: RoadmapItemRef::Loose {
                category: "idea".into(),
                label: format!("loose-{key}"),
            },
            timeframe: timeframe.into(),
            status: status.into(),
            target_date: None,
            depends_on: None,
            rationale: None,
        }
    }

    #[test]
    fn test_parse_roadmap_yaml_minimal() {
        let yaml = r#"
api_version: companyos/v1
kind: roadmap
metadata:
  id: 11111111-1111-1111-1111-111111111111
  title: Minimal roadmap
  author: pm
  created_at: "2026-05-20"
  description: A minimal test roadmap
  tags: [test]
spec:
  domain: test-domain
  status: active
  narrative: A small narrative for testing.
  items:
    - key: alpha
      title: First item
      ref:
        type: project
        project_slug: my-project
      timeframe: present
      status: in_progress
"#;
        let parsed = parse_roadmap_yaml(yaml).expect("parse ok");
        assert_eq!(parsed.metadata.id, "11111111-1111-1111-1111-111111111111");
        assert_eq!(parsed.spec.domain, "test-domain");
        assert_eq!(parsed.spec.items.len(), 1);
        assert_eq!(parsed.spec.items[0].key, "alpha");
        match &parsed.spec.items[0].ref_ {
            RoadmapItemRef::Project { project_slug } => assert_eq!(project_slug, "my-project"),
            _ => panic!("expected project ref"),
        }
    }

    #[test]
    fn test_count_helpers() {
        let items = vec![
            make_item("a", "done", "past"),
            make_item("b", "done", "past"),
            make_item("c", "blocked", "present"),
            make_item("d", "in_progress", "present"),
        ];
        let by_status = count_by_status(&items);
        assert_eq!(by_status.get("done"), Some(&2));
        assert_eq!(by_status.get("blocked"), Some(&1));
        assert_eq!(by_status.get("in_progress"), Some(&1));

        let by_tf = count_by_timeframe(&items);
        assert_eq!(by_tf.get("past"), Some(&2));
        assert_eq!(by_tf.get("present"), Some(&2));
    }

    #[test]
    fn test_group_items_by_status() {
        let items = vec![
            make_item("a", "done", "past"),
            make_item("b", "blocked", "present"),
            make_item("c", "blocked", "future"),
        ];
        let grouped = group_items_by(&items, |i| i.status.clone());
        assert_eq!(grouped.get("done").map(Vec::len), Some(1));
        assert_eq!(grouped.get("blocked").map(Vec::len), Some(2));
    }

    /// Validates the FORM of the JSON returned by summarize_roadmap.
    /// Per the RFC section 7.b: "vérifier la forme du JSON retourné, pas de
    /// tester rmcp lui-même".
    #[test]
    fn test_roadmap_summary_serializes_to_expected_json_shape() {
        let items = vec![
            make_item("a", "done", "past"),
            make_item("b", "blocked", "present"),
        ];
        let mut by_status = count_by_status(&items);
        let mut by_timeframe = count_by_timeframe(&items);
        let mut items_by_status = group_items_by(&items, |i| i.status.clone());
        let mut items_by_timeframe = group_items_by(&items, |i| i.timeframe.clone());
        for s in ALL_STATUSES {
            by_status.entry((*s).into()).or_insert(0);
            items_by_status.entry((*s).into()).or_default();
        }
        for t in ALL_TIMEFRAMES {
            by_timeframe.entry((*t).into()).or_insert(0);
            items_by_timeframe.entry((*t).into()).or_default();
        }
        let blocked: Vec<_> = items
            .iter()
            .filter(|i| i.status == "blocked")
            .cloned()
            .collect();

        let summary = RoadmapSummary {
            roadmap: RoadmapHeader {
                id: "rid".into(),
                title: "T".into(),
                domain: "dom".into(),
                status: "active".into(),
                narrative: "story".into(),
            },
            summary: RoadmapCounters {
                items_total: items.len(),
                blocked_count: blocked.len(),
                by_status,
                by_timeframe,
            },
            blocked_items: blocked,
            items_by_timeframe,
            items_by_status,
            drift_warnings: Vec::new(),
        };

        let json = serde_json::to_value(&summary).expect("serialize");
        let obj = json.as_object().expect("object");

        // Top-level keys
        for k in [
            "roadmap",
            "summary",
            "blocked_items",
            "items_by_timeframe",
            "items_by_status",
            "drift_warnings",
        ] {
            assert!(obj.contains_key(k), "missing top-level key '{k}'");
        }

        // roadmap header keys
        let rm = obj["roadmap"].as_object().unwrap();
        for k in ["id", "title", "domain", "status", "narrative"] {
            assert!(rm.contains_key(k), "missing roadmap.{k}");
        }

        // summary keys
        let sm = obj["summary"].as_object().unwrap();
        for k in ["items_total", "blocked_count", "by_status", "by_timeframe"] {
            assert!(sm.contains_key(k), "missing summary.{k}");
        }

        // by_status: 5 canonical keys present (zero-init)
        let by_st = sm["by_status"].as_object().unwrap();
        for s in ALL_STATUSES {
            assert!(by_st.contains_key(*s), "missing by_status.{s}");
        }

        // by_timeframe: 3 canonical keys present
        let by_tf = sm["by_timeframe"].as_object().unwrap();
        for t in ALL_TIMEFRAMES {
            assert!(by_tf.contains_key(*t), "missing by_timeframe.{t}");
        }

        // items_by_status: same 5 canonical keys
        let ibs = obj["items_by_status"].as_object().unwrap();
        for s in ALL_STATUSES {
            assert!(ibs.contains_key(*s), "missing items_by_status.{s}");
        }
        // items_by_timeframe: 3 keys
        let ibt = obj["items_by_timeframe"].as_object().unwrap();
        for t in ALL_TIMEFRAMES {
            assert!(ibt.contains_key(*t), "missing items_by_timeframe.{t}");
        }

        // blocked_items contains exactly the items with status=blocked
        let bi = obj["blocked_items"].as_array().unwrap();
        assert_eq!(bi.len(), 1);
        assert_eq!(bi[0]["key"].as_str(), Some("b"));
    }

    #[test]
    fn test_roadmap_item_ref_serializes_with_type_tag() {
        let project = RoadmapItemRef::Project {
            project_slug: "p".into(),
        };
        let rfc = RoadmapItemRef::Rfc { id: "u".into() };
        let loose = RoadmapItemRef::Loose {
            category: "hotfix".into(),
            label: "l".into(),
        };
        let pv = serde_json::to_value(&project).unwrap();
        assert_eq!(pv["type"].as_str(), Some("project"));
        assert_eq!(pv["project_slug"].as_str(), Some("p"));
        let rv = serde_json::to_value(&rfc).unwrap();
        assert_eq!(rv["type"].as_str(), Some("rfc"));
        let lv = serde_json::to_value(&loose).unwrap();
        assert_eq!(lv["type"].as_str(), Some("loose"));
        assert_eq!(lv["category"].as_str(), Some("hotfix"));
        assert_eq!(lv["label"].as_str(), Some("l"));
    }

    #[test]
    fn test_roadmap_item_ref_loose_roundtrips() {
        // type: loose must deserialize back into the Loose variant.
        let yaml = "type: loose\ncategory: observation\nlabel: a constat\n";
        let parsed: RoadmapItemRef = serde_yaml::from_str(yaml).expect("parse loose");
        match parsed {
            RoadmapItemRef::Loose { category, label } => {
                assert_eq!(category, "observation");
                assert_eq!(label, "a constat");
            }
            _ => panic!("expected loose variant"),
        }
    }

    // --- effective_status / mapping unit tests (pure, exhaustive) ---

    #[test]
    fn test_map_rfc_status_table() {
        assert_eq!(
            map_rfc_status(&SourceStatus::Rfc("draft".into())),
            "planned"
        );
        assert_eq!(
            map_rfc_status(&SourceStatus::Rfc("approved".into())),
            "in_progress"
        );
        assert_eq!(
            map_rfc_status(&SourceStatus::Rfc("implemented".into())),
            "done"
        );
        assert_eq!(
            map_rfc_status(&SourceStatus::Rfc("rejected".into())),
            "cancelled"
        );
        // Status outside the known enum -> neutral fallback.
        assert_eq!(
            map_rfc_status(&SourceStatus::Rfc("weird".into())),
            "planned"
        );
        // Source absent -> neutral fallback.
        assert_eq!(map_rfc_status(&SourceStatus::None), "planned");
        // Wrong variant -> neutral fallback.
        assert_eq!(map_rfc_status(&SourceStatus::Project(true)), "planned");
    }

    #[test]
    fn test_map_project_status_binary() {
        assert_eq!(
            map_project_status(&SourceStatus::Project(true)),
            "in_progress"
        );
        assert_eq!(map_project_status(&SourceStatus::Project(false)), "planned");
        assert_eq!(map_project_status(&SourceStatus::None), "planned");
        // Wrong variant -> planned.
        assert_eq!(
            map_project_status(&SourceStatus::Rfc("implemented".into())),
            "planned"
        );
    }

    #[test]
    fn test_effective_status_blocked_short_circuits() {
        // blocked wins regardless of ref type or source status.
        let rfc = RoadmapItemRef::Rfc { id: "x".into() };
        let (st, drift) =
            effective_status(&rfc, "blocked", &SourceStatus::Rfc("implemented".into()));
        assert_eq!(st, "blocked");
        assert!(!drift, "blocked never drifts");

        let proj = RoadmapItemRef::Project {
            project_slug: "p".into(),
        };
        let (st, drift) = effective_status(&proj, "blocked", &SourceStatus::Project(true));
        assert_eq!(st, "blocked");
        assert!(!drift);

        let loose = RoadmapItemRef::Loose {
            category: "idea".into(),
            label: "l".into(),
        };
        let (st, drift) = effective_status(&loose, "blocked", &SourceStatus::None);
        assert_eq!(st, "blocked");
        assert!(!drift);
    }

    #[test]
    fn test_effective_status_loose_respects_yaml() {
        let loose = RoadmapItemRef::Loose {
            category: "exploration".into(),
            label: "l".into(),
        };
        for yaml in ["planned", "in_progress", "done", "cancelled"] {
            let (st, drift) = effective_status(&loose, yaml, &SourceStatus::None);
            assert_eq!(st, yaml, "loose respects manual status");
            assert!(!drift, "loose never drifts");
        }
    }

    #[test]
    fn test_effective_status_rfc_computes_and_flags_drift() {
        let rfc = RoadmapItemRef::Rfc { id: "x".into() };
        // YAML matches computed -> no drift.
        let (st, drift) = effective_status(&rfc, "done", &SourceStatus::Rfc("implemented".into()));
        assert_eq!(st, "done");
        assert!(!drift);
        // YAML diverges from computed -> drift.
        let (st, drift) = effective_status(&rfc, "done", &SourceStatus::Rfc("approved".into()));
        assert_eq!(st, "in_progress");
        assert!(drift, "approved RFC with done YAML drifts");
    }

    #[test]
    fn test_effective_status_project_computes_and_flags_drift() {
        let proj = RoadmapItemRef::Project {
            project_slug: "p".into(),
        };
        let (st, drift) = effective_status(&proj, "in_progress", &SourceStatus::Project(true));
        assert_eq!(st, "in_progress");
        assert!(!drift);
        let (st, drift) = effective_status(&proj, "done", &SourceStatus::Project(false));
        assert_eq!(st, "planned");
        assert!(drift);
    }
}
