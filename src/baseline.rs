//! Bounded, immutable source context for explaining a project's purpose and
//! collaboration. Declared dependency routes are reading aids, not business
//! explanations or execution traces.
use crate::{
    index::Index,
    model::{identity, Evidence, Node, Package, Snapshot},
};
use anyhow::Result;
use rusqlite::params;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const ROUTE_LIMIT: usize = 4;
const ROUTE_DEPTH: usize = 6;
const CONTEXT_CHARS: usize = 24_000;

#[derive(Clone)]
struct Document {
    path: String,
    hash: String,
    content: String,
}

impl Document {
    fn span(&self, start: usize, end: usize) -> Evidence {
        let position = |byte| {
            let before = &self.content[..byte];
            (
                before.bytes().filter(|b| *b == b'\n').count() + 1,
                before
                    .rsplit_once('\n')
                    .map_or(before.len(), |(_, tail)| tail.len())
                    + 1,
            )
        };
        let (start_line, start_column) = position(start);
        let (end_line, end_column) = position(end);
        Evidence {
            path: self.path.clone(),
            content_hash: self.hash.clone(),
            start_byte: start,
            end_byte: end,
            start_line,
            start_column,
            end_line,
            end_column,
        }
    }

    fn declaration(&self, text: &str) -> Evidence {
        let exact = self.content.find(text);
        let offset = exact
            .or_else(|| {
                self.content
                    .split_inclusive('\n')
                    .scan(0, |offset, line| {
                        let start = *offset;
                        *offset += line.len();
                        Some((start, line))
                    })
                    .find(|(_, line)| {
                        line.trim()
                            .split_once('=')
                            .is_some_and(|(key, _)| key.trim() == "description")
                    })
                    .map(|(start, _)| start)
            })
            .unwrap_or(0);
        let start = self.content[..offset].rfind('\n').map_or(0, |i| i + 1);
        let end = exact.map_or_else(
            || {
                self.content[offset..]
                    .find('\n')
                    .map_or(self.content.len(), |i| offset + i)
            },
            |offset| offset + text.len(),
        );
        self.span(start, end)
    }
}

struct Sources<'a> {
    index: &'a Index,
    hashes: BTreeMap<String, String>,
    loaded: BTreeMap<String, Document>,
}

impl<'a> Sources<'a> {
    fn document(&mut self, path: &str) -> Result<Option<Document>> {
        let Some(hash) = self.hashes.get(path) else {
            return Ok(None);
        };
        if !self.loaded.contains_key(path) {
            self.loaded.insert(
                path.into(),
                Document {
                    path: path.into(),
                    hash: hash.clone(),
                    content: self.index.content(hash)?,
                },
            );
        }
        Ok(self.loaded.get(path).cloned())
    }
}

fn manifest_path(package: &Package) -> String {
    let filename = if package.language == "python" {
        "pyproject.toml"
    } else {
        "Cargo.toml"
    };
    if package.root.is_empty() {
        filename.into()
    } else {
        format!("{}/{filename}", package.root)
    }
}

fn description(sources: &mut Sources<'_>, package: &Package) -> Result<Option<Value>> {
    let Some(mut document) = sources.document(&manifest_path(package))? else {
        return Ok(None);
    };
    let Ok(manifest) = toml::from_str::<toml::Value>(&document.content) else {
        return Ok(None);
    };
    let section = if package.language == "python" {
        "project"
    } else {
        "package"
    };
    let declaration = manifest.get(section).and_then(|p| p.get("description"));
    let text = if declaration
        .and_then(|d| d.get("workspace"))
        .and_then(toml::Value::as_bool)
        == Some(true)
    {
        let Some(root) = sources.document("Cargo.toml")? else {
            return Ok(None);
        };
        document = root;
        toml::from_str::<toml::Value>(&document.content)
            .ok()
            .and_then(|m| {
                m.get("workspace")?
                    .get("package")?
                    .get("description")?
                    .as_str()
                    .map(str::to_owned)
            })
    } else {
        declaration
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                manifest
                    .get("tool")?
                    .get("poetry")?
                    .get("description")?
                    .as_str()
                    .map(str::to_owned)
            })
    };
    Ok(text.filter(|text| !text.trim().is_empty()).map(|text| {
        json!({"text":text,"package_id":package.id,"evidence":[document.declaration(&text)],
            "basis":crate::localize!("清单中的目的声明原文", "Original purpose declaration in the manifest")})
    }))
}

/// Skip Markdown decoration and take a complete opening prose paragraph. The
/// returned range is copied from the stored document without rewriting it.
fn purpose_paragraph(content: &str) -> Option<(usize, usize)> {
    let mut offset = 0;
    let mut start = None;
    let mut end = 0;
    let mut fenced = false;
    let mut html_comment = false;
    for line in content.split_inclusive('\n') {
        let text = line.trim();
        let decoration = text.is_empty()
            || text.starts_with('#')
            || text.starts_with("![")
            || text.starts_with("[![")
            || text.starts_with('<')
            || text.starts_with("- ")
            || text.starts_with("* ")
            || text.starts_with("| ")
            || text.starts_with("[!")
            || text.chars().all(|c| matches!(c, '=' | '-' | '*' | ' '));
        if text.starts_with("```") || text.starts_with("~~~") {
            fenced = !fenced;
            if start.is_some() {
                break;
            }
        }
        if text.starts_with("<!--") {
            html_comment = true;
        }
        if start.is_some() && (decoration || fenced || html_comment) {
            break;
        }
        if !decoration && !fenced && !html_comment {
            start.get_or_insert(offset);
            end = offset + line.trim_end_matches(['\n', '\r']).len();
        }
        if text.contains("-->") {
            html_comment = false;
        }
        offset += line.len();
    }
    start.map(|start| (start, end))
}

fn entries(index: &Index, snapshot: &Snapshot) -> Result<Vec<Node>> {
    let units: BTreeMap<_, _> = snapshot
        .project
        .packages
        .iter()
        .flat_map(|p| &p.units)
        .map(|u| (u.id.as_str(), u))
        .collect();
    let mut statement = index.connection.prepare(
        "SELECT data FROM nodes WHERE snapshot=?1 AND ((name='main' AND kind='function') OR json_extract(data,'$.attributes.entry_kind') IN ('python_main','python_script')) ORDER BY path,start,id",
    )?;
    let rows = statement.query_map([&snapshot.id], |row| row.get::<_, String>(0))?;
    let mut result = Vec::new();
    for row in rows {
        let mut node: Node = serde_json::from_str(&row?)?;
        if node.is_test {
            continue;
        }
        let python_entry = node
            .attributes
            .get("entry_kind")
            .is_some_and(|kind| matches!(kind.as_str(), "python_main" | "python_script"));
        let bin_entry = units.get(node.unit.as_str()).is_some_and(|unit| {
            unit.kind == "bin"
                && node.qualified_name == format!("{}::main", unit.name.replace('-', "_"))
        });
        if bin_entry || python_entry {
            if bin_entry {
                node.attributes.insert("entry_kind".into(), "bin".into());
            }
            result.push(node);
        }
    }
    Ok(result)
}

fn node_value(node: &Node) -> Value {
    json!({"id":node.id,"name":node.qualified_name,"kind":node.kind,"package":node.package,
        "signature":node.signature,"conditions":node.conditions,"evidence":node.evidence})
}

fn dimension<'a>(understanding: &'a Value, id: &str) -> &'a [Value] {
    understanding["dimensions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|dimension| dimension["id"] == id)
        .and_then(|dimension| dimension["items"].as_array())
        .map_or(&[], Vec::as_slice)
}

struct Relation {
    source: String,
    target: String,
    value: Value,
}

fn dependency_evidence(document: &Document, alias: &str) -> Option<Evidence> {
    let mut section = false;
    let mut offset = 0;
    for line in document.content.split_inclusive('\n') {
        let text = line.trim();
        if text.starts_with('[') {
            section = text.contains("dependencies")
                && !text.contains("dev-dependencies")
                && !text.contains("build-dependencies");
            if section && text.trim_end_matches(']').ends_with(&format!(".{alias}")) {
                return Some(
                    document.span(offset, offset + line.trim_end_matches(['\n', '\r']).len()),
                );
            }
        } else if section
            && text
                .split_once('=')
                .is_some_and(|(key, _)| key.trim().trim_matches(['\'', '"']) == alias)
        {
            return Some(document.span(offset, offset + line.trim_end_matches(['\n', '\r']).len()));
        }
        offset += line.len();
    }
    None
}

fn relations(sources: &mut Sources<'_>, snapshot: &Snapshot) -> Result<Vec<Relation>> {
    let packages: BTreeMap<_, _> = snapshot
        .project
        .packages
        .iter()
        .map(|p| (p.root.as_str(), p))
        .collect();
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    for package in &snapshot.project.packages {
        for dependency in &package.dependencies {
            if !matches!(
                dependency.kind.as_str(),
                "normal" | "compile" | "provided" | "system"
            ) {
                continue;
            }
            let Some(target) = dependency
                .path
                .as_deref()
                .and_then(|path| packages.get(path))
            else {
                continue;
            };
            if !seen.insert((
                &package.id,
                &target.id,
                &dependency.alias,
                &dependency.condition,
                dependency.optional,
            )) {
                continue;
            }
            let mut evidence = sources
                .document(&manifest_path(package))?
                .and_then(|doc| dependency_evidence(&doc, &dependency.alias));
            if evidence.is_none() {
                evidence = sources
                    .document("Cargo.toml")?
                    .and_then(|doc| dependency_evidence(&doc, &dependency.alias));
            }
            result.push(Relation {
                source: package.id.clone(), target: target.id.clone(),
                value: json!({"source":package.id,"source_name":package.name,"target":target.id,"target_name":target.name,
                    "alias":dependency.alias,"kind":dependency.kind,"optional":dependency.optional,
                    "condition":dependency.condition,"resolution":"declared","evidence":evidence.into_iter().collect::<Vec<_>>(),
                    "basis":crate::localize!("内部生产依赖声明；箭头从使用方指向被依赖方", "Declared internal production dependency; arrow points from consumer to dependency")}),
            });
        }
    }
    Ok(result)
}

fn walk_routes(
    current: &str,
    outgoing: &BTreeMap<String, Vec<usize>>,
    relations: &[Relation],
    seen: &mut BTreeSet<String>,
    path: &mut Vec<usize>,
    result: &mut Vec<Vec<usize>>,
    truncated: &mut bool,
) {
    if result.len() >= 64 {
        *truncated = true;
        return;
    }
    let edges = outgoing.get(current).map_or(&[][..], Vec::as_slice);
    if edges.is_empty() || path.len() >= ROUTE_DEPTH {
        *truncated |= !edges.is_empty();
        result.push(path.clone());
        return;
    }
    *truncated |= edges.len() > 6;
    for edge in edges.iter().take(6) {
        path.push(*edge);
        let target = &relations[*edge].target;
        if seen.insert(target.clone()) {
            walk_routes(target, outgoing, relations, seen, path, result, truncated);
            seen.remove(target);
        } else {
            result.push(path.clone());
        }
        path.pop();
        if result.len() >= 64 {
            break;
        }
    }
}

fn contracts(index: &Index, snapshot: &Snapshot, package: &Package) -> Result<Vec<Node>> {
    let production: BTreeSet<_> = package
        .units
        .iter()
        .filter(|u| {
            matches!(
                u.kind.as_str(),
                "lib" | "bin" | "python" | "module" | "script"
            )
        })
        .map(|u| u.id.as_str())
        .collect();
    let mut statement = index.connection.prepare(
        "SELECT data FROM nodes WHERE snapshot=?1 AND package=?2 AND kind IN ('trait','type','function') AND (json_extract(data,'$.visibility') LIKE 'pub%' OR json_extract(data,'$.visibility')='public') ORDER BY CASE kind WHEN 'trait' THEN 0 WHEN 'type' THEN 1 ELSE 2 END,path,start LIMIT 64",
    )?;
    let rows = statement.query_map(params![snapshot.id, package.id], |row| {
        row.get::<_, String>(0)
    })?;
    let mut result = Vec::new();
    for row in rows {
        let node: Node = serde_json::from_str(&row?)?;
        if !node.is_test
            && (package.language == "python" || production.contains(node.unit.as_str()))
        {
            result.push(node);
        }
    }
    Ok(result)
}

struct ContextRequest<'a> {
    title: &'a str,
    kind: &'a str,
    reason: &'a str,
    evidence: &'a Evidence,
    node_ids: &'a [String],
    max_chars: usize,
}

fn append_context(
    sources: &mut Sources<'_>,
    output: &mut Vec<Value>,
    remaining: &mut usize,
    request: ContextRequest<'_>,
) -> Result<()> {
    let ContextRequest {
        title,
        kind,
        reason,
        evidence,
        node_ids,
        max_chars,
    } = request;
    if *remaining == 0
        || output.iter().any(|item| {
            item["evidence"]["path"] == evidence.path
                && item["evidence"]["start_byte"] == evidence.start_byte
        })
    {
        return Ok(());
    }
    let Some(document) = sources.document(&evidence.path)? else {
        return Ok(());
    };
    if document.hash != evidence.content_hash
        || evidence.end_byte > document.content.len()
        || evidence.start_byte > evidence.end_byte
        || !document.content.is_char_boundary(evidence.start_byte)
        || !document.content.is_char_boundary(evidence.end_byte)
    {
        return Ok(());
    }
    let excerpt = &document.content[evidence.start_byte..evidence.end_byte];
    let cap = max_chars.min(*remaining);
    let bytes = excerpt
        .char_indices()
        .nth(cap)
        .map_or(excerpt.len(), |(byte, _)| byte);
    let text = &excerpt[..bytes];
    *remaining -= text.chars().count();
    let excerpt_evidence = document.span(evidence.start_byte, evidence.start_byte + bytes);
    let id = identity(&[
        &excerpt_evidence.path,
        &excerpt_evidence.content_hash,
        &excerpt_evidence.start_byte.to_string(),
        &excerpt_evidence.end_byte.to_string(),
    ]);
    output.push(json!({"id":format!("source:{}", &id[..24]),"title":title,
        "kind":if matches!(kind,"project_documentation"|"purpose_declaration") { "declared" } else { "source" },"role":kind,"reason":reason,"text":text,
        "evidence":excerpt_evidence,"node_ids":node_ids,
        "truncated":bytes < excerpt.len(),"complete_evidence":evidence}));
    Ok(())
}

fn source_neighborhood(
    index: &Index,
    snapshot: &Snapshot,
    entries: &[Node],
    sources: &mut Sources<'_>,
    context: &mut Vec<Value>,
    remaining: &mut usize,
) -> Result<(Vec<Value>, bool)> {
    let mut queue: VecDeque<_> = entries.iter().take(3).cloned().map(|n| (n, 0)).collect();
    let mut seen = BTreeSet::new();
    let mut unknown = Vec::new();
    let mut truncated = entries.len() > 3;
    while let Some((node, depth)) = queue.pop_front() {
        if seen.contains(&node.id) {
            continue;
        }
        if seen.len() >= 9 {
            truncated = true;
            break;
        }
        seen.insert(node.id.clone());
        append_context(
            sources,
            context,
            remaining,
            ContextRequest {
                title: &node.qualified_name,
                kind: if depth == 0 {
                    "entry_body"
                } else {
                    "resolved_call_body"
                },
                reason: if depth == 0 {
                    crate::localize!(
                    "真实入口函数体：确认输入从哪里进入和交给哪些实现。",
                    "Actual entry body: identify incoming inputs and the implementations it calls."
                )
                } else {
                    crate::localize!("入口附近已解析调用的真实函数体：阅读具体处理和返回，而非只看函数名。", "Actual body of a resolved call near the entry: read its processing and return behavior.")
                },
                evidence: &node.evidence,
                node_ids: std::slice::from_ref(&node.id),
                max_chars: 3_200,
            },
        )?;
        if depth >= 2 {
            truncated |= !index
                .connected_calls(&snapshot.id, &node.id, false, 1)?
                .is_empty();
            continue;
        }
        let mut edges = index.connected_calls(&snapshot.id, &node.id, false, 25)?;
        truncated |= edges.len() > 24;
        edges.truncate(24);
        edges.sort_by_key(|edge| edge.evidence.start_byte);
        for edge in edges {
            if let Some(target) = &edge.target {
                if queue.len() < 12 {
                    if let Some(target) = index
                        .find_nodes(&snapshot.id, target, 1)?
                        .into_iter()
                        .next()
                    {
                        if !target.is_test {
                            queue.push_back((target, depth + 1));
                        }
                    }
                } else {
                    truncated = true;
                }
            } else if edge.resolution != "external" && unknown.len() < 4 {
                unknown.push(json!({"question":crate::localize!("{} 在调用 {} 时，实际接到哪个实现？", "Which implementation does {} invoke through {}?", node.qualified_name,edge.target_name),
                    "why":crate::localize!("这个入口附近的调用尚未绑定目标；需阅读实现或补足解析才能说明后续协作。", "This call near the entry has no bound target; inspect its implementation or resolve it to explain subsequent collaboration."),
                    "object":{"kind":"unresolved_call","id":edge.id,"title":edge.target_name,"source":node.id},
                    "evidence":[edge.evidence],"node_ids":[node.id]}));
            }
        }
    }
    Ok((unknown, truncated))
}

/// Produce the source/evidence packet used by a reader or an explanation model.
/// A completed business explanation must be supplied by the reading workflow;
/// this deterministic packet intentionally keeps `structural_only` status.
pub fn build(
    index: &Index,
    snapshot: &Snapshot,
    understanding: &Value,
    reading_guide: &Value,
) -> Result<Value> {
    crate::source::check_cancelled()?;
    let mut sources = Sources {
        index,
        hashes: index.file_hashes(&snapshot.id)?,
        loaded: BTreeMap::new(),
    };
    let entries = entries(index, snapshot)?;
    let packages: BTreeMap<_, _> = snapshot
        .project
        .packages
        .iter()
        .map(|p| (p.id.as_str(), p))
        .collect();
    let readme_path = sources
        .hashes
        .keys()
        .filter(|path| !path.contains('/') && path.to_ascii_lowercase().starts_with("readme"))
        .min_by_key(|path| (!path.eq_ignore_ascii_case("readme.md"), *path))
        .cloned();
    let mut purpose = Value::Null;
    let mut context = Vec::new();
    let mut remaining = CONTEXT_CHARS;
    if let Some(document) = readme_path
        .as_deref()
        .map(|path| sources.document(path))
        .transpose()?
        .flatten()
    {
        if let Some((start, end)) = purpose_paragraph(&document.content) {
            purpose = json!({"status":"declared","text":document.content[start..end],
                "basis":crate::localize!("根 README 开头目的段原文", "Original opening purpose paragraph in the root README"),
                "evidence":[document.span(start,end)],"node_ids":[],
                "so_what":crate::localize!("这是项目自述的目标；后续阅读应解释入口、接口和状态如何实现这段声明。", "This is the project's stated goal; subsequent reading should explain how entries, interfaces, and state implement it.")});
        }
        append_context(&mut sources,&mut context,&mut remaining,ContextRequest {
            title: &document.path,kind: "project_documentation",
            reason: crate::localize!("项目原文提供目标、使用方式和领域词汇，供结合源码解释。", "Project documentation supplies goals, usage, and domain terms to connect with source."),
            evidence: &document.span(0,document.content.len()),node_ids: &[],max_chars:4_500,
        })?;
    }
    if purpose.is_null() {
        let candidate = snapshot
            .project
            .packages
            .iter()
            .find(|p| p.root.is_empty())
            .or_else(|| {
                entries
                    .first()
                    .and_then(|n| packages.get(n.package.as_str()).copied())
            });
        if let Some(package) = candidate {
            if let Some(mut value) = description(&mut sources, package)? {
                value["status"] = json!("declared");
                value["node_ids"] = json!([]);
                value["so_what"] = json!(crate::localize!("清单提供目的声明；需要结合入口和契约解释它对应的业务过程。", "The manifest states intent; connect it with entries and contracts to explain the corresponding business process."));
                purpose = value;
            }
        }
    }
    if purpose.is_null() {
        purpose = json!({"status":"unknown","text":null,"evidence":[],"node_ids":[],
            "basis":crate::localize!("快照中未找到根 README 目的段或项目清单描述。", "No root README purpose paragraph or project manifest description was found in the snapshot."),
            "so_what":crate::localize!("项目解决什么问题仍需回答；包名与依赖不足以确定业务目标。", "The problem this project solves remains to be answered; package names and dependencies do not establish its business goal.")});
    }

    for evidence in purpose["evidence"].as_array().into_iter().flatten() {
        if let Ok(evidence) = serde_json::from_value::<Evidence>(evidence.clone()) {
            append_context(
                &mut sources,&mut context,&mut remaining,ContextRequest {
                    title: &evidence.path,kind: "purpose_declaration",
                    reason: crate::localize!("目的声明的准确原文范围，供业务解释引用。", "Exact original span of the purpose declaration for citation in a business explanation."),
                    evidence: &evidence,node_ids: &[],max_chars: 1_200,
                },
            )?;
        }
    }
    let relations = relations(&mut sources, snapshot)?;
    let mut incoming = BTreeMap::<String, BTreeSet<String>>::new();
    let mut outgoing = BTreeMap::<String, Vec<usize>>::new();
    for (i, relation) in relations.iter().enumerate() {
        outgoing.entry(relation.source.clone()).or_default().push(i);
        incoming
            .entry(relation.target.clone())
            .or_default()
            .insert(relation.source.clone());
    }
    for edges in outgoing.values_mut() {
        edges.sort_by_key(|i| {
            (
                relations[*i].value["optional"].as_bool().unwrap_or(false),
                std::cmp::Reverse(incoming.get(&relations[*i].target).map_or(0, BTreeSet::len)),
                relations[*i].target.clone(),
            )
        });
    }
    let mut seeds: Vec<(String, Value)> = entries
        .iter()
        .take(3)
        .map(|n| (n.package.clone(), node_value(n)))
        .collect();
    if seeds.is_empty() {
        seeds.extend(
            reading_guide["starts"]
                .as_array()
                .into_iter()
                .flatten()
                .take(3)
                .filter_map(|start| Some((start["package"].as_str()?.into(), start.clone()))),
        );
    }
    let mut paths = Vec::new();
    let mut truncated = false;
    let mut selected_packages = Vec::new();
    let mut selected_ids = BTreeSet::new();
    for (seed, entry) in seeds {
        let mut routes = Vec::new();
        walk_routes(
            &seed,
            &outgoing,
            &relations,
            &mut BTreeSet::from([seed.clone()]),
            &mut Vec::new(),
            &mut routes,
            &mut truncated,
        );
        routes.sort_by_key(|route| std::cmp::Reverse(route.len()));
        truncated |= routes.len() > 2;
        for route in routes.into_iter().take(2) {
            if paths.len() >= ROUTE_LIMIT {
                truncated = true;
                break;
            }
            let ids: Vec<_> = std::iter::once(seed.as_str())
                .chain(route.iter().map(|i| relations[*i].target.as_str()))
                .collect();
            let mut roles = Vec::new();
            for id in &ids {
                let Some(package) = packages.get(id) else {
                    continue;
                };
                if selected_ids.insert((*id).to_owned()) {
                    selected_packages.push(*package);
                }
                roles.push(
                    json!({"id":package.id,"name":package.name,"root":package.root,
                    "declared_purpose":description(&mut sources,package)?}),
                );
            }
            let title = ids
                .iter()
                .filter_map(|id| packages.get(id).map(|p| p.name.as_str()))
                .collect::<Vec<_>>()
                .join(" → ");
            let route_relations: Vec<_> =
                route.iter().map(|i| relations[*i].value.clone()).collect();
            paths.push(json!({"title":title,"entry":entry,"packages":roles,"relations":route_relations,
                "summary":if route.is_empty() {
                    crate::localize!("这个阅读入口未声明其他内部生产依赖；协作需从包内调用和契约继续阅读。", "This reading entry declares no other internal production dependencies; continue through calls and contracts within the package.")
                } else {
                    crate::localize!("从入口所属包沿内部生产依赖声明连接使用方和承载方；逐项核对可选依赖与条件。", "Connect consumers and providers from the entry package through declared internal production dependencies; inspect optional dependencies and conditions.")
                },
                "so_what":crate::localize!("这条路径定位了需要一起阅读的代码；各包如何共同完成项目目标仍需结合函数体、契约和文档解释。", "This route locates code to read together; explaining how the packages accomplish the project goal requires their bodies, contracts, and documentation."),
                "basis":crate::localize!("声明依赖阅读路径；不证明业务流程或运行时顺序", "Reading route through declared dependencies; establishes neither a business workflow nor runtime order")}));
        }
    }
    if selected_packages.is_empty() {
        selected_packages.extend(snapshot.project.packages.iter().take(3));
    }

    let (mut questions, neighborhood_truncated) = source_neighborhood(
        index,
        snapshot,
        &entries,
        &mut sources,
        &mut context,
        &mut remaining,
    )?;
    let purpose_evidence = purpose["evidence"].as_array().cloned().unwrap_or_default();
    questions.insert(0,json!({"question":if purpose["status"] == "unknown" {
        crate::localize!("这个项目为谁解决什么问题，主要输入和可观察输出是什么？", "Whose problem does this project solve, and what are its main inputs and observable outputs?")
    } else {
        crate::localize!("项目目的声明对应哪个具体输入、处理过程与输出，入口怎样连接这些环节？", "Which concrete inputs, processing, and outputs implement the stated purpose, and how do the entries connect them?")
    },"why":crate::localize!("需要把项目声明与真实函数体连接，才能形成业务解释。", "A business explanation requires connecting the project declaration with actual function bodies."),
        "object":{"kind":"project_purpose","id":snapshot.id,"title":readme_path},"evidence":purpose_evidence,"node_ids":entries.iter().take(3).map(|n| &n.id).collect::<Vec<_>>()}));
    let mut boundaries = Vec::new();
    let mut reading = Vec::new();
    if !purpose_evidence.is_empty() {
        reading.push(json!({"title":purpose_evidence[0]["path"],"kind":"purpose","focus":purpose["text"],
            "why":crate::localize!("先取得作者使用的目标与领域词汇，后续用实现确认输入输出和职责。", "Start with the author's goal and domain terms, then inspect implementations to establish inputs, outputs, and responsibilities."),
            "evidence":purpose_evidence,"node_ids":[]}));
    }
    for node in entries.iter().take(2) {
        reading.push(json!({"title":node.qualified_name,"kind":"entry","focus":node.signature,
            "why":crate::localize!("这是生产程序的实际入口；阅读它的函数体可确认首先处理的输入和交给的实现。", "This is an actual production program entry; its body establishes initial input handling and called implementations."),
            "evidence":[node.evidence],"node_ids":[node.id]}));
    }
    // Shared dependencies are useful contract boundaries, without assigning a
    // business responsibility from names or dependency counts.
    selected_packages.sort_by_key(|p| {
        (
            std::cmp::Reverse(incoming.get(&p.id).map_or(0, BTreeSet::len)),
            p.name.clone(),
        )
    });
    for package in selected_packages.iter().take(4) {
        crate::source::check_cancelled()?;
        let contract_nodes = contracts(index, snapshot, package)?;
        let interfaces: Vec<_> = contract_nodes
            .iter()
            .filter(|node| node.kind != "type")
            .take(4)
            .map(node_value)
            .collect();
        let state: Vec<_> = contract_nodes.iter().filter(|node| node.kind == "type").take(3).map(|node| {
            let mut value = node_value(node);
            value["role"] = json!(if node.attributes.get("rust.kind").is_some_and(|kind| kind == "enum") {
                crate::localize!("枚举定义；状态表示候选，需读用法确认", "Enum declaration; candidate state representation, inspect usage to confirm")
            } else {
                crate::localize!("数据类型定义；承载的状态与约束需读用法确认", "Data type declaration; inspect usage to establish its state and constraints")
            });
            if let Some(data) = dimension(understanding,"data").iter().find(|item| item["node_ids"].as_array().is_some_and(|ids| ids.iter().any(|id| id == &node.id))) {
                value["members"] = data["members"].clone();
            }
            value
        }).collect();
        let declared_purpose = description(&mut sources, package)?;
        let documented_components: Vec<_> = understanding["architecture"]["components"].as_array().into_iter().flatten()
            .filter(|component| component["package_id"] == package.id && component["basis"].as_str().is_some_and(|basis| basis.starts_with("source documentation")))
            .take(3).map(|component| json!({"title":component["title"],"text":component["summary"],"evidence":component["evidence"],"node_ids":component["node_ids"]})).collect();
        let users: Vec<_> = incoming
            .get(&package.id)
            .into_iter()
            .flatten()
            .filter_map(|id| packages.get(id.as_str()).map(|p| p.name.as_str()))
            .collect();
        let evidence: Vec<_> = contract_nodes
            .iter()
            .take(5)
            .map(|node| &node.evidence)
            .collect();
        let node_ids: Vec<_> = contract_nodes.iter().take(5).map(|node| &node.id).collect();
        boundaries.push(json!({"title":package.name,"package_id":package.id,"declared_purpose":declared_purpose,
            "documented_components":documented_components,"declared_consumers":users,"interfaces":interfaces,"state":state,
            "summary":crate::localize!("这些公开声明定位此包可供阅读的接口和数据契约；使用方来自生产依赖声明。", "These public declarations locate the package's interfaces and data contracts; consumers come from production dependency declarations."),
            "so_what":crate::localize!("应解释使用方究竟传入什么、取得什么，以及哪些类型保存或约束状态；公开声明本身不能回答协作的业务含义。", "Explain what consumers pass in and receive, and which types retain or constrain state; public declarations alone do not establish the business meaning of the collaboration."),
            "evidence":evidence,"node_ids":node_ids}));
        if declared_purpose.is_none() && documented_components.is_empty() {
            questions.push(json!({"question":crate::localize!("{} 在项目目标中承担什么职责，调用它的使用方期望什么结果？", "What responsibility does {} have in the project goal, and what result do its consumers expect?",package.name),
                "why":crate::localize!("这个包有可定位的结构，但缺少已采集的目的或职责原文。", "This package has locatable structure but no captured purpose or responsibility statement."),
                "object":{"kind":"package_responsibility","id":package.id,"title":package.name,"path":package.root},"evidence":evidence,"node_ids":node_ids}));
        }
        for node in contract_nodes.iter().take(2) {
            let reason = crate::localize!("主路径附近的公开接口或数据类型：结合定义解释输入输出、状态和边界。", "Public interface or data type near the selected routes: connect its definition with inputs, outputs, state, and boundaries.");
            append_context(
                &mut sources,
                &mut context,
                &mut remaining,
                ContextRequest {
                    title: &node.qualified_name,
                    kind: "contract_definition",
                    reason,
                    evidence: &node.evidence,
                    node_ids: std::slice::from_ref(&node.id),
                    max_chars: 1_800,
                },
            )?;
        }
        if reading.len() < 5 {
            if let Some(node) = contract_nodes.first() {
                reading.push(json!({"title":node.qualified_name,"kind":"contract","focus":node.signature,
                    "why":crate::localize!("它位于声明依赖阅读路径中的 {}；读定义和调用方以确认双方交换的数据与承诺。", "It belongs to {} on the declared dependency reading routes; read its definition and consumers to establish exchanged data and obligations.",package.name),
                    "evidence":[node.evidence],"node_ids":[node.id]}));
            }
        }
    }
    for (position, item) in reading.iter_mut().enumerate() {
        item["order"] = json!(position + 1);
    }
    questions.truncate(8);
    let source_context_truncated = neighborhood_truncated
        || remaining == 0
        || context.iter().any(|item| item["truncated"] == true);
    Ok(
        json!({"schema_version":1,"snapshot_id":snapshot.id,"locale":crate::i18n::current().tag(),
        "status":"structural_only","missing_business_explanation":true,
        "title":crate::localize!("项目解释的源码证据包", "Source evidence packet for explaining the project"),
        "summary":crate::localize!("已定位目的原文、入口、声明依赖阅读路径及接口/状态定义。业务目标如何由这些代码共同实现，仍待结合真实源码解释。", "Purpose statements, entries, declared dependency reading routes, and interface/state definitions are located. How this code jointly implements the business goal still requires source reading and explanation."),
        "purpose":purpose,"entries":entries.iter().take(6).map(node_value).collect::<Vec<_>>(),"entries_total":entries.len(),
        "structural_routes":{"paths":paths,"relations_total":relations.len(),"truncated":truncated,
            "basis":crate::localize!("内部生产依赖声明；不是运行时调用顺序，也不等于已理解业务主线", "Declared internal production dependencies; neither runtime call order nor an established understanding of the business workflow")},
        "boundaries":boundaries,"reading":reading,"questions":questions,"source_context":context,
        "source_context_char_limit":CONTEXT_CHARS,"source_context_chars":CONTEXT_CHARS-remaining,
        "source_context_truncated":source_context_truncated,
        "source_context_scope":crate::localize!("入口及最多两层已解析调用附近的有界源码；超出深度、节点数或字符预算的实现需继续扩展阅读。", "Bounded source near entries and at most two levels of resolved calls; implementations beyond the depth, node, or character budget require further reading."),
        "limits":{"routes":ROUTE_LIMIT,"route_depth":ROUTE_DEPTH,"reading":5,"questions":8,
            "source_entries":3,"source_call_depth":2,"source_call_nodes":9,"source_body_chars":3200},
        "analysis_basis":crate::localize!("仅使用不可变索引快照中的文档、定义与已解析关系；职责缺证据时保留待回答问题。", "Uses only documents, declarations, and resolved relations from the immutable index snapshot; missing responsibility evidence becomes a question.")}),
    )
}
