use super::{FileContext, FrontendCache, LanguageFrontend};
use crate::model::{
    digest, identity, Capability, Diagnostic, Edge, Evidence, FileFacts, Node, ProjectModel,
};
use crate::source::{check_cancelled, SourceFile, SourceSet};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tree_sitter::{Node as SyntaxNode, ParseOptions, Parser, Tree};

pub struct PythonFrontend;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Binding {
    pub name: String,
    pub kind: String,
    pub target: String,
    pub node: Option<String>,
    pub unconditional: bool,
}

fn children(node: SyntaxNode<'_>) -> Vec<SyntaxNode<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}
fn text<'a>(node: SyntaxNode<'_>, content: &'a str) -> &'a str {
    &content[node.byte_range()]
}
fn qualified(owner: &str, name: &str) -> String {
    if owner.is_empty() {
        name.into()
    } else {
        format!("{owner}.{name}")
    }
}
fn under(path: &str, root: &str) -> bool {
    path == root || root.is_empty() || path.strip_prefix(root).is_some_and(|s| s.starts_with('/'))
}
fn parse_tree(content: &str) -> Result<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_python::LANGUAGE.into())?;
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
        .context("Python parser cancelled or exceeded the 10-second per-file budget")
}

impl LanguageFrontend for PythonFrontend {
    fn language(&self) -> &str {
        "python"
    }
    fn version(&self) -> &str {
        "tree-sitter-python:0.25.0:1"
    }
    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability {
            name: "python_syntax".into(), provider: self.version().into(),
            scope: "Captured Python modules, including src layouts, namespace directories and loose source".into(),
            limitations: vec![
                "Declarations, imports, calls and annotations are syntax facts; dynamic dispatch and runtime types are not inferred".into(),
                "Decorators and main guards identify explicit declarations, not verified framework registration or execution".into(),
                "Generated code, import side effects, setup.py and interpreter execution are not evaluated".into(),
                "Comprehensions and conditional bindings conservatively block static resolution; no complete Python control-flow model is claimed".into(),
            ],
        }]
    }
    fn plan(
        &self,
        sources: &SourceSet,
        project: &ProjectModel,
        _cache: Option<&dyn FrontendCache>,
    ) -> Result<Vec<FileContext>> {
        let mut plan = Vec::new();
        for path in sources
            .files
            .keys()
            .filter(|p| p.ends_with(".py") || p.ends_with(".pyi"))
        {
            check_cancelled()?;
            let owner = project
                .packages
                .iter()
                .filter(|p| p.language == "python" && under(path, &p.root))
                .max_by_key(|p| p.root.len());
            let unit = owner.and_then(|p| {
                p.units
                    .iter()
                    .filter(|u| under(path, &u.source))
                    .max_by_key(|u| u.source.len())
            });
            let root = owner.map_or("", |p| p.root.as_str());
            let relative = path
                .strip_prefix(root)
                .unwrap_or(path)
                .trim_start_matches('/');
            let library_root = owner.and_then(|p| {
                p.units
                    .iter()
                    .filter(|u| u.kind == "lib" && under(path, &u.source))
                    .max_by_key(|u| u.source.len())
            });
            let relative = library_root.map_or(relative, |unit| {
                path.strip_prefix(&unit.source)
                    .unwrap_or(path)
                    .trim_start_matches('/')
            });
            let mut module = relative
                .trim_end_matches(".pyi")
                .trim_end_matches(".py")
                .replace('/', ".");
            if let Some(prefix) = module.strip_suffix(".__init__") {
                module = prefix.into();
            }
            let file_name = path.rsplit('/').next().unwrap_or(path);
            plan.push(FileContext {
                path: path.clone(),
                package: owner
                    .map(|p| p.id.clone())
                    .unwrap_or_else(|| "python-unlinked".into()),
                unit: unit
                    .map(|u| u.id.clone())
                    .unwrap_or_else(|| format!("unlinked:{}", owner.map_or("", |p| p.id.as_str()))),
                module,
                conditions: Vec::new(),
                is_test: unit.is_some_and(|u| u.kind == "test")
                    || file_name.starts_with("test_")
                    || file_name.ends_with("_test.py")
                    || file_name == "conftest.py",
                linked: unit.is_some(),
            });
        }
        Ok(plan)
    }
    fn parse(&self, file: &SourceFile, context: &FileContext) -> Result<FileFacts> {
        let tree = parse_tree(&file.content)?;
        let root = tree.root_node();
        let mut collector = Collector {
            file,
            context,
            facts: FileFacts::default(),
            bindings: BTreeMap::new(),
            conditions: context.conditions.clone(),
        };
        let name = context.module.rsplit('.').next().unwrap_or(&context.module);
        let mut module = collector.make_node(root, name, &context.module, "module", None);
        module.signature = format!("module {}", context.module);
        module
            .attributes
            .insert("python.kind".into(), "module".into());
        if let Some(doc) = docstring(root, &file.content) {
            module.attributes.insert("doc".into(), doc);
            if let Some(syntax) = docstring_syntax(root) {
                module.attributes.insert(
                    "python.doc_evidence".into(),
                    serde_json::to_string(&vec![collector.evidence(syntax)])?,
                );
            }
        }
        if file.path.ends_with("/__main__.py") || file.path == "__main__.py" {
            module
                .attributes
                .insert("entry_kind".into(), "python_script".into());
        }
        let parent = module.id.clone();
        collector.facts.nodes.push(module);
        collector.walk(root, &context.module, &parent, None, true, 0)?;
        let lexical_redirects: Vec<_> = collector
            .facts
            .nodes
            .iter()
            .filter_map(|n| {
                n.attributes
                    .get("python.lexical_scope")
                    .map(|parent| (n.id.clone(), parent.clone()))
            })
            .collect();
        for (scope, parent) in lexical_redirects {
            if let Some(bindings) = collector.bindings.remove(&scope) {
                collector
                    .bindings
                    .entry(parent)
                    .or_default()
                    .extend(bindings);
            }
        }
        for node in &mut collector.facts.nodes {
            if let Some(bindings) = collector.bindings.remove(&node.id) {
                node.attributes
                    .insert("python.bindings".into(), serde_json::to_string(&bindings)?);
            }
        }
        if root.has_error() {
            collector.facts.diagnostics.push(Diagnostic::warning(
                "python_parse_error",
                "Python syntax contains errors; successfully parsed declarations are retained",
                Some(&file.path),
            ));
        }
        if !context.linked {
            collector.facts.diagnostics.push(Diagnostic::warning(
                "unlinked_source",
                "Python source is outside discovered source roots; retained structurally",
                Some(&file.path),
            ));
        }
        Ok(collector.facts)
    }
}

fn docstring(body: SyntaxNode<'_>, content: &str) -> Option<String> {
    let string = docstring_syntax(body)?;
    let raw = text(string, content);
    let prefix_end = raw.find(['\'', '"']).unwrap_or(0);
    let prefix = raw[..prefix_end].to_ascii_lowercase();
    if prefix.contains('f') || prefix.contains('b') {
        return None;
    }
    let value = &raw[prefix_end..];
    let quotes = if value.starts_with("\"\"\"") || value.starts_with("'''") {
        3
    } else {
        1
    };
    Some(
        value
            .get(quotes..value.len().saturating_sub(quotes))
            .unwrap_or(value)
            .trim()
            .into(),
    )
}
fn docstring_syntax(body: SyntaxNode<'_>) -> Option<SyntaxNode<'_>> {
    let first = children(body).into_iter().find(|n| n.kind() != "comment")?;
    if first.kind() != "expression_statement" {
        return None;
    }
    let string = children(first).into_iter().next()?;
    (string.kind() == "string").then_some(string)
}

struct Collector<'a> {
    file: &'a SourceFile,
    context: &'a FileContext,
    facts: FileFacts,
    bindings: BTreeMap<String, Vec<Binding>>,
    conditions: Vec<String>,
}

impl Collector<'_> {
    fn evidence(&self, syntax: SyntaxNode<'_>) -> Evidence {
        Evidence {
            path: self.file.path.clone(),
            content_hash: self.file.hash.clone(),
            start_byte: syntax.start_byte(),
            end_byte: syntax.end_byte(),
            start_line: syntax.start_position().row + 1,
            start_column: syntax.start_position().column,
            end_line: syntax.end_position().row + 1,
            end_column: syntax.end_position().column,
        }
    }
    fn make_node(
        &self,
        syntax: SyntaxNode<'_>,
        name: &str,
        qualified: &str,
        kind: &str,
        parent: Option<&str>,
    ) -> Node {
        let stable_key = identity(&[&self.context.unit, &self.file.path, kind, qualified]);
        let body_start = syntax
            .child_by_field_name("body")
            .map_or(syntax.end_byte(), |n| n.start_byte());
        let mut attributes = BTreeMap::from([
            ("python.kind".into(), syntax.kind().into()),
            ("linked".into(), self.context.linked.to_string()),
        ]);
        if let Some(name) = syntax.child_by_field_name("name") {
            attributes.insert("definition_start".into(), name.start_byte().to_string());
        }
        Node {
            id: identity(&[&stable_key, &syntax.start_byte().to_string()]),
            stable_key,
            name: name.into(),
            qualified_name: qualified.into(),
            kind: kind.into(),
            language: "python".into(),
            package: self.context.package.clone(),
            unit: self.context.unit.clone(),
            parent: parent.map(str::to_owned),
            visibility: if name.starts_with('_') {
                "private"
            } else {
                "public"
            }
            .into(),
            signature: self.file.content[syntax.start_byte()..body_start]
                .trim()
                .into(),
            fingerprint: digest(text(syntax, &self.file.content)),
            evidence: self.evidence(syntax),
            conditions: self.conditions.clone(),
            is_test: self.context.is_test,
            attributes,
        }
    }
    fn edge(
        &mut self,
        source: &str,
        target: SyntaxNode<'_>,
        kind: &str,
        target_name: Option<String>,
    ) {
        let evidence = self.evidence(target);
        self.facts.edges.push(Edge {
            id: identity(&[
                source,
                kind,
                &target.start_byte().to_string(),
                &target.end_byte().to_string(),
            ]),
            source: source.into(),
            target: None,
            target_name: target_name.unwrap_or_else(|| text(target, &self.file.content).into()),
            kind: kind.into(),
            resolution: "unresolved".into(),
            evidence,
            conditions: self.conditions.clone(),
            provider: PythonFrontend.version().into(),
        });
    }
    fn bind(
        &mut self,
        scope: &str,
        name: &str,
        kind: &str,
        target: &str,
        node: Option<&str>,
        unconditional: bool,
    ) {
        self.bindings
            .entry(scope.into())
            .or_default()
            .push(Binding {
                name: name.into(),
                kind: kind.into(),
                target: target.into(),
                node: node.map(str::to_owned),
                unconditional,
            });
    }
    fn annotation(&mut self, source: &str, syntax: SyntaxNode<'_>) {
        self.edge(source, syntax, "type_usage", None);
    }
    fn parameter(&mut self, scope: &str, syntax: SyntaxNode<'_>) {
        if let Some(annotation) = syntax.child_by_field_name("type") {
            self.annotation(scope, annotation);
        }
        let name = syntax.child_by_field_name("name").or_else(|| {
            if syntax.kind() == "identifier" {
                Some(syntax)
            } else {
                children(syntax).into_iter().find(|n| {
                    matches!(
                        n.kind(),
                        "identifier" | "list_splat_pattern" | "dictionary_splat_pattern"
                    )
                })
            }
        });
        if let Some(name) = name {
            let raw = text(name, &self.file.content)
                .trim_start_matches('*')
                .to_owned();
            self.bind(scope, &raw, "parameter", "", None, true);
        }
    }
    fn import(&mut self, syntax: SyntaxNode<'_>, scope: &str, unconditional: bool) {
        let module = syntax.child_by_field_name("module_name");
        let prefix = module
            .map(|n| text(n, &self.file.content).to_owned())
            .unwrap_or_default();
        let absolute = if prefix.starts_with('.') {
            let levels = prefix.chars().take_while(|c| *c == '.').count();
            let mut parts: Vec<_> = self.context.module.split('.').collect();
            let init = self.file.path.ends_with("/__init__.py") || self.file.path == "__init__.py";
            let drop = levels.saturating_sub(usize::from(init));
            if drop >= parts.len() {
                "<unknown-relative-import>".into()
            } else {
                parts.truncate(parts.len() - drop);
                let suffix = prefix.trim_start_matches('.');
                if suffix.is_empty() {
                    parts.join(".")
                } else {
                    qualified(&parts.join("."), suffix)
                }
            }
        } else {
            prefix
        };
        for child in children(syntax) {
            if module.is_some_and(|m| m.id() == child.id()) {
                continue;
            }
            if child.kind() == "wildcard_import" {
                self.bind(scope, "*", "dynamic", &absolute, None, false);
                self.edge(scope, child, "imports", Some(format!("{absolute}.*")));
                continue;
            }
            if !matches!(child.kind(), "dotted_name" | "aliased_import") {
                continue;
            }
            let target = child.child_by_field_name("name").unwrap_or(child);
            let raw = text(target, &self.file.content).to_owned();
            let alias = child
                .child_by_field_name("alias")
                .map(|n| text(n, &self.file.content).to_owned());
            let from = module.is_some();
            let bound = alias.clone().unwrap_or_else(|| {
                if from {
                    raw.clone()
                } else {
                    raw.split('.').next().unwrap_or(&raw).into()
                }
            });
            let imported = if from {
                qualified(&absolute, &raw)
            } else {
                raw.clone()
            };
            let binding_target = if !from && alias.is_none() {
                bound.clone()
            } else {
                imported.clone()
            };
            self.bind(
                scope,
                &bound,
                if from {
                    "import_symbol"
                } else {
                    "import_module"
                },
                &binding_target,
                None,
                unconditional,
            );
            self.edge(scope, target, "imports", Some(imported));
        }
    }
    fn assigned(
        &mut self,
        target: SyntaxNode<'_>,
        syntax: SyntaxNode<'_>,
        owner: &str,
        scope: &str,
        class: Option<(&str, &str)>,
        unconditional: bool,
    ) {
        if target.kind() == "identifier" {
            let name = text(target, &self.file.content).to_owned();
            let class_field = class.is_some_and(|(id, _)| id == scope);
            let mut node = self.make_node(
                syntax,
                &name,
                &qualified(owner, &name),
                if class_field { "field" } else { "variable" },
                Some(scope),
            );
            node.attributes
                .insert("definition_start".into(), target.start_byte().to_string());
            node.signature = text(syntax, &self.file.content).into();
            node.fingerprint = digest(text(syntax, &self.file.content));
            if let Some(value) = syntax.child_by_field_name("right") {
                node.attributes.insert(
                    "python.initializer".into(),
                    text(value, &self.file.content).into(),
                );
            }
            if let Some(annotation) = syntax.child_by_field_name("type") {
                node.attributes.insert(
                    "python.declared_type".into(),
                    text(annotation, &self.file.content).into(),
                );
            }
            self.bind(scope, &name, "variable", "", Some(&node.id), unconditional);
            self.facts.nodes.push(node);
        } else if target.kind() == "attribute" {
            let object = target.child_by_field_name("object");
            let attribute = target.child_by_field_name("attribute");
            if let (Some((class_id, class_name)), Some(object), Some(attribute)) =
                (class, object, attribute)
            {
                if text(object, &self.file.content) == "self" {
                    let name = text(attribute, &self.file.content);
                    let mut node = self.make_node(
                        syntax,
                        name,
                        &qualified(class_name, name),
                        "field",
                        Some(class_id),
                    );
                    node.attributes
                        .insert("python.kind".into(), "instance_attribute_assignment".into());
                    node.signature = text(syntax, &self.file.content).into();
                    self.facts.nodes.push(node);
                }
            }
            if let Some(object) = object {
                if object.kind() == "identifier" && text(object, &self.file.content) != "self" {
                    self.bind(
                        scope,
                        text(object, &self.file.content),
                        "dynamic",
                        "",
                        None,
                        false,
                    );
                }
            }
        } else {
            for child in children(target) {
                self.assigned(child, syntax, owner, scope, class, unconditional);
            }
        }
    }
    fn walk(
        &mut self,
        syntax: SyntaxNode<'_>,
        owner: &str,
        scope: &str,
        class: Option<(&str, &str)>,
        unconditional: bool,
        depth: usize,
    ) -> Result<()> {
        check_cancelled()?;
        anyhow::ensure!(
            depth < 512,
            "Python syntax exceeds the 512-level traversal budget"
        );
        match syntax.kind() {
            "decorated_definition" => {
                let decorators: Vec<_> = children(syntax)
                    .into_iter()
                    .filter(|n| n.kind() == "decorator")
                    .collect();
                if let Some(definition) = syntax.child_by_field_name("definition") {
                    let before = self.facts.nodes.len();
                    self.walk(definition, owner, scope, class, false, depth + 1)?;
                    let evidence = self.evidence(syntax);
                    if let Some(node) = self.facts.nodes.get_mut(before) {
                        let raw: Vec<_> = decorators
                            .iter()
                            .map(|n| text(*n, &self.file.content).to_owned())
                            .collect();
                        node.attributes
                            .insert("decorators".into(), serde_json::to_string(&raw)?);
                        node.evidence = evidence;
                        node.fingerprint = digest(text(syntax, &self.file.content));
                        node.signature = format!("{}\n{}", raw.join("\n"), node.signature);
                        if raw.iter().any(|s| {
                            let callable =
                                s.trim_start_matches('@').split('(').next().unwrap_or("");
                            matches!(
                                callable.rsplit('.').next().unwrap_or(""),
                                "route" | "get" | "post" | "put" | "patch" | "delete" | "websocket"
                            )
                        }) {
                            node.attributes
                                .insert("entry_kind".into(), "http_route".into());
                        }
                    }
                }
                for decorator in decorators {
                    self.walk(decorator, owner, scope, class, unconditional, depth + 1)?;
                }
                return Ok(());
            }
            "function_definition" | "class_definition" => {
                let Some(name_syntax) = syntax.child_by_field_name("name") else {
                    return Ok(());
                };
                let name = text(name_syntax, &self.file.content).to_owned();
                let q = qualified(owner, &name);
                let is_class = syntax.kind() == "class_definition";
                let mut node = self.make_node(
                    syntax,
                    &name,
                    &q,
                    if is_class { "type" } else { "function" },
                    Some(scope),
                );
                node.attributes.insert(
                    "python.kind".into(),
                    if is_class {
                        "class"
                    } else if text(syntax, &self.file.content).starts_with("async ") {
                        "async_function"
                    } else {
                        "function"
                    }
                    .into(),
                );
                if name.starts_with("test_") || name.starts_with("Test") {
                    node.is_test = true;
                    node.attributes.insert("test".into(), "true".into());
                }
                if let Some(body) = syntax.child_by_field_name("body") {
                    if let Some(doc) = docstring(body, &self.file.content) {
                        node.attributes.insert("doc".into(), doc);
                        if let Some(syntax) = docstring_syntax(body) {
                            node.attributes.insert(
                                "python.doc_evidence".into(),
                                serde_json::to_string(&vec![self.evidence(syntax)])?,
                            );
                        }
                    }
                }
                if let Some(parameters) = syntax.child_by_field_name("parameters") {
                    node.attributes.insert(
                        "parameters".into(),
                        text(parameters, &self.file.content).into(),
                    );
                }
                if let Some(ret) = syntax.child_by_field_name("return_type") {
                    node.attributes
                        .insert("return_type".into(), text(ret, &self.file.content).into());
                }
                let id = node.id.clone();
                self.bind(
                    scope,
                    &name,
                    if is_class { "class" } else { "function" },
                    &q,
                    Some(&id),
                    unconditional,
                );
                self.facts.nodes.push(node);
                if let Some(parameters) = syntax.child_by_field_name("parameters") {
                    for parameter in children(parameters) {
                        self.parameter(&id, parameter);
                        if let Some(value) = parameter.child_by_field_name("value") {
                            self.walk(value, owner, scope, class, unconditional, depth + 1)?;
                        }
                    }
                }
                if let Some(ret) = syntax.child_by_field_name("return_type") {
                    self.annotation(&id, ret);
                }
                if let Some(bases) = syntax.child_by_field_name("superclasses") {
                    for base in children(bases) {
                        self.edge(&id, base, "extends", None);
                    }
                }
                if let Some(body) = syntax.child_by_field_name("body") {
                    self.walk(
                        body,
                        &q,
                        &id,
                        if is_class { Some((&id, &q)) } else { class },
                        true,
                        depth + 1,
                    )?;
                }
                return Ok(());
            }
            "lambda" => {
                let name = format!(
                    "<lambda@{}:{}>",
                    syntax.start_position().row + 1,
                    syntax.start_position().column
                );
                let mut node = self.make_node(
                    syntax,
                    &name,
                    &qualified(owner, &name),
                    "function",
                    Some(scope),
                );
                node.attributes
                    .insert("python.kind".into(), "lambda".into());
                let id = node.id.clone();
                self.facts.nodes.push(node);
                if let Some(parameters) = syntax.child_by_field_name("parameters") {
                    for parameter in children(parameters) {
                        self.parameter(&id, parameter);
                    }
                }
                if let Some(body) = syntax.child_by_field_name("body") {
                    self.walk(body, owner, &id, class, true, depth + 1)?;
                }
                return Ok(());
            }
            "import_statement" | "import_from_statement" | "future_import_statement" => {
                self.import(syntax, scope, unconditional);
                return Ok(());
            }
            "call" => {
                if let Some(callee) = syntax.child_by_field_name("function") {
                    self.edge(scope, callee, "calls", None);
                }
            }
            "assignment" | "augmented_assignment" | "named_expression" => {
                if let Some(target) = syntax
                    .child_by_field_name("left")
                    .or_else(|| syntax.child_by_field_name("name"))
                {
                    self.assigned(target, syntax, owner, scope, class, unconditional);
                    self.edge(scope, target, "writes", None);
                }
                if let Some(annotation) = syntax.child_by_field_name("type") {
                    self.annotation(scope, annotation);
                }
            }
            "global_statement" | "nonlocal_statement" => {
                for name in children(syntax)
                    .into_iter()
                    .filter(|n| n.kind() == "identifier")
                {
                    let name = text(name, &self.file.content).to_owned();
                    self.bind(scope, &name, "dynamic", "", None, false);
                    if syntax.kind() == "global_statement" {
                        if let Some(module) = self
                            .facts
                            .nodes
                            .iter()
                            .find(|n| n.kind == "module")
                            .map(|n| n.id.clone())
                        {
                            self.bind(&module, &name, "dynamic", "", None, false);
                        }
                    }
                }
            }
            "delete_statement" => {
                for name in children(syntax)
                    .into_iter()
                    .filter(|n| n.kind() == "identifier")
                {
                    self.bind(
                        scope,
                        text(name, &self.file.content),
                        "dynamic",
                        "",
                        None,
                        false,
                    );
                }
            }
            "for_statement" | "for_in_clause" => {
                if let Some(target) = syntax.child_by_field_name("left") {
                    self.assigned(target, syntax, owner, scope, class, false);
                }
            }
            "as_pattern" => {
                if let Some(alias) = syntax.child_by_field_name("alias") {
                    self.assigned(alias, syntax, owner, scope, class, false);
                }
            }
            "if_statement" => {
                if let Some(condition) = syntax.child_by_field_name("condition") {
                    let compact: String = text(condition, &self.file.content)
                        .chars()
                        .filter(|c| !c.is_whitespace())
                        .collect();
                    if matches!(
                        compact.as_str(),
                        "__name__==\"__main__\""
                            | "__name__=='__main__'"
                            | "\"__main__\"==__name__"
                            | "'__main__'==__name__"
                    ) && self
                        .facts
                        .nodes
                        .iter()
                        .any(|n| n.id == scope && n.kind == "module")
                    {
                        let mut node = self.make_node(
                            syntax,
                            "__main__",
                            &qualified(owner, "<main>"),
                            "function",
                            Some(scope),
                        );
                        node.attributes
                            .insert("entry_kind".into(), "python_main".into());
                        node.attributes
                            .insert("python.kind".into(), "main_guard".into());
                        node.attributes
                            .insert("python.lexical_scope".into(), scope.into());
                        let id = node.id.clone();
                        self.facts.nodes.push(node);
                        if let Some(body) = syntax.child_by_field_name("consequence") {
                            self.walk(body, owner, &id, class, false, depth + 1)?;
                        }
                        for child in children(syntax) {
                            if syntax
                                .child_by_field_name("consequence")
                                .is_none_or(|body| body.id() != child.id())
                            {
                                self.walk(child, owner, scope, class, false, depth + 1)?;
                            }
                        }
                        return Ok(());
                    }
                }
            }
            _ => {}
        }
        let control = matches!(
            syntax.kind(),
            "if_statement"
                | "elif_clause"
                | "else_clause"
                | "try_statement"
                | "except_clause"
                | "for_statement"
                | "while_statement"
                | "match_statement"
                | "case_clause"
                | "with_statement"
        );
        let condition_len = self.conditions.len();
        if control {
            let summary = syntax
                .child_by_field_name("condition")
                .map(|n| text(n, &self.file.content))
                .unwrap_or(syntax.kind());
            self.conditions.push(format!("python:{summary}"));
        }
        for child in children(syntax) {
            self.walk(
                child,
                owner,
                scope,
                class,
                unconditional && !control,
                depth + 1,
            )?;
        }
        self.conditions.truncate(condition_len);
        Ok(())
    }
}
