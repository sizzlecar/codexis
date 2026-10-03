//! Project-level changes and review questions, independent of diff pagination.
use crate::{
    index::Index,
    model::{Evidence, Node, Snapshot},
};
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

const LIMIT: usize = 100;
const CACHE_VERSION: u32 = 1;

fn prepare(index: &Index) -> Result<()> {
    index.connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS latest_review_batches(
        head_snapshot TEXT PRIMARY KEY,base_snapshot TEXT NOT NULL,updated INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS review_batch_cache(
        base_snapshot TEXT NOT NULL,head_snapshot TEXT NOT NULL,locale TEXT NOT NULL,
        version INTEGER NOT NULL,data TEXT NOT NULL,
        PRIMARY KEY(base_snapshot,head_snapshot,locale,version));",
    )?;
    Ok(())
}

fn cache(index: &Index, before: &Snapshot, after: &Snapshot, value: &Value) -> Result<()> {
    prepare(index)?;
    index.connection.execute(
        "INSERT OR REPLACE INTO review_batch_cache VALUES(?1,?2,?3,?4,?5)",
        params![
            before.id,
            after.id,
            crate::i18n::current().tag(),
            CACHE_VERSION,
            serde_json::to_string(value)?
        ],
    )?;
    Ok(())
}

fn file_evidence(index: &Index, path: &str, hash: &str) -> Result<Evidence> {
    let content = index.content(hash)?;
    Ok(Evidence {
        path: path.into(),
        content_hash: hash.into(),
        start_byte: 0,
        end_byte: content.len(),
        start_line: 1,
        start_column: 0,
        end_line: content.lines().count().max(1),
        end_column: 0,
    })
}

fn component(node: &Node) -> String {
    let mut parts = node
        .qualified_name
        .split(if node.language == "python" { "." } else { "::" });
    let package = parts.next().unwrap_or(&node.package);
    let next = parts.next();
    match next {
        Some(module) if node.parent.is_some() && node.kind != "module" => {
            format!("{package}::{module}")
        }
        _ => package.into(),
    }
}

pub fn build(index: &Index, before: &Snapshot, after: &Snapshot) -> Result<Value> {
    let value = collect(index, before, after)?;
    cache(index, before, after, &value)?;
    index.connection.execute(
        "INSERT OR REPLACE INTO latest_review_batches VALUES(?1,?2,?3)",
        params![after.id, before.id, crate::analysis::now_ms()],
    )?;
    Ok(limit(value))
}

fn collect(index: &Index, before: &Snapshot, after: &Snapshot) -> Result<Value> {
    let old_nodes = index.all_nodes(&before.id)?;
    let new_nodes = index.all_nodes(&after.id)?;
    let old_hashes = index.file_hashes(&before.id)?;
    let new_hashes = index.file_hashes(&after.id)?;
    let paths: BTreeSet<_> = old_hashes
        .keys()
        .chain(new_hashes.keys())
        .cloned()
        .collect();
    let changed_paths: BTreeSet<_> = paths
        .into_iter()
        .filter(|p| old_hashes.get(p) != new_hashes.get(p))
        .collect();
    let mut old_map = BTreeMap::<&str, Vec<&Node>>::new();
    let mut new_map = BTreeMap::<&str, Vec<&Node>>::new();
    for n in &old_nodes {
        old_map.entry(&n.stable_key).or_default().push(n);
    }
    for n in &new_nodes {
        new_map.entry(&n.stable_key).or_default().push(n);
    }
    let keys: BTreeSet<_> = old_map.keys().chain(new_map.keys()).copied().collect();
    let mut groups = BTreeMap::<String, Value>::new();
    let mut checklist = Vec::new();
    let mut changed_symbols = 0;
    for key in keys {
        crate::source::check_cancelled()?;
        let old = old_map.get(key).cloned().unwrap_or_default();
        let new = new_map.get(key).cloned().unwrap_or_default();
        let mut old_signatures: Vec<_> = old
            .iter()
            .map(|n| (&n.fingerprint, &n.conditions))
            .collect();
        let mut new_signatures: Vec<_> = new
            .iter()
            .map(|n| (&n.fingerprint, &n.conditions))
            .collect();
        old_signatures.sort();
        new_signatures.sort();
        if old_signatures == new_signatures {
            continue;
        }
        let current = new.first().or(old.first()).unwrap();
        if matches!(current.kind.as_str(), "file" | "module" | "impl") {
            continue;
        }
        changed_symbols += 1;
        let state = if old.is_empty() {
            "added"
        } else if new.is_empty() {
            "removed"
        } else {
            "modified"
        };
        let name = component(current);
        let group=groups.entry(name.clone()).or_insert_with(||json!({"id":name,"title":name,"summary":"","basis":"syntax_change","files":[],"node_ids":[],"evidence":[],"changes":[],"total":0}));
        let total = group["total"].as_u64().unwrap_or(0) + 1;
        group["total"] = json!(total);
        {
            group["node_ids"]
                .as_array_mut()
                .unwrap()
                .push(json!(current.id));
            group["evidence"]
                .as_array_mut()
                .unwrap()
                .push(json!(current.evidence));
            group["changes"].as_array_mut().unwrap().push(json!({"title":current.qualified_name,"state":state,"node_id":current.id,"snapshot_id":if new.is_empty(){&before.id}else{&after.id},"before":old.iter().map(|n|&n.evidence).collect::<Vec<_>>(),"after":new.iter().map(|n|&n.evidence).collect::<Vec<_>>()}));
        }
        let public = current.visibility.starts_with("pub") || current.visibility == "public";
        let interface_change = public
            && (old.len() != 1
                || new.len() != 1
                || old[0].signature != new[0].signature
                || old[0].visibility != new[0].visibility
                || old[0].conditions != new[0].conditions);
        if interface_change || matches!(current.kind.as_str(), "field" | "variant") {
            checklist.push(json!({"id":format!("interface:{key}"),"title":crate::localize!("核查接口与数据契约：{}","Review interface and data contracts: {}",current.qualified_name),"summary":crate::localize!("{state}；检查调用方、数据形状与配置条件。","{state}; check callers, data shape, and configuration conditions.",),"priority":"high","dimensions":["architecture","data","verification"],"basis":"syntax_change","evidence":[current.evidence],"node_ids":[current.id],"snapshot_id":if new.is_empty(){&before.id}else{&after.id}}));
        }
    }
    // File-only changes (configuration and documentation included) always get
    // represented, even when no declaration has changed.
    let mut artifacts = Vec::new();
    for path in &changed_paths {
        let hash = new_hashes.get(path).or(old_hashes.get(path)).unwrap();
        let evidence = file_evidence(index, path, hash)?;
        let state = if !old_hashes.contains_key(path) {
            "added"
        } else if !new_hashes.contains_key(path) {
            "removed"
        } else {
            "modified"
        };
        let mut matched = false;
        for group in groups.values_mut() {
            if group["evidence"]
                .as_array()
                .is_some_and(|es| es.iter().any(|e| e["path"] == path.as_str()))
            {
                group["files"].as_array_mut().unwrap().push(json!(path));
                matched = true;
            }
        }
        if !matched {
            let id = path.split('/').next().unwrap_or(path);
            let group=groups.entry(format!("files:{id}")).or_insert_with(||json!({"id":format!("files:{id}"),"title":id,"summary":crate::localize!("文件与配置变化","File and configuration changes"),"basis":"content_hash","files":[],"node_ids":[],"evidence":[],"changes":[],"total":0}));
            group["files"].as_array_mut().unwrap().push(json!(path));
            {
                group["evidence"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!(evidence));
            }
        }
        if !path.ends_with(".rs") && !path.ends_with(".py") && !path.ends_with(".pyi") {
            artifacts.push(json!({"id":path,"title":path,"summary":crate::localize!("{state}；对照实现检查意图、配置与接口约定。","{state}; compare intent, configuration, and interface contracts with implementation.",),"basis":"content_hash","evidence":[evidence],"snapshot_id":if new_hashes.contains_key(path){&after.id}else{&before.id}}));
        }
    }
    let old_understanding = crate::understanding::build(index, before)?;
    let new_understanding = crate::understanding::build(index, after)?;
    let deps = |value: &Value| -> BTreeMap<String, Value> {
        value["architecture"]["dependencies"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|d| {
                (
                    format!(
                        "{}|{}|{}|{}",
                        d["source"], d["target"], d["kind"], d["resolution"]
                    ),
                    d.clone(),
                )
            })
            .collect()
    };
    let old_deps = deps(&old_understanding);
    let new_deps = deps(&new_understanding);
    let mut boundary_changes = Vec::new();
    for key in old_deps
        .keys()
        .chain(new_deps.keys())
        .collect::<BTreeSet<_>>()
    {
        let old = old_deps.get(key);
        let new = new_deps.get(key);
        if old.is_some() && new.is_some() {
            continue;
        }
        let mut change = new.or(old).unwrap().clone();
        change["state"] = json!(if new.is_some() { "added" } else { "removed" });
        let name = |id: &Value| -> String {
            new_understanding["architecture"]["components"]
                .as_array()
                .into_iter()
                .flatten()
                .chain(
                    old_understanding["architecture"]["components"]
                        .as_array()
                        .into_iter()
                        .flatten(),
                )
                .find(|c| &c["id"] == id)
                .and_then(|c| c["title"].as_str())
                .unwrap_or("unexpanded component")
                .to_owned()
        };
        change["title"] = json!(crate::localize!(
            "跨模块关系 {} → {}",
            "Cross-component relation {} → {}",
            name(&change["source"]),
            name(&change["target"])
        ));
        change["summary"]=json!(crate::localize!("依赖边界发生变化；需要核查职责与分层约定，不能据此断言行为已改变。","A dependency boundary changed. Review responsibilities and layering; this alone does not prove a behavior change."));
        change["snapshot_id"] = json!(if new.is_some() { &after.id } else { &before.id });
        let mut question = change.clone();
        question["priority"] = json!("high");
        question["dimensions"] = json!(["architecture", "behavior", "verification"]);
        checklist.push(question);
        boundary_changes.push(change);
    }
    // Package manifest dependencies exist independently of symbol resolution.
    let declared = |s: &Snapshot| -> BTreeSet<String> {
        s.project
            .packages
            .iter()
            .flat_map(|p| {
                p.dependencies.iter().map(move |d| {
                    format!(
                        "{} → {} [{}; optional={}; {:?}]",
                        p.name, d.package, d.kind, d.optional, d.condition
                    )
                })
            })
            .collect()
    };
    let old_declared = declared(before);
    let new_declared = declared(after);
    let declared_changes:Vec<_>=old_declared.symmetric_difference(&new_declared).map(|d|json!({"title":d,"state":if new_declared.contains(d){"added"}else{"removed"},"basis":"manifest_declaration"})).collect();
    checklist.extend(artifacts.iter().cloned().map(|mut a| {
        a["priority"] = json!("normal");
        a["dimensions"] = json!(["intent", "runtime", "verification"]);
        a
    }));
    let constraints = crate::constraints::evaluate(index, after)?;
    checklist.extend(
        constraints["items"]
            .as_array()
            .into_iter()
            .flatten()
            .cloned(),
    );
    let knowledge_updates: Vec<_> = crate::knowledge::list(index, after, None, false)?
        .into_iter()
        .filter(|r| r["valid"] == false)
        .collect();
    checklist.extend(knowledge_updates.iter().cloned().map(|mut r| {
        r["priority"] = json!("high");
        r["dimensions"] = json!(["knowledge", "verification"]);
        r
    }));
    for group in groups.values_mut() {
        group["summary"] = json!(crate::localize!(
            "{} 项声明变化，{} 个关联文件",
            "{} declaration changes, {} related files",
            group["total"],
            group["files"].as_array().unwrap().len()
        ));
        group["truncated"] = json!(group["total"].as_u64().unwrap_or(0) > LIMIT as u64);
    }
    let group_total = groups.len();
    let checklist_total = checklist.len();
    let boundary_total = boundary_changes.len();
    let artifact_total = artifacts.len();
    let dependencies_incomplete = old_understanding["architecture"]["dependencies_truncated"]
        == true
        || new_understanding["architecture"]["dependencies_truncated"] == true
        || old_understanding["architecture"]["truncated"] == true
        || new_understanding["architecture"]["truncated"] == true;
    let mut verification_items = Vec::new();
    for dimension in new_understanding["dimensions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| d["id"] == "verification")
    {
        for item in dimension["items"].as_array().into_iter().flatten() {
            if item["evidence"].as_array().is_some_and(|es| {
                es.iter().any(|e| {
                    e["path"]
                        .as_str()
                        .is_some_and(|p| changed_paths.contains(p))
                })
            }) {
                verification_items.push(item.clone());
            }
        }
    }
    Ok(
        json!({"id":format!("{}:{}",before.id,after.id),"base_snapshot":before.id,"head_snapshot":after.id,
        "summary":crate::localize!("{} 个文件、{} 项声明变化；按 {} 组组织，{} 项建议核查。","{} files, {} declaration changes; {} groups and {} review questions.",changed_paths.len(),changed_symbols,group_total,checklist_total),
        "groups":groups.into_values().collect::<Vec<_>>(),"groups_total":group_total,
        "boundary_changes":boundary_changes,"boundary_changes_total":boundary_total,"boundary_comparison_truncated":dependencies_incomplete,
        "declared_dependency_changes":declared_changes,"artifact_changes":artifacts,"artifact_changes_total":artifact_total,
        "checklist":checklist,"checklist_total":checklist_total,
        "changed_files_total":changed_paths.len(),"changed_symbols_total":changed_symbols,
        "changed_paths":changed_paths,"dependencies_incomplete":dependencies_incomplete,
        "locale":crate::i18n::current().tag(),
        "knowledge_updates":knowledge_updates,"verification":verification_items,
        "intent":crate::localize!("未提供任务说明；按可观察的结构和证据组织改动，不推断代码由谁生成。","No task description supplied. Changes are organized by observable structure and evidence; code authorship is not inferred."),
        "truncated":dependencies_incomplete,
        "limits":[crate::localize!("调用关联仅覆盖已有解析结果；未知边可能隐藏影响。","Call associations cover indexed targets only; unknown edges may hide impacts."),crate::localize!("测试定义或引用不证明覆盖或执行通过。","Test definitions and references do not prove coverage or passing execution."),crate::localize!("分组按源码命名和目录，不自动确认业务职责。","Groups follow source names and directories; business responsibilities are not automatically confirmed.")]}),
    )
}

/// The latest explicitly generated comparison for this exact head snapshot.
/// Other locales are rendered from stored snapshots, without live source/Git.
pub fn latest(index: &Index, snapshot: &Snapshot, scope: Option<&str>) -> Result<Option<Value>> {
    prepare(index)?;
    let base: Option<String> = index
        .connection
        .query_row(
            "SELECT base_snapshot FROM latest_review_batches WHERE head_snapshot=?1",
            [&snapshot.id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(base) = base else { return Ok(None) };
    let encoded:Option<String>=index.connection.query_row(
        "SELECT data FROM review_batch_cache WHERE base_snapshot=?1 AND head_snapshot=?2 AND locale=?3 AND version=?4",
        params![base,snapshot.id,crate::i18n::current().tag(),CACHE_VERSION],|r| r.get(0)).optional()?;
    let mut value = if let Some(encoded) = encoded {
        serde_json::from_str(&encoded)?
    } else {
        let before = index.snapshot(Some(&base))?;
        let value = collect(index, &before, snapshot)?;
        cache(index, &before, snapshot, &value)?;
        value
    };
    // Human conclusions can be revised after review. Refresh these entries
    // rather than presenting a confirmed conclusion as still awaiting review.
    let knowledge: Vec<_> = crate::knowledge::list(index, snapshot, None, false)?
        .into_iter()
        .filter(|r| r["valid"] == false)
        .collect();
    if let Some(items) = value["checklist"].as_array_mut() {
        items.retain(|item| item["basis"] != "user_confirmed");
        items.extend(knowledge.iter().cloned().map(|mut record| {
            record["priority"] = json!("high");
            record["dimensions"] = json!(["knowledge", "verification"]);
            record
        }));
    }
    value["knowledge_updates"] = json!(knowledge);
    if let Some(scope) = scope {
        filter_scope(index, &base, snapshot, scope, &mut value)?;
    }
    value["checklist_total"] = json!(value["checklist"].as_array().map_or(0, Vec::len));
    value["summary"] = json!(if scope.is_some() {
        crate::localize!(
            "当前范围关联 {} 组改动，{} 项建议核查。",
            "The selected scope has {} change groups and {} review questions.",
            value["groups_total"],
            value["checklist_total"]
        )
    } else {
        crate::localize!(
            "{} 个文件、{} 项声明变化；按 {} 组组织，{} 项建议核查。",
            "{} files, {} declaration changes; {} groups and {} review questions.",
            value["changed_files_total"],
            value["changed_symbols_total"],
            value["groups_total"],
            value["checklist_total"]
        )
    });
    Ok(Some(limit(value)))
}

fn limit(mut value: Value) -> Value {
    let mut truncated = value["dependencies_incomplete"] == true;
    if let Some(groups) = value["groups"].as_array_mut() {
        for group in groups {
            let mut group_truncated = false;
            for section in ["changes", "evidence", "node_ids", "files"] {
                if let Some(items) = group[section].as_array_mut() {
                    let count = items.len();
                    items.truncate(LIMIT);
                    group_truncated |= count > LIMIT;
                    group[format!("{section}_total")] = json!(count);
                }
            }
            group["truncated"] = json!(group_truncated);
            truncated |= group_truncated;
        }
    }
    for section in [
        "groups",
        "checklist",
        "boundary_changes",
        "artifact_changes",
        "declared_dependency_changes",
        "knowledge_updates",
        "verification",
    ] {
        if let Some(items) = value[section].as_array_mut() {
            let total = items.len();
            items.truncate(LIMIT);
            value[format!("{section}_total")] = json!(total);
            value[format!("{section}_truncated")] = json!(total > LIMIT);
            truncated |= total > LIMIT;
        }
    }
    value["boundary_comparison_truncated"] = json!(
        value["dependencies_incomplete"] == true || value["boundary_changes_truncated"] == true
    );
    value["truncated"] = json!(truncated);
    value
}

fn filter_scope(
    index: &Index,
    base: &str,
    snapshot: &Snapshot,
    query: &str,
    value: &mut Value,
) -> Result<()> {
    let before = index.snapshot(Some(base))?;
    let packages: BTreeSet<_> = before
        .project
        .packages
        .iter()
        .chain(&snapshot.project.packages)
        .filter(|p| {
            p.name.contains(query)
                || (!p.root.is_empty() && p.root.contains(query))
                || p.id == query
        })
        .map(|p| p.id.as_str())
        .collect();
    let nodes = index
        .all_nodes(base)?
        .into_iter()
        .chain(index.all_nodes(&snapshot.id)?);
    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for node in nodes {
        if node.qualified_name.contains(query)
            || node.evidence.path.contains(query)
            || node.id == query
            || packages.contains(node.package.as_str())
        {
            ids.insert(node.id);
            paths.insert(node.evidence.path);
        }
    }
    paths.extend(
        index
            .file_hashes(base)?
            .into_keys()
            .chain(index.file_hashes(&snapshot.id)?.into_keys())
            .filter(|p| p.contains(query)),
    );
    let matches = |item: &Value| {
        item["title"]
            .as_str()
            .is_some_and(|title| title.contains(query))
            || item["node_ids"].as_array().is_some_and(|nodes| {
                nodes
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|id| ids.contains(id))
            })
            || item["node_id"].as_str().is_some_and(|id| ids.contains(id))
            || item["evidence"].as_array().is_some_and(|evidence| {
                evidence.iter().any(|e| {
                    e["path"]
                        .as_str()
                        .is_some_and(|p| paths.contains(p) || p.contains(query))
                })
            })
            || item["files"].as_array().is_some_and(|files| {
                files
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|p| paths.contains(p) || p.contains(query))
            })
    };
    for section in [
        "groups",
        "checklist",
        "boundary_changes",
        "artifact_changes",
        "declared_dependency_changes",
        "knowledge_updates",
        "verification",
    ] {
        if let Some(items) = value[section].as_array_mut() {
            items.retain(matches);
            for item in items {
                if let Some(changes) = item["changes"].as_array_mut() {
                    changes.retain(matches);
                }
                if let Some(evidence) = item["evidence"].as_array_mut() {
                    evidence.retain(|e| {
                        e["path"]
                            .as_str()
                            .is_some_and(|p| paths.contains(p) || p.contains(query))
                    });
                }
                if let Some(files) = item["files"].as_array_mut() {
                    files.retain(|p| {
                        p.as_str()
                            .is_some_and(|p| paths.contains(p) || p.contains(query))
                    });
                }
                if let Some(nodes) = item["node_ids"].as_array_mut() {
                    nodes.retain(|id| id.as_str().is_some_and(|id| ids.contains(id)));
                }
                if section == "groups" {
                    item["total"] = json!(item["changes"].as_array().map_or(0, Vec::len));
                }
            }
        }
    }
    value["groups_total"] = json!(value["groups"].as_array().map_or(0, Vec::len));
    value["scope"] = json!(query);
    Ok(())
}

/// Presentation items share the eight-view evidence contract and retain the
/// comparison IDs needed to inspect each side of an archived change.
pub fn dimension_items(batch: &Value) -> Vec<Value> {
    let mut evidence = BTreeMap::new();
    let mut node_ids = BTreeSet::new();
    for item in ["groups", "checklist"]
        .into_iter()
        .flat_map(|section| batch[section].as_array().into_iter().flatten())
    {
        for span in item["evidence"].as_array().into_iter().flatten() {
            evidence
                .entry((
                    span["path"].to_string(),
                    span["content_hash"].to_string(),
                    span["start_byte"].to_string(),
                ))
                .or_insert_with(|| span.clone());
        }
        node_ids.extend(
            item["node_ids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned),
        );
    }
    let mut items = vec![
        json!({"id":format!("batch:{}",batch["id"].as_str().unwrap_or("")),
        "title":crate::localize!("最近改动批次","Latest change batch"),"summary":batch["summary"],
        "basis":"stored snapshot comparison","evidence":evidence.into_values().take(8).collect::<Vec<_>>(),
        "node_ids":node_ids.into_iter().take(100).collect::<Vec<_>>(),"base_snapshot":batch["base_snapshot"],
        "head_snapshot":batch["head_snapshot"],"entry_kind":"batch_summary"}),
    ];
    for (section, entry_kind) in [("groups", "change_group"), ("checklist", "review_question")] {
        for item in batch[section].as_array().into_iter().flatten() {
            let mut item = item.clone();
            item["entry_kind"] = json!(entry_kind);
            item["base_snapshot"] = batch["base_snapshot"].clone();
            item["head_snapshot"] = batch["head_snapshot"].clone();
            items.push(item);
        }
    }
    items
}
