use super::{FileContext, FrontendCache, LanguageFrontend};
use crate::model::{
    digest, identity, Capability, Diagnostic, Edge, Evidence, FileFacts, Node, ProjectModel,
};
use crate::project::join_path;
use crate::source::{check_cancelled, SourceFile, SourceSet};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tree_sitter::{Node as SyntaxNode, ParseOptions, Parser, Tree};

pub struct RustFrontend;

fn parse_tree(content: &str) -> Result<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
    let started = Instant::now();
    let mut progress = |_: &tree_sitter::ParseState| {
        if crate::source::CANCELLED.load(Ordering::Relaxed)
            || started.elapsed() > Duration::from_secs(10)
        {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    parser
        .parse_with_options(
            &mut |offset, _| content.as_bytes().get(offset..).unwrap_or_default(),
            None,
            Some(ParseOptions::new().progress_callback(&mut progress)),
        )
        .context("Rust parser cancelled or exceeded the 10-second per-file budget")
}

fn text<'a>(node: SyntaxNode<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

fn parent_path(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

fn join_name(module: &str, name: &str) -> String {
    if module.is_empty() {
        name.into()
    } else {
        format!("{module}::{name}")
    }
}

fn is_test_cfg(attribute: &str) -> bool {
    let normalized = attribute
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>();
    normalized
        .strip_prefix("#[cfg(")
        .and_then(|s| s.strip_suffix(")]"))
        .is_some_and(|expression| {
            eval_test_cfg(expression, false, 0) == Some(false)
                && eval_test_cfg(expression, true, 0) != Some(false)
        })
}

// Classify only branches which require cfg(test). Other build predicates are
// unknown, so any(test, feature=...) must remain eligible for production.
fn eval_test_cfg(expression: &str, test_enabled: bool, depth: usize) -> Option<bool> {
    if depth > 64 {
        return None;
    }
    if expression == "test" {
        return Some(test_enabled);
    }
    let (operation, body) = expression.split_once('(')?;
    let body = body.strip_suffix(')')?;
    if operation == "not" {
        return eval_test_cfg(body, test_enabled, depth + 1).map(|v| !v);
    }
    if operation != "all" && operation != "any" {
        return None;
    }
    let mut level = 0;
    let mut quoted = false;
    let mut escaped = false;
    let mut start = 0;
    let mut values = Vec::new();
    for (position, ch) in body.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && quoted {
            escaped = true;
            continue;
        }
        if ch == '"' {
            quoted = !quoted;
        }
        if quoted {
            continue;
        }
        match ch {
            '(' => level += 1,
            ')' => level -= 1,
            ',' if level == 0 => {
                values.push(eval_test_cfg(
                    &body[start..position],
                    test_enabled,
                    depth + 1,
                ));
                start = position + 1;
            }
            _ => {}
        }
    }
    if start < body.len() {
        values.push(eval_test_cfg(&body[start..], test_enabled, depth + 1));
    }
    if operation == "all" {
        if values.contains(&Some(false)) {
            Some(false)
        } else if values.iter().all(|v| *v == Some(true)) {
            Some(true)
        } else {
            None
        }
    } else if values.contains(&Some(true)) {
        Some(true)
    } else if values.iter().all(|v| *v == Some(false)) {
        Some(false)
    } else {
        None
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct ModuleLink {
    names: Vec<String>,
    explicit_path: Option<String>,
    conditions: Vec<String>,
}

fn module_links(
    node: SyntaxNode<'_>,
    content: &str,
    names: &[String],
    conditions: &[String],
    output: &mut Vec<ModuleLink>,
) {
    if names.len() > 128 {
        return;
    }
    let mut attributes = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "attribute_item" {
            attributes.push(text(child, content).to_owned());
            continue;
        }
        if matches!(child.kind(), "line_comment" | "block_comment") {
            continue;
        }
        if child.kind() == "mod_item" {
            if let Some(name) = child.child_by_field_name("name") {
                let mut path = names.to_vec();
                path.push(text(name, content).into());
                let mut cfg = conditions.to_vec();
                cfg.extend(attributes.iter().filter(|a| a.contains("cfg")).cloned());
                if let Some(body) = child.child_by_field_name("body") {
                    module_links(body, content, &path, &cfg, output);
                } else {
                    let explicit_path = attributes
                        .iter()
                        .find(|a| a.starts_with("#[path"))
                        .and_then(|a| a.split('"').nth(1))
                        .map(str::to_owned);
                    output.push(ModuleLink {
                        names: path,
                        explicit_path,
                        conditions: cfg,
                    });
                }
            }
        }
        attributes.clear();
    }
}

impl LanguageFrontend for RustFrontend {
    fn language(&self) -> &str {
        "rust"
    }
    fn version(&self) -> &str {
        "tree-sitter-rust:0.24.2:4"
    }
    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability {
            name: "rust_syntax".into(),
            provider: self.version().into(),
            scope: "Cargo packages and UTF-8 Rust source files".into(),
            limitations: vec![
                "Call expressions are not resolved without semantic enrichment".into(),
                "Macros are recorded without expanding generated code".into(),
                "Conditional branches retain source predicates; they are not runtime paths".into(),
                "Declared type uses, enum state candidates and route bindings are syntax evidence, not inferred data flow or verified framework dispatch".into(),
            ],
        }]
    }

    fn plan(
        &self,
        sources: &SourceSet,
        project: &ProjectModel,
        cache: Option<&dyn FrontendCache>,
    ) -> Result<Vec<FileContext>> {
        let mut plans = Vec::new();
        let mut seen = BTreeSet::new();
        let mut links_cache = BTreeMap::<String, Vec<ModuleLink>>::new();
        let mut missing = Vec::new();
        for file in sources.files.values().filter(|f| f.path.ends_with(".rs")) {
            check_cancelled()?;
            let key = identity(&["rust_module_links", self.version(), &file.hash]);
            if let Some(record) = cache.map(|c| c.load_record(&key)).transpose()?.flatten() {
                links_cache.insert(file.path.clone(), serde_json::from_str(&record)?);
            } else {
                missing.push((file, key));
            }
        }
        for batch in missing.chunks(64) {
            let results = super::parallel_map(batch, |(file, _)| {
                check_cancelled()?;
                let tree = parse_tree(&file.content)?;
                let mut links = Vec::new();
                module_links(tree.root_node(), &file.content, &[], &[], &mut links);
                Ok(links)
            })?;
            for ((file, key), links) in batch.iter().zip(results) {
                if let Some(cache) = cache {
                    cache.store_record(key, &serde_json::to_string(&links)?)?;
                }
                links_cache.insert(file.path.clone(), links);
            }
        }
        for package in &project.packages {
            for unit in &package.units {
                let mut pending = vec![(
                    unit.source.clone(),
                    unit.name.replace('-', "_"),
                    unit.required_features
                        .iter()
                        .map(|f| format!("target requires feature={f:?}"))
                        .collect::<Vec<_>>(),
                    parent_path(&unit.source).to_owned(),
                )];
                while let Some((path, module, conditions, module_dir)) = pending.pop() {
                    check_cancelled()?;
                    if !seen.insert((unit.id.clone(), path.clone(), module.clone()))
                        || module.matches("::").count() > 64
                    {
                        continue;
                    }
                    let Some(file) = sources.files.get(&path) else {
                        continue;
                    };
                    plans.push(FileContext {
                        path: path.clone(),
                        package: package.id.clone(),
                        unit: unit.id.clone(),
                        module: module.clone(),
                        is_test: unit.kind == "test" || conditions.iter().any(|c| is_test_cfg(c)),
                        conditions: conditions.clone(),
                        linked: true,
                    });
                    if !links_cache.contains_key(&path) {
                        let key = identity(&["rust_module_links", self.version(), &file.hash]);
                        let cached = cache.map(|c| c.load_record(&key)).transpose()?.flatten();
                        let links = if let Some(record) = cached {
                            serde_json::from_str(&record)?
                        } else {
                            let tree = parse_tree(&file.content)?;
                            let mut links = Vec::new();
                            module_links(tree.root_node(), &file.content, &[], &[], &mut links);
                            if let Some(cache) = cache {
                                cache.store_record(&key, &serde_json::to_string(&links)?)?;
                            }
                            links
                        };
                        links_cache.insert(path.clone(), links);
                    }
                    for link in &links_cache[&path] {
                        let mut cfg = conditions.clone();
                        cfg.extend(link.conditions.clone());
                        let names_path = link.names.join("/");
                        let candidates = if let Some(explicit) = &link.explicit_path {
                            let inline_parent = &link.names[..link.names.len().saturating_sub(1)];
                            let base = if inline_parent.is_empty() {
                                parent_path(&path).to_owned()
                            } else {
                                join_path(&module_dir, &inline_parent.join("/")).unwrap_or_default()
                            };
                            vec![join_path(&base, explicit)]
                        } else {
                            vec![
                                join_path(&module_dir, &format!("{names_path}.rs")),
                                join_path(&module_dir, &format!("{names_path}/mod.rs")),
                            ]
                        };
                        for candidate in candidates
                            .into_iter()
                            .flatten()
                            .filter(|p| sources.files.contains_key(p))
                        {
                            let next_dir = if candidate.ends_with("/mod.rs") {
                                parent_path(&candidate).to_owned()
                            } else {
                                candidate.trim_end_matches(".rs").to_owned()
                            };
                            pending.push((
                                candidate,
                                join_name(&module, &link.names.join("::")),
                                cfg.clone(),
                                next_dir,
                            ));
                        }
                    }
                }
            }
        }
        let linked: BTreeSet<_> = plans.iter().map(|p| p.path.clone()).collect();
        for path in sources
            .files
            .keys()
            .filter(|p| p.ends_with(".rs") && !linked.contains(*p))
        {
            let package = project
                .packages
                .iter()
                .filter(|p| p.root.is_empty() || path.starts_with(&format!("{}/", p.root)))
                .max_by_key(|p| p.root.len());
            let package_id = package
                .map(|p| p.id.as_str())
                .unwrap_or("workspace-unlinked");
            let package_name = package.map(|p| p.name.as_str()).unwrap_or("workspace");
            let package_root = package.map(|p| p.root.as_str()).unwrap_or("");
            let relative = path
                .strip_prefix(&format!("{package_root}/"))
                .unwrap_or(path);
            plans.push(FileContext {
                path: path.clone(),
                package: package_id.into(),
                unit: format!("unlinked:{package_id}"),
                module: format!(
                    "{}::unlinked::{}",
                    package_name.replace('-', "_"),
                    relative.trim_end_matches(".rs").replace('/', "::")
                ),
                conditions: Vec::new(),
                is_test: relative.starts_with("tests/"),
                linked: false,
            });
        }
        plans.sort_by(|a, b| (&a.path, &a.unit, &a.module).cmp(&(&b.path, &b.unit, &b.module)));
        Ok(plans)
    }

    fn parse(&self, file: &SourceFile, context: &FileContext) -> Result<FileFacts> {
        let tree = parse_tree(&file.content)?;
        let mut collector = Collector {
            file,
            context,
            facts: FileFacts::default(),
        };
        let root = tree.root_node();
        let mut module = collector.make_node(
            root,
            context.module.rsplit("::").next().unwrap_or("root"),
            &context.module,
            "module",
            None,
            &context.conditions,
            context.is_test,
        );
        collector.inner_documentation(root, &mut module);
        let parent = module.id.clone();
        collector.facts.nodes.push(module);
        collector.walk_items(
            root,
            &context.module,
            &parent,
            &context.conditions,
            context.is_test,
            0,
        );
        if root.has_error() {
            collector.facts.diagnostics.push(Diagnostic::warning(
                "rust_parse_error",
                "Rust syntax contains errors; successfully parsed declarations are retained",
                Some(&file.path),
            ));
        }
        if !context.linked {
            collector.facts.diagnostics.push(Diagnostic::warning(
                "unlinked_source",
                "Source was indexed but is not linked to a discovered compilation unit",
                Some(&file.path),
            ));
        }
        Ok(collector.facts)
    }
}

struct Collector<'a> {
    file: &'a SourceFile,
    context: &'a FileContext,
    facts: FileFacts,
}

impl Collector<'_> {
    fn evidence(&self, syntax: SyntaxNode<'_>) -> Evidence {
        let start = syntax.start_position();
        let end = syntax.end_position();
        Evidence {
            path: self.file.path.clone(),
            content_hash: self.file.hash.clone(),
            start_byte: syntax.start_byte(),
            end_byte: syntax.end_byte(),
            start_line: start.row + 1,
            start_column: start.column,
            end_line: end.row + 1,
            end_column: end.column,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn make_node(
        &self,
        syntax: SyntaxNode<'_>,
        name: &str,
        qualified: &str,
        kind: &str,
        parent: Option<&str>,
        conditions: &[String],
        is_test: bool,
    ) -> Node {
        let stable_key = identity(&[&self.context.unit, &self.file.path, kind, qualified]);
        let id = identity(&[&stable_key, &syntax.start_byte().to_string()]);
        let source = text(syntax, &self.file.content);
        let signature_end = syntax
            .child_by_field_name("body")
            .map(|b| b.start_byte() - syntax.start_byte())
            .unwrap_or(source.len());
        let mut cursor = syntax.walk();
        let visibility = syntax
            .named_children(&mut cursor)
            .find(|n| n.kind() == "visibility_modifier")
            .map(|n| text(n, &self.file.content))
            .unwrap_or("private")
            .to_owned();
        Node {
            id,
            stable_key,
            name: name.into(),
            qualified_name: qualified.into(),
            kind: kind.into(),
            language: "rust".into(),
            package: self.context.package.clone(),
            unit: self.context.unit.clone(),
            parent: parent.map(str::to_owned),
            visibility,
            signature: source[..signature_end].trim().chars().take(2048).collect(),
            fingerprint: digest(source),
            evidence: self.evidence(syntax),
            conditions: conditions.to_vec(),
            is_test,
            attributes: BTreeMap::new(),
        }
    }

    fn edge(
        &mut self,
        source: &str,
        target_name: &str,
        kind: &str,
        syntax: SyntaxNode<'_>,
        conditions: &[String],
    ) {
        let evidence = self.evidence(syntax);
        self.facts.edges.push(Edge {
            id: identity(&[source, kind, target_name, &evidence.start_byte.to_string()]),
            source: source.into(),
            target: None,
            target_name: target_name.chars().take(1024).collect(),
            kind: kind.into(),
            resolution: "unresolved".into(),
            evidence,
            conditions: conditions.to_vec(),
            provider: "rust_syntax".into(),
        });
    }

    fn walk_items(
        &mut self,
        syntax: SyntaxNode<'_>,
        module: &str,
        parent: &str,
        conditions: &[String],
        is_test: bool,
        depth: usize,
    ) {
        if depth > 128 {
            self.facts.diagnostics.push(Diagnostic::warning(
                "syntax_depth_limit",
                "Declaration nesting exceeds 128 levels; deeper items were omitted",
                Some(&self.file.path),
            ));
            return;
        }
        let mut cursor = syntax.walk();
        let mut attributes = Vec::<String>::new();
        let mut attribute_evidence = Vec::new();
        let mut docs = Vec::new();
        let mut doc_evidence = Vec::new();
        for child in syntax.named_children(&mut cursor) {
            if child.kind() == "attribute_item" {
                attributes.push(text(child, &self.file.content).to_owned());
                attribute_evidence.push(self.evidence(child));
                continue;
            }
            if matches!(child.kind(), "line_comment" | "block_comment") {
                let comment = text(child, &self.file.content);
                if (comment.starts_with("///") && !comment.starts_with("////"))
                    || (comment.starts_with("/**") && !comment.starts_with("/***"))
                {
                    docs.push(comment.to_owned());
                    doc_evidence.push(self.evidence(child));
                }
                continue;
            }
            let mut cfg = conditions.to_vec();
            cfg.extend(attributes.iter().filter(|v| v.contains("cfg")).cloned());
            let test = is_test
                || attributes.iter().any(|v| {
                    v == "#[test]"
                        || v.starts_with("#[tokio::test")
                        || v.starts_with("#[async_std::test")
                        || is_test_cfg(v)
                });
            let kind = match child.kind() {
                "function_item" | "function_signature_item" => Some("function"),
                "struct_item" | "enum_item" | "union_item" | "type_item" => Some("type"),
                "trait_item" => Some("trait"),
                "mod_item" => Some("module"),
                "const_item" | "static_item" => Some("value"),
                "macro_definition" => Some("macro"),
                "impl_item" => Some("impl"),
                "field_declaration" => Some("field"),
                "enum_variant" => Some("variant"),
                _ => None,
            };
            if let Some(kind) = kind {
                let name = child
                    .child_by_field_name("name")
                    .or_else(|| child.child_by_field_name("type"));
                if let Some(name_node) = name {
                    let raw_name = text(name_node, &self.file.content).to_owned();
                    let name = if kind == "impl" {
                        if let Some(tr) = child.child_by_field_name("trait") {
                            format!("<{raw_name} as {}>", text(tr, &self.file.content))
                        } else {
                            format!("<{raw_name}>")
                        }
                    } else {
                        raw_name
                    };
                    let qualified = join_name(module, &name);
                    let mut node =
                        self.make_node(child, &name, &qualified, kind, Some(parent), &cfg, test);
                    if kind == "function" {
                        if let Some(trait_item) =
                            syntax.parent().filter(|p| p.kind() == "trait_item")
                        {
                            let mut cursor = trait_item.walk();
                            node.visibility = trait_item
                                .named_children(&mut cursor)
                                .find(|n| n.kind() == "visibility_modifier")
                                .map(|n| text(n, &self.file.content).to_owned())
                                .unwrap_or_else(|| "private".into());
                            node.attributes
                                .insert("visibility_origin".into(), "trait".into());
                        }
                    }
                    node.attributes.insert(
                        "definition_start".into(),
                        name_node.start_byte().to_string(),
                    );
                    if !attributes.is_empty() {
                        node.attributes
                            .insert("rust.attributes".into(), attributes.join("\n"));
                        node.attributes.insert(
                            "rust.attribute_evidence".into(),
                            serde_json::to_string(&attribute_evidence)
                                .expect("evidence is serializable"),
                        );
                    }
                    if !docs.is_empty() {
                        node.attributes.insert("rust.doc".into(), docs.join("\n"));
                        node.attributes.insert(
                            "rust.doc_evidence".into(),
                            serde_json::to_string(&doc_evidence).expect("evidence is serializable"),
                        );
                    }
                    node.attributes.insert(
                        "rust.kind".into(),
                        child.kind().trim_end_matches("_item").to_owned(),
                    );
                    if kind == "module" {
                        if let Some(body) = child.child_by_field_name("body") {
                            self.inner_documentation(body, &mut node);
                        }
                    }
                    if kind == "variant" {
                        node.attributes
                            .insert("rust.state_candidate".into(), "true".into());
                        if let Some(owner) = self.facts.nodes.iter().find(|n| n.id == parent) {
                            node.visibility = owner.visibility.clone();
                            node.attributes
                                .insert("visibility_origin".into(), "enum".into());
                        }
                    }
                    self.declared_types(child, &mut node, &cfg);
                    if kind == "function" && name == "main" {
                        node.attributes.insert("entry".into(), "main".into());
                    }
                    if kind == "impl" {
                        node.attributes.insert(
                            "rust.self_type".into(),
                            text(name_node, &self.file.content).into(),
                        );
                        if let Some(tr) = child.child_by_field_name("trait") {
                            node.attributes
                                .insert("rust.trait".into(), text(tr, &self.file.content).into());
                            self.edge(
                                &node.id,
                                text(tr, &self.file.content),
                                "implements",
                                tr,
                                &cfg,
                            );
                        }
                    }
                    let id = node.id.clone();
                    self.facts.nodes.push(node);
                    if let Some(body) = child.child_by_field_name("body") {
                        if matches!(kind, "module" | "impl" | "trait") {
                            self.walk_items(body, &qualified, &id, &cfg, test, depth + 1);
                        } else if kind == "function" {
                            let mut routes = Vec::new();
                            self.route_bindings(body, &mut routes, 0);
                            if !routes.is_empty() {
                                if let Some(node) = self.facts.nodes.iter_mut().find(|n| n.id == id)
                                {
                                    node.attributes.insert(
                                        "rust.route_bindings".into(),
                                        serde_json::to_string(&routes)
                                            .expect("route syntax is serializable"),
                                    );
                                }
                            }
                            self.walk_calls(body, &id, &cfg, 0);
                            self.walk_items(body, &qualified, &id, &cfg, test, depth + 1);
                        } else if matches!(kind, "type" | "variant") {
                            if body.kind() == "ordered_field_declaration_list" {
                                self.tuple_fields(body, &qualified, &id, &cfg, test);
                            } else {
                                self.walk_items(body, &qualified, &id, &cfg, test, depth + 1);
                            }
                        }
                    }
                }
            } else if child.kind() == "use_declaration" {
                if let Some(argument) = child.child_by_field_name("argument") {
                    self.edge(
                        parent,
                        text(argument, &self.file.content),
                        "imports",
                        argument,
                        &cfg,
                    );
                }
            } else if child.kind() == "macro_invocation" {
                self.edge(
                    parent,
                    text(child, &self.file.content)
                        .split('!')
                        .next()
                        .unwrap_or("macro"),
                    "macro_call",
                    child,
                    &cfg,
                );
            }
            attributes.clear();
            attribute_evidence.clear();
            docs.clear();
            doc_evidence.clear();
        }
    }

    fn declared_types(&mut self, syntax: SyntaxNode<'_>, node: &mut Node, conditions: &[String]) {
        if let Some(params) = syntax.child_by_field_name("parameters") {
            node.attributes.insert(
                "rust.parameters".into(),
                text(params, &self.file.content).into(),
            );
            let mut cursor = params.walk();
            let mut declarations = Vec::new();
            for parameter in params.named_children(&mut cursor) {
                if let Some(ty) = parameter.child_by_field_name("type") {
                    self.edge(
                        &node.id,
                        text(ty, &self.file.content),
                        "type_usage",
                        ty,
                        conditions,
                    );
                    declarations.push(serde_json::json!({
                        "pattern": parameter.child_by_field_name("pattern").map(|n| text(n, &self.file.content)),
                        "type": text(ty, &self.file.content),
                        "evidence": self.evidence(ty),
                    }));
                }
            }
            if !declarations.is_empty() {
                node.attributes.insert(
                    "rust.parameter_types".into(),
                    serde_json::to_string(&declarations).expect("parameter syntax is serializable"),
                );
            }
        }
        if let Some(ty) = syntax.child_by_field_name("return_type") {
            node.attributes.insert(
                "rust.return_type".into(),
                text(ty, &self.file.content).into(),
            );
            self.edge(
                &node.id,
                text(ty, &self.file.content),
                "type_usage",
                ty,
                conditions,
            );
        }
        if let Some(ty) = syntax.child_by_field_name("type") {
            node.attributes.insert(
                "rust.declared_type".into(),
                text(ty, &self.file.content).into(),
            );
            self.edge(
                &node.id,
                text(ty, &self.file.content),
                "type_usage",
                ty,
                conditions,
            );
        }
    }

    fn inner_documentation(&self, syntax: SyntaxNode<'_>, node: &mut Node) {
        let mut docs = Vec::new();
        let mut evidence = Vec::new();
        let mut cursor = syntax.walk();
        for child in syntax.named_children(&mut cursor) {
            if matches!(child.kind(), "line_comment" | "block_comment") {
                let comment = text(child, &self.file.content);
                if comment.starts_with("//!") || comment.starts_with("/*!") {
                    docs.push(comment.to_owned());
                    evidence.push(self.evidence(child));
                }
            } else if child.kind() != "inner_attribute_item" {
                break;
            }
        }
        if !docs.is_empty() {
            let mut combined = node.attributes.get("rust.doc").cloned().unwrap_or_default();
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&docs.join("\n"));
            let mut existing: Vec<Evidence> = node
                .attributes
                .get("rust.doc_evidence")
                .and_then(|v| serde_json::from_str(v).ok())
                .unwrap_or_default();
            existing.extend(evidence);
            node.attributes.insert("rust.doc".into(), combined);
            node.attributes.insert(
                "rust.doc_evidence".into(),
                serde_json::to_string(&existing).expect("evidence is serializable"),
            );
        }
    }

    fn tuple_fields(
        &mut self,
        syntax: SyntaxNode<'_>,
        owner: &str,
        parent: &str,
        conditions: &[String],
        is_test: bool,
    ) {
        let mut cursor = syntax.walk();
        let types: Vec<_> = syntax.children_by_field_name("type", &mut cursor).collect();
        let mut pending_attributes = Vec::new();
        let mut pending_evidence = Vec::new();
        let mut visibility = None;
        let mut index = 0;
        let mut cursor = syntax.walk();
        for child in syntax.named_children(&mut cursor) {
            if child.kind() == "attribute_item" {
                pending_attributes.push(text(child, &self.file.content).to_owned());
                pending_evidence.push(self.evidence(child));
                continue;
            }
            if child.kind() == "visibility_modifier" {
                visibility = Some(child);
                continue;
            }
            if !types.iter().any(|ty| ty.id() == child.id()) {
                continue;
            }
            let name = index.to_string();
            index += 1;
            let mut cfg = conditions.to_vec();
            cfg.extend(
                pending_attributes
                    .iter()
                    .filter(|v| v.contains("cfg"))
                    .cloned(),
            );
            let test = is_test || pending_attributes.iter().any(|v| is_test_cfg(v));
            let mut node = self.make_node(
                child,
                &name,
                &join_name(owner, &name),
                "field",
                Some(parent),
                &cfg,
                test,
            );
            node.attributes
                .insert("rust.kind".into(), "tuple_field".into());
            node.attributes.insert(
                "rust.declared_type".into(),
                text(child, &self.file.content).into(),
            );
            if let Some(vis) = visibility {
                node.visibility = text(vis, &self.file.content).into();
                node.evidence.start_byte = vis.start_byte();
                node.evidence.start_line = vis.start_position().row + 1;
                node.evidence.start_column = vis.start_position().column;
                node.signature = self.file.content[vis.start_byte()..child.end_byte()].into();
                node.fingerprint = digest(&node.signature);
            }
            if !pending_attributes.is_empty() {
                node.attributes
                    .insert("rust.attributes".into(), pending_attributes.join("\n"));
                node.attributes.insert(
                    "rust.attribute_evidence".into(),
                    serde_json::to_string(&pending_evidence).expect("evidence is serializable"),
                );
            }
            self.edge(
                &node.id,
                text(child, &self.file.content),
                "type_usage",
                child,
                &cfg,
            );
            self.facts.nodes.push(node);
            pending_attributes.clear();
            pending_evidence.clear();
            visibility = None;
        }
    }

    fn route_bindings(
        &self,
        syntax: SyntaxNode<'_>,
        routes: &mut Vec<serde_json::Value>,
        depth: usize,
    ) {
        if depth > 256 || syntax.kind() == "function_item" || syntax.kind() == "macro_invocation" {
            return;
        }
        if syntax.kind() == "call_expression" {
            if let (Some(function), Some(arguments)) = (
                syntax.child_by_field_name("function"),
                syntax.child_by_field_name("arguments"),
            ) {
                if function.kind() == "field_expression"
                    && function
                        .child_by_field_name("field")
                        .is_some_and(|n| text(n, &self.file.content) == "route")
                {
                    let mut cursor = arguments.walk();
                    let args: Vec<_> = arguments
                        .named_children(&mut cursor)
                        .filter(|n| !matches!(n.kind(), "line_comment" | "block_comment"))
                        .collect();
                    if args.len() == 2
                        && matches!(args[0].kind(), "string_literal" | "raw_string_literal")
                    {
                        routes.push(serde_json::json!({
                            "kind": "route_method_syntax",
                            "path_literal": text(args[0], &self.file.content),
                            "handler_expression": text(args[1], &self.file.content),
                            "evidence": self.evidence(syntax),
                            "path_evidence": self.evidence(args[0]),
                            "handler_evidence": self.evidence(args[1]),
                            "resolution": "unresolved",
                        }));
                    }
                }
            }
        }
        let mut cursor = syntax.walk();
        for child in syntax.named_children(&mut cursor) {
            self.route_bindings(child, routes, depth + 1);
        }
    }

    fn walk_calls(
        &mut self,
        syntax: SyntaxNode<'_>,
        parent: &str,
        conditions: &[String],
        depth: usize,
    ) {
        if depth > 256 {
            self.facts.diagnostics.push(Diagnostic::warning(
                "syntax_depth_limit",
                "Expression nesting exceeds 256 levels; deeper calls were omitted",
                Some(&self.file.path),
            ));
            return;
        }
        if syntax.kind() == "function_item" {
            return;
        }
        if syntax.kind() == "call_expression" {
            if let Some(function) = syntax.child_by_field_name("function") {
                let mut target = function;
                if target.kind() == "generic_function" {
                    target = target.child_by_field_name("function").unwrap_or(target);
                }
                let position = target
                    .child_by_field_name("field")
                    .or_else(|| target.child_by_field_name("name"))
                    .unwrap_or(target);
                self.edge(
                    parent,
                    text(function, &self.file.content),
                    "calls",
                    position,
                    conditions,
                );
            }
        } else if syntax.kind() == "macro_invocation" {
            self.edge(
                parent,
                text(syntax, &self.file.content)
                    .split('!')
                    .next()
                    .unwrap_or("macro"),
                "macro_call",
                syntax,
                conditions,
            );
            return;
        }
        let mut cursor = syntax.walk();
        for child in syntax.named_children(&mut cursor) {
            self.walk_calls(child, parent, conditions, depth + 1);
        }
    }
}
