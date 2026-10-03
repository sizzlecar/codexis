use crate::index::Index;
use crate::model::{Node, Report, Snapshot};
use anyhow::Result;
use serde_json::{json, Value};
use similar::TextDiff;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

struct Change {
    before: Option<Node>,
    after: Option<Node>,
    state: &'static str,
    priority: usize,
    reasons: Vec<String>,
}

fn relevant(node: &Node) -> bool {
    matches!(
        node.kind.as_str(),
        "function" | "type" | "trait" | "value" | "macro" | "field" | "variant"
    )
}

fn node_map(nodes: Vec<Node>) -> BTreeMap<String, Vec<Node>> {
    let mut map = BTreeMap::<String, Vec<Node>>::new();
    for node in nodes.into_iter().filter(relevant) {
        map.entry(node.stable_key.clone()).or_default().push(node);
    }
    map
}

fn make_change(before: Option<Node>, after: Option<Node>, state: &'static str) -> Change {
    let mut priority = 1;
    let mut reasons = Vec::new();
    if let (Some(old), Some(new)) = (&before, &after) {
        if old.signature != new.signature || old.visibility != new.visibility {
            reasons.push("definition or signature changed".into());
            if old.visibility.starts_with("pub") || new.visibility.starts_with("pub") {
                priority = 3;
                reasons.push("public interface requires review".into());
            }
        } else {
            reasons.push("implementation changed".into());
        }
        if old.conditions != new.conditions {
            reasons.push("conditional compilation changed".into());
            priority = 3;
        }
    } else if state == "removed" {
        reasons.push("definition removed; inspect previous callers".into());
        priority = if before
            .as_ref()
            .is_some_and(|n| n.visibility.starts_with("pub"))
        {
            3
        } else {
            2
        };
    } else {
        reasons.push("definition added".into());
    }
    Change {
        before,
        after,
        state,
        priority,
        reasons,
    }
}

fn impacts(
    index: &Index,
    snapshot: &Snapshot,
    node: &Node,
    limit: usize,
) -> Result<(Vec<Value>, bool)> {
    let mut queue = VecDeque::from([(node.id.clone(), Vec::<Value>::new(), 0)]);
    let mut seen = BTreeSet::new();
    let mut output = Vec::new();
    let mut truncated = false;
    while let Some((id, path, depth)) = queue.pop_front() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let calls = index.connected_calls(&snapshot.id, &id, true, limit + 1)?;
        if depth >= 2 {
            truncated |= calls.iter().any(|e| !seen.contains(&e.source));
            continue;
        }
        for edge in calls {
            if seen.contains(&edge.source) {
                continue;
            }
            if output.len() >= limit {
                truncated = true;
                break;
            }
            if let Some(caller) = index.find_nodes(&snapshot.id, &edge.source, 1)?.pop() {
                let mut route = path.clone();
                route.push(json!({"source":edge.source,"target":edge.target,"resolution":edge.resolution,"evidence":edge.evidence}));
                output.push(json!({"caller":caller,"path":route,"snapshot_id":snapshot.id,"interpretation":"potential impact; review this caller"}));
                queue.push_back((caller.id, route, depth + 1));
            }
        }
        if truncated {
            break;
        }
    }
    Ok((output, truncated))
}

pub fn compare(
    index: &Index,
    before: &Snapshot,
    after: &Snapshot,
    limit: usize,
    offset: usize,
) -> Result<Report<Value>> {
    let mut old = node_map(index.all_nodes(&before.id)?);
    let mut new = node_map(index.all_nodes(&after.id)?);
    let keys: BTreeSet<_> = old.keys().chain(new.keys()).cloned().collect();
    let mut changes = Vec::new();
    for key in keys {
        let mut old_nodes = old.remove(&key).unwrap_or_default();
        let mut new_nodes = new.remove(&key).unwrap_or_default();
        // Identical conditional definitions must not appear as changes merely
        // because several definitions share a qualified name.
        old_nodes.retain(|previous| {
            if let Some(position) = new_nodes.iter().position(|current| {
                current.fingerprint == previous.fingerprint
                    && current.conditions == previous.conditions
            }) {
                new_nodes.remove(position);
                false
            } else {
                true
            }
        });
        if old_nodes.len() == 1 && new_nodes.len() == 1 {
            let previous = old_nodes.into_iter().next().unwrap();
            let current = new_nodes.into_iter().next().unwrap();
            if previous.fingerprint != current.fingerprint
                || previous.conditions != current.conditions
            {
                changes.push(make_change(Some(previous), Some(current), "modified"));
            }
        } else {
            for node in old_nodes {
                changes.push(make_change(Some(node), None, "removed"));
            }
            for node in new_nodes {
                changes.push(make_change(None, Some(node), "added"));
            }
        }
    }
    changes.sort_by(|a, b| {
        b.priority.cmp(&a.priority).then_with(|| {
            let a = a.after.as_ref().or(a.before.as_ref()).unwrap();
            let b = b.after.as_ref().or(b.before.as_ref()).unwrap();
            (&a.evidence.path, &a.qualified_name, &a.id).cmp(&(
                &b.evidence.path,
                &b.qualified_name,
                &b.id,
            ))
        })
    });
    let total_changes = changes.len();
    let mut rows = Vec::new();
    for change in changes.into_iter().skip(offset).take(limit) {
        crate::source::check_cancelled()?;
        let current = change.after.as_ref().or(change.before.as_ref()).unwrap();
        let current_snapshot = if change.after.is_some() {
            after
        } else {
            before
        };
        let (old_impacts, old_truncated) = change
            .before
            .as_ref()
            .map(|n| impacts(index, before, n, 30))
            .transpose()?
            .unwrap_or_default();
        let (new_impacts, new_truncated) = change
            .after
            .as_ref()
            .map(|n| impacts(index, after, n, 30))
            .transpose()?
            .unwrap_or_default();
        let mark = crate::marks::get(index, current_snapshot, current)?;
        rows.push(json!({"id":current.id,"state":change.state,"before":change.before,"after":change.after,
            "reasons":change.reasons,"priority":match change.priority {3=>"interface",2=>"callers",_=>"implementation"},
            "previous_impacts":old_impacts,"current_impacts":new_impacts,"impacts_truncated":old_truncated||new_truncated,
            "mark_snapshot":current_snapshot.id,"mark":mark}));
    }
    let old_files = index.file_hashes(&before.id)?;
    let new_files = index.file_hashes(&after.id)?;
    let paths: BTreeSet<_> = old_files.keys().chain(new_files.keys()).cloned().collect();
    let mut files = Vec::new();
    let mut changed_files = 0;
    for path in paths {
        if old_files.get(&path) == new_files.get(&path) {
            continue;
        }
        changed_files += 1;
        if changed_files <= offset || files.len() >= limit {
            continue;
        }
        let old_content = old_files
            .get(&path)
            .map(|h| index.content(h))
            .transpose()?
            .unwrap_or_default();
        let new_content = new_files
            .get(&path)
            .map(|h| index.content(h))
            .transpose()?
            .unwrap_or_default();
        let diff = TextDiff::configure()
            .timeout(Duration::from_millis(500))
            .diff_lines(&old_content, &new_content);
        let unified = diff
            .unified_diff()
            .context_radius(3)
            .header(&format!("a/{path}"), &format!("b/{path}"))
            .to_string();
        let diff_truncated = unified.lines().count() > 160
            || unified.lines().take(160).any(|l| l.chars().count() > 800);
        let bounded_diff = unified
            .lines()
            .take(160)
            .map(|line| {
                let mut text = line.chars().take(800).collect::<String>();
                if line.chars().count() > 800 {
                    text.push_str(" … [line truncated]");
                }
                text
            })
            .collect::<Vec<_>>()
            .join("\n");
        files.push(json!({"path":path,"state":if !old_files.contains_key(&path){"added"}else if !new_files.contains_key(&path){"removed"}else{"modified"},
            "before_hash":old_files.get(&path),"after_hash":new_files.get(&path),
            "diff":bounded_diff,"diff_truncated":diff_truncated}));
    }
    let batch = crate::batch::build(index, before, after)?;
    let mut report = Report::new(
        after,
        json!({"kind":"review","base_snapshot":before.id,"head_snapshot":after.id,
        "base_revision":before.source_revision,"head_revision":after.source_revision,"base_context":before.context,
        "total_changes":total_changes,"total_changed_files":changed_files,"changes":rows,"files":files,"batch":batch,
        "comparison_scope":"supported source files and project manifests",
        "impact_scope":"resolved incoming call paths, at most two hops; incomplete and dynamic relationships may hide other impacts",
        "impact_analysis":if before.context.analysis!="semantic" || after.context.analysis!="semantic" {"not computed: syntax-only snapshots; use --analysis semantic for call impact"} else if before.completeness.status!="complete" || after.completeness.status!="complete" {"partial semantic evidence; not a complete impact assessment"} else {"bounded semantic evidence within the declared profile; not proof of all runtime impact"},
        "base_completeness":before.completeness,"base_diagnostics":before.diagnostics,
        "matching":"stable qualified identity; ambiguous matches and moves are additions/removals, not guessed renames",
        "next":"Use inspect <id> with --snapshot <base_snapshot or head_snapshot> to inspect each side"}),
    );
    report.completeness.truncated = offset.saturating_add(limit) < total_changes.max(changed_files);
    if report.completeness.truncated {
        report.completeness.next_cursor = Some(offset.saturating_add(limit).to_string());
    }
    if before.completeness.status != "complete" {
        report.completeness.status = "partial".into();
    }
    Ok(report)
}
