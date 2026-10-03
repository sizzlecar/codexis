use super::{dimension, item, small_node, unique_evidence, Facts};
use crate::model::{Node, Snapshot};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub(super) const DEPTH_LIMIT: usize = 3;
pub(super) const NODE_LIMIT: usize = 60;
pub(super) const EDGE_LIMIT: usize = 120;

pub(super) fn build(snapshot: &Snapshot, facts: &Facts) -> Value {
    let units: BTreeMap<_, _> = snapshot
        .project
        .packages
        .iter()
        .flat_map(|p| &p.units)
        .map(|u| (u.id.as_str(), u))
        .collect();
    let mut entries: Vec<(&Node, &str)> = Vec::new();
    let incoming: BTreeSet<_> = facts
        .edges
        .iter()
        .filter(|e| e.kind == "calls")
        .filter_map(|e| e.target.as_deref())
        .collect();
    for node in &facts.nodes {
        if node.is_test {
            continue;
        }
        let kind = node.attributes.get("entry_kind").map(String::as_str);
        if matches!(
            kind,
            Some("python_main" | "python_script" | "http_route" | "bin")
        ) {
            entries.push((node, kind.unwrap()));
        } else if node.kind == "function"
            && node.name == "main"
            && units.get(node.unit.as_str()).is_some_and(|u| {
                u.kind == "bin"
                    && node.qualified_name == format!("{}::main", u.name.replace('-', "_"))
            })
        {
            entries.push((node, "program"));
        } else if node
            .attributes
            .get("rust.route_bindings")
            .is_some_and(|raw| raw != "[]")
        {
            entries.push((node, "route_registration_syntax"));
        } else if node.kind == "function"
            && (node.visibility.starts_with("pub") || node.visibility == "public")
            && !incoming.contains(node.id.as_str())
        {
            entries.push((node, "public_api_candidate"));
        }
    }
    if entries.is_empty() {
        // A source starting point is useful for private Python scripts and small
        // binaries too. Its role is labeled as a candidate, not a runtime entry.
        for node in facts
            .nodes
            .iter()
            .filter(|n| n.kind == "function" && !n.is_test && !incoming.contains(n.id.as_str()))
            .take(20)
        {
            entries.push((node, "source_start_candidate"));
        }
    }
    entries.sort_by_key(|(n, kind)| {
        (
            entry_rank(kind),
            n.evidence.path.clone(),
            n.evidence.start_byte,
            n.id.clone(),
        )
    });
    entries.retain(|(node, _)| facts.scope.is_none() || facts.scope_node_ids.contains(&node.id));
    let total = entries.len();
    let items = entries
        .into_iter()
        .take(super::ITEM_LIMIT)
        .map(|(n, kind)| scenario(n, kind, facts))
        .collect();
    let mut result = dimension(
        "behavior",
        crate::localize!("行为与控制", "Behavior and control"),
        crate::localize!("从程序入口、路由声明和公开接口查看有界调用场景与未知边界。", "Explore bounded call scenarios and unknown boundaries from program entries, route declarations, and public APIs."),
        items,
        vec![
            crate::localize!("调用邻域不代表运行时顺序或所有分支；动态派发、宏展开、反射和外部调用可能缺失。", "Call neighborhoods do not establish runtime order or all branches; dynamic dispatch, macro expansion, reflection, and external calls may be absent."),
            crate::localize!("入口候选和路由绑定只描述已有语法；不保证框架实际注册或处理请求。", "Entry candidates and route bindings describe syntax only; they do not guarantee framework registration or request handling."),
            crate::localize!("每个场景最多展开 3 层、60 个节点和 120 条边，截断在场景内标明。", "Each scenario expands at most 3 levels, 60 nodes, and 120 edges; scenario truncation is marked."),
        ],
    );
    result["total"] = json!(total);
    result["truncated"] = json!(total > super::ITEM_LIMIT);
    result
}

fn entry_rank(kind: &str) -> usize {
    match kind {
        "program" | "python_main" | "python_script" | "bin" => 0,
        "http_route" | "route_registration_syntax" => 1,
        _ => 2,
    }
}

fn scenario(seed: &Node, kind: &str, facts: &Facts) -> Value {
    let mut queue = VecDeque::from([(seed.id.as_str(), 0)]);
    let mut seen = BTreeSet::from([seed.id.as_str()]);
    let mut expanded = BTreeSet::new();
    let mut nodes = vec![small_node(seed)];
    let mut edges = Vec::new();
    let mut unknown = Vec::new();
    let mut boundary = Vec::new();
    let mut truncated = false;
    while let Some((id, depth)) = queue.pop_front() {
        if !expanded.insert(id) {
            continue;
        }
        let outgoing: Vec<_> = facts
            .outgoing(id)
            .filter(|e| matches!(e.kind.as_str(), "calls" | "macro_call"))
            .collect();
        if depth >= DEPTH_LIMIT {
            truncated |= !outgoing.is_empty();
            continue;
        }
        for edge in outgoing {
            if edges.len() >= EDGE_LIMIT {
                truncated = true;
                break;
            }
            edges.push(
                json!({"id":edge.id,"source":edge.source,"target":edge.target,
                "target_name":edge.target_name,"kind":edge.kind,"resolution":edge.resolution,
                "evidence":edge.evidence,"conditions":edge.conditions,"provider":edge.provider}),
            );
            let Some(next) = edge.target.as_deref().and_then(|id| facts.node(id)) else {
                let location = json!({"source":edge.source,"target_name":edge.target_name,"resolution":edge.resolution,
                    "evidence":edge.evidence,"reason":if edge.resolution == "external" {"external boundary"} else {"target not established in this snapshot"}});
                if edge.resolution == "external" {
                    boundary.push(location)
                } else {
                    unknown.push(location)
                }
                continue;
            };
            if seen.contains(next.id.as_str()) {
                continue;
            }
            if nodes.len() >= NODE_LIMIT {
                truncated = true;
                continue;
            }
            seen.insert(next.id.as_str());
            nodes.push(small_node(next));
            queue.push_back((next.id.as_str(), depth + 1));
        }
    }
    let node_ids: Vec<_> = nodes
        .iter()
        .filter_map(|n| n["id"].as_str())
        .map(str::to_owned)
        .collect();
    let evidence = unique_evidence(
        std::iter::once(&seed.evidence).chain(facts.outgoing(&seed.id).map(|e| &e.evidence)),
        8,
    );
    let mut result = item(
        format!("scenario:{}", seed.id),
        &seed.qualified_name,
        crate::localize!("{} 个可定位节点，{} 条调用/宏关系；{} 个未知断点，{} 个外部边界。", "{} locatable nodes, {} call/macro relations; {} unknown breakpoints, {} external boundaries.",
            nodes.len(),
            edges.len(),
            unknown.len(),
            boundary.len()
        ),
        if kind.ends_with("candidate") {
            "inference: source/public API candidate; no runtime entry claim"
        } else {
            "declared entry and indexed call neighborhood"
        },
        &evidence,
        &node_ids,
    );
    result["entry_kind"] = json!(kind);
    result["nodes"] = json!(nodes);
    result["edges"] = json!(edges);
    result["unknown_breakpoints"] = json!(unknown);
    result["external_boundaries"] = json!(boundary);
    result["truncated"] = json!(truncated);
    result["route_bindings"] = seed
        .attributes
        .get("rust.route_bindings")
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or_else(|| json!([]));
    result["async_declared"] = json!(
        seed.attributes
            .get("python.kind")
            .is_some_and(|k| k == "async_function")
            || seed.signature.split_whitespace().any(|p| p == "async")
    );
    result["return_type"] = json!(seed
        .attributes
        .get("rust.return_type")
        .or_else(|| seed.attributes.get("return_type")));
    result
}
