//! Bounded lexical resolution of captured facts, without running Python or importing modules.
use super::{SemanticOutcome, SemanticResolver};
use crate::frontend::python::Binding;
use crate::model::{AnalysisContext, Capability, Diagnostic, FileFacts, Node, ProjectModel};
use crate::source::{check_cancelled, SourceSet};
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::{Duration, Instant};

pub struct PythonSemanticResolver;

impl SemanticResolver for PythonSemanticResolver {
    fn provider(&self) -> Result<String> {
        Ok("python-static:1".into())
    }

    fn enrich(
        &self,
        _source: &SourceSet,
        project: &ProjectModel,
        context: &AnalysisContext,
        facts: &mut FileFacts,
        progress: bool,
    ) -> Result<SemanticOutcome> {
        let provider = self.provider()?;
        let started = Instant::now();
        let timeout = Duration::from_secs(context.semantic_timeout_secs.max(1));
        let nodes: HashMap<_, _> = facts.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let mut definitions = BTreeMap::<&str, Vec<&Node>>::new();
        let mut bindings = BTreeMap::<&str, Vec<Binding>>::new();
        for node in &facts.nodes {
            if node.language != "python" {
                continue;
            }
            definitions
                .entry(node.qualified_name.as_str())
                .or_default()
                .push(node);
            if let Some(value) = node.attributes.get("python.bindings") {
                bindings.insert(node.id.as_str(), serde_json::from_str(value)?);
            }
        }
        let selected_packages: BTreeSet<_> = project
            .packages
            .iter()
            .filter(|p| {
                context
                    .scope
                    .as_ref()
                    .is_some_and(|s| p.name == *s || p.root == *s)
            })
            .map(|p| p.id.as_str())
            .collect();
        let selected = |node: &Node| {
            node.language == "python"
                && context.scope.as_ref().is_none_or(|scope| {
                    selected_packages.contains(node.package.as_str())
                        || node.evidence.path.starts_with(scope)
                        || node.qualified_name.starts_with(scope)
                })
        };
        let selected_count = facts
            .edges
            .iter()
            .filter(|e| {
                e.kind == "calls" && nodes.get(e.source.as_str()).is_some_and(|n| selected(n))
            })
            .count();
        if context.scope.is_some() && selected_count == 0 {
            bail!("the selected scope contains no eligible Python call sites");
        }
        let mut examined = 0;
        let mut resolved = 0;
        let mut blocked = 0;
        let mut partial = false;
        let mut diagnostics = Vec::new();
        for edge in &mut facts.edges {
            check_cancelled()?;
            if !matches!(edge.kind.as_str(), "calls" | "imports") {
                continue;
            }
            let Some(caller) = nodes.get(edge.source.as_str()).copied() else {
                continue;
            };
            if !selected(caller) {
                continue;
            }
            if examined >= context.semantic_request_limit || started.elapsed() >= timeout {
                partial = true;
                diagnostics.push(Diagnostic::warning("semantic_budget", format!("Python static resolution stopped after {examined} sites; selected scope has {selected_count} call sites"), None));
                break;
            }
            examined += 1;
            let target = if edge.kind == "imports" {
                unique(&definitions, &edge.target_name)
                    .filter(|n| n.kind == "module" || safe_definition(n, &bindings))
            } else {
                resolve_call(caller, &edge.target_name, &nodes, &definitions, &bindings)
            };
            if let Some(target) = target {
                edge.target = Some(target.id.clone());
                edge.resolution = "resolved".into();
                edge.provider = provider.clone();
                resolved += 1;
            } else if edge.kind == "calls" {
                blocked += 1;
            }
        }
        if blocked > 0 {
            partial = true;
            diagnostics.push(Diagnostic::warning("python_dynamic_breakpoints", format!("{blocked} selected calls remain unresolved: dynamic/member calls, external targets, shadowed, conditional or decorated bindings are not guessed"), None));
        }
        if progress {
            eprintln!("Python static resolution: {resolved} targets resolved, {blocked} call breakpoints, {examined} sites examined");
        }
        Ok(SemanticOutcome {
            capability: Capability {
                name: "python_static_resolution".into(), provider,
                scope: context.scope.as_ref().map_or_else(|| "Captured Python lexical bindings".into(), |s| format!("Captured Python lexical bindings in {s}")),
                limitations: vec![
                    "Only unique unconditional function declarations and explicit local import aliases are linked".into(),
                    "Instance/class dispatch, decorator effects, wildcard imports, rebinding, conditional definitions and dynamic execution remain breakpoints".into(),
                    "Package import initialization and runtime mutation are not modeled; resolved edges describe static binding evidence, not guaranteed execution".into(),
                ],
            }, diagnostics, partial,
        })
    }
}

fn unique<'a>(definitions: &BTreeMap<&str, Vec<&'a Node>>, name: &str) -> Option<&'a Node> {
    let values = definitions.get(name)?;
    (values.len() == 1).then(|| values[0])
}

fn safe_definition(node: &Node, bindings: &BTreeMap<&str, Vec<Binding>>) -> bool {
    if node.kind != "function"
        || node
            .attributes
            .get("python.kind")
            .is_some_and(|k| matches!(k.as_str(), "lambda" | "main_guard"))
    {
        return false;
    }
    let Some(parent) = node.parent.as_deref() else {
        return false;
    };
    let Some(scope) = bindings.get(parent) else {
        return false;
    };
    if scope.iter().any(|b| b.name == "*") {
        return false;
    }
    let matches: Vec<_> = scope.iter().filter(|b| b.name == node.name).collect();
    matches.len() == 1
        && matches[0].kind == "function"
        && matches[0].unconditional
        && matches[0].node.as_deref() == Some(node.id.as_str())
}

fn resolve_call<'a>(
    caller: &'a Node,
    name: &str,
    nodes: &HashMap<&str, &'a Node>,
    definitions: &BTreeMap<&str, Vec<&'a Node>>,
    bindings: &BTreeMap<&str, Vec<Binding>>,
) -> Option<&'a Node> {
    // Attribute syntax is accepted only after an explicit module import binding.
    // Calls such as self.f(), object.f(), factory().f() and indexed callables stop here.
    let parts: Vec<_> = name.split('.').collect();
    if parts.is_empty()
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.chars().all(|c| c.is_alphanumeric() || c == '_'))
    {
        return None;
    }
    let first = parts[0];
    let mut scope = Some(caller);
    for _ in 0..128 {
        let node = scope?;
        if let Some(redirect) = node.attributes.get("python.lexical_scope") {
            scope = nodes.get(redirect.as_str()).copied();
            continue;
        }
        // A class namespace is not a lexical enclosing scope for method bodies.
        if node.kind != "type" || node.id == caller.id {
            if let Some(entries) = bindings.get(node.id.as_str()) {
                if entries.iter().any(|b| b.name == "*") {
                    return None;
                }
                let candidates: Vec<_> = entries.iter().filter(|b| b.name == first).collect();
                if !candidates.is_empty() {
                    if candidates.len() != 1 || !candidates[0].unconditional {
                        return None;
                    }
                    let binding = candidates[0];
                    let target = match binding.kind.as_str() {
                        "function" if parts.len() == 1 => {
                            nodes.get(binding.node.as_deref()?).copied()
                        }
                        "import_symbol" if parts.len() == 1 => unique(definitions, &binding.target),
                        "import_module" | "import_symbol" if parts.len() > 1 => {
                            // An imported symbol may be a function or class; only a known module can own this attribute call.
                            if binding.kind == "import_symbol"
                                && unique(definitions, &binding.target)
                                    .is_none_or(|n| n.kind != "module")
                            {
                                return None;
                            }
                            unique(
                                definitions,
                                &format!("{}.{}", binding.target, parts[1..].join(".")),
                            )
                        }
                        _ => None,
                    }?;
                    if safe_definition(target, bindings)
                        && target
                            .parent
                            .as_deref()
                            .and_then(|id| nodes.get(id))
                            .is_none_or(|p| p.kind != "type")
                    {
                        return Some(target);
                    }
                    return None;
                }
            }
        }
        scope = node.parent.as_deref().and_then(|id| nodes.get(id)).copied();
    }
    None
}
