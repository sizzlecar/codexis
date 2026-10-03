use super::{
    artifacts::Artifact, dimension as make_dimension, item, stable_id, unique_evidence, Facts,
    ITEM_LIMIT,
};
use crate::model::{Evidence, Node, Snapshot};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct Component {
    pub id: String,
    pub title: String,
    pub package_id: String,
    pub package_name: String,
    pub paths: BTreeSet<String>,
    pub node_ids: Vec<String>,
    pub evidence: Vec<Evidence>,
    pub summary: String,
    pub basis: String,
    pub symbol_count: usize,
}

impl Component {
    pub fn value(&self) -> Value {
        let mut value = item(
            &self.id,
            &self.title,
            &self.summary,
            &self.basis,
            &self.evidence,
            &self.node_ids.iter().take(100).cloned().collect::<Vec<_>>(),
        );
        value["package_id"] = json!(self.package_id);
        value["package_name"] = json!(self.package_name);
        value["paths"] = json!(self.paths.iter().take(100).collect::<Vec<_>>());
        value["paths_total"] = json!(self.paths.len());
        value["paths_truncated"] = json!(self.paths.len() > 100);
        value["symbol_count"] = json!(self.symbol_count);
        value["node_ids_total"] = json!(self.node_ids.len());
        value["node_ids_truncated"] = json!(self.node_ids.len() > 100);
        value
    }
}

pub(super) fn components(
    snapshot: &Snapshot,
    facts: &Facts,
    _artifacts: &[Artifact],
) -> Vec<Component> {
    let packages: BTreeMap<_, _> = snapshot
        .project
        .packages
        .iter()
        .map(|p| (&p.id, p))
        .collect();
    let mut groups = BTreeMap::<(String, String), Vec<&Node>>::new();
    for node in &facts.nodes {
        let root = packages.get(&node.package).map_or("", |p| p.root.as_str());
        let name = component_name(node, root, facts);
        groups
            .entry((node.package.clone(), name))
            .or_default()
            .push(node);
    }
    groups.into_iter().map(|((package_id, name), nodes)| {
        let package_name = packages.get(&package_id).map_or_else(|| package_id.clone(), |p| p.name.clone());
        let paths: BTreeSet<_> = nodes.iter().map(|n| n.evidence.path.clone()).collect();
        let functions = nodes.iter().filter(|n| n.kind == "function" && !n.is_test).count();
        let types = nodes.iter().filter(|n| matches!(n.kind.as_str(), "type" | "trait")).count();
        let tests = nodes.iter().filter(|n| n.kind == "function" && n.is_test).count();
        let documented = nodes.iter().filter(|n| n.kind == "module").find_map(|n| {
            n.attributes.get("rust.doc").or_else(|| n.attributes.get("python.doc")).or_else(|| n.attributes.get("doc"))
                .filter(|doc| !doc.trim().is_empty())
        });
        let mut evidence = Vec::new();
        for node in nodes.iter().filter(|n| n.kind == "module") {
            if let Some(raw) = node.attributes.get("rust.doc_evidence").or_else(|| node.attributes.get("python.doc_evidence")) {
                if let Ok(spans) = serde_json::from_str::<Vec<Evidence>>(raw) { evidence.extend(spans.into_iter().take(4)); }
            }
        }
        evidence.extend(unique_evidence(nodes.iter().map(|n| &n.evidence), 8));
        evidence.truncate(8);
        let (summary, basis) = if let Some(doc) = documented {
            (super::short(doc.trim(), 600), "source documentation; declared intent, not independently verified behavior".into())
        } else {
            (crate::localize!("{} 个源码文件；{functions} 个函数，{types} 个类型/接口，{tests} 个测试函数。", "{} source files; {functions} functions, {types} types/interfaces, {tests} test functions.", paths.len()),
                "structural grouping by declared module and source directory; responsibility not inferred".into())
        };
        Component {
            id:stable_id("component", &format!("{package_id}/{name}")),
            title:format!("{package_name} / {name}"), package_id,package_name,
            paths,node_ids:nodes.iter().map(|n| n.id.clone()).collect(),
            evidence,summary,basis,symbol_count:nodes.len(),
        }
    }).collect()
}

fn component_name(node: &Node, package_root: &str, facts: &Facts) -> String {
    let relative = if package_root.is_empty() {
        node.evidence.path.as_str()
    } else {
        node.evidence
            .path
            .strip_prefix(&format!("{package_root}/"))
            .unwrap_or(&node.evidence.path)
    };
    if relative.starts_with("tests/") {
        return "tests".into();
    }
    if relative.starts_with("examples/") {
        return "examples".into();
    }
    if node.language == "rust" {
        let mut cursor = Some(node);
        let mut branch = None;
        let mut seen = BTreeSet::new();
        while let Some(n) = cursor {
            if !seen.insert(n.id.as_str()) {
                break;
            }
            if n.kind == "module" {
                let parts: Vec<_> = n.qualified_name.split("::").collect();
                if parts.len() > 1 {
                    branch = Some(parts[1].to_owned());
                }
            }
            cursor = n.parent.as_deref().and_then(|id| facts.node(id));
        }
        if let Some(branch) = branch {
            return branch;
        }
    } else if node.language == "python" {
        let mut cursor = Some(node);
        let mut seen = BTreeSet::new();
        while let Some(n) = cursor {
            if !seen.insert(n.id.as_str()) {
                break;
            }
            if n.kind == "module" {
                let namespace = facts.python_namespaces.get(&n.package);
                let relative = namespace
                    .and_then(|namespace| n.qualified_name.strip_prefix(&format!("{namespace}.")))
                    .unwrap_or_else(|| {
                        if namespace.is_some_and(|namespace| &n.qualified_name == namespace) {
                            "core"
                        } else {
                            &n.qualified_name
                        }
                    });
                let branch = relative.split('.').next().unwrap_or(&n.name);
                return if matches!(branch, "__main__" | "__init__") {
                    "core".into()
                } else {
                    branch.to_owned()
                };
            }
            cursor = n.parent.as_deref().and_then(|id| facts.node(id));
        }
    }
    let source = relative.strip_prefix("src/").unwrap_or(relative);
    let first = source.split('/').next().unwrap_or(source);
    if !first.contains('.') {
        return first.to_owned();
    }
    let stem = first.rsplit_once('.').map_or(first, |(stem, _)| stem);
    match stem {
        "lib" | "main" | "mod" | "__init__" | "__main__" => "core".into(),
        _ => stem.into(),
    }
}

pub(super) fn build(
    snapshot: &Snapshot,
    facts: &Facts,
    components: &[Component],
    artifacts: &[Artifact],
) -> Value {
    let roots: BTreeMap<_, _> = snapshot
        .project
        .packages
        .iter()
        .map(|p| (p.id.as_str(), p.root.as_str()))
        .collect();
    let component_ids: BTreeSet<_> = components.iter().map(|c| c.id.as_str()).collect();
    let node_components: BTreeMap<_, _> = facts
        .nodes
        .iter()
        .filter_map(|n| {
            let root = roots.get(n.package.as_str()).copied().unwrap_or("");
            let key = stable_id(
                "component",
                &format!("{}/{}", n.package, component_name(n, root, facts)),
            );
            component_ids
                .get(key.as_str())
                .map(|id| (n.id.as_str(), *id))
        })
        .collect();
    let mut groups = BTreeMap::<(String, String, String, String), Vec<&crate::model::Edge>>::new();
    for edge in &facts.edges {
        if !matches!(edge.kind.as_str(), "calls" | "imports") {
            continue;
        }
        let Some(source) = node_components.get(edge.source.as_str()) else {
            continue;
        };
        let Some(target) = edge
            .target
            .as_deref()
            .and_then(|id| node_components.get(id))
        else {
            continue;
        };
        if source == target {
            continue;
        }
        groups
            .entry((
                (*source).to_owned(),
                (*target).to_owned(),
                edge.kind.clone(),
                edge.resolution.clone(),
            ))
            .or_default()
            .push(edge);
    }
    let dependencies:Vec<_> = groups.into_iter().map(|((source,target,kind,resolution), edges)| {
        let evidence = unique_evidence(edges.iter().map(|e| &e.evidence), 20);
        let node_ids:BTreeSet<_> = edges.iter().flat_map(|e| std::iter::once(e.source.clone()).chain(e.target.clone())).collect();
        json!({"id":stable_id("dependency", &format!("{source}/{target}/{kind}/{resolution}")),
            "source":source,"target":target,"kind":kind,"resolution":resolution,"count":edges.len(),
            "basis":"indexed relation with a known target; call relations do not establish runtime order",
            "evidence":evidence,"evidence_total":edges.len(),"evidence_truncated":edges.len() > 20,
            "node_ids":node_ids.iter().take(100).collect::<Vec<_>>(),"node_ids_total":node_ids.len(),
            "node_ids_truncated":node_ids.len() > 100})
    }).collect();
    let imports:Vec<_> = facts.edges.iter().filter(|e| e.kind == "imports").map(|edge| {
        let source = node_components.get(edge.source.as_str()).copied();
        let target = edge.target.as_deref().and_then(|id| node_components.get(id)).copied();
        let node_ids:Vec<_> = std::iter::once(edge.source.clone()).chain(edge.target.clone()).collect();
        let mut value = item(format!("import:{}",edge.id),&edge.target_name,
            crate::localize!("导入声明；解析方式 {}。", "Import declaration; resolution: {}.",edge.resolution),
            "import syntax; a missing target remains unresolved and creates no definite component dependency",
            std::slice::from_ref(&edge.evidence),&node_ids);
        value["source"] = json!(source);
        value["target"] = json!(target);
        value["relation"] = json!(edge);
        value
    }).collect();
    let mut declared = Vec::new();
    for package in &snapshot.project.packages {
        let manifest = artifacts.iter().find(|a| {
            a.kind == "manifest"
                && (if package.root.is_empty() {
                    !a.path.contains('/')
                } else {
                    a.path
                        .strip_prefix(&format!("{}/", package.root))
                        .is_some_and(|s| !s.contains('/'))
                })
        });
        for dep in &package.dependencies {
            let target = dep
                .path
                .as_ref()
                .and_then(|root| snapshot.project.packages.iter().find(|p| &p.root == root));
            declared.push(json!({"source":package.id,"source_name":package.name,
                "target":target.map(|p| &p.id),"target_name":dep.package,"alias":dep.alias,
                "kind":dep.kind,"optional":dep.optional,"condition":dep.condition,
                "resolution":dep.resolution,"basis":"manifest declaration; separate from module and call relations",
                "evidence":manifest.map(|m| vec![m.evidence_for(&dep.alias)]).unwrap_or_default(),"node_ids":[]}));
        }
    }
    declared.sort_by_key(Value::to_string);
    let component_values: Vec<_> = components
        .iter()
        .map(Component::value)
        .filter(|value| super::matches_scoped_item(value, facts))
        .collect();
    let selected_ids: BTreeSet<_> = component_values
        .iter()
        .filter_map(|c| c["id"].as_str())
        .collect();
    let select_relation = |value: &Value| {
        facts.scope.is_none()
            || super::matches_item(value, facts.scope.as_deref())
            || value["source"]
                .as_str()
                .is_some_and(|id| selected_ids.contains(id))
            || value["target"]
                .as_str()
                .is_some_and(|id| selected_ids.contains(id))
    };
    let dependencies: Vec<_> = dependencies.into_iter().filter(select_relation).collect();
    let imports: Vec<_> = imports.into_iter().filter(select_relation).collect();
    let declared: Vec<_> = declared
        .into_iter()
        .filter(|value| super::matches_item(value, facts.scope.as_deref()))
        .collect();
    let dependency_total = dependencies.len();
    let declared_total = declared.len();
    let imports_total = imports.len();
    json!({"components":component_values.iter().take(ITEM_LIMIT).collect::<Vec<_>>(),
        "total":component_values.len(),"truncated":component_values.len() > ITEM_LIMIT,
        "dependencies":dependencies.into_iter().take(ITEM_LIMIT).collect::<Vec<_>>(),
        "dependencies_total":dependency_total,"dependencies_truncated":dependency_total > ITEM_LIMIT,
        "declared_dependencies":declared.into_iter().take(ITEM_LIMIT).collect::<Vec<_>>(),
        "declared_dependencies_total":declared_total,"declared_dependencies_truncated":declared_total > ITEM_LIMIT,
        "imports":imports.into_iter().take(ITEM_LIMIT).collect::<Vec<_>>(),
        "imports_total":imports_total,"imports_truncated":imports_total > ITEM_LIMIT,
        "basis":"subsystems group declared modules and top-level source directories; only known-target relations form component edges"})
}

pub(super) fn dimension(architecture: &Value, components: &[Component], facts: &Facts) -> Value {
    let mut items: Vec<_> = components
        .iter()
        .map(Component::value)
        .filter(|value| super::matches_scoped_item(value, facts))
        .collect();
    let component_total = items.len();
    for edge in architecture["dependencies"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let name = |id: &str| {
            components
                .iter()
                .find(|c| c.id == id)
                .map_or(id.to_owned(), |c| c.title.clone())
        };
        let mut edge = edge.clone();
        edge["title"] = json!(format!(
            "{} → {}",
            name(edge["source"].as_str().unwrap_or("")),
            name(edge["target"].as_str().unwrap_or(""))
        ));
        edge["summary"] = json!(crate::localize!(
            "{} 条 {} 关系，解析方式 {}。",
            "{} {} relations; resolution: {}.",
            edge["count"],
            edge["kind"].as_str().unwrap_or(""),
            edge["resolution"].as_str().unwrap_or("")
        ));
        items.push(edge);
    }
    items.extend(
        architecture["imports"]
            .as_array()
            .into_iter()
            .flatten()
            .cloned(),
    );
    let mut value = make_dimension(
        "architecture",
        crate::localize!("架构与边界", "Architecture and boundaries"),
        crate::localize!("查看单包内部与跨包子系统、已解析依赖和声明依赖。", "Explore subsystems within and across packages, resolved relations, and declared dependencies."),
        items,
        vec![
            crate::localize!("目录和模块划分代表源码结构；业务职责仅引用已有文档，不由名称推断。", "Directories and modules describe source structure; responsibilities quote existing documentation rather than infer business roles from names."),
            crate::localize!("未解析的导入和调用没有生成确定的子系统依赖；声明依赖单独展示。", "Unresolved imports and calls create no definite subsystem dependency; manifest dependencies are shown separately."),
            crate::localize!("默认显示最多 100 个组件和 100 组依赖；完整数量与截断信息见 architecture。", "The default view shows up to 100 components and 100 dependency groups; architecture reports full counts and truncation."),
        ],
    );
    let total = component_total
        + architecture["dependencies_total"].as_u64().unwrap_or(0) as usize
        + architecture["imports_total"].as_u64().unwrap_or(0) as usize;
    value["total"] = json!(total);
    value["truncated"] = json!(total > ITEM_LIMIT);
    value
}
