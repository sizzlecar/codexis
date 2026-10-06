//! Python facts for the flow map. Receivers are typed from parameter and
//! field annotations, class constructions and imports; nothing is executed.
use super::{
    compact, evidence_at, is_code, is_internal_path, is_status, ConfigField, Event, EventKind,
    Function, Program, ReadMode, Route, SharedField, SharedState, Target,
};
use crate::{
    index::Index,
    model::{Evidence, Node},
};
use anyhow::Result;
use std::collections::{BTreeMap, HashMap, HashSet};
use tree_sitter::{Node as Syntax, Parser};

const NON_ANCHOR: &[&str] = &[
    "get",
    "items",
    "keys",
    "values",
    "append",
    "extend",
    "format",
    "join",
    "split",
    "strip",
    "lower",
    "upper",
    "copy",
    "dict",
    "list",
    "str",
    "int",
    "len",
    "isinstance",
    "getattr",
    "setattr",
    "hasattr",
    "print",
    "debug",
    "info",
    "warning",
    "error",
    "exception",
];
const SYNC: &[&str] = &[
    "Lock(",
    "RLock(",
    "Semaphore(",
    "BoundedSemaphore(",
    "Condition(",
    "Event(",
    "Queue(",
    "LifoQueue(",
    "PriorityQueue(",
    "SimpleQueue(",
    "ContextVar(",
];
const HTTP_VERBS: &[&str] = &["get", "post", "put", "delete", "patch", "head", "options"];

#[derive(Clone, Copy, PartialEq)]
enum Origin {
    Param,
    Accessor,
    Held,
}

#[derive(Clone, Debug, PartialEq)]
enum Ty {
    Class(String),
    Module(String),
    Unknown,
}

struct World<'a> {
    by_id: HashMap<&'a str, &'a Node>,
    classes: HashMap<String, &'a Node>,
    class_names: HashMap<String, Vec<String>>,
    methods: HashMap<String, Vec<&'a Node>>,
    functions: HashMap<String, &'a Node>,
    modules: HashMap<String, &'a Node>,
    module_of_path: HashMap<String, String>,
    fields: HashMap<String, Vec<(String, String, String)>>,
    variables: HashMap<String, String>,
    initializers: HashMap<String, String>,
    config_paths: HashMap<String, String>,
    shared_fields: HashMap<String, HashSet<String>>,
}

pub(super) fn program(index: &Index, nodes: &[Node]) -> Result<Program> {
    let nodes: Vec<&Node> = nodes.iter().filter(|n| n.language == "python").collect();
    let world = World::new(&nodes);
    let mut by_file: BTreeMap<(&str, &str), Vec<&Node>> = BTreeMap::new();
    for node in nodes.iter().filter(|n| n.kind == "function") {
        by_file
            .entry((
                node.evidence.path.as_str(),
                node.evidence.content_hash.as_str(),
            ))
            .or_default()
            .push(node);
    }
    let mut contents = Vec::new();
    for ((path, hash), nodes) in by_file {
        contents.push((
            path.to_owned(),
            hash.to_owned(),
            index.content(hash)?,
            nodes,
        ));
    }
    let results = crate::frontend::parallel_map(&contents, |(path, hash, content, nodes)| {
        Ok(world.file(path, hash, content, nodes))
    })?;
    let mut functions: HashMap<String, Function> = HashMap::new();
    let mut statuses: HashMap<String, ExitCode> = HashMap::new();
    for (file_functions, file_statuses) in results {
        functions.extend(file_functions);
        for (class, status) in file_statuses {
            statuses.entry(class).or_insert(status);
        }
    }
    // A raised exception class (or a subclass) mapped to a status is an exit.
    for function in functions.values_mut() {
        function.events.retain_mut(|event| {
            let EventKind::Construct { owner, .. } = &event.kind else {
                return true;
            };
            match world.mapped_status(owner, &statuses) {
                Some((status, code)) => {
                    event.kind = EventKind::Exit { status, code };
                    true
                }
                None => false,
            }
        });
    }
    let mut routes = Vec::new();
    for node in nodes.iter().filter(|n| n.kind == "function") {
        let Some(raw) = node.attributes.get("decorators") else {
            continue;
        };
        let decorators: Vec<String> = serde_json::from_str(raw).unwrap_or_default();
        let module = node.qualified_name.rsplit_once('.').map_or("", |(m, _)| m);
        for decorator in decorators {
            let prefix = world.router_prefix(&decorator, module);
            for (method, path) in decorator_routes(&decorator) {
                let path = format!("{}{}", prefix.trim_end_matches('/'), path);
                routes.push(Route {
                    method,
                    internal: is_internal_path(&path),
                    path,
                    handler: Some(world.label(node)),
                    handler_id: Some(node.id.clone()),
                    evidence: node.evidence.clone(),
                });
            }
        }
    }
    let mut mains: Vec<String> = nodes
        .iter()
        .filter(|n| n.kind == "function" && n.name == "main")
        .filter(|n| {
            n.parent
                .as_deref()
                .and_then(|p| world.by_id.get(p))
                .is_some_and(|p| p.kind == "module")
        })
        .map(|n| n.id.clone())
        .collect();
    mains.sort();
    let public = nodes
        .iter()
        .filter(|n| n.kind == "function" && !n.name.starts_with('_'))
        .map(|n| n.id.clone())
        .collect();
    Ok(Program {
        functions,
        routes,
        mains,
        public,
        config: world.config_fields(&nodes),
        shared: world.shared_states(&nodes),
    })
}

fn decorator_routes(decorator: &str) -> Vec<(String, String)> {
    let body = decorator.trim().trim_start_matches('@');
    let Some((callee, args)) = body.split_once('(') else {
        return vec![];
    };
    let verb = callee.rsplit('.').next().unwrap_or("").trim();
    let Some(path) = first_string(args) else {
        return vec![];
    };
    if !path.starts_with('/') {
        return vec![];
    }
    if HTTP_VERBS.contains(&verb) {
        return vec![(verb.to_ascii_uppercase(), path)];
    }
    if verb == "websocket" {
        return vec![("WS".into(), path)];
    }
    if verb != "route" && verb != "api_route" {
        return vec![];
    }
    let methods: Vec<String> = args
        .split_once("methods")
        .map(|(_, rest)| {
            rest.split(']')
                .next()
                .unwrap_or("")
                .split(['"', '\''])
                .skip(1)
                .step_by(2)
                .map(|m| m.to_ascii_uppercase())
                .collect()
        })
        .unwrap_or_default();
    if methods.is_empty() {
        vec![("GET".into(), path)]
    } else {
        methods.into_iter().map(|m| (m, path.clone())).collect()
    }
}

fn first_string(text: &str) -> Option<String> {
    let start = text.find(['"', '\''])?;
    let quote = text[start..].chars().next()?;
    let rest = &text[start + 1..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_owned())
}

/// The first class-like name in an annotation (`Optional["Repo"]` → Repo).
fn annotation_names(annotation: &str) -> Vec<String> {
    annotation
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.'))
        .filter(|t| !t.is_empty())
        .map(|t| t.rsplit('.').next().unwrap_or(t).to_owned())
        .filter(|t| {
            t.starts_with(|c: char| c.is_ascii_uppercase())
                && !matches!(
                    t.as_str(),
                    "Optional"
                        | "Union"
                        | "List"
                        | "Dict"
                        | "Set"
                        | "Tuple"
                        | "Annotated"
                        | "Sequence"
                        | "Iterable"
                        | "Mapping"
                        | "Type"
                        | "Callable"
                        | "Any"
                        | "None"
                        | "Literal"
                        | "ClassVar"
                        | "Final"
                        | "Awaitable"
                )
        })
        .collect()
}

impl<'a> World<'a> {
    fn new(nodes: &[&'a Node]) -> Self {
        let by_id: HashMap<&str, &Node> = nodes.iter().map(|n| (n.id.as_str(), *n)).collect();
        let mut world = World {
            by_id,
            classes: HashMap::new(),
            class_names: HashMap::new(),
            methods: HashMap::new(),
            functions: HashMap::new(),
            modules: HashMap::new(),
            module_of_path: HashMap::new(),
            fields: HashMap::new(),
            variables: HashMap::new(),
            initializers: HashMap::new(),
            config_paths: HashMap::new(),
            shared_fields: HashMap::new(),
        };
        for node in nodes {
            match node.kind.as_str() {
                "module" => {
                    world.modules.insert(node.qualified_name.clone(), node);
                    world
                        .module_of_path
                        .insert(node.evidence.path.clone(), node.qualified_name.clone());
                }
                "type" => {
                    world.classes.insert(node.qualified_name.clone(), node);
                    world
                        .class_names
                        .entry(node.name.clone())
                        .or_default()
                        .push(node.qualified_name.clone());
                }
                _ => {}
            }
        }
        for node in nodes {
            let parent = node
                .parent
                .as_deref()
                .and_then(|p| world.by_id.get(p))
                .copied();
            match (node.kind.as_str(), parent) {
                ("function", Some(parent)) if parent.kind == "type" => world
                    .methods
                    .entry(parent.qualified_name.clone())
                    .or_default()
                    .push(node),
                ("function", Some(parent)) if parent.kind == "module" => {
                    world.functions.insert(node.qualified_name.clone(), node);
                }
                ("field", Some(parent)) if parent.kind == "type" => {
                    // `self.x = value` fields keep their assignment as signature.
                    let initializer = node
                        .attributes
                        .get("python.initializer")
                        .cloned()
                        .or_else(|| {
                            node.signature
                                .split_once('=')
                                .map(|(_, value)| value.trim().to_owned())
                        })
                        .unwrap_or_default();
                    let ty = node
                        .attributes
                        .get("python.declared_type")
                        .cloned()
                        .or_else(|| {
                            initializer
                                .split_once('(')
                                .map(|(callee, _)| callee.trim().to_owned())
                        })
                        .or_else(|| {
                            // A constructor parameter stored as-is keeps its annotation.
                            let parameter = initializer.trim();
                            parameter
                                .chars()
                                .all(|c| c.is_alphanumeric() || c == '_')
                                .then(|| init_annotation(nodes, &parent.id, parameter))
                                .flatten()
                        })
                        .unwrap_or_default();
                    world
                        .fields
                        .entry(parent.qualified_name.clone())
                        .or_default()
                        .push((node.name.clone(), ty, initializer));
                }
                ("variable", Some(parent)) if parent.kind == "module" => {
                    let ty = node
                        .attributes
                        .get("python.declared_type")
                        .cloned()
                        .or_else(|| {
                            node.attributes.get("python.initializer").and_then(|i| {
                                i.split_once('(')
                                    .map(|(callee, _)| callee.trim().to_owned())
                            })
                        })
                        .unwrap_or_default();
                    world.variables.insert(node.qualified_name.clone(), ty);
                    if let Some(initializer) = node.attributes.get("python.initializer") {
                        world
                            .initializers
                            .insert(node.qualified_name.clone(), initializer.clone());
                    }
                }
                _ => {}
            }
        }
        world.config_paths = world.config_roots(nodes);
        for (class, fields) in &world.fields {
            let sync: HashSet<String> = fields
                .iter()
                .filter(|(_, _, init)| SYNC.iter().any(|m| init.contains(m)))
                .map(|(name, _, _)| name.clone())
                .collect();
            if !sync.is_empty() {
                world.shared_fields.insert(class.clone(), sync);
            }
        }
        world
    }

    /// `router = APIRouter(prefix="/items")` declared in the same module.
    fn router_prefix(&self, decorator: &str, module: &str) -> String {
        let callee = decorator
            .trim()
            .trim_start_matches('@')
            .split('(')
            .next()
            .unwrap_or("");
        let Some((object, _)) = callee.rsplit_once('.') else {
            return String::new();
        };
        let Some(initializer) = self.initializers.get(&format!("{module}.{object}")) else {
            return String::new();
        };
        initializer
            .split_once("prefix")
            .and_then(|(_, rest)| first_string(rest.trim_start().trim_start_matches('=')))
            .filter(|p| p.starts_with('/'))
            .unwrap_or_default()
    }

    /// Classes named by an annotation, following one level of module-level
    /// aliases such as `Deps = Annotated[Services, Depends(get_services)]`.
    fn annotated_class(
        &self,
        annotation: &str,
        module: &str,
        imports: &[(String, String)],
    ) -> Option<String> {
        for name in annotation_names(annotation) {
            if let Some(found) = self.class(&name, module, imports) {
                return Some(found);
            }
            let alias = self
                .initializers
                .get(&format!("{module}.{name}"))
                .or_else(|| {
                    imports
                        .iter()
                        .find(|(bound, _)| *bound == name)
                        .and_then(|(_, full)| self.initializers.get(full))
                });
            if let Some(alias) = alias {
                let alias_module = module;
                if let Some(found) = annotation_names(alias)
                    .into_iter()
                    .find_map(|n| self.class(&n, alias_module, imports))
                {
                    return Some(found);
                }
            }
        }
        None
    }

    fn mapped_status(&self, class: &str, statuses: &HashMap<String, ExitCode>) -> Option<ExitCode> {
        let mut current = Some(class.to_owned());
        for _ in 0..6 {
            let name = current.take()?;
            if let Some(found) = statuses.get(&name) {
                return Some(found.clone());
            }
            let node = self.classes.get(&name)?;
            let bases = node.signature.split_once('(')?.1.split(')').next()?;
            let module = name.rsplit_once('.').map_or("", |(m, _)| m);
            current = annotation_names(bases)
                .into_iter()
                .find_map(|base| self.class(&base, module, &[]));
        }
        None
    }

    fn label(&self, node: &Node) -> String {
        match node.parent.as_deref().and_then(|p| self.by_id.get(p)) {
            Some(parent) if parent.kind == "type" => format!("{}.{}", parent.name, node.name),
            _ => {
                let parts: Vec<_> = node.qualified_name.rsplit('.').take(2).collect();
                match parts.as_slice() {
                    [name, module] => format!("{module}.{name}"),
                    _ => node.name.clone(),
                }
            }
        }
    }

    /// Resolve a class name visible in `module` to its qualified name.
    fn class(&self, name: &str, module: &str, imports: &[(String, String)]) -> Option<String> {
        let local = format!("{module}.{name}");
        if self.classes.contains_key(&local) {
            return Some(local);
        }
        if let Some((_, full)) = imports.iter().find(|(alias, _)| alias == name) {
            if self.classes.contains_key(full) {
                return Some(full.clone());
            }
        }
        match self.class_names.get(name).map(Vec::as_slice) {
            Some([only]) => Some(only.clone()),
            _ => None,
        }
    }

    fn config_roots(&self, nodes: &[&Node]) -> HashMap<String, String> {
        let mut paths = HashMap::new();
        for node in nodes.iter().filter(|n| n.kind == "type") {
            if !node.signature.contains("BaseSettings") {
                continue;
            }
            let mut stack = vec![(node.qualified_name.clone(), String::new(), 0)];
            while let Some((class, prefix, depth)) = stack.pop() {
                if depth > 6 || paths.contains_key(&class) {
                    continue;
                }
                paths.insert(class.clone(), prefix.clone());
                let module = class.rsplit_once('.').map_or("", |(m, _)| m).to_owned();
                for (field, ty, _) in self.fields.get(&class).into_iter().flatten() {
                    for name in annotation_names(ty) {
                        if let Some(nested) = self.class(&name, &module, &[]) {
                            let nested_node = self.classes[&nested];
                            if nested_node.signature.contains("BaseModel")
                                || nested_node.signature.contains("BaseSettings")
                            {
                                stack.push((nested, join(&prefix, field), depth + 1));
                            }
                        }
                    }
                }
            }
        }
        paths
    }

    fn config_fields(&self, nodes: &[&Node]) -> Vec<ConfigField> {
        let mut result = Vec::new();
        for node in nodes.iter().filter(|n| n.kind == "field") {
            let Some(parent) = node.parent.as_deref().and_then(|p| self.by_id.get(p)) else {
                continue;
            };
            let Some(prefix) = self.config_paths.get(&parent.qualified_name) else {
                continue;
            };
            result.push(ConfigField {
                path: join(prefix, &node.name),
                ty: compact(
                    node.attributes
                        .get("python.declared_type")
                        .map_or("", String::as_str),
                    48,
                ),
                evidence: node.evidence.clone(),
                reads: vec![],
            });
        }
        result.sort_by(|a, b| a.path.cmp(&b.path));
        result.dedup_by(|a, b| a.path == b.path);
        result
    }

    fn shared_states(&self, nodes: &[&Node]) -> Vec<SharedState> {
        let mut result = Vec::new();
        for (class, sync) in &self.shared_fields {
            let Some(node) = self.classes.get(class) else {
                continue;
            };
            let mut fields = nodes
                .iter()
                .filter(|f| {
                    f.kind == "field"
                        && f.parent.as_deref() == Some(&node.id)
                        && sync.contains(&f.name)
                })
                .map(|f| SharedField {
                    name: f.name.clone(),
                    ty: compact(
                        f.attributes
                            .get("python.initializer")
                            .map(String::as_str)
                            .or_else(|| f.signature.split_once('=').map(|(_, v)| v.trim()))
                            .unwrap_or(""),
                        48,
                    ),
                    evidence: f.evidence.clone(),
                })
                .collect::<Vec<_>>();
            fields.dedup_by(|a, b| a.name == b.name);
            result.push(SharedState {
                name: node.name.clone(),
                held: true,
                evidence: node.evidence.clone(),
                fields,
                accesses: vec![],
                on_trunk: false,
            });
        }
        for node in nodes.iter().filter(|n| n.kind == "variable") {
            let initializer = node
                .attributes
                .get("python.initializer")
                .map_or("", String::as_str);
            let module_level = node
                .parent
                .as_deref()
                .and_then(|p| self.by_id.get(p))
                .is_some_and(|p| p.kind == "module");
            if module_level && SYNC.iter().any(|m| initializer.contains(m)) {
                result.push(SharedState {
                    name: node
                        .qualified_name
                        .rsplit('.')
                        .take(2)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .join("."),
                    held: true,
                    evidence: node.evidence.clone(),
                    fields: vec![SharedField {
                        name: node.name.clone(),
                        ty: compact(initializer, 48),
                        evidence: node.evidence.clone(),
                    }],
                    accesses: vec![],
                    on_trunk: false,
                });
            }
        }
        result.sort_by(|a, b| a.name.cmp(&b.name));
        result
    }

    fn field_type(&self, class: &str, field: &str) -> Ty {
        let module = class.rsplit_once('.').map_or("", |(m, _)| m);
        for (name, ty, _) in self.fields.get(class).into_iter().flatten() {
            if name == field {
                for candidate in annotation_names(ty) {
                    if let Some(found) = self.class(&candidate, module, &[]) {
                        return Ty::Class(found);
                    }
                }
            }
        }
        Ty::Unknown
    }

    fn is_protocol(&self, class: &str) -> bool {
        self.classes.get(class).is_some_and(|node| {
            node.signature.split_once('(').is_some_and(|(_, bases)| {
                bases.split(')').next().unwrap_or("").contains("Protocol")
            })
        })
    }

    /// Parameter names after `self`, for structural matching.
    fn parameter_names(node: &Node) -> Vec<String> {
        let raw = node.attributes.get("parameters").map_or("", String::as_str);
        raw.trim()
            .trim_start_matches('(')
            .trim_end_matches(')')
            .split(',')
            .filter_map(|part| {
                let name = part
                    .split([':', '='])
                    .next()?
                    .trim()
                    .trim_start_matches('*');
                (!name.is_empty() && name != "self" && name != "cls").then(|| name.to_owned())
            })
            .collect()
    }

    /// Classes that implement every method of a Protocol with the same
    /// parameter names (typing.Protocol's structural conformance).
    fn implementations(&self, protocol: &str, name: &str) -> Vec<String> {
        let Some(required) = self.methods.get(protocol) else {
            return vec![];
        };
        let mut found = Vec::new();
        for (class, methods) in &self.methods {
            if class == protocol || self.is_protocol(class) {
                continue;
            }
            let conforms = required.iter().all(|wanted| {
                methods.iter().any(|m| {
                    m.name == wanted.name
                        && Self::parameter_names(m) == Self::parameter_names(wanted)
                })
            });
            if conforms {
                if let Some(method) = methods.iter().find(|m| m.name == name) {
                    found.push(method.id.clone());
                }
            }
        }
        found.sort();
        found
    }

    fn method(&self, class: &str, name: &str) -> Target {
        if self.is_protocol(class) {
            let mut found = self.implementations(class, name);
            return match found.len() {
                0 => Target::Unknown,
                1 => Target::Function(found.remove(0)),
                _ => Target::Dispatch(found),
            };
        }
        let mut current = Some(class.to_owned());
        let mut depth = 0;
        while let Some(class_name) = current.take() {
            if let Some(found) = self
                .methods
                .get(&class_name)
                .and_then(|methods| methods.iter().find(|m| m.name == name))
            {
                return Target::Function(found.id.clone());
            }
            // Single, project-defined base class only.
            let node = self.classes.get(&class_name);
            let base = node.and_then(|n| {
                let signature = &n.signature;
                let bases = signature.split_once('(')?.1.split(')').next()?;
                let names = annotation_names(bases);
                let module = class_name.rsplit_once('.').map_or("", |(m, _)| m);
                match names.as_slice() {
                    [one] => self.class(one, module, &[]),
                    _ => None,
                }
            });
            depth += 1;
            if depth < 4 {
                current = base;
            }
        }
        if self.classes.contains_key(class) {
            Target::Unknown
        } else {
            Target::External
        }
    }

    fn return_type(&self, id: &str, module: &str, imports: &[(String, String)]) -> Ty {
        let Some(node) = self.by_id.get(id) else {
            return Ty::Unknown;
        };
        if node.name == "__init__" {
            if let Some(class) = node.parent.as_deref().and_then(|p| self.by_id.get(p)) {
                return Ty::Class(class.qualified_name.clone());
            }
        }
        node.attributes
            .get("return_type")
            .and_then(|t| {
                annotation_names(t)
                    .into_iter()
                    .find_map(|name| self.class(&name, module, imports))
            })
            .map(Ty::Class)
            .unwrap_or(Ty::Unknown)
    }

    fn file(&self, path: &str, hash: &str, content: &str, nodes: &[&'a Node]) -> FileResult {
        let mut parser = Parser::new();
        if parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .is_err()
        {
            return (vec![], vec![]);
        }
        let Some(tree) = parser.parse(content, None) else {
            return (vec![], vec![]);
        };
        let root = tree.root_node();
        let module = self.module_of_path.get(path).cloned().unwrap_or_default();
        let imports = imports(root, content, &module, path);
        let mut output = Vec::new();
        let mut statuses = Vec::new();
        for node in nodes {
            let Some(syntax) = root
                .descendant_for_byte_range(node.evidence.start_byte, node.evidence.end_byte)
                .and_then(|s| function_syntax(s, node.evidence.start_byte))
            else {
                continue;
            };
            let Some(body) = syntax.child_by_field_name("body") else {
                continue;
            };
            let class = node
                .parent
                .as_deref()
                .and_then(|p| self.by_id.get(p))
                .filter(|p| p.kind == "type")
                .map(|p| p.qualified_name.clone());
            let mut walker = Walker {
                world: self,
                path,
                hash,
                content,
                module: module.clone(),
                imports: &imports,
                class: class.clone(),
                locals: Vec::new(),
                guards: Vec::new(),
                events: Vec::new(),
                statuses: Vec::new(),
                status_locals: Vec::new(),
            };
            if let Some(parameters) = syntax.child_by_field_name("parameters") {
                walker.parameters(parameters);
            }
            walker.walk(body);
            statuses.extend(walker.statuses);
            let mut events = walker.events;
            events.sort_by_key(|e| (e.at, e.end));
            output.push((
                node.id.clone(),
                Function {
                    label: self.label(node),
                    sink: node
                        .name
                        .to_ascii_lowercase()
                        .split('_')
                        .any(|w| matches!(w, "error" | "errors" | "fail" | "abort")),
                    owner: class.and_then(|c| self.classes.get(&c).map(|n| n.name.clone())),
                    evidence: node.evidence.clone(),
                    events,
                },
            ));
        }
        (output, statuses)
    }
}

/// Annotation of a named `__init__` parameter of the class.
fn init_annotation(nodes: &[&Node], class: &str, parameter: &str) -> Option<String> {
    let init = nodes.iter().find(|n| {
        n.kind == "function" && n.name == "__init__" && n.parent.as_deref() == Some(class)
    })?;
    let parameters = init.attributes.get("parameters")?;
    let inner = parameters
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')');
    inner.split(',').find_map(|part| {
        let (name, annotation) = part.split_once(':')?;
        (name.trim() == parameter).then(|| {
            annotation
                .split('=')
                .next()
                .unwrap_or(annotation)
                .trim()
                .to_owned()
        })
    })
}

fn join(prefix: &str, field: &str) -> String {
    if prefix.is_empty() {
        field.to_owned()
    } else {
        format!("{prefix}.{field}")
    }
}

fn function_syntax(syntax: Syntax<'_>, start: usize) -> Option<Syntax<'_>> {
    let mut current = Some(syntax);
    while let Some(candidate) = current {
        if candidate.start_byte() == start {
            match candidate.kind() {
                "function_definition" => return Some(candidate),
                "decorated_definition" => {
                    return candidate
                        .child_by_field_name("definition")
                        .filter(|d| d.kind() == "function_definition")
                }
                _ => {}
            }
        }
        current = candidate.parent();
    }
    None
}

fn text<'s>(syntax: Syntax<'_>, content: &'s str) -> &'s str {
    &content[syntax.byte_range()]
}

fn named(syntax: Syntax<'_>) -> Vec<Syntax<'_>> {
    let mut cursor = syntax.walk();
    syntax
        .named_children(&mut cursor)
        .filter(|n| n.kind() != "comment")
        .collect()
}

/// Module-level imports as (bound name, absolute dotted path).
fn imports(root: Syntax<'_>, content: &str, module: &str, path: &str) -> Vec<(String, String)> {
    let mut output = Vec::new();
    for statement in named(root) {
        match statement.kind() {
            "import_statement" => {
                for child in named(statement) {
                    let (name, alias) = match child.kind() {
                        "aliased_import" => (
                            child
                                .child_by_field_name("name")
                                .map(|n| text(n, content).to_owned()),
                            child
                                .child_by_field_name("alias")
                                .map(|n| text(n, content).to_owned()),
                        ),
                        "dotted_name" => (Some(text(child, content).to_owned()), None),
                        _ => (None, None),
                    };
                    if let Some(name) = name {
                        // `import a.b` binds `a`; `import a.b as x` binds `x` to `a.b`.
                        match alias {
                            Some(alias) => output.push((alias, name)),
                            None => {
                                let first = name.split('.').next().unwrap_or(&name).to_owned();
                                output.push((first.clone(), first));
                            }
                        }
                    }
                }
            }
            "import_from_statement" => {
                let source = statement.child_by_field_name("module_name");
                let prefix = source
                    .map(|s| text(s, content).to_owned())
                    .unwrap_or_default();
                let absolute = if prefix.starts_with('.') {
                    let levels = prefix.chars().take_while(|c| *c == '.').count();
                    let mut parts: Vec<&str> = module.split('.').collect();
                    let init = path.ends_with("__init__.py");
                    let drop = levels.saturating_sub(usize::from(init));
                    if drop >= parts.len() {
                        continue;
                    }
                    parts.truncate(parts.len() - drop);
                    let suffix = prefix.trim_start_matches('.');
                    if suffix.is_empty() {
                        parts.join(".")
                    } else {
                        format!("{}.{suffix}", parts.join("."))
                    }
                } else {
                    prefix
                };
                for child in named(statement) {
                    if source.is_some_and(|s| s.id() == child.id()) {
                        continue;
                    }
                    let (name, alias) = match child.kind() {
                        "aliased_import" => (
                            child
                                .child_by_field_name("name")
                                .map(|n| text(n, content).to_owned()),
                            child
                                .child_by_field_name("alias")
                                .map(|n| text(n, content).to_owned()),
                        ),
                        "dotted_name" => (Some(text(child, content).to_owned()), None),
                        _ => (None, None),
                    };
                    if let Some(name) = name {
                        output.push((
                            alias.unwrap_or_else(|| name.clone()),
                            format!("{absolute}.{name}"),
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    output
}

/// An error status and its machine-readable code.
type ExitCode = (String, Option<String>);
type FileResult = (Vec<(String, Function)>, Vec<(String, ExitCode)>);

struct Walker<'w, 'a> {
    world: &'w World<'a>,
    path: &'w str,
    hash: &'w str,
    content: &'w str,
    module: String,
    imports: &'w [(String, String)],
    class: Option<String>,
    locals: Vec<(String, Ty, Origin)>,
    guards: Vec<(String, usize, Evidence)>,
    events: Vec<Event>,
    /// Exception classes mapped to statuses in dictionaries such as
    /// `{NotFound: (404, "not_found", ..)}`.
    statuses: Vec<(String, ExitCode)>,
    /// Locals assigned only error status literals (`status = 409 if .. else 503`).
    status_locals: Vec<(String, String)>,
}

impl Walker<'_, '_> {
    fn evidence(&self, syntax: Syntax<'_>) -> Evidence {
        let start = syntax.start_position();
        let end = syntax.end_position();
        evidence_at(
            self.path,
            self.hash,
            syntax.start_byte(),
            syntax.end_byte(),
            (start.row, start.column),
            (end.row, end.column),
        )
    }

    fn push(&mut self, at: Syntax<'_>, range: Syntax<'_>, kind: EventKind) {
        self.events.push(Event {
            at: at.start_byte(),
            start: range.start_byte(),
            end: range.end_byte(),
            evidence: self.evidence(at),
            arm: None,
            guard: self.guards.last().cloned(),
            kind,
        });
    }

    fn parameters(&mut self, parameters: Syntax<'_>) {
        for parameter in named(parameters) {
            let (name, annotation) = match parameter.kind() {
                "identifier" => (Some(text(parameter, self.content)), None),
                "typed_parameter" => (
                    named(parameter)
                        .into_iter()
                        .find(|n| n.kind() == "identifier")
                        .map(|n| text(n, self.content)),
                    parameter
                        .child_by_field_name("type")
                        .map(|t| text(t, self.content)),
                ),
                "typed_default_parameter" | "default_parameter" => (
                    parameter
                        .child_by_field_name("name")
                        .map(|n| text(n, self.content)),
                    parameter
                        .child_by_field_name("type")
                        .map(|t| text(t, self.content)),
                ),
                _ => (None, None),
            };
            let Some(name) = name else {
                continue;
            };
            if name == "self" || name == "cls" {
                continue;
            }
            let ty = annotation
                .and_then(|a| self.world.annotated_class(a, &self.module, self.imports))
                .map(Ty::Class)
                .unwrap_or(Ty::Unknown);
            self.locals.push((name.to_owned(), ty, Origin::Param));
        }
    }

    fn lookup(&self, name: &str) -> (Ty, Origin) {
        if let Some((_, ty, origin)) = self.locals.iter().rev().find(|(local, _, _)| local == name)
        {
            return (ty.clone(), *origin);
        }
        if let Some(ty) = self.variable(&format!("{}.{name}", self.module)) {
            return (ty, Origin::Held);
        }
        if let Some((_, full)) = self.imports.iter().find(|(alias, _)| alias == name) {
            if self.world.modules.contains_key(full) {
                return (Ty::Module(full.clone()), Origin::Held);
            }
            if let Some(ty) = self.variable(full) {
                return (ty, Origin::Held);
            }
        }
        (Ty::Unknown, Origin::Held)
    }

    fn variable(&self, qualified: &str) -> Option<Ty> {
        let declared = self.world.variables.get(qualified)?;
        let module = qualified.rsplit_once('.').map_or("", |(m, _)| m);
        annotation_names(declared)
            .into_iter()
            .find_map(|n| self.world.class(&n, module, self.imports))
            .map(Ty::Class)
    }

    fn walk(&mut self, syntax: Syntax<'_>) -> (Ty, Origin) {
        match syntax.kind() {
            // Nested functions (stream generators, callbacks) run on behalf of
            // the enclosing function; their calls belong to its flow.
            "function_definition" => {
                let mark = self.locals.len();
                if let Some(parameters) = syntax.child_by_field_name("parameters") {
                    for parameter in named(parameters) {
                        let name = parameter.child_by_field_name("name").unwrap_or(parameter);
                        self.locals.push((
                            text(name, self.content).to_owned(),
                            Ty::Unknown,
                            Origin::Held,
                        ));
                    }
                }
                if let Some(body) = syntax.child_by_field_name("body") {
                    self.walk(body);
                }
                self.locals.truncate(mark);
                (Ty::Unknown, Origin::Held)
            }
            "decorated_definition" => {
                if let Some(definition) = syntax.child_by_field_name("definition") {
                    if definition.kind() == "function_definition" {
                        self.walk(definition);
                    }
                }
                (Ty::Unknown, Origin::Held)
            }
            "class_definition" | "comment" => (Ty::Unknown, Origin::Held),
            "dictionary" => {
                for pair in named(syntax) {
                    if pair.kind() == "pair" {
                        if let (Some(key), Some(value)) = (
                            pair.child_by_field_name("key"),
                            pair.child_by_field_name("value"),
                        ) {
                            self.mapping(key, value);
                        }
                    }
                }
                for child in named(syntax) {
                    self.walk(child);
                }
                (Ty::Unknown, Origin::Held)
            }
            "raise_statement" => {
                if let Some(raised) = named(syntax).first().copied() {
                    let callee = if raised.kind() == "call" {
                        raised.child_by_field_name("function")
                    } else {
                        Some(raised)
                    };
                    if let Some(class) = callee.and_then(|c| self.exception_class(c)) {
                        self.push(
                            syntax,
                            syntax,
                            EventKind::Construct {
                                owner: class,
                                variant: String::new(),
                            },
                        );
                    }
                }
                for child in named(syntax) {
                    self.walk(child);
                }
                (Ty::Unknown, Origin::Held)
            }
            "identifier" => {
                let name = text(syntax, self.content);
                if name == "self" {
                    return (
                        self.class.clone().map(Ty::Class).unwrap_or(Ty::Unknown),
                        Origin::Held,
                    );
                }
                self.lookup(name)
            }
            "call" => self.call(syntax),
            "attribute" => self.attribute(syntax, None),
            "await" | "parenthesized_expression" => {
                let mut result = (Ty::Unknown, Origin::Held);
                for child in named(syntax) {
                    result = self.walk(child);
                }
                result
            }
            "assignment" => {
                let value = syntax
                    .child_by_field_name("right")
                    .map(|v| self.walk(v))
                    .unwrap_or((Ty::Unknown, Origin::Held));
                let annotated = syntax.child_by_field_name("type").and_then(|t| {
                    annotation_names(text(t, self.content))
                        .into_iter()
                        .find_map(|n| self.world.class(&n, &self.module, self.imports))
                });
                if let (Some(left), Some(right)) = (
                    syntax.child_by_field_name("left"),
                    syntax.child_by_field_name("right"),
                ) {
                    if left.kind() == "identifier"
                        && matches!(
                            right.kind(),
                            "integer" | "conditional_expression" | "parenthesized_expression"
                        )
                    {
                        let mut found = std::collections::BTreeSet::new();
                        collect_statuses(right, self.content, &mut found);
                        if !found.is_empty() {
                            let joined: Vec<String> = found.iter().map(i64::to_string).collect();
                            self.status_locals
                                .push((text(left, self.content).to_owned(), joined.join("/")));
                        }
                    }
                }
                if let Some(left) = syntax.child_by_field_name("left") {
                    if left.kind() == "identifier" {
                        let ty = annotated.map(Ty::Class).unwrap_or(value.0);
                        self.locals
                            .push((text(left, self.content).to_owned(), ty, value.1));
                    } else {
                        self.walk(left);
                    }
                }
                (Ty::Unknown, Origin::Held)
            }
            "for_statement" | "while_statement" => {
                let body = syntax.child_by_field_name("body");
                let header_end = body.map_or(syntax.end_byte(), |b| b.start_byte());
                let header = compact(
                    self.content[syntax.start_byte()..header_end].trim_end_matches(':'),
                    72,
                );
                self.push(syntax, syntax, EventKind::Loop { header });
                for field in ["right", "condition"] {
                    if let Some(value) = syntax.child_by_field_name(field) {
                        self.walk(value);
                    }
                }
                if let Some(left) = syntax.child_by_field_name("left") {
                    if left.kind() == "identifier" {
                        self.locals.push((
                            text(left, self.content).to_owned(),
                            Ty::Unknown,
                            Origin::Held,
                        ));
                    }
                }
                if let Some(body) = body {
                    self.walk(body);
                }
                if let Some(alternative) = syntax.child_by_field_name("alternative") {
                    self.walk(alternative);
                }
                (Ty::Unknown, Origin::Held)
            }
            "if_statement" => {
                let condition = syntax.child_by_field_name("condition");
                if let Some(condition) = condition {
                    self.walk(condition);
                }
                if let Some(consequence) = syntax.child_by_field_name("consequence") {
                    let label = condition
                        .map(|c| format!("if {}", compact(text(c, self.content), 56)))
                        .unwrap_or_default();
                    let evidence =
                        condition.map_or_else(|| self.evidence(syntax), |c| self.evidence(c));
                    self.guards.push((label, syntax.start_byte(), evidence));
                    self.walk(consequence);
                    self.guards.pop();
                }
                for child in named(syntax) {
                    if matches!(child.kind(), "elif_clause" | "else_clause") {
                        self.walk(child);
                    }
                }
                (Ty::Unknown, Origin::Held)
            }
            _ => {
                for child in named(syntax) {
                    self.walk(child);
                }
                (Ty::Unknown, Origin::Held)
            }
        }
    }

    fn attribute(&mut self, syntax: Syntax<'_>, consumer: Option<&str>) -> (Ty, Origin) {
        let (Some(object), Some(name)) = (
            syntax.child_by_field_name("object"),
            syntax.child_by_field_name("attribute"),
        ) else {
            return (Ty::Unknown, Origin::Held);
        };
        let (receiver, origin) = self.walk(object);
        let field = text(name, self.content);
        match &receiver {
            Ty::Class(class) => {
                if let Some(prefix) = self.world.config_paths.get(class) {
                    let mode = match origin {
                        Origin::Accessor => ReadMode::Current,
                        Origin::Param => ReadMode::Passed,
                        Origin::Held => ReadMode::Held,
                    };
                    let path = join(prefix, field);
                    // Record only the deepest configuration field of a chain.
                    if let Some(last) = self.events.last_mut() {
                        if matches!(&last.kind, EventKind::Read { path: previous, .. } if path.starts_with(previous.as_str()) && last.start == syntax.start_byte())
                        {
                            self.events.pop();
                        }
                    }
                    self.push(syntax, syntax, EventKind::Read { path, mode });
                }
                if self
                    .world
                    .shared_fields
                    .get(class)
                    .is_some_and(|f| f.contains(field))
                {
                    let state = self
                        .world
                        .classes
                        .get(class)
                        .map(|n| n.name.clone())
                        .unwrap_or_default();
                    self.push(
                        name,
                        syntax,
                        EventKind::Touch {
                            state,
                            field: field.to_owned(),
                            op: consumer.unwrap_or("read").to_owned(),
                        },
                    );
                }
                (self.world.field_type(class, field), origin)
            }
            Ty::Module(module) => {
                let full = format!("{module}.{field}");
                if self.world.modules.contains_key(&full) {
                    (Ty::Module(full), origin)
                } else if let Some(ty) = self.variable(&full) {
                    (ty, Origin::Held)
                } else {
                    (Ty::Unknown, origin)
                }
            }
            Ty::Unknown => (Ty::Unknown, origin),
        }
    }

    fn call(&mut self, syntax: Syntax<'_>) -> (Ty, Origin) {
        let Some(function) = syntax.child_by_field_name("function") else {
            return (Ty::Unknown, Origin::Held);
        };
        let arguments = syntax.child_by_field_name("arguments");
        if let Some(arguments) = arguments {
            if let Some((status, code)) = self.exit(function, arguments) {
                self.push(syntax, syntax, EventKind::Exit { status, code });
                return (Ty::Unknown, Origin::Held);
            }
        }
        let mut dispatch_label: Option<String> = None;
        let (target, method_call, name) = match function.kind() {
            "attribute" => {
                let method = function
                    .child_by_field_name("attribute")
                    .map(|a| text(a, self.content).to_owned())
                    .unwrap_or_default();
                let object = function.child_by_field_name("object");
                let receiver = match object {
                    Some(object) if object.kind() == "attribute" => {
                        self.attribute(object, Some(&method)).0
                    }
                    Some(object) => self.walk(object).0,
                    None => Ty::Unknown,
                };
                let target = match receiver {
                    Ty::Class(class) => {
                        let target = self.world.method(&class, &method);
                        if matches!(target, Target::Dispatch(_)) {
                            let short = class.rsplit('.').next().unwrap_or(&class);
                            dispatch_label = Some(format!("{short}.{method}"));
                        }
                        target
                    }
                    Ty::Module(module) => {
                        let full = format!("{module}.{method}");
                        match (
                            self.world.functions.get(&full),
                            self.world.classes.get(&full),
                        ) {
                            (Some(node), _) => Target::Function(node.id.clone()),
                            (None, Some(_)) => self.constructor(&full),
                            _ => Target::External,
                        }
                    }
                    Ty::Unknown => Target::Unknown,
                };
                (target, true, method)
            }
            "identifier" => {
                let name = text(function, self.content).to_owned();
                (self.resolve_name(&name), false, name)
            }
            _ => {
                self.walk(function);
                (Target::Unknown, false, String::new())
            }
        };
        let label = match &target {
            Target::Function(id) => self
                .world
                .by_id
                .get(id.as_str())
                .map(|n| self.world.label(n))
                .unwrap_or_else(|| name.clone()),
            Target::Dispatch(_) => dispatch_label.clone().unwrap_or_else(|| name.clone()),
            _ => compact(text(function, self.content), 48),
        };
        let anchor = match &target {
            Target::Function(_) | Target::Dispatch(_) => true,
            _ => method_call && !NON_ANCHOR.contains(&name.as_str()),
        };
        if anchor {
            self.push(
                function
                    .child_by_field_name("attribute")
                    .unwrap_or(function),
                syntax,
                EventKind::Call {
                    label,
                    target: target.clone(),
                    anchor,
                },
            );
        }
        if let Some(arguments) = arguments {
            for argument in named(arguments) {
                self.walk(argument);
            }
        }
        match &target {
            Target::Function(id) => (
                self.world.return_type(id, &self.module, self.imports),
                Origin::Accessor,
            ),
            _ => (Ty::Unknown, Origin::Accessor),
        }
    }

    /// `{NotFound: (404, "not_found", "..")}` maps an exception class.
    fn mapping(&mut self, key: Syntax<'_>, value: Syntax<'_>) {
        let Some(class) = self.exception_class(key) else {
            return;
        };
        if value.kind() != "tuple" {
            return;
        }
        let parts = named(value);
        let status = parts
            .iter()
            .find(|p| p.kind() == "integer")
            .and_then(|p| status_value(text(*p, self.content)));
        if let Some(status) = status {
            let code = parts
                .iter()
                .filter(|p| p.kind() == "string")
                .map(|p| string_value(text(*p, self.content)))
                .find(|v| is_code(v));
            self.statuses.push((class, (status, code)));
        }
    }

    /// A project class named by an identifier or `module.Class` attribute.
    fn exception_class(&self, syntax: Syntax<'_>) -> Option<String> {
        let name = text(syntax, self.content);
        let last = name.rsplit('.').next().unwrap_or(name);
        if !last.starts_with(|c: char| c.is_ascii_uppercase()) {
            return None;
        }
        self.world.class(last, &self.module, self.imports)
    }

    fn constructor(&self, class: &str) -> Target {
        match self.world.method(class, "__init__") {
            Target::Function(id) => Target::Function(id),
            _ => Target::External,
        }
    }

    fn resolve_name(&self, name: &str) -> Target {
        if self.locals.iter().any(|(local, _, _)| local == name) {
            return Target::Unknown;
        }
        let local = format!("{}.{name}", self.module);
        if let Some(node) = self.world.functions.get(&local) {
            return Target::Function(node.id.clone());
        }
        if self.world.classes.contains_key(&local) {
            return self.constructor(&local);
        }
        if let Some((_, full)) = self.imports.iter().find(|(alias, _)| alias == name) {
            if let Some(node) = self.world.functions.get(full) {
                return Target::Function(node.id.clone());
            }
            if self.world.classes.contains_key(full) {
                return self.constructor(full);
            }
        }
        Target::External
    }

    /// `HTTPException(status_code=404, detail="not_found")`, `abort(403)` and
    /// responses built with an explicit error status.
    fn exit(
        &self,
        function: Syntax<'_>,
        arguments: Syntax<'_>,
    ) -> Option<(String, Option<String>)> {
        let callee = text(function, self.content);
        let last = callee.rsplit('.').next().unwrap_or(callee);
        let mut status = None;
        let mut code = None;
        let mut strings = Vec::new();
        for (position, argument) in named(arguments).into_iter().enumerate() {
            match argument.kind() {
                "keyword_argument" => {
                    let key = argument
                        .child_by_field_name("name")
                        .map(|n| text(n, self.content))
                        .unwrap_or("");
                    let Some(value) = argument.child_by_field_name("value") else {
                        continue;
                    };
                    match key {
                        "status_code" | "status" => {
                            let raw = text(value, self.content);
                            status = status_value(raw).or_else(|| {
                                self.status_locals
                                    .iter()
                                    .rev()
                                    .find(|(local, _)| local == raw)
                                    .map(|(_, statuses)| statuses.clone())
                            })
                        }
                        "detail" | "code" | "error" | "error_code" | "message" => {
                            if value.kind() == "string" {
                                let literal = string_value(text(value, self.content));
                                if is_code(&literal) {
                                    code = Some(literal);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                "identifier" if position == 0 => {
                    let name = text(argument, self.content);
                    if let Some((_, statuses)) = self
                        .status_locals
                        .iter()
                        .rev()
                        .find(|(local, _)| local == name)
                    {
                        status = status.or_else(|| Some(statuses.clone()));
                    }
                }
                "integer" | "attribute" if position == 0 => {
                    if matches!(
                        last,
                        "abort" | "HTTPException" | "HTTPError" | "HttpError" | "Abort"
                    ) || last.ends_with("Exception")
                        || last.ends_with("Error")
                    {
                        status = status.or_else(|| status_value(text(argument, self.content)));
                    }
                }
                "string" => {
                    let literal = string_value(text(argument, self.content));
                    if is_code(&literal) {
                        strings.push(literal);
                    }
                }
                _ => {}
            }
        }
        let status = status?;
        Some((status, code.or_else(|| strings.pop())))
    }
}

fn collect_statuses(
    syntax: Syntax<'_>,
    content: &str,
    output: &mut std::collections::BTreeSet<i64>,
) {
    if syntax.kind() == "integer" {
        if let Ok(value) = text(syntax, content).parse::<i64>() {
            if is_status(value) {
                output.insert(value);
            }
        }
        return;
    }
    for child in named(syntax) {
        collect_statuses(child, content, output);
    }
}

fn status_value(text: &str) -> Option<String> {
    if let Ok(value) = text.trim().parse::<i64>() {
        return is_status(value).then(|| value.to_string());
    }
    // starlette/fastapi `status.HTTP_404_NOT_FOUND`, http.HTTPStatus.NOT_FOUND is not mapped.
    let name = text.rsplit('.').next()?;
    let digits: String = name
        .strip_prefix("HTTP_")?
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits
        .parse::<i64>()
        .ok()
        .filter(|v| is_status(*v))
        .map(|v| v.to_string())
}

fn string_value(text: &str) -> String {
    let trimmed = text.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    trimmed.trim_matches(['"', '\'']).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decorators_name_methods_and_paths() {
        assert_eq!(
            decorator_routes(r#"@app.get("/items/{id}")"#),
            vec![("GET".into(), "/items/{id}".into())]
        );
        assert_eq!(
            decorator_routes(r#"@bp.route("/x", methods=["POST", "PUT"])"#),
            vec![("POST".into(), "/x".into()), ("PUT".into(), "/x".into())]
        );
        assert!(decorator_routes("@property").is_empty());
    }

    #[test]
    fn statuses_come_from_literals_and_named_constants() {
        assert_eq!(status_value("404"), Some("404".into()));
        assert_eq!(status_value("status.HTTP_409_CONFLICT"), Some("409".into()));
        assert_eq!(status_value("200"), None);
    }
}
