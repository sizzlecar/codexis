//! A source-backed reading guide, computed from the existing snapshot. No
//! semantic backend, business-role guesses or extra user command is required.
use crate::{
    index::Index,
    model::{Node, Package, Snapshot},
};
use anyhow::Result;
use rusqlite::params;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

const PACKAGE_LIMIT: usize = 32;
const START_LIMIT: usize = 4;

fn description(
    index: &Index,
    snapshot: &Snapshot,
    package: &Package,
    hashes: &BTreeMap<String, String>,
) -> Result<Value> {
    let filename = match package.language.as_str() {
        "rust" => "Cargo.toml",
        "python" => "pyproject.toml",
        _ => return Ok(Value::Null),
    };
    let mut path = if package.root.is_empty() {
        filename.to_owned()
    } else {
        format!("{}/{filename}", package.root)
    };
    let Some(hash) = hashes.get(&path) else {
        return Ok(Value::Null);
    };
    let mut evidence_hash = hash.clone();
    let content = index.content(hash)?;
    let text = if package.language == "rust" {
        let Ok(manifest) = toml::from_str::<toml::Value>(&content) else {
            return Ok(Value::Null);
        };
        let declaration = manifest.get("package").and_then(|p| p.get("description"));
        if declaration
            .and_then(|d| d.get("workspace"))
            .and_then(toml::Value::as_bool)
            == Some(true)
        {
            if let Some(root) = hashes.get("Cargo.toml") {
                path = "Cargo.toml".into();
                evidence_hash = root.clone();
                let Ok(root) = toml::from_str::<toml::Value>(&index.content(root)?) else {
                    return Ok(Value::Null);
                };
                root.get("workspace")
                    .and_then(|w| w.get("package"))
                    .and_then(|p| p.get("description"))
                    .and_then(toml::Value::as_str)
                    .map(str::to_owned)
            } else {
                None
            }
        } else {
            declaration.and_then(toml::Value::as_str).map(str::to_owned)
        }
    } else {
        let Ok(manifest) = toml::from_str::<toml::Value>(&content) else {
            return Ok(Value::Null);
        };
        manifest
            .get("project")
            .and_then(|p| p.get("description"))
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
    };
    Ok(text
        .filter(|s| !s.trim().is_empty())
        .map(|text| {
            json!({
                "text":text, "path":path, "content_hash":evidence_hash, "basis":"manifest declaration",
                "language":snapshot.context.language,
            })
        })
        .unwrap_or(Value::Null))
}

pub(super) fn build(index: &Index, snapshot: &Snapshot, entries: &[Node]) -> Result<Value> {
    let hashes = index.file_hashes(&snapshot.id)?;
    let mut statement = index.connection.prepare(
        "SELECT package,COUNT(DISTINCT path) FROM nodes WHERE snapshot=?1 GROUP BY package",
    )?;
    let counts = statement
        .query_map([&snapshot.id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?))
        })?
        .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
    let packages = &snapshot.project.packages;
    let by_root: BTreeMap<_, _> = packages.iter().map(|p| (p.root.as_str(), p)).collect();
    let mut incoming = BTreeMap::<&str, BTreeSet<&str>>::new();
    let mut dependencies = Vec::new();
    for package in packages {
        let mut seen = BTreeSet::new();
        for dep in &package.dependencies {
            if !matches!(
                dep.kind.as_str(),
                "normal" | "compile" | "provided" | "system"
            ) {
                continue;
            }
            let Some(target) = dep.path.as_deref().and_then(|p| by_root.get(p)) else {
                continue;
            };
            if !seen.insert((&target.id, dep.optional, &dep.condition)) {
                continue;
            }
            incoming.entry(&target.id).or_default().insert(&package.id);
            dependencies.push(json!({"source":package.name,"target":target.name,
                "source_id":package.id,"target_id":target.id,"kind":dep.kind,
                "optional":dep.optional,"condition":dep.condition,"resolution":"declared"}));
        }
    }
    let mut ordered: Vec<_> = packages.iter().collect();
    ordered.sort_by_key(|p| {
        (
            std::cmp::Reverse(incoming.get(p.id.as_str()).map_or(0, BTreeSet::len)),
            &p.name,
        )
    });
    let summaries = ordered.iter().take(PACKAGE_LIMIT).map(|p| Ok(json!({
        "id":p.id,"name":p.name,"root":p.root,"source_files":counts.get(&p.id).copied().unwrap_or(0),
        "dependents":incoming.get(p.id.as_str()).map_or(0, BTreeSet::len),
        "description":description(index, snapshot, p, &hashes)?,
    }))).collect::<Result<Vec<_>>>()?;
    let mut starts: Vec<_> = entries
        .iter()
        .filter(|n| {
            matches!(
                n.attributes.get("entry_kind").map(String::as_str),
                Some("bin" | "python_main" | "python_script")
            )
        })
        .collect();
    starts.sort_by_key(|n| {
        (
            !n.evidence.path.ends_with("/main.rs"),
            &n.evidence.path,
            &n.id,
        )
    });
    let start_total = starts.len();
    let mut start_points: Vec<_> = starts
        .into_iter()
        .take(START_LIMIT)
        .map(|n| {
            json!({
                "name":n.qualified_name,"kind":"program","evidence":n.evidence,"package":n.package,
            })
        })
        .collect();
    if start_points.is_empty() {
        for package in &ordered {
            for unit in package.units.iter().filter(|u| u.kind == "lib") {
                if let Some(hash) = hashes.get(&unit.source) {
                    start_points.push(
                        json!({"name":package.name,"kind":"library","package":package.id,
                        "evidence":{"path":unit.source,"content_hash":hash,"start_line":1}}),
                    );
                }
                if start_points.len() >= START_LIMIT {
                    break;
                }
            }
            if start_points.len() >= START_LIMIT {
                break;
            }
        }
    }
    if start_points.is_empty() {
        // Python libraries and loose sources: supply actual declaration locations,
        // explicitly not inferred framework routes or runtime entry points.
        let mut statement = index.connection.prepare(
            "SELECT data FROM nodes WHERE snapshot=?1 AND kind IN ('trait','type','function','module') ORDER BY path,start,id LIMIT 64")?;
        let rows = statement.query_map(params![snapshot.id], |r| r.get::<_, String>(0))?;
        let mut seen = BTreeSet::new();
        for row in rows {
            let node: Node = serde_json::from_str(&row?)?;
            if node.is_test || !seen.insert(node.evidence.path.clone()) {
                continue;
            }
            start_points.push(json!({"name":node.qualified_name,"kind":"source","package":node.package,"evidence":node.evidence}));
            if start_points.len() >= START_LIMIT {
                break;
            }
        }
    }
    Ok(json!({"packages":summaries,"packages_total":packages.len(),
        "packages_omitted":packages.len().saturating_sub(PACKAGE_LIMIT),
        "starts":start_points,"program_entries_total":start_total,
        "dependencies":dependencies,"dependency_basis":"declared internal production dependencies; not runtime call order"}))
}
