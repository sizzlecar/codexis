use super::{
    architecture::Component, artifacts::Artifact, dimension_scoped as dimension, item, short,
    small_node, stable_id, unique_evidence, Facts,
};
use crate::model::{Evidence, Node, Snapshot};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub(super) fn intent(
    snapshot: &Snapshot,
    facts: &Facts,
    components: &[Component],
    artifacts: &[Artifact],
) -> Value {
    let mut items = Vec::new();
    for package in &snapshot.project.packages {
        let manifest_name = if package.language == "python" {
            "pyproject.toml"
        } else {
            "Cargo.toml"
        };
        let path = if package.root.is_empty() {
            manifest_name.into()
        } else {
            format!("{}/{manifest_name}", package.root)
        };
        if let Some(manifest) = artifacts.iter().find(|a| a.path == path) {
            if let Ok(parsed) = toml::from_str::<toml::Value>(&manifest.content) {
                let section = if package.language == "python" {
                    "project"
                } else {
                    "package"
                };
                let value = parsed.get(section).and_then(|v| v.get("description"));
                let inherited = value
                    .and_then(|v| v.get("workspace"))
                    .and_then(toml::Value::as_bool)
                    == Some(true);
                let origin = if inherited {
                    artifacts
                        .iter()
                        .find(|a| a.path == "Cargo.toml")
                        .unwrap_or(manifest)
                } else {
                    manifest
                };
                let description = if inherited {
                    toml::from_str::<toml::Value>(&origin.content)
                        .ok()
                        .and_then(|v| {
                            v.get("workspace")?
                                .get("package")?
                                .get("description")?
                                .as_str()
                                .map(str::to_owned)
                        })
                } else {
                    value
                        .and_then(toml::Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| {
                            parsed
                                .get("tool")?
                                .get("poetry")?
                                .get("description")?
                                .as_str()
                                .map(str::to_owned)
                        })
                };
                if let Some(text) = description.filter(|s| !s.trim().is_empty()) {
                    let mut value = item(
                        stable_id("intent", &package.id),
                        &package.name,
                        short(&text, 600),
                        "manifest description; declared intent",
                        &[origin.evidence_for(&text)],
                        &[],
                    );
                    value["package_id"] = json!(package.id);
                    items.push(value);
                }
            }
        }
    }
    for artifact in artifacts.iter().filter(|a| a.kind == "documentation") {
        let mut value = artifact.item();
        value["basis"] = json!("stored documentation excerpt; declared intent");
        let references: Vec<_> = components
            .iter()
            .filter(|c| c.paths.iter().any(|p| artifact.content.contains(p)))
            .take(30)
            .collect();
        value["component_ids"] = json!(references.iter().map(|c| &c.id).collect::<Vec<_>>());
        value["node_ids"] = json!(references
            .iter()
            .flat_map(|c| c.node_ids.iter().take(3))
            .collect::<Vec<_>>());
        value["relation_basis"] = json!(
            "literal source-path mentions in document; not a verified implementation mapping"
        );
        items.push(value);
    }
    for component in components
        .iter()
        .filter(|c| c.basis.starts_with("source documentation"))
    {
        let mut value = component.value();
        value["id"] = json!(format!("intent:{}", component.id));
        items.push(value);
    }
    for node in &facts.nodes {
        if node.is_test || !matches!(node.kind.as_str(), "function" | "trait" | "type") {
            continue;
        }
        if let Some(doc) = doc(node).filter(|d| !d.trim().is_empty()) {
            let evidence = doc_evidence(node);
            items.push(item(
                format!("intent:{}", node.id),
                &node.qualified_name,
                short(doc, 600),
                "source documentation; declared intent",
                &evidence,
                std::slice::from_ref(&node.id),
            ));
        }
    }
    dimension(
        "intent",
        crate::localize!("能力与意图", "Capabilities and intent"),
        crate::localize!("关联项目声明、模块职责文档和实现符号，理解代码为什么存在。", "Connect project declarations, documented module responsibilities, and implementation symbols to understand intent."),
        items,
        vec![
            crate::localize!("职责和能力说明引用文档或清单原文；没有文档的模块仅在架构维度显示结构，不猜业务含义。", "Responsibilities and capabilities quote documentation or manifests. Undocumented modules show structure without inferred business meaning."),
            crate::localize!("文档声明可能滞后于实现；路径提及只提供阅读关联，不证明实现满足需求。", "Documentation may lag behind code; path mentions provide reading links, not proof that requirements are satisfied."),
        ],
        Some(facts),
    )
}

pub(super) fn data(facts: &Facts, artifacts: &[Artifact]) -> Value {
    let mut items = Vec::new();
    let mut children = BTreeMap::<&str, Vec<&Node>>::new();
    for node in &facts.nodes {
        if let Some(parent) = node.parent.as_deref() {
            children.entry(parent).or_default().push(node)
        }
    }
    for node in &facts.nodes {
        if matches!(node.kind.as_str(), "type" | "trait") {
            let direct = children.get(node.id.as_str()).cloned().unwrap_or_default();
            let members: Vec<_> = direct
                .iter()
                .filter(|n| matches!(n.kind.as_str(), "field" | "variant" | "variable"))
                .copied()
                .collect();
            let state_candidate = node
                .attributes
                .get("rust.kind")
                .is_some_and(|k| k == "enum" || k == "enum_item");
            let evidence = unique_evidence(
                std::iter::once(&node.evidence).chain(members.iter().map(|n| &n.evidence)),
                10,
            );
            let node_ids: Vec<_> = std::iter::once(node.id.clone())
                .chain(members.iter().take(99).map(|n| n.id.clone()))
                .collect();
            let mut value = item(
                format!("data:{}", node.id),
                &node.qualified_name,
                crate::localize!(
                    "{} 个已索引字段/变体{}。",
                    "{} indexed fields/variants{}.",
                    members.len(),
                    if state_candidate {
                        crate::localize!(
                            "；枚举可作为状态表示候选",
                            "; enum declaration is a candidate state representation"
                        )
                    } else {
                        ""
                    }
                ),
                if state_candidate {
                    "declaration fact; inference: enum is a state representation candidate"
                } else {
                    "type and member declarations"
                },
                &evidence,
                &node_ids,
            );
            value["state_candidate"] = json!(state_candidate);
            value["members"] = json!(members
                .iter()
                .take(100)
                .map(|n| {
                    let mut m = small_node(n);
                    m["declared_type"] = json!(n
                        .attributes
                        .get("rust.declared_type")
                        .or_else(|| n.attributes.get("declared_type"))
                        .or_else(|| n.attributes.get("python.declared_type"))
                        .or_else(|| n.attributes.get("type")));
                    m
                })
                .collect::<Vec<_>>());
            value["members_total"] = json!(members.len());
            value["members_truncated"] = json!(members.len() > 100);
            value["type_usages"] = json!(facts
                .outgoing(&node.id)
                .filter(|e| e.kind == "type_usage")
                .take(30)
                .collect::<Vec<_>>());
            items.push(value);
        } else if node.kind == "function" {
            let params = attr(node, "parameters");
            let return_type = attr(node, "return_type");
            let usages: Vec<_> = facts
                .outgoing(&node.id)
                .filter(|e| e.kind == "type_usage")
                .collect();
            if return_type.is_none() && usages.is_empty() {
                continue;
            }
            let evidence = unique_evidence(
                std::iter::once(&node.evidence).chain(usages.iter().map(|e| &e.evidence)),
                8,
            );
            let mut value = item(format!("contract:{}",node.id),&node.qualified_name,
                crate::localize!("输入 {}；返回 {}。", "Inputs: {}; return: {}.",short(params.unwrap_or(crate::localize!("未提取参数", "parameters not extracted")),220),short(return_type.unwrap_or(crate::localize!("未声明返回类型", "return type not declared")),140)),
                "parameter, return annotation and type-usage syntax; unresolved types are not assigned to declarations",
                &evidence,std::slice::from_ref(&node.id));
            value["parameters"] = json!(params);
            value["return_type"] = json!(return_type);
            value["type_usages"] = json!(usages.iter().take(100).collect::<Vec<_>>());
            value["type_usages_total"] = json!(usages.len());
            value["type_usages_truncated"] = json!(usages.len() > 100);
            items.push(value);
        } else if node.kind == "field"
            && node
                .parent
                .as_deref()
                .is_none_or(|id| facts.node(id).is_none_or(|n| n.kind != "type"))
        {
            let mut value = item(
                format!("field:{}", node.id),
                &node.qualified_name,
                &node.signature,
                "field declaration or explicit assignment syntax",
                std::slice::from_ref(&node.evidence),
                std::slice::from_ref(&node.id),
            );
            value["declared_type"] = json!(node
                .attributes
                .get("rust.declared_type")
                .or_else(|| node.attributes.get("declared_type"))
                .or_else(|| node.attributes.get("type")));
            items.push(value);
        }
    }
    for edge in &facts.edges {
        if matches!(
            edge.kind.as_str(),
            "reads" | "writes" | "assigns" | "field_read" | "field_write"
        ) {
            let ids: Vec<_> = std::iter::once(edge.source.clone())
                .chain(edge.target.clone())
                .collect();
            let mut value = item(
                format!("data_access:{}", edge.id),
                &edge.target_name,
                crate::localize!(
                    "{}；解析方式 {}。",
                    "{}; resolution: {}.",
                    edge.kind,
                    edge.resolution
                ),
                "indexed read/write syntax relation",
                std::slice::from_ref(&edge.evidence),
                &ids,
            );
            value["relation"] = json!(edge);
            items.push(value);
        }
    }
    items.extend(
        artifacts
            .iter()
            .filter(|a| a.kind == "schema")
            .map(Artifact::item),
    );
    dimension("data",crate::localize!("数据与状态", "Data and state"),crate::localize!("查看核心数据声明、输入输出类型、枚举状态候选和数据库/协议文件。", "Explore data declarations, input/output types, enum state candidates, and database/protocol artifacts."),items,
        vec![crate::localize!("类型注解和枚举声明不能证明状态迁移、数据流或资源生命周期；未推断运行时所有权与持久化行为。", "Annotations and enum declarations do not establish state transitions, data flow, or resource lifetimes; runtime ownership and persistence are not inferred."),
            crate::localize!("只展示已有字段/读写关系；缺少记录不代表不存在读写。未解析类型保留语法与证据。", "Only indexed fields and access relations are shown; absent records do not imply absent access. Unresolved types retain syntax and evidence.")],Some(facts))
}

pub(super) fn runtime(snapshot: &Snapshot, facts: &Facts, artifacts: &[Artifact]) -> Value {
    let mut items: Vec<_> = artifacts
        .iter()
        .filter(|a| {
            matches!(
                a.kind,
                "configuration" | "deployment" | "manifest" | "lockfile"
            )
        })
        .map(Artifact::item)
        .collect();
    for package in &snapshot.project.packages {
        if package.features.is_empty() {
            continue;
        }
        let manifests: Vec<_> = artifacts
            .iter()
            .filter(|a| {
                a.kind == "manifest"
                    && a.path.ends_with("Cargo.toml")
                    && (package.root.is_empty() && a.path == "Cargo.toml"
                        || a.path == format!("{}/Cargo.toml", package.root))
            })
            .collect();
        for (feature, values) in &package.features {
            let evidence: Vec<_> = manifests.iter().map(|a| a.evidence_for(feature)).collect();
            let mut value = item(
                stable_id("feature", &format!("{}/{}", package.id, feature)),
                format!("{} / feature {feature}", package.name),
                crate::localize!(
                    "声明启用项：{}",
                    "Declared enabled entries: {}",
                    values.join(", ")
                ),
                "Cargo feature declaration; not evidence of enabled runtime behavior",
                &evidence,
                &[],
            );
            value["feature"] = json!(feature);
            value["package_id"] = json!(package.id);
            value["declared_enables"] = json!(values);
            value["requested_in_analysis"] = json!(snapshot.context.features.contains(feature));
            items.push(value);
        }
    }
    for node in facts.nodes.iter().filter(|n| !n.conditions.is_empty()) {
        let mut value = item(
            format!("conditional:{}", node.id),
            &node.qualified_name,
            node.conditions.join("; "),
            "declared conditional-compilation or source conditions",
            std::slice::from_ref(&node.evidence),
            std::slice::from_ref(&node.id),
        );
        value["conditions"] = json!(node.conditions);
        items.push(value);
    }
    for edge in &facts.edges {
        let target = edge.target_name.as_str();
        if edge.kind == "calls"
            && (target.starts_with("std::env::")
                || target.starts_with("std::fs::")
                || target.starts_with("os.getenv")
                || target.starts_with("os.environ")
                || target.starts_with("subprocess.")
                || target.starts_with("std::process::")
                || target == "open")
        {
            let mut value = item(format!("runtime_boundary:{}",edge.id),target,
                crate::localize!("调用语法 {}；解析方式 {}。", "Call syntax: {}; resolution: {}.",target,edge.resolution),
                "inference: explicit call spelling suggests a runtime boundary; target binding may be unresolved",
                std::slice::from_ref(&edge.evidence),std::slice::from_ref(&edge.source));
            value["relation"] = json!(edge);
            items.push(value);
        }
    }
    dimension(
        "runtime",
        crate::localize!("配置与运行", "Configuration and runtime"),
        crate::localize!("查看配置、部署、依赖清单、feature 分支和显式运行边界候选。", "Explore configuration, deployment, manifests, feature conditions, and explicit runtime boundary candidates."),
        items,
        vec![
            crate::localize!("配置和部署文件只证明声明存在；没有执行构建、部署或推断当前运行环境。", "Configuration and deployment artifacts establish declarations only; no builds or deployments were executed and no live environment was inferred."),
            crate::localize!("feature 请求与默认启用/传递启用不同；配置值和外部资源绑定尚未完整解析。", "Requested features differ from default and transitive activation; configuration values and external resource bindings are not fully resolved."),
        ],
        Some(facts),
    )
}

pub(super) fn verification(facts: &Facts, components: &[Component]) -> Value {
    let tests: Vec<_> = facts
        .nodes
        .iter()
        .filter(|n| n.kind == "function" && n.is_test)
        .collect();
    let mut items = Vec::new();
    let mut linked = BTreeMap::<String, BTreeSet<String>>::new();
    for test in &tests {
        let mut queue = VecDeque::from([(test.id.as_str(), 0)]);
        let mut seen = BTreeSet::from([test.id.as_str()]);
        let mut relations = Vec::new();
        let mut unknown = 0;
        let mut truncated = false;
        while let Some((id, depth)) = queue.pop_front() {
            let calls: Vec<_> = facts.outgoing(id).filter(|e| e.kind == "calls").collect();
            if depth >= 3 {
                truncated |= !calls.is_empty();
                continue;
            }
            for edge in calls {
                if relations.len() >= 120 {
                    truncated = true;
                    break;
                }
                relations.push(edge);
                if let Some(target) = edge.target.as_deref().and_then(|id| facts.node(id)) {
                    if !target.is_test {
                        linked
                            .entry(target.id.clone())
                            .or_default()
                            .insert(test.id.clone());
                    }
                    if seen.len() >= 60 {
                        truncated = true;
                        continue;
                    }
                    if seen.insert(target.id.as_str()) {
                        queue.push_back((target.id.as_str(), depth + 1));
                    }
                } else if edge.resolution != "external" {
                    unknown += 1
                }
            }
        }
        let node_ids: Vec<_> = seen.iter().map(|s| (*s).to_owned()).collect();
        let evidence = unique_evidence(
            std::iter::once(&test.evidence).chain(relations.iter().map(|e| &e.evidence)),
            8,
        );
        let mut value = item(format!("test:{}",test.id),&test.qualified_name,
            crate::localize!("{} 条有界测试调用关系；{unknown} 个未解析调用。", "{} bounded test-call relations; {unknown} unresolved calls.",relations.len()),
            "declared test and indexed call relations; neither test execution nor coverage measurement",&evidence,&node_ids);
        value["related_nodes"] = json!(seen
            .iter()
            .filter_map(|id| facts.node(id))
            .filter(|n| !n.is_test)
            .map(small_node)
            .collect::<Vec<_>>());
        value["relations"] = json!(relations);
        value["unknown_calls"] = json!(unknown);
        value["truncated"] = json!(truncated);
        items.push(value);
    }
    for component in components {
        let related: BTreeSet<_> = component
            .node_ids
            .iter()
            .filter_map(|id| linked.get(id))
            .flatten()
            .cloned()
            .collect();
        let mut value = item(
            format!("verification:{}", component.id),
            &component.title,
            if related.is_empty() {
                crate::localize!("未发现可解析到该子系统的测试调用关系；可继续检查集成测试、动态调用和外部验证。", "No resolved test-call association was found for this subsystem; inspect integration tests, dynamic calls, and external validation.")
                    .into()
            } else {
                crate::localize!("{} 个测试函数沿有界已解析调用关联到该子系统。", "{} test functions are associated with this subsystem through bounded resolved calls.",
                    related.len()
                )
            },
            "bounded indexed test-call association; absence is not evidence of missing coverage",
            &component.evidence,
            &component
                .node_ids
                .iter()
                .take(100)
                .cloned()
                .collect::<Vec<_>>(),
        );
        value["related_test_ids"] = json!(related);
        value["node_ids_total"] = json!(component.node_ids.len());
        value["node_ids_truncated"] = json!(component.node_ids.len() > 100);
        items.push(value);
    }
    dimension(
        "verification",
        crate::localize!("验证与约束", "Verification and constraints"),
        crate::localize!("查看测试声明与代码的显式调用关联，定位仍需人工核查的证据缺口。", "Inspect declared tests and explicit call associations to identify evidence that still needs review."),
        items,
        vec![
            crate::localize!("没有执行测试，也没有测量覆盖率；未发现关联不等于没有测试或验证。", "Tests were not executed and coverage was not measured; missing associations do not imply missing tests or validation."),
            crate::localize!("测试关联最多展开 3 层、60 个节点、120 条边；动态测试、fixture 和外部测试可能遗漏。", "Test associations expand at most 3 levels, 60 nodes, and 120 edges; dynamic tests, fixtures, and external tests may be omitted."),
            crate::localize!("架构约束需要明确声明；没有声明时不自动推断禁止依赖或安全保证。", "Architecture constraints require explicit declarations; forbidden dependencies and safety guarantees are not inferred without them."),
        ],
        Some(facts),
    )
}

fn attr<'a>(node: &'a Node, name: &str) -> Option<&'a str> {
    node.attributes
        .get(&format!("rust.{name}"))
        .or_else(|| node.attributes.get(&format!("python.{name}")))
        .or_else(|| node.attributes.get(name))
        .map(String::as_str)
}

fn doc(node: &Node) -> Option<&str> {
    attr(node, "doc")
}

fn doc_evidence(node: &Node) -> Vec<Evidence> {
    for name in ["rust.doc_evidence", "python.doc_evidence", "doc_evidence"] {
        if let Some(raw) = node.attributes.get(name) {
            if let Ok(evidence) = serde_json::from_str::<Vec<Evidence>>(raw) {
                if !evidence.is_empty() {
                    return evidence;
                }
            }
        }
    }
    vec![node.evidence.clone()]
}
