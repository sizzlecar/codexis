//! Eight evidence-backed views of one immutable project snapshot.
//!
//! Explanations are deliberately separated from facts. In particular, call
//! neighborhoods are not execution traces and an absent test relation is not
//! proof that a behavior is untested.
use crate::{
    index::Index,
    model::{identity, Edge, Evidence, Node, Snapshot},
};
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

mod architecture;
mod artifacts;
mod behavior;
mod facets;

pub const DIMENSION_IDS: [&str; 8] = [
    "intent",
    "architecture",
    "behavior",
    "data",
    "runtime",
    "changes",
    "verification",
    "knowledge",
];
pub const ITEM_LIMIT: usize = 100;
const CACHE_VERSION: u32 = 4;

pub(super) struct Facts {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    by_id: BTreeMap<String, usize>,
    outgoing: BTreeMap<String, Vec<usize>>,
    scope: Option<String>,
    scope_node_ids: BTreeSet<String>,
    python_namespaces: BTreeMap<String, String>,
}

impl Facts {
    fn load(index: &Index, snapshot: &Snapshot, scope: Option<&str>) -> Result<Self> {
        let nodes: Vec<_> = index
            .all_nodes(&snapshot.id)?
            .into_iter()
            .filter(|n| matches!(n.language.as_str(), "rust" | "python"))
            .collect();
        let by_id: BTreeMap<_, _> = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.clone(), i))
            .collect();
        let scope_packages: BTreeSet<_> = snapshot
            .project
            .packages
            .iter()
            .filter(|p| {
                scope.is_some_and(|q| {
                    p.name.contains(q)
                        || (!p.root.is_empty() && p.root.contains(q))
                        || p.id.contains(q)
                })
            })
            .map(|p| p.id.as_str())
            .collect();
        let mut scope_node_ids = BTreeSet::new();
        if let Some(query) = scope {
            for node in nodes.iter().filter(|n| {
                n.qualified_name.contains(query)
                    || n.evidence.path.contains(query)
                    || n.id.contains(query)
                    || scope_packages.contains(n.package.as_str())
            }) {
                scope_node_ids.insert(node.id.clone());
                let mut parent = node.parent.as_deref();
                let mut seen = BTreeSet::new();
                while let Some(id) = parent {
                    if !seen.insert(id) {
                        break;
                    }
                    let Some(ancestor) = by_id.get(id).map(|i| &nodes[*i]) else {
                        break;
                    };
                    scope_node_ids.insert(ancestor.id.clone());
                    if ancestor.kind == "module" {
                        break;
                    }
                    parent = ancestor.parent.as_deref();
                }
            }
        }
        let mut namespaces = BTreeMap::<String, (BTreeSet<String>, bool)>::new();
        for node in nodes
            .iter()
            .filter(|n| n.language == "python" && n.kind == "module" && !n.is_test)
        {
            let entry = namespaces.entry(node.package.clone()).or_default();
            entry.0.insert(
                node.qualified_name
                    .split('.')
                    .next()
                    .unwrap_or(&node.name)
                    .to_owned(),
            );
            entry.1 |= node.qualified_name.contains('.');
        }
        let python_namespaces = namespaces
            .into_iter()
            .filter_map(|(package, (names, dotted))| {
                if dotted && names.len() == 1 {
                    Some((package, names.into_iter().next().unwrap()))
                } else {
                    None
                }
            })
            .collect();
        let mut edges: Vec<_> = index
            .all_edges(&snapshot.id)?
            .into_iter()
            .filter(|e| by_id.contains_key(&e.source))
            .collect();
        edges.sort_by(|a, b| {
            (&a.evidence.path, a.evidence.start_byte, &a.id).cmp(&(
                &b.evidence.path,
                b.evidence.start_byte,
                &b.id,
            ))
        });
        let mut outgoing = BTreeMap::<String, Vec<usize>>::new();
        for (i, edge) in edges.iter().enumerate() {
            outgoing.entry(edge.source.clone()).or_default().push(i);
        }
        Ok(Self {
            nodes,
            edges,
            by_id,
            outgoing,
            scope: scope.map(str::to_owned),
            scope_node_ids,
            python_namespaces,
        })
    }

    fn node(&self, id: &str) -> Option<&Node> {
        self.by_id.get(id).map(|i| &self.nodes[*i])
    }

    fn outgoing<'a>(&'a self, id: &str) -> impl Iterator<Item = &'a Edge> {
        self.outgoing
            .get(id)
            .into_iter()
            .flatten()
            .map(|i| &self.edges[*i])
    }
}

pub fn build(index: &Index, snapshot: &Snapshot) -> Result<Value> {
    build_scoped(index, snapshot, None)
}

/// Filter candidate items before limits, retaining the complete relation graph
/// for call neighborhoods and test associations outside the selected scope.
pub fn build_scoped(index: &Index, snapshot: &Snapshot, scope: Option<&str>) -> Result<Value> {
    let cache_key = identity(&[
        &snapshot.id,
        "understanding scope",
        scope.unwrap_or(""),
        crate::i18n::current().tag(),
    ]);
    index.connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS understanding_cache(
            snapshot TEXT NOT NULL,version INTEGER NOT NULL,data TEXT NOT NULL,
            PRIMARY KEY(snapshot,version))",
    )?;
    let cached: Option<String> = index
        .connection
        .query_row(
            "SELECT data FROM understanding_cache WHERE snapshot=?1 AND version=?2",
            params![cache_key, CACHE_VERSION],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(data) = cached {
        if let Ok(value) = serde_json::from_str(&data) {
            return Ok(value);
        }
    }
    let result = build_uncached(index, snapshot, scope)?;
    index.connection.execute(
        "INSERT OR REPLACE INTO understanding_cache(snapshot,version,data) VALUES(?1,?2,?3)",
        params![cache_key, CACHE_VERSION, serde_json::to_string(&result)?],
    )?;
    Ok(result)
}

fn build_uncached(index: &Index, snapshot: &Snapshot, scope: Option<&str>) -> Result<Value> {
    crate::source::check_cancelled()?;
    let facts = Facts::load(index, snapshot, scope)?;
    let artifacts = artifacts::load(index, snapshot)?;
    let components = architecture::components(snapshot, &facts, &artifacts);
    let architecture = architecture::build(snapshot, &facts, &components, &artifacts);
    let dimensions = vec![
        facets::intent(snapshot, &facts, &components, &artifacts),
        architecture::dimension(&architecture, &components, &facts),
        behavior::build(snapshot, &facts),
        facets::data(&facts, &artifacts),
        facets::runtime(snapshot, &facts, &artifacts),
        dimension(
            "changes",
            crate::localize!("变更与演进", "Changes and evolution"),
            crate::localize!("选择两个版本或工作区改动，核查能力、接口和关系的变化。", "Compare two revisions or working-tree changes to review capabilities, interfaces, and relation changes."),
            Vec::new(),
            vec![crate::localize!("当前视图描述单个快照；未比较版本，不能推断改动或演进。", "This view describes one snapshot; changes and evolution require a comparison.")],
        ),
        facets::verification(&facts, &components),
        dimension(
            "knowledge",
            crate::localize!("认知与阅读", "Knowledge and reading"),
            crate::localize!("沿子系统、关键场景和证据建立理解，并保存已确认结论与疑问。", "Build understanding through subsystems, key scenarios, and evidence, then record confirmed conclusions and questions."),
            Vec::new(),
            vec![crate::localize!("未载入人工认知记录；源码结构不能代表开发者已经理解或确认。", "No human knowledge records were loaded; source structure does not establish what a developer understands or has confirmed.")],
        ),
    ];
    let matching_artifacts: Vec<_> = artifacts
        .iter()
        .filter(|a| scope.is_none_or(|q| a.path.contains(q)))
        .collect();
    let artifact_total = matching_artifacts.len();
    let artifact_items: Vec<_> = matching_artifacts
        .iter()
        .take(ITEM_LIMIT)
        .map(|a| a.value())
        .collect();
    Ok(json!({
        "schema_version":1,
        "snapshot_id":snapshot.id,
        "locale":crate::i18n::current().tag(),
        "scope":scope,
        "dimensions":dimensions,
        "architecture":architecture,
        "artifacts":artifact_items,
        "artifacts_total":artifact_total,
        "artifacts_truncated":artifact_total > ITEM_LIMIT,
        "analysis_basis":"Stored snapshot source, declarations and indexed relations; inferred candidates are explicitly labeled.",
        "limits":{"dimension_items":ITEM_LIMIT,"scenario_depth":behavior::DEPTH_LIMIT,
            "scenario_nodes":behavior::NODE_LIMIT,"scenario_edges":behavior::EDGE_LIMIT},
    }))
}

pub(super) fn dimension(
    id: &str,
    title: &str,
    summary: &str,
    items: Vec<Value>,
    limitations: Vec<&str>,
) -> Value {
    dimension_scoped(id, title, summary, items, limitations, None)
}

pub(super) fn matches_item(value: &Value, scope: Option<&str>) -> bool {
    scope.is_none_or(|query| value.to_string().contains(query))
}

pub(super) fn matches_scoped_item(value: &Value, facts: &Facts) -> bool {
    matches_item(value, facts.scope.as_deref())
        || value["node_ids"].as_array().is_some_and(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .any(|id| facts.scope_node_ids.contains(id))
        })
}

pub(super) fn dimension_scoped(
    id: &str,
    title: &str,
    summary: &str,
    mut items: Vec<Value>,
    limitations: Vec<&str>,
    facts: Option<&Facts>,
) -> Value {
    if let Some(facts) = facts {
        items.retain(|value| matches_scoped_item(value, facts));
    }
    items.sort_by(|a, b| {
        // Each builder emits stable IDs and evidence. Sort by source location
        // first so repeated snapshots have a predictable reading order.
        let key = |v: &Value| -> (String, u64, String) {
            (
                v.pointer("/evidence/0/path")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                v.pointer("/evidence/0/start_line")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                v["id"].as_str().unwrap_or("").to_owned(),
            )
        };
        key(a).cmp(&key(b))
    });
    let total = items.len();
    items.truncate(ITEM_LIMIT);
    json!({"id":id,"title":title,"summary":summary,"items":items,
        "limitations":limitations,"total":total,"truncated":total > ITEM_LIMIT,
        "next_offset":if total > ITEM_LIMIT {Some(ITEM_LIMIT)} else {None}})
}

pub(super) fn item(
    id: impl AsRef<str>,
    title: impl AsRef<str>,
    summary: impl AsRef<str>,
    basis: impl AsRef<str>,
    evidence: &[Evidence],
    node_ids: &[String],
) -> Value {
    json!({"id":id.as_ref(),"title":title.as_ref(),"summary":summary.as_ref(),
        "basis":basis.as_ref(),"evidence":evidence,"node_ids":node_ids})
}

pub(super) fn unique_evidence<'a>(
    evidence: impl Iterator<Item = &'a Evidence>,
    limit: usize,
) -> Vec<Evidence> {
    let mut seen = BTreeSet::new();
    evidence
        .filter(|e| seen.insert((e.path.as_str(), e.start_byte, e.end_byte)))
        .take(limit)
        .cloned()
        .collect()
}

pub(super) fn small_node(node: &Node) -> Value {
    json!({"id":node.id,"name":node.qualified_name,"kind":node.kind,
        "evidence":node.evidence,"is_test":node.is_test,"conditions":node.conditions})
}

pub(super) fn short(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

pub(super) fn stable_id(kind: &str, key: &str) -> String {
    format!("{kind}:{}", &identity(&[kind, key])[..24])
}
