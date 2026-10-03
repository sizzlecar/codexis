use crate::index::Index;
use crate::model::{Diagnostic, Node, Report, Snapshot};
use crate::source::{SourceProvider, WorkingTreeSource};
use anyhow::{bail, Result};
use rusqlite::params;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Instant;

mod guide;

pub fn understanding_data(index: &Index, snapshot: &Snapshot) -> Result<Value> {
    understanding_data_scoped(index, snapshot, None)
}

fn understanding_data_scoped(
    index: &Index,
    snapshot: &Snapshot,
    scope: Option<&str>,
) -> Result<Value> {
    let mut data = crate::understanding::build_scoped(index, snapshot, scope)?;
    let mut constraints = crate::constraints::evaluate(index, snapshot)?;
    if let Some(scope) = scope {
        let packages: BTreeSet<_> = snapshot
            .project
            .packages
            .iter()
            .filter(|p| p.name.contains(scope) || p.root.contains(scope) || p.id == scope)
            .map(|p| p.id.as_str())
            .collect();
        let matching_nodes: BTreeSet<_> = index
            .all_nodes(&snapshot.id)?
            .into_iter()
            .filter(|n| {
                n.id == scope
                    || n.qualified_name.contains(scope)
                    || n.evidence.path.contains(scope)
                    || packages.contains(n.package.as_str())
            })
            .map(|n| n.id)
            .collect();
        if let Some(items) = constraints["items"].as_array_mut() {
            items.retain(|item| {
                item["evidence"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|e| e["path"].as_str().is_some_and(|path| path.contains(scope)))
                    || item["node_ids"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|id| id.as_str().is_some_and(|id| matching_nodes.contains(id)))
            });
            let total = items.len();
            constraints["total"] = json!(total);
        }
        constraints["scope"] = json!(scope);
        if constraints["truncated"] == true {
            constraints["scope_count_incomplete"] = json!(true);
            if let Some(limits) = constraints["limitations"].as_array_mut() {
                limits.push(json!(crate::localize!("范围内核查项来自已返回的候选；候选被截断，当前数量为下界。","Scoped review items come from returned candidates; candidates were truncated, so the current count is a lower bound.")));
            }
        }
    }
    data["constraints"] = constraints.clone();
    let records: Vec<_> = crate::knowledge::list(index, snapshot, None, false)?
        .into_iter()
        .filter(|r| scope.is_none_or(|s| r.to_string().contains(s)))
        .collect();
    let batch = crate::batch::latest(index, snapshot, scope)?;
    if let Some(batch) = &batch {
        data["change_batch"] = batch.clone();
    }
    if let Some(dimensions) = data["dimensions"].as_array_mut() {
        for dimension in dimensions {
            if dimension["id"] == "changes" {
                if let Some(batch) = &batch {
                    let mut items = crate::batch::dimension_items(batch);
                    let total = 1
                        + batch["groups_total"].as_u64().unwrap_or(0)
                        + batch["checklist_total"].as_u64().unwrap_or(0);
                    items.truncate(100);
                    dimension["items"] = json!(items);
                    dimension["total"] = json!(total);
                    dimension["truncated"] = json!(total > 100 || batch["truncated"] == true);
                    dimension["summary"] = batch["summary"].clone();
                    dimension["base_snapshot"] = batch["base_snapshot"].clone();
                    dimension["head_snapshot"] = batch["head_snapshot"].clone();
                    dimension["limitations"] = batch["limits"].clone();
                    dimension["comparison_available"] = json!(true);
                } else {
                    dimension["comparison_available"] = json!(false);
                    dimension["summary"]=json!(crate::localize!("当前快照尚未生成改动批次；先运行 review --base <提交> --worktree 或 --head <提交>。","No change batch has been generated for this snapshot. Run review --base <revision> --worktree or --head <revision> first."));
                }
            }
            if dimension["id"] == "verification" {
                let total = dimension["total"].as_u64().unwrap_or(0)
                    + constraints["total"].as_u64().unwrap_or(0);
                let truncated = dimension["truncated"] == true
                    || constraints["truncated"] == true
                    || total > 100;
                if let Some(items) = dimension["items"].as_array_mut() {
                    let mut merged: Vec<_> = constraints["items"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .cloned()
                        .collect();
                    merged.append(items);
                    merged.truncate(100);
                    *items = merged;
                }
                dimension["total"] = json!(total);
                dimension["truncated"] = json!(truncated);
            }
            if dimension["id"] == "knowledge" {
                dimension["total"] = json!(records.len());
                dimension["items"] = json!(records.iter().take(100).collect::<Vec<_>>());
                dimension["truncated"] = json!(records.len() > 100);
                dimension["summary"] = json!(crate::localize!(
                    "{} 条认知记录；{} 条需复核。",
                    "{} knowledge records; {} need review.",
                    records.len(),
                    records.iter().filter(|r| r["valid"] == false).count()
                ));
                dimension["limitations"]=json!([crate::localize!("结论由用户记录；证据变化会使其待复核，已确认不代表自动证明正确。","Conclusions are recorded by users. Evidence changes require review; confirmation does not automatically prove correctness.")]);
            }
        }
    }
    Ok(data)
}

pub fn understand(
    index: &Index,
    snapshot: &Snapshot,
    dimension: &str,
    scope: Option<&str>,
) -> Result<Report<Value>> {
    let mut understanding = understanding_data_scoped(index, snapshot, scope)?;
    if let Some(dimensions) = understanding["dimensions"].as_array_mut() {
        dimensions.retain(|d| dimension == "all" || d["id"] == dimension);
    }
    let mut report = Report::new(
        snapshot,
        json!({"kind":"understanding","project_root":snapshot.project_root,"scope":scope,"understanding":understanding}),
    );
    report.completeness.truncated |= payload_truncated(&report.data);
    Ok(report)
}

fn payload_truncated(value: &Value) -> bool {
    match value {
        Value::Object(fields) => fields.iter().any(|(key, value)| {
            ((key == "truncated" || key.ends_with("_truncated")) && value == true)
                || payload_truncated(value)
        }),
        Value::Array(items) => items.iter().any(payload_truncated),
        _ => false,
    }
}

pub fn check_freshness(index: &Index, snapshot: &mut Snapshot) -> Result<()> {
    if !snapshot.source_revision.starts_with("worktree:") && snapshot.source_revision != "directory"
    {
        return Ok(());
    }
    let current = WorkingTreeSource {
        root: snapshot.project_root.clone().into(),
    }
    .snapshot()?;
    let hashes = index.file_hashes(&snapshot.id)?;
    if hashes.len() != current.files.len()
        || current
            .files
            .iter()
            .any(|(p, f)| hashes.get(p) != Some(&f.hash))
    {
        snapshot.completeness.stale = true;
        snapshot.diagnostics.push(Diagnostic::warning("stale_snapshot", "Source changed since this snapshot; evidence refers to stored source. Run analyze to refresh.", None));
    }
    Ok(())
}

pub fn overview(index: &Index, snapshot: &Snapshot) -> Result<Report<Value>> {
    let mut statement = index.connection.prepare("SELECT data FROM nodes WHERE snapshot=?1 AND ((name='main' AND kind='function') OR json_extract(data,'$.attributes.entry_kind') IN ('python_main','python_script')) ORDER BY qualified,path")?;
    let rows = statement.query_map([&snapshot.id], |r| r.get::<_, String>(0))?;
    let mut entries = Vec::<Node>::new();
    let units: BTreeMap<_, _> = snapshot
        .project
        .packages
        .iter()
        .flat_map(|p| p.units.iter())
        .map(|u| (u.id.as_str(), u))
        .collect();
    for row in rows {
        let mut node: Node = serde_json::from_str(&row?)?;
        if !node.is_test
            && node
                .attributes
                .get("entry_kind")
                .is_some_and(|v| matches!(v.as_str(), "python_main" | "python_script"))
        {
            entries.push(node);
            continue;
        }
        if let Some(unit) = units.get(node.unit.as_str()) {
            if !node.is_test
                && ["bin", "example", "build"].contains(&unit.kind.as_str())
                && node.qualified_name == format!("{}::main", unit.name.replace('-', "_"))
            {
                node.attributes
                    .insert("entry_kind".into(), unit.kind.clone());
                entries.push(node);
            }
        }
    }
    let rank = |n: &Node| match n.attributes.get("entry_kind").map(String::as_str) {
        Some("bin") => 0,
        Some("example") => 1,
        _ => 2,
    };
    entries.sort_by(|a, b| {
        (rank(a), &a.evidence.path, &a.id).cmp(&(rank(b), &b.evidence.path, &b.id))
    });
    let entry_points_total = entries.len();
    let reading_guide = guide::build(index, snapshot, &entries)?;
    let understanding = understanding_data(index, snapshot)?;
    let baseline = crate::interpretation::data(index, snapshot, &understanding, &reading_guide)?;
    entries.truncate(30);
    let mut report = Report::new(
        snapshot,
        json!({
            "kind": "overview", "project": snapshot.project, "stats": snapshot.stats,
            "project_root":snapshot.project_root, "reading_guide":reading_guide,"understanding":understanding,"baseline":baseline,
            "source_revision": snapshot.source_revision, "entries": entries,"entry_points_total":entry_points_total,"entry_points_limit":30,
            "source_accounting": {
                "indexed_source_files":snapshot.stats.source_files,
                "unlinked_indexed_files":snapshot.diagnostics.iter().filter(|d|d.code=="unlinked_source").count(),
                "skipped_files_with_diagnostics":snapshot.diagnostics.iter().filter(|d|["source_too_large","non_utf8_source"].contains(&d.code.as_str())).count(),
                "ignore_policy":"VCS ignore rules; hidden directories except .cargo, .github, .gitlab, .circleci and .devcontainer; build, dependency and virtual-environment directories; symlinks are not followed",
                "scope_note":"counts cover admitted source files, not ignored dependency/build trees; unlinked source is indexed structurally but excluded from semantic calls"
            },
            "next_queries": ["codexis map --level package", "codexis inspect <symbol>", "codexis trace <symbol> --direction callees", "codexis review --base HEAD --worktree"]
        }),
    );
    report.completeness.truncated |= entry_points_total > 30 || payload_truncated(&report.data);
    Ok(report)
}

pub fn map(
    index: &Index,
    snapshot: &Snapshot,
    level: &str,
    scope: Option<&str>,
    limit: usize,
    offset: usize,
) -> Result<Report<Value>> {
    let mut result = if level == "package" {
        let mut packages = Vec::new();
        for package in &snapshot.project.packages {
            if scope.is_some_and(|q| !package.name.contains(q)) {
                continue;
            }
            let mut statement = index.connection.prepare("SELECT kind,COUNT(*) FROM nodes WHERE snapshot=?1 AND package=?2 GROUP BY kind ORDER BY kind")?;
            let counts = statement
                .query_map(params![snapshot.id, package.id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?))
                })?
                .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
            packages.push(json!({"package": package, "counts":counts}));
        }
        let total = packages.len();
        let page: Vec<_> = packages.into_iter().skip(offset).take(limit).collect();
        let mut report = Report::new(
            snapshot,
            json!({"kind":"map", "level":level, "items":page, "total":total}),
        );
        report.completeness.truncated = offset.saturating_add(limit) < total;
        report
    } else {
        let mut statement = index.connection.prepare(
            "SELECT data FROM nodes WHERE snapshot=?1 AND kind='module' ORDER BY qualified,path,id",
        )?;
        let rows = statement.query_map([&snapshot.id], |r| r.get::<_, String>(0))?;
        let mut modules = Vec::new();
        for row in rows {
            let node: Node = serde_json::from_str(&row?)?;
            if scope.is_none_or(|q| node.qualified_name.contains(q)) {
                modules.push(node);
            }
        }
        let total = modules.len();
        let page: Vec<_> = modules.into_iter().skip(offset).take(limit).collect();
        let mut report = Report::new(
            snapshot,
            json!({"kind":"map", "level":level, "items":page, "total":total}),
        );
        report.completeness.truncated = offset.saturating_add(limit) < total;
        report
    };
    if result.completeness.truncated {
        result.completeness.next_cursor = Some(offset.saturating_add(limit).to_string());
    }
    Ok(result)
}

pub fn inspect(
    index: &Index,
    snapshot: &Snapshot,
    query: &str,
    limit: usize,
) -> Result<Report<Value>> {
    let candidates = index.find_nodes(&snapshot.id, query, limit + 1)?;
    if candidates.is_empty() {
        bail!("no symbol matches {query:?}");
    }
    if candidates.len() != 1 {
        let truncated = candidates.len() > limit;
        let mut result = Report::new(
            snapshot,
            json!({"kind":"candidates", "query":query, "candidates":candidates.into_iter().take(limit).collect::<Vec<_>>(), "next":"Use a candidate ID with inspect or trace"}),
        );
        result.completeness.truncated = truncated;
        return Ok(result);
    }
    let node = &candidates[0];
    let content = index.content(&node.evidence.content_hash)?;
    let first = node.evidence.start_line.saturating_sub(3);
    let snippet: Vec<_> = content
        .lines()
        .enumerate()
        .skip(first)
        .take(45)
        .map(|(i, line)| json!({"line":i+1,"text":line.chars().take(600).collect::<String>()}))
        .collect();
    let outgoing = index.connected_edges(&snapshot.id, &node.id, false, 51)?;
    let incoming = index.connected_edges(&snapshot.id, &node.id, true, 51)?;
    let truncated = outgoing.len() > 50 || incoming.len() > 50;
    let mut result = Report::new(
        snapshot,
        json!({"kind":"inspect","node":node,"source":snippet,"mark":crate::marks::get(index,snapshot,node)?,
        "outgoing":outgoing.into_iter().take(50).collect::<Vec<_>>(), "incoming":incoming.into_iter().take(50).collect::<Vec<_>>() }),
    );
    result.completeness.truncated = truncated;
    Ok(result)
}

pub fn trace(
    index: &Index,
    snapshot: &Snapshot,
    query: &str,
    incoming: bool,
    depth: usize,
    limit: usize,
) -> Result<Report<Value>> {
    let candidates = index.find_nodes(&snapshot.id, query, 21)?;
    if candidates.len() != 1 {
        return inspect(index, snapshot, query, 20);
    }
    let seed = candidates[0].clone();
    let mut nodes = BTreeMap::from([(seed.id.clone(), seed.clone())]);
    let mut edges = BTreeMap::new();
    let mut expanded = BTreeSet::new();
    let mut queue = VecDeque::from([(seed.id.clone(), 0)]);
    let mut truncated = false;
    let started = Instant::now();
    while let Some((id, distance)) = queue.pop_front() {
        crate::source::check_cancelled()?;
        if started.elapsed().as_secs() >= 10 {
            truncated = true;
            break;
        }
        if !expanded.insert(id.clone()) {
            continue;
        }
        let calls = index.connected_calls(&snapshot.id, &id, incoming, limit * 5 + 1)?;
        if distance == depth {
            if !calls.is_empty() {
                truncated = true;
            }
            continue;
        }
        for edge in calls {
            if edges.len() >= limit * 5 {
                truncated = true;
                break;
            }
            let next = if incoming {
                Some(&edge.source)
            } else {
                edge.target.as_ref()
            };
            if let Some(next) = next {
                if !nodes.contains_key(next) {
                    if nodes.len() >= limit {
                        truncated = true;
                        continue;
                    }
                    if let Some(node) = index.find_nodes(&snapshot.id, next, 1)?.pop() {
                        nodes.insert(next.clone(), node);
                        queue.push_back((next.clone(), distance + 1));
                    }
                }
            }
            edges.insert(edge.id.clone(), edge);
        }
    }
    let mut result = Report::new(
        snapshot,
        json!({"kind":"trace", "root":seed, "direction":if incoming {"callers"} else {"callees"},
        "depth":depth,"limit":limit,"nodes":nodes.into_values().collect::<Vec<_>>(),"edges":edges.into_values().collect::<Vec<_>>(),
        "next":"Use a node ID to explore another local neighborhood; increase --depth or --limit for a wider query"}),
    );
    result.completeness.truncated = truncated;
    Ok(result)
}
