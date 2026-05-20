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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RoadmapItemRef {
    Project { project_slug: String },
    Rfc { id: String },
    External { label: String },
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
            ref_: RoadmapItemRef::External {
                label: format!("ext-{key}"),
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
        let ext = RoadmapItemRef::External { label: "l".into() };
        let pv = serde_json::to_value(&project).unwrap();
        assert_eq!(pv["type"].as_str(), Some("project"));
        assert_eq!(pv["project_slug"].as_str(), Some("p"));
        let rv = serde_json::to_value(&rfc).unwrap();
        assert_eq!(rv["type"].as_str(), Some("rfc"));
        let ev = serde_json::to_value(&ext).unwrap();
        assert_eq!(ev["type"].as_str(), Some("external"));
    }
}
