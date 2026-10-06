//! Rust facts for the flow map. Function bodies are re-read from the stored
//! snapshot; receivers are typed only from declarations (parameters, fields,
//! annotated or constructed locals and return types), never from names alone.
use super::{
    compact, evidence_at, is_code, is_internal_path, is_status, ConfigField, Event, EventKind,
    Function, Program, ReadMode, Route, SharedField, SharedState, Target,
};
use crate::{
    index::Index,
    model::{Evidence, Node},
};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use tree_sitter::{Node as Syntax, Parser};

const ROUTE_VERBS: &[&str] = &[
    "route",
    "register",
    "add_route",
    "at",
    "nest",
    "resource",
    "service",
    "handle",
    "mount",
];
const HTTP_VERBS: &[&str] = &[
    "get", "post", "put", "delete", "patch", "head", "options", "any",
];
const ENTRY_METHODS: &[&str] = &[
    "handle", "call", "serve", "run", "process", "execute", "respond",
];
const NON_ANCHOR: &[&str] = &[
    "map",
    "map_err",
    "and_then",
    "or_else",
    "ok_or",
    "ok_or_else",
    "unwrap",
    "unwrap_or",
    "unwrap_or_else",
    "unwrap_or_default",
    "expect",
    "clone",
    "cloned",
    "copied",
    "into",
    "to_string",
    "to_owned",
    "as_ref",
    "as_deref",
    "as_mut",
    "as_str",
    "iter",
    "into_iter",
    "collect",
    "then",
    "then_some",
    "borrow",
    "borrow_mut",
    "ok",
    "err",
    "from",
    "is_some",
    "is_none",
    "is_ok",
    "is_err",
    "len",
    "is_empty",
    "filter",
    "inspect_err",
    "context",
    "with_context",
    "transpose",
    "flatten",
    "into_inner",
];
const SYNC_MARKERS: &[&str] = &[
    "Mutex<",
    "RwLock<",
    "Atomic",
    "ArcSwap",
    "DashMap",
    "Semaphore",
    "Notify",
    "OnceLock",
    "OnceCell",
    "LazyLock",
    "Lazy<",
    "mpsc::",
    "broadcast::",
    "watch::",
    "Condvar",
    "RefCell<",
];

#[derive(Clone, Debug, PartialEq)]
enum Ty {
    Named(String, Vec<Ty>),
    Dyn(String),
    Unknown,
}

#[derive(Clone, Copy, PartialEq)]
enum Origin {
    Param,
    Accessor,
    Held,
}

struct Method<'a> {
    name: String,
    node: &'a Node,
    trait_name: Option<String>,
}

struct World<'a> {
    by_id: HashMap<&'a str, &'a Node>,
    types: HashMap<String, Vec<&'a Node>>,
    fields: HashMap<&'a str, Vec<(String, String)>>,
    methods: HashMap<String, Vec<Method<'a>>>,
    trait_methods: HashMap<String, Vec<&'a Node>>,
    trait_impls: HashMap<String, BTreeSet<String>>,
    free: HashMap<String, &'a Node>,
    free_names: HashSet<String>,
    owner: HashMap<&'a str, String>,
    crates: HashSet<String>,
    modules: HashMap<String, Vec<&'a Node>>,
    config_paths: HashMap<String, String>,
    shared_fields: HashMap<String, HashSet<String>>,
}

pub(super) fn program(index: &Index, nodes: &[Node]) -> Result<Program> {
    let mut by_file: BTreeMap<(&str, &str), Vec<&Node>> = BTreeMap::new();
    for node in nodes.iter().filter(|n| n.language == "rust") {
        if node.kind == "function" || node.kind == "module" {
            by_file
                .entry((
                    node.evidence.path.as_str(),
                    node.evidence.content_hash.as_str(),
                ))
                .or_default()
                .push(node);
        }
    }
    let files: Vec<_> = by_file.into_iter().collect();
    // Configuration roots come from loader calls anywhere in the source; this
    // pass reads one file at a time.
    let mut loaded = BTreeSet::new();
    for ((_, hash), _) in &files {
        loaded_types(&index.content(hash)?, &mut loaded);
    }
    let world = World::new(nodes, &loaded);
    let config = world.config_fields(nodes);
    let shared = world.shared_states(nodes);
    // Bounded batches keep at most a few hundred source files in memory.
    let mut results = Vec::with_capacity(files.len());
    for batch in files.chunks(256) {
        let mut contents = Vec::with_capacity(batch.len());
        for ((path, hash), nodes) in batch {
            contents.push((
                path.to_string(),
                hash.to_string(),
                index.content(hash)?,
                nodes.clone(),
            ));
        }
        results.extend(crate::frontend::parallel_map(
            &contents,
            |(path, hash, content, nodes)| Ok(world.file(path, hash, content, nodes)),
        )?);
    }
    let mut functions: HashMap<String, Function> = HashMap::new();
    let mut routes = Vec::new();
    let mut statuses: HashMap<(String, String), ExitCode> = HashMap::new();
    for (file_functions, file_routes, file_statuses) in results {
        functions.extend(file_functions);
        routes.extend(file_routes);
        for (key, value) in file_statuses {
            statuses.entry(key).or_insert(value);
        }
    }
    // Variants mapped to an error status are exits wherever they are built.
    for function in functions.values_mut() {
        function.events.retain_mut(|event| {
            let EventKind::Construct { owner, variant } = &event.kind else {
                return true;
            };
            match statuses.get(&(owner.clone(), variant.clone())) {
                Some((status, code)) => {
                    event.kind = EventKind::Exit {
                        status: status.clone(),
                        code: code.clone(),
                    };
                    true
                }
                None => false,
            }
        });
    }
    for node in nodes.iter().filter(|n| n.kind == "function") {
        if let Some(attributes) = node.attributes.get("rust.attributes") {
            for attribute in attributes.lines() {
                if let Some((method, path)) = attribute_route(attribute) {
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
    }
    let mains = nodes
        .iter()
        .filter(|n| {
            n.kind == "function" && n.name == "main" && !world.owner.contains_key(n.id.as_str())
        })
        .map(|n| n.id.clone())
        .collect();
    let public = nodes
        .iter()
        .filter(|n| n.kind == "function" && n.visibility.starts_with("pub") && n.language == "rust")
        .map(|n| n.id.clone())
        .collect();
    Ok(Program {
        functions,
        routes,
        mains,
        public,
        config,
        shared,
    })
}

/// Types deserialized from configuration formats: `let x: T = serde_yaml::from_str(..)`
/// or `toml::from_str::<T>(..)`.
fn loaded_types(content: &str, output: &mut BTreeSet<String>) {
    const FORMATS: &[&str] = &[
        "yaml", "toml", "ron::", "json5", "envy", "figment", "config::",
    ];
    for line in content.lines() {
        let lower = line.to_ascii_lowercase();
        if !FORMATS.iter().any(|f| lower.contains(f))
            || ![
                "from_str",
                "from_slice",
                "from_reader",
                "deserialize",
                "from_env",
                "extract",
            ]
            .iter()
            .any(|f| line.contains(f))
        {
            continue;
        }
        let mut found = None;
        if let Some(open) = line.find("::<") {
            let rest = &line[open + 3..];
            found = rest.split(['>', ',']).next().map(simple);
        }
        if found.is_none() {
            if let Some(after) = line.trim_start().strip_prefix("let ") {
                let after = after.trim_start_matches("mut ");
                if let Some((_, ty)) = after.split_once(':') {
                    found = ty.split('=').next().map(simple);
                }
            }
        }
        if let Some(name) = found.filter(|n| {
            n.starts_with(|c: char| c.is_ascii_uppercase()) && n != "Self" && n != "Value"
        }) {
            output.insert(name);
        }
    }
}

fn attribute_route(attribute: &str) -> Option<(String, String)> {
    let inner = attribute.strip_prefix("#[")?.strip_suffix(']')?;
    let (name, rest) = inner.split_once('(')?;
    let verb = name.rsplit("::").next()?.trim();
    if !HTTP_VERBS.contains(&verb) && verb != "route" {
        return None;
    }
    let path = rest.split('"').nth(1)?;
    path.starts_with('/').then(|| {
        (
            if verb == "route" {
                String::new()
            } else {
                verb.to_ascii_uppercase()
            },
            path.to_owned(),
        )
    })
}

fn simple(name: &str) -> String {
    let name = name.split('<').next().unwrap_or(name).trim();
    name.rsplit("::").next().unwrap_or(name).trim().to_owned()
}

fn split_top(text: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in text.char_indices() {
        match c {
            '<' | '(' | '[' | '{' => depth += 1,
            '>' | ')' | ']' | '}' => depth -= 1,
            c if c == separator && depth == 0 => {
                parts.push(text[start..i].trim());
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(text[start..].trim());
    parts.into_iter().filter(|p| !p.is_empty()).collect()
}

fn parse_ty(text: &str, self_type: Option<&str>) -> Ty {
    let mut text = text.trim();
    loop {
        let before = text;
        text = text.trim_start_matches('&').trim_start();
        if text.starts_with('\'') {
            text = text
                .split_once(' ')
                .map_or("", |(_, rest)| rest)
                .trim_start();
        }
        text = text.strip_prefix("mut ").unwrap_or(text).trim_start();
        if before == text {
            break;
        }
    }
    if let Some(bound) = text
        .strip_prefix("dyn ")
        .or_else(|| text.strip_prefix("impl "))
    {
        let first = split_top(bound, '+').into_iter().next().unwrap_or("");
        return Ty::Dyn(simple(first));
    }
    if let Some(inner) = text.strip_prefix('[') {
        let inner = inner.trim_end_matches(']');
        let element = split_top(inner, ';').into_iter().next().unwrap_or("");
        return Ty::Named("Slice".into(), vec![parse_ty(element, self_type)]);
    }
    if text.is_empty() || text.starts_with('(') || text == "_" {
        return Ty::Unknown;
    }
    let (path, args) = match text.find('<') {
        Some(open) if text.ends_with('>') => (&text[..open], &text[open + 1..text.len() - 1]),
        _ => (text, ""),
    };
    let mut name = simple(path);
    if name == "Self" {
        match self_type {
            Some(own) => name = own.to_owned(),
            None => return Ty::Unknown,
        }
    }
    if name.is_empty() || name.len() == 1 && name.chars().all(|c| c.is_ascii_uppercase()) {
        return Ty::Unknown;
    }
    let args = split_top(args, ',')
        .into_iter()
        .filter(|a| !a.starts_with('\''))
        .map(|a| parse_ty(a.split('=').next_back().unwrap_or(a), self_type))
        .collect();
    Ty::Named(name, args)
}

fn deref(mut ty: Ty) -> Ty {
    loop {
        match ty {
            Ty::Named(ref name, ref args)
                if matches!(
                    name.as_str(),
                    "Arc"
                        | "Rc"
                        | "Box"
                        | "Pin"
                        | "Cow"
                        | "ManuallyDrop"
                        | "Ref"
                        | "RefMut"
                        | "MutexGuard"
                        | "RwLockReadGuard"
                        | "RwLockWriteGuard"
                        | "Guard"
                        | "Weak"
                        | "Owned"
                ) && !args.is_empty() =>
            {
                ty = args[0].clone();
            }
            _ => return ty,
        }
    }
}

fn first_arg(ty: &Ty) -> Ty {
    match ty {
        Ty::Named(_, args) => args.first().cloned().unwrap_or(Ty::Unknown),
        _ => Ty::Unknown,
    }
}

fn element(ty: &Ty) -> Ty {
    match deref(ty.clone()) {
        Ty::Named(name, args) => match name.as_str() {
            "BTreeMap" | "HashMap" | "IndexMap" | "DashMap" => {
                args.get(1).cloned().unwrap_or(Ty::Unknown)
            }
            "Vec" | "VecDeque" | "Slice" | "SmallVec" | "BTreeSet" | "HashSet" => {
                args.first().cloned().unwrap_or(Ty::Unknown)
            }
            _ => Ty::Unknown,
        },
        _ => Ty::Unknown,
    }
}

fn unwrap(ty: &Ty) -> Ty {
    match deref(ty.clone()) {
        Ty::Named(name, args) if matches!(name.as_str(), "Option" | "Result" | "Poll") => {
            args.first().cloned().map(deref).unwrap_or(Ty::Unknown)
        }
        other => other,
    }
}

/// Return types of library methods that only unwrap, borrow or look up values.
fn library_method(receiver: &Ty, method: &str) -> Ty {
    let base = deref(receiver.clone());
    match method {
        "unwrap" | "expect" | "unwrap_or_default" | "unwrap_or" | "unwrap_or_else"
        | "unwrap_unchecked" | "into_inner" => unwrap(&base),
        "ok_or" | "ok_or_else" | "context" | "with_context" => {
            Ty::Named("Result".into(), vec![unwrap(&base)])
        }
        "ok" => Ty::Named("Option".into(), vec![unwrap(&base)]),
        "clone" | "to_owned" | "as_ref" | "as_deref" | "as_mut" | "borrow" | "borrow_mut"
        | "cloned" | "copied" => base,
        "lock" | "read" | "write" | "try_lock" | "blocking_lock" | "load" | "load_full"
        | "get_ref" => match &base {
            Ty::Named(name, _)
                if matches!(
                    name.as_str(),
                    "Mutex" | "RwLock" | "ArcSwap" | "ArcSwapOption" | "ArcSwapAny"
                ) =>
            {
                deref(first_arg(&base))
            }
            _ => Ty::Unknown,
        },
        "get" | "get_mut" | "first" | "last" => match &base {
            Ty::Named(name, _)
                if matches!(
                    name.as_str(),
                    "BTreeMap" | "HashMap" | "IndexMap" | "Vec" | "VecDeque" | "Slice" | "SmallVec"
                ) =>
            {
                Ty::Named("Option".into(), vec![element(&base)])
            }
            _ => Ty::Unknown,
        },
        _ => Ty::Unknown,
    }
}

impl<'a> World<'a> {
    fn new(nodes: &'a [Node], loaded: &BTreeSet<String>) -> Self {
        let mut world = World {
            by_id: nodes.iter().map(|n| (n.id.as_str(), n)).collect(),
            types: HashMap::new(),
            fields: HashMap::new(),
            methods: HashMap::new(),
            trait_methods: HashMap::new(),
            trait_impls: HashMap::new(),
            free: HashMap::new(),
            free_names: HashSet::new(),
            owner: HashMap::new(),
            crates: HashSet::new(),
            modules: HashMap::new(),
            config_paths: HashMap::new(),
            shared_fields: HashMap::new(),
        };
        for node in nodes.iter().filter(|n| n.language == "rust") {
            if let Some(first) = node.qualified_name.split("::").next() {
                world.crates.insert(first.to_owned());
            }
            match node.kind.as_str() {
                "type" | "trait" => world.types.entry(node.name.clone()).or_default().push(node),
                "module" => world
                    .modules
                    .entry(node.evidence.path.clone())
                    .or_default()
                    .push(node),
                _ => {}
            }
        }
        for node in nodes.iter().filter(|n| n.language == "rust") {
            let parent = node
                .parent
                .as_deref()
                .and_then(|p| world.by_id.get(p))
                .copied();
            match node.kind.as_str() {
                "field" => {
                    if let (Some(parent), Some(ty)) =
                        (parent, node.attributes.get("rust.declared_type"))
                    {
                        if parent.kind == "type" {
                            world
                                .fields
                                .entry(parent.id.as_str())
                                .or_default()
                                .push((node.name.clone(), ty.clone()));
                        }
                    }
                }
                "function" => match parent {
                    Some(parent) if parent.kind == "impl" => {
                        let self_type = parent
                            .attributes
                            .get("rust.self_type")
                            .map(|t| match parse_ty(t, None) {
                                Ty::Named(name, _) | Ty::Dyn(name) => name,
                                Ty::Unknown => String::new(),
                            })
                            .unwrap_or_default();
                        if self_type.is_empty() {
                            continue;
                        }
                        let trait_name = parent.attributes.get("rust.trait").map(|t| simple(t));
                        if let Some(trait_name) = &trait_name {
                            world
                                .trait_impls
                                .entry(trait_name.clone())
                                .or_default()
                                .insert(self_type.clone());
                        }
                        world.owner.insert(&node.id, self_type.clone());
                        world.methods.entry(self_type).or_default().push(Method {
                            name: node.name.clone(),
                            node,
                            trait_name,
                        });
                    }
                    Some(parent) if parent.kind == "trait" => {
                        world.owner.insert(&node.id, parent.name.clone());
                        world
                            .trait_methods
                            .entry(parent.name.clone())
                            .or_default()
                            .push(node);
                    }
                    Some(parent) if parent.kind == "module" => {
                        world.free.insert(node.qualified_name.clone(), node);
                        world.free_names.insert(node.name.clone());
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        for modules in world.modules.values_mut() {
            modules.sort_by_key(|m| std::cmp::Reverse(m.evidence.start_byte));
        }
        world.config_paths = world.config_roots(nodes, loaded);
        for (type_id, fields) in &world.fields {
            let sync: HashSet<String> = fields
                .iter()
                .filter(|(_, ty)| SYNC_MARKERS.iter().any(|m| ty.contains(m)))
                .map(|(name, _)| name.clone())
                .collect();
            if let (false, Some(node)) = (sync.is_empty(), world.by_id.get(type_id)) {
                world
                    .shared_fields
                    .entry(node.name.clone())
                    .or_default()
                    .extend(sync);
            }
        }
        world
    }

    fn fields_of(&self, name: &str) -> Vec<&(String, String)> {
        self.types
            .get(name)
            .into_iter()
            .flatten()
            .flat_map(|ty| self.fields.get(ty.id.as_str()).into_iter().flatten())
            .collect()
    }

    fn label(&self, node: &Node) -> String {
        match self.owner.get(node.id.as_str()) {
            Some(owner) => format!("{owner}::{}", node.name),
            None => {
                let parts: Vec<_> = node.qualified_name.rsplit("::").take(2).collect();
                match parts.as_slice() {
                    [name, module] if node.name != "main" => format!("{module}::{name}"),
                    [name, module] => format!("{module}::{name}"),
                    _ => node.name.clone(),
                }
            }
        }
    }

    fn deserialize(node: &Node) -> bool {
        node.attributes.get("rust.attributes").is_some_and(|a| {
            a.lines()
                .any(|l| l.contains("derive") && l.contains("Deserialize"))
        })
    }

    /// Configuration roots are types deserialized by YAML/TOML/environment
    /// loaders in the source; without such a loader, Deserialize types named
    /// *Config/*Settings that no other Deserialize type contains.
    fn config_roots(&self, nodes: &[Node], loaded: &BTreeSet<String>) -> HashMap<String, String> {
        let deserialized: Vec<&Node> = nodes
            .iter()
            .filter(|n| n.kind == "type" && Self::deserialize(n))
            .collect();
        let config_types: HashSet<&str> = deserialized.iter().map(|n| n.name.as_str()).collect();
        let mut roots: Vec<&str> = loaded
            .iter()
            .map(String::as_str)
            .filter(|name| config_types.contains(name))
            .collect();
        if roots.is_empty() {
            let mut nested = HashSet::new();
            for node in &deserialized {
                for (_, ty) in self.fields.get(node.id.as_str()).into_iter().flatten() {
                    let (inner, _) = self.config_inner(ty);
                    nested.insert(inner);
                }
            }
            roots = config_types
                .iter()
                .copied()
                .filter(|name| {
                    !nested.contains(*name)
                        && ["Config", "Settings", "Configuration"]
                            .iter()
                            .any(|suffix| name.ends_with(suffix))
                })
                .collect();
        }
        roots.sort();
        let mut paths = HashMap::new();
        for root in roots {
            let mut stack = vec![(root.to_owned(), String::new(), 0)];
            while let Some((name, prefix, depth)) = stack.pop() {
                if depth > 6 || paths.contains_key(&name) {
                    continue;
                }
                paths.insert(name.clone(), prefix.clone());
                for (field, ty) in self.fields_of(&name) {
                    let (inner, star) = self.config_inner(ty);
                    if config_types.contains(inner.as_str()) {
                        let mut path = join_path(&prefix, field);
                        if star {
                            path.push_str(".*");
                        }
                        stack.push((inner, path, depth + 1));
                    }
                }
            }
        }
        paths
    }

    fn config_inner(&self, ty: &str) -> (String, bool) {
        let mut ty = parse_ty(ty, None);
        let mut star = false;
        loop {
            ty = deref(ty);
            match &ty {
                Ty::Named(name, args) if name == "Option" => {
                    ty = first_arg(&Ty::Named(name.clone(), args.clone()))
                }
                Ty::Named(name, _)
                    if matches!(
                        name.as_str(),
                        "Vec" | "BTreeMap" | "HashMap" | "IndexMap" | "BTreeSet" | "HashSet"
                    ) =>
                {
                    star = true;
                    ty = element(&ty);
                }
                Ty::Named(name, _) => return (name.clone(), star),
                _ => return (String::new(), star),
            }
        }
    }

    fn config_fields(&self, nodes: &[Node]) -> Vec<ConfigField> {
        let mut result = Vec::new();
        for node in nodes.iter().filter(|n| n.kind == "field") {
            let Some(parent) = node.parent.as_deref().and_then(|p| self.by_id.get(p)) else {
                continue;
            };
            let Some(prefix) = self.config_paths.get(&parent.name) else {
                continue;
            };
            if !Self::deserialize(parent) || !self.config_paths.contains_key(&parent.name) {
                continue;
            }
            let ty = node
                .attributes
                .get("rust.declared_type")
                .cloned()
                .unwrap_or_default();
            let (inner, star) = self.config_inner(&ty);
            let mut path = join_path(prefix, &node.name);
            if star && self.config_paths.contains_key(&inner) {
                path.push_str(".*");
            }
            result.push(ConfigField {
                path,
                ty: compact(&ty, 48),
                evidence: node.evidence.clone(),
                reads: vec![],
            });
        }
        result.sort_by(|a, b| a.path.cmp(&b.path));
        result.dedup_by(|a, b| a.path == b.path);
        result
    }

    fn shared_states(&self, nodes: &[Node]) -> Vec<SharedState> {
        let mut result = Vec::new();
        for node in nodes.iter().filter(|n| n.kind == "type") {
            let Some(sync) = self.shared_fields.get(&node.name) else {
                continue;
            };
            let fields = nodes
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
                            .get("rust.declared_type")
                            .map_or("", String::as_str),
                        48,
                    ),
                    evidence: f.evidence.clone(),
                })
                .collect::<Vec<_>>();
            if fields.is_empty() || result.iter().any(|s: &SharedState| s.name == node.name) {
                continue;
            }
            let held = self.fields.values().flatten().any(|(_, ty)| {
                ty.split(|c: char| !c.is_alphanumeric() && c != '_')
                    .any(|token| token == node.name)
            });
            result.push(SharedState {
                name: node.name.clone(),
                held,
                evidence: node.evidence.clone(),
                fields,
                accesses: vec![],
                on_trunk: false,
            });
        }
        result
    }

    fn field_type(&self, owner: &str, field: &str, crate_name: &str) -> Ty {
        let Some(candidates) = self.types.get(owner) else {
            return Ty::Unknown;
        };
        let mut found = Vec::new();
        for ty in candidates {
            if let Some(fields) = self.fields.get(ty.id.as_str()) {
                for (name, declared) in fields {
                    if name == field && !found.contains(declared) {
                        found.push(declared.clone());
                    }
                }
            }
        }
        if found.len() > 1 {
            // Same-named types in several crates: prefer the caller's crate.
            let local: Vec<_> = candidates
                .iter()
                .filter(|ty| ty.qualified_name.starts_with(&format!("{crate_name}::")))
                .filter_map(|ty| self.fields.get(ty.id.as_str()))
                .flatten()
                .filter(|(name, _)| name == field)
                .map(|(_, declared)| declared.clone())
                .collect();
            if local.len() == 1 {
                found = local;
            }
        }
        match found.as_slice() {
            [one] => parse_ty(one, Some(owner)),
            _ => Ty::Unknown,
        }
    }

    fn method(&self, owner: &str, name: &str, crate_name: &str) -> Target {
        if let Some(methods) = self.methods.get(owner) {
            let found: Vec<_> = methods.iter().filter(|m| m.name == name).collect();
            let preferred: Vec<_> = found
                .iter()
                .filter(|m| {
                    m.node
                        .qualified_name
                        .starts_with(&format!("{crate_name}::"))
                })
                .collect();
            if found.len() == 1 {
                return Target::Function(found[0].node.id.clone());
            }
            if preferred.len() == 1 {
                return Target::Function(preferred[0].node.id.clone());
            }
            if !found.is_empty() {
                return Target::Unknown;
            }
        }
        if self.trait_methods.contains_key(owner) {
            return self.dispatch(owner, name);
        }
        if self.types.contains_key(owner) {
            Target::Unknown
        } else {
            Target::External
        }
    }

    fn dispatch(&self, trait_name: &str, name: &str) -> Target {
        let mut candidates = Vec::new();
        for implementor in self.trait_impls.get(trait_name).into_iter().flatten() {
            for method in self.methods.get(implementor).into_iter().flatten() {
                if method.name == name && method.trait_name.as_deref() == Some(trait_name) {
                    candidates.push(method.node.id.clone());
                }
            }
        }
        let declared = self
            .trait_methods
            .get(trait_name)
            .into_iter()
            .flatten()
            .find(|m| m.name == name);
        match (candidates.len(), declared) {
            (0, Some(default)) => Target::Function(default.id.clone()),
            (0, None) => Target::Unknown,
            (1, _) => Target::Function(candidates.remove(0)),
            _ => Target::Dispatch(candidates),
        }
    }

    fn return_type(&self, target: &Target, owner: Option<&str>) -> Ty {
        let node = match target {
            Target::Function(id) => self.by_id.get(id.as_str()).copied(),
            Target::Dispatch(ids) => ids
                .first()
                .and_then(|id| self.by_id.get(id.as_str()).copied()),
            _ => None,
        };
        let Some(node) = node else {
            return Ty::Unknown;
        };
        let self_type = self
            .owner
            .get(node.id.as_str())
            .map(String::as_str)
            .or(owner);
        node.attributes
            .get("rust.return_type")
            .map(|t| parse_ty(t, self_type))
            .unwrap_or(Ty::Unknown)
    }

    fn file(&self, path: &str, hash: &str, content: &str, nodes: &[&'a Node]) -> FileResult {
        let mut parser = Parser::new();
        if parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .is_err()
        {
            return (vec![], vec![], vec![]);
        }
        let Some(tree) = parser.parse(content, None) else {
            return (vec![], vec![], vec![]);
        };
        let root = tree.root_node();
        let mut uses: Vec<(usize, usize, String, String, String)> = Vec::new();
        collect_uses(root, content, &mut |syntax, alias, full| {
            let module = self.module_at(path, syntax.start_byte());
            uses.push((syntax.start_byte(), syntax.end_byte(), module, alias, full));
        });
        let mut functions = Vec::new();
        let mut routes = Vec::new();
        let mut statuses = Vec::new();
        for node in nodes.iter().filter(|n| n.kind == "function") {
            let Some(syntax) = root
                .descendant_for_byte_range(node.evidence.start_byte, node.evidence.end_byte)
                .and_then(|s| {
                    let mut current = Some(s);
                    while let Some(candidate) = current {
                        if candidate.kind() == "function_item"
                            && candidate.start_byte() == node.evidence.start_byte
                        {
                            return Some(candidate);
                        }
                        current = candidate.parent();
                    }
                    None
                })
            else {
                continue;
            };
            let Some(body) = syntax.child_by_field_name("body") else {
                continue;
            };
            let module = self.module_at(path, node.evidence.start_byte);
            let crate_name = node
                .qualified_name
                .split("::")
                .next()
                .unwrap_or("")
                .to_owned();
            let module_uses: Vec<(String, String)> = uses
                .iter()
                .filter(|(_, _, m, _, _)| *m == module)
                .map(|(_, _, _, alias, full)| (alias.clone(), full.clone()))
                .collect();
            let owner = self.owner.get(node.id.as_str()).cloned();
            let mut walker = Walker {
                world: self,
                path,
                hash,
                content,
                node,
                owner: owner.clone(),
                crate_name,
                module,
                uses: module_uses,
                locals: Vec::new(),
                arms: Vec::new(),
                guards: Vec::new(),
                loop_paths: Vec::new(),
                events: Vec::new(),
                routes: Vec::new(),
                statuses: Vec::new(),
            };
            walker.params();
            walker.walk(body);
            let mut events = walker.events;
            events.sort_by_key(|e| (e.at, e.end));
            routes.extend(walker.routes);
            statuses.extend(walker.statuses);
            functions.push((
                node.id.clone(),
                Function {
                    label: self.label(node),
                    sink: is_sink(&node.name),
                    owner,
                    evidence: node.evidence.clone(),
                    events,
                },
            ));
        }
        (functions, routes, statuses)
    }

    fn module_at(&self, path: &str, at: usize) -> String {
        self.modules
            .get(path)
            .and_then(|modules| {
                modules
                    .iter()
                    .find(|m| {
                        m.evidence.start_byte <= at
                            && at < m.evidence.end_byte.max(m.evidence.start_byte + 1)
                    })
                    .or_else(|| modules.last())
            })
            .map(|m| m.qualified_name.clone())
            .unwrap_or_default()
    }

    fn resolve_type_entry(&self, name: &str) -> Option<String> {
        let methods = self.methods.get(name)?;
        let trait_methods: Vec<_> = methods.iter().filter(|m| m.trait_name.is_some()).collect();
        if trait_methods.len() == 1 {
            return Some(trait_methods[0].node.id.clone());
        }
        ENTRY_METHODS.iter().find_map(|entry| {
            trait_methods
                .iter()
                .chain(
                    methods
                        .iter()
                        .filter(|m| m.trait_name.is_none())
                        .collect::<Vec<_>>()
                        .iter(),
                )
                .find(|m| m.name == *entry)
                .map(|m| m.node.id.clone())
        })
    }
}

fn join_path(prefix: &str, field: &str) -> String {
    if prefix.is_empty() {
        field.to_owned()
    } else {
        format!("{prefix}.{field}")
    }
}

fn text<'s>(syntax: Syntax<'_>, content: &'s str) -> &'s str {
    &content[syntax.byte_range()]
}

fn named(syntax: Syntax<'_>) -> Vec<Syntax<'_>> {
    let mut cursor = syntax.walk();
    syntax
        .named_children(&mut cursor)
        .filter(|n| {
            !matches!(
                n.kind(),
                "line_comment" | "block_comment" | "attribute_item"
            )
        })
        .collect()
}

fn collect_uses(
    syntax: Syntax<'_>,
    content: &str,
    emit: &mut dyn FnMut(Syntax<'_>, String, String),
) {
    if syntax.kind() == "use_declaration" {
        if let Some(argument) = syntax.child_by_field_name("argument") {
            for (alias, full) in use_paths(text(argument, content)) {
                emit(syntax, alias, full);
            }
        }
        return;
    }
    if syntax.kind() == "function_item" {
        return;
    }
    for child in named(syntax) {
        collect_uses(child, content, emit);
    }
}

/// Expand `a::{b, c::d as e, f::*}` into (alias, full path) pairs.
fn use_paths(text: &str) -> Vec<(String, String)> {
    let mut normalized = String::new();
    let words: Vec<&str> = text.split_whitespace().collect();
    for (i, word) in words.iter().enumerate() {
        if *word == "as" && i > 0 {
            normalized.push('@');
            continue;
        }
        normalized.push_str(word);
    }
    let mut output = Vec::new();
    expand_use("", &normalized, &mut output);
    output
}

fn expand_use(prefix: &str, text: &str, output: &mut Vec<(String, String)>) {
    let join = |a: &str, b: &str| {
        if a.is_empty() {
            b.to_owned()
        } else {
            format!("{a}::{b}")
        }
    };
    if let Some(open) = text.find('{') {
        let head = text[..open].trim_end_matches("::");
        let inner = text[open + 1..].trim_end_matches('}');
        let base = join(prefix, head);
        for part in split_top(inner, ',') {
            expand_use(&base, part, output);
        }
        return;
    }
    let (path, alias) = match text.split_once('@') {
        Some((path, alias)) => (path, alias.to_owned()),
        None => (text, String::new()),
    };
    let full = join(prefix, path);
    if path == "*" || full.ends_with("::*") {
        output.push(("*".into(), full.trim_end_matches("::*").to_owned()));
        return;
    }
    if path == "self" {
        let name = if alias.is_empty() {
            prefix.rsplit("::").next().unwrap_or(prefix).to_owned()
        } else {
            alias
        };
        output.push((name, prefix.to_owned()));
        return;
    }
    let name = if alias.is_empty() {
        full.rsplit("::").next().unwrap_or(&full).to_owned()
    } else {
        alias
    };
    output.push((name, full));
}

/// A route handler's display label and function, if it resolves to one.
type HandlerRef = (String, Option<String>);
/// An error status and its machine-readable code.
type ExitCode = (String, Option<String>);
type VariantStatus = ((String, String), ExitCode);
type FileResult = (Vec<(String, Function)>, Vec<Route>, Vec<VariantStatus>);

struct Walker<'w, 'a> {
    world: &'w World<'a>,
    path: &'w str,
    hash: &'w str,
    content: &'w str,
    node: &'a Node,
    owner: Option<String>,
    crate_name: String,
    module: String,
    uses: Vec<(String, String)>,
    locals: Vec<(String, Ty, Origin)>,
    arms: Vec<String>,
    guards: Vec<(String, usize, Evidence)>,
    loop_paths: Vec<Vec<(String, Option<String>)>>,
    events: Vec<Event>,
    routes: Vec<Route>,
    statuses: Vec<VariantStatus>,
}

impl Walker<'_, '_> {
    /// `Enum::Variant` or `Self::Variant` with a project type as owner.
    fn variant(&self, path: &str) -> Option<(String, String)> {
        let path: String = path.chars().filter(|c| !c.is_whitespace()).collect();
        let segments: Vec<&str> = path.split("::").collect();
        let [.., owner, variant] = segments.as_slice() else {
            return None;
        };
        if !variant.starts_with(|c: char| c.is_ascii_uppercase())
            || !owner.starts_with(|c: char| c.is_ascii_uppercase())
        {
            return None;
        }
        let owner = if *owner == "Self" {
            self.owner.clone()?
        } else {
            simple(owner)
        };
        self.world
            .types
            .contains_key(&owner)
            .then(|| (owner, (*variant).to_owned()))
    }

    fn construct(&mut self, syntax: Syntax<'_>, path: &str) {
        if let Some((owner, variant)) = self.variant(path) {
            self.event(syntax, EventKind::Construct { owner, variant });
        }
    }

    /// A match arm `Enum::Variant .. => (StatusCode::X, .., "code", ..)`.
    fn arm_status(&mut self, pattern: Syntax<'_>, value: Syntax<'_>) {
        let source = text(pattern, self.content);
        let head = source.split(['(', '{', ' ', '|']).next().unwrap_or(source);
        let Some(key) = self.variant(head) else {
            return;
        };
        let mut stack = vec![(value, 0)];
        while let Some((node, depth)) = stack.pop() {
            if depth > 4 || node.kind() == "closure_expression" {
                continue;
            }
            if node.kind() == "tuple_expression" {
                let parts = named(node);
                let status = parts.iter().find_map(|p| match p.kind() {
                    "scoped_identifier" => {
                        status_constant(text(*p, self.content)).map(|v| v.to_string())
                    }
                    "integer_literal" => text(*p, self.content)
                        .parse::<i64>()
                        .ok()
                        .filter(|v| is_status(*v))
                        .map(|v| v.to_string()),
                    _ => None,
                });
                if let Some(status) = status.filter(|s| s.as_str() >= "400") {
                    let code = parts
                        .iter()
                        .filter(|p| p.kind() == "string_literal")
                        .map(|p| literal(*p, self.content))
                        .find(|v| is_code(v));
                    self.statuses.push((key, (status, code)));
                    return;
                }
            }
            for child in named(node) {
                stack.push((child, depth + 1));
            }
        }
    }
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

    fn event(&mut self, syntax: Syntax<'_>, kind: EventKind) {
        self.events.push(Event {
            at: syntax.start_byte(),
            start: syntax.start_byte(),
            end: syntax.end_byte(),
            evidence: self.evidence(syntax),
            arm: self.arms.last().cloned(),
            guard: self.guards.last().cloned(),
            kind,
        });
    }

    fn params(&mut self) {
        let Some(raw) = self.node.attributes.get("rust.parameter_types") else {
            return;
        };
        let Ok(params) = serde_json::from_str::<Vec<serde_json::Value>>(raw) else {
            return;
        };
        for param in params {
            let (Some(pattern), Some(ty)) = (param["pattern"].as_str(), param["type"].as_str())
            else {
                continue;
            };
            let name = pattern.trim_start_matches("mut ").trim();
            if name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                let ty = parse_ty(ty, self.owner.as_deref());
                self.locals.push((name.to_owned(), ty, Origin::Param));
            }
        }
    }

    fn lookup(&self, name: &str) -> (Ty, Origin) {
        self.locals
            .iter()
            .rev()
            .find(|(local, _, _)| local == name)
            .map(|(_, ty, origin)| (ty.clone(), *origin))
            .unwrap_or((Ty::Unknown, Origin::Held))
    }

    fn walk_children(&mut self, syntax: Syntax<'_>) {
        for child in named(syntax) {
            self.walk(child);
        }
    }

    /// Visit an expression once, recording events, and return its type.
    fn walk(&mut self, syntax: Syntax<'_>) -> (Ty, Origin) {
        match syntax.kind() {
            "function_item" | "macro_invocation" | "macro_definition" | "attribute_item"
            | "line_comment" | "block_comment" => (Ty::Unknown, Origin::Held),
            "self" => (
                self.owner
                    .clone()
                    .map(|o| Ty::Named(o, vec![]))
                    .unwrap_or(Ty::Unknown),
                Origin::Held,
            ),
            "identifier" => self.lookup(text(syntax, self.content)),
            "call_expression" => self.call(syntax),
            "field_expression" => self.field(syntax, None),
            "index_expression" => {
                let parts = named(syntax);
                let (ty, origin) = parts
                    .first()
                    .map(|p| self.chain(*p))
                    .unwrap_or((Ty::Unknown, Origin::Held));
                for part in parts.iter().skip(1) {
                    self.walk(*part);
                }
                (element(&ty), origin)
            }
            "try_expression" => {
                let (ty, origin) = named(syntax)
                    .first()
                    .map(|p| self.chain(*p))
                    .unwrap_or((Ty::Unknown, Origin::Held));
                (unwrap(&ty), origin)
            }
            "block" | "async_block" | "unsafe_block" => {
                let mark = self.locals.len();
                let mut result = (Ty::Unknown, Origin::Held);
                for child in named(syntax) {
                    result = self.walk(child);
                }
                self.locals.truncate(mark);
                result
            }
            "if_expression" => {
                let mark = self.locals.len();
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
                self.locals.truncate(mark);
                if let Some(alternative) = syntax.child_by_field_name("alternative") {
                    self.walk(alternative);
                }
                (Ty::Unknown, Origin::Held)
            }
            "await_expression"
            | "parenthesized_expression"
            | "reference_expression"
            | "unary_expression" => {
                let mut result = (Ty::Unknown, Origin::Held);
                for child in named(syntax) {
                    result = self.walk(child);
                }
                (deref(result.0), result.1)
            }
            "scoped_identifier" => {
                self.construct(syntax, text(syntax, self.content));
                (Ty::Unknown, Origin::Held)
            }
            "struct_expression" => {
                if let Some(name) = syntax.child_by_field_name("name") {
                    self.construct(syntax, text(name, self.content));
                }
                let name = syntax
                    .child_by_field_name("name")
                    .map(|n| simple(text(n, self.content)))
                    .unwrap_or_default();
                if let Some(body) = syntax.child_by_field_name("body") {
                    self.walk_children(body);
                }
                let name = if name == "Self" {
                    self.owner.clone().unwrap_or_default()
                } else {
                    name
                };
                (Ty::Named(name, vec![]), Origin::Held)
            }
            "let_declaration" => {
                let value = syntax
                    .child_by_field_name("value")
                    .map(|v| self.walk(v))
                    .unwrap_or((Ty::Unknown, Origin::Held));
                if let Some(alternative) = syntax.child_by_field_name("alternative") {
                    self.walk(alternative);
                }
                let ty = syntax
                    .child_by_field_name("type")
                    .map(|t| parse_ty(text(t, self.content), self.owner.as_deref()))
                    .unwrap_or(value.0);
                if let Some(pattern) = syntax.child_by_field_name("pattern") {
                    self.bind(pattern, ty, value.1);
                }
                (Ty::Unknown, Origin::Held)
            }
            "let_condition" => {
                let value = syntax
                    .child_by_field_name("value")
                    .map(|v| self.walk(v))
                    .unwrap_or((Ty::Unknown, Origin::Held));
                if let Some(pattern) = syntax.child_by_field_name("pattern") {
                    let source = text(pattern, self.content);
                    let inner = named(pattern);
                    if (source.starts_with("Some(") || source.starts_with("Ok("))
                        && inner.len() == 2
                        && inner[1].kind() == "identifier"
                    {
                        self.locals.push((
                            text(inner[1], self.content).to_owned(),
                            unwrap(&value.0),
                            value.1,
                        ));
                    } else {
                        self.bind(pattern, Ty::Unknown, value.1);
                    }
                }
                (Ty::Unknown, Origin::Held)
            }
            "closure_expression" => {
                let mark = self.locals.len();
                if let Some(parameters) = syntax.child_by_field_name("parameters") {
                    self.bind(parameters, Ty::Unknown, Origin::Held);
                }
                if let Some(body) = syntax.child_by_field_name("body") {
                    self.walk(body);
                }
                self.locals.truncate(mark);
                (Ty::Unknown, Origin::Held)
            }
            "match_expression" => {
                if let Some(value) = syntax.child_by_field_name("value") {
                    self.walk(value);
                }
                if let Some(body) = syntax.child_by_field_name("body") {
                    for arm in named(body) {
                        let mark = self.locals.len();
                        let label = arm
                            .child_by_field_name("pattern")
                            .and_then(|p| arm_label(text(p, self.content)));
                        if let Some(pattern) = arm.child_by_field_name("pattern") {
                            self.bind(pattern, Ty::Unknown, Origin::Held);
                        }
                        if let Some(label) = &label {
                            self.arms.push(label.clone());
                        }
                        if let Some(value) = arm.child_by_field_name("value") {
                            if let Some(pattern) = arm.child_by_field_name("pattern") {
                                self.arm_status(pattern, value);
                            }
                            self.walk(value);
                        }
                        if label.is_some() {
                            self.arms.pop();
                        }
                        self.locals.truncate(mark);
                    }
                }
                (Ty::Unknown, Origin::Held)
            }
            "for_expression" | "while_expression" | "loop_expression" => {
                let body = syntax.child_by_field_name("body");
                let header_end = body.map_or(syntax.end_byte(), |b| b.start_byte());
                let header = compact(&self.content[syntax.start_byte()..header_end], 72);
                self.event(syntax, EventKind::Loop { header });
                let mut paths = Vec::new();
                if let Some(value) = syntax.child_by_field_name("value") {
                    self.walk(value);
                    collect_route_literals(value, self.content, &mut paths);
                }
                if let Some(condition) = syntax.child_by_field_name("condition") {
                    self.walk(condition);
                }
                let mark = self.locals.len();
                if let Some(pattern) = syntax.child_by_field_name("pattern") {
                    self.bind(pattern, Ty::Unknown, Origin::Held);
                }
                self.loop_paths.push(paths);
                if let Some(body) = body {
                    self.walk(body);
                }
                self.loop_paths.pop();
                self.locals.truncate(mark);
                (Ty::Unknown, Origin::Held)
            }
            _ => {
                self.walk_children(syntax);
                (Ty::Unknown, Origin::Held)
            }
        }
    }

    fn bind(&mut self, pattern: Syntax<'_>, ty: Ty, origin: Origin) {
        match pattern.kind() {
            "identifier" => self
                .locals
                .push((text(pattern, self.content).to_owned(), ty, origin)),
            "mut_pattern" | "ref_pattern" => {
                if let Some(inner) = named(pattern).last() {
                    self.bind(*inner, ty, origin);
                }
            }
            _ => {
                let mut stack = vec![pattern];
                while let Some(next) = stack.pop() {
                    match next.kind() {
                        "identifier" => self.locals.push((
                            text(next, self.content).to_owned(),
                            Ty::Unknown,
                            Origin::Held,
                        )),
                        "scoped_identifier" | "type_identifier" | "scoped_type_identifier" => {}
                        "tuple_struct_pattern" | "struct_pattern" => {
                            let skip = next.child_by_field_name("type").map(|t| t.id());
                            stack.extend(named(next).into_iter().filter(|c| Some(c.id()) != skip));
                        }
                        _ => stack.extend(named(next)),
                    }
                }
            }
        }
    }

    /// Typed field access. Config and shared-state hits are recorded once,
    /// for the deepest field of a chain (`consumer` names the method that
    /// consumes the chain, if any).
    fn field(&mut self, syntax: Syntax<'_>, consumer: Option<&str>) -> (Ty, Origin) {
        let (Some(value), Some(name)) = (
            syntax.child_by_field_name("value"),
            syntax.child_by_field_name("field"),
        ) else {
            return (Ty::Unknown, Origin::Held);
        };
        let (receiver, origin) = self.chain(value);
        let field = text(name, self.content);
        self.field_hits(syntax, &receiver, field, origin, consumer);
        let base = deref(receiver);
        let ty = match &base {
            Ty::Named(owner, _) => self.world.field_type(owner, field, &self.crate_name),
            _ => Ty::Unknown,
        };
        (ty, origin)
    }

    /// Walk a value that is itself continued by an index, `?` or method.
    fn chain(&mut self, syntax: Syntax<'_>) -> (Ty, Origin) {
        if syntax.kind() == "field_expression" {
            self.field_inner(syntax)
        } else {
            self.walk(syntax)
        }
    }

    fn field_inner(&mut self, syntax: Syntax<'_>) -> (Ty, Origin) {
        let (Some(value), Some(name)) = (
            syntax.child_by_field_name("value"),
            syntax.child_by_field_name("field"),
        ) else {
            return (Ty::Unknown, Origin::Held);
        };
        let (receiver, origin) = self.chain(value);
        let field = text(name, self.content);
        let base = deref(receiver);
        if let Ty::Named(owner, _) = &base {
            if self
                .world
                .shared_fields
                .get(owner)
                .is_some_and(|fields| fields.contains(field))
            {
                self.event(
                    name,
                    EventKind::Touch {
                        state: owner.clone(),
                        field: field.to_owned(),
                        op: "read".into(),
                    },
                );
            }
        }
        let ty = match &base {
            Ty::Named(owner, _) => self.world.field_type(owner, field, &self.crate_name),
            _ => Ty::Unknown,
        };
        (ty, origin)
    }

    fn field_hits(
        &mut self,
        syntax: Syntax<'_>,
        receiver: &Ty,
        field: &str,
        origin: Origin,
        consumer: Option<&str>,
    ) {
        let Ty::Named(owner, _) = deref(receiver.clone()) else {
            return;
        };
        if let Some(prefix) = self.world.config_paths.get(&owner) {
            let mut path = join_path(prefix, field);
            if let Ty::Named(element_name, _) =
                element(&self.world.field_type(&owner, field, &self.crate_name))
            {
                if self.world.config_paths.contains_key(&element_name) {
                    path.push_str(".*");
                }
            }
            let mode = match origin {
                Origin::Accessor => ReadMode::Current,
                Origin::Param => ReadMode::Passed,
                Origin::Held => ReadMode::Held,
            };
            self.event(syntax, EventKind::Read { path, mode });
        }
        if self
            .world
            .shared_fields
            .get(&owner)
            .is_some_and(|fields| fields.contains(field))
        {
            self.event(
                syntax,
                EventKind::Touch {
                    state: owner,
                    field: field.to_owned(),
                    op: consumer.unwrap_or("read").to_owned(),
                },
            );
        }
    }

    fn call(&mut self, syntax: Syntax<'_>) -> (Ty, Origin) {
        let Some(function) = syntax.child_by_field_name("function") else {
            return (Ty::Unknown, Origin::Held);
        };
        let arguments = syntax.child_by_field_name("arguments");
        let function = if function.kind() == "generic_function" {
            function.child_by_field_name("function").unwrap_or(function)
        } else {
            function
        };
        if let Some(arguments) = arguments {
            if let Some((status, code)) = self.exit(function, arguments) {
                self.event(syntax, EventKind::Exit { status, code });
                let (target, _, _, _) = self.resolve(function, false);
                return (
                    self.world.return_type(&target, self.owner.as_deref()),
                    Origin::Held,
                );
            }
        }
        if function.kind() == "scoped_identifier"
            && self.variant(text(function, self.content)).is_some()
        {
            self.construct(syntax, text(function, self.content));
            if let Some(arguments) = arguments {
                self.walk_children(arguments);
            }
            return (Ty::Unknown, Origin::Held);
        }
        let (target, receiver, method, receiver_origin) = self.resolve(function, true);
        let name_node = function
            .child_by_field_name("field")
            .or_else(|| function.child_by_field_name("name"))
            .unwrap_or(function);
        let name = text(name_node, self.content).to_owned();
        if let Some(arguments) = arguments {
            if ROUTE_VERBS.contains(&name.as_str()) {
                self.route(syntax, arguments, &name);
            }
        }
        let label = match &target {
            Target::Function(id) => self
                .world
                .by_id
                .get(id.as_str())
                .map(|n| self.world.label(n))
                .unwrap_or_else(|| name.clone()),
            Target::Dispatch(ids) => ids
                .first()
                .and_then(|id| self.world.by_id.get(id.as_str()))
                .map(|n| {
                    let trait_name = n
                        .parent
                        .as_deref()
                        .and_then(|p| self.world.by_id.get(p))
                        .and_then(|p| p.attributes.get("rust.trait"))
                        .map(|t| simple(t))
                        .unwrap_or_default();
                    format!("{trait_name}::{}", n.name)
                })
                .unwrap_or_else(|| name.clone()),
            _ => compact(text(function, self.content), 48),
        };
        // Constructors, library paths (`Instant::now`) and combinators do not
        // anchor exits; project calls and method calls on values do.
        let method_call = function.kind() == "field_expression";
        let anchor = match &target {
            Target::Function(_) | Target::Dispatch(_) => true,
            Target::Unknown | Target::External => {
                method_call && !NON_ANCHOR.contains(&name.as_str())
            }
        };
        // Library calls that cannot anchor an exit are never shown; skip them.
        if anchor {
            self.events.push(Event {
                at: name_node.start_byte(),
                start: syntax.start_byte(),
                end: syntax.end_byte(),
                evidence: self.evidence(name_node),
                arm: self.arms.last().cloned(),
                guard: self.guards.last().cloned(),
                kind: EventKind::Call {
                    label,
                    target: target.clone(),
                    anchor,
                },
            });
        }
        if let Some(arguments) = arguments {
            self.walk_children(arguments);
        }
        // Library lookups and unwraps keep where a value came from; project
        // accessors and lock/load operations read the current value.
        match &target {
            Target::Function(_) | Target::Dispatch(_) => (
                self.world.return_type(&target, self.owner.as_deref()),
                Origin::Accessor,
            ),
            _ => {
                let ty = method
                    .as_deref()
                    .map(|m| library_method(&receiver, m))
                    .unwrap_or(Ty::Unknown);
                let origin = match method.as_deref() {
                    Some(
                        "lock" | "read" | "write" | "load" | "load_full" | "borrow" | "try_lock",
                    ) => Origin::Accessor,
                    Some(_) => receiver_origin,
                    None => Origin::Accessor,
                };
                (ty, origin)
            }
        }
    }

    /// Resolve the callee. Returns the target, the receiver type of a method
    /// call and the method name for library return types.
    fn resolve(
        &mut self,
        function: Syntax<'_>,
        record: bool,
    ) -> (Target, Ty, Option<String>, Origin) {
        match function.kind() {
            "field_expression" => {
                let (Some(value), Some(field)) = (
                    function.child_by_field_name("value"),
                    function.child_by_field_name("field"),
                ) else {
                    return (Target::Unknown, Ty::Unknown, None, Origin::Held);
                };
                let method = text(field, self.content).to_owned();
                let (receiver, origin) = if !record {
                    (Ty::Unknown, Origin::Held)
                } else if value.kind() == "field_expression" {
                    self.field(value, Some(&method))
                } else {
                    self.walk(value)
                };
                let target = match deref(receiver.clone()) {
                    Ty::Named(owner, _) => {
                        let target = self.world.method(&owner, &method, &self.crate_name);
                        if matches!(target, Target::External)
                            && matches!(owner.as_str(), "Option" | "Result")
                        {
                            Target::External
                        } else {
                            target
                        }
                    }
                    Ty::Dyn(trait_name) if self.world.trait_methods.contains_key(&trait_name) => {
                        self.world.dispatch(&trait_name, &method)
                    }
                    Ty::Dyn(_) => Target::External,
                    Ty::Unknown => Target::Unknown,
                };
                (target, receiver, Some(method), origin)
            }
            "scoped_identifier" => (
                self.resolve_path(text(function, self.content)),
                Ty::Unknown,
                None,
                Origin::Accessor,
            ),
            "identifier" => {
                let name = text(function, self.content);
                if self.locals.iter().any(|(local, _, _)| local == name) {
                    return (Target::Unknown, Ty::Unknown, None, Origin::Held);
                }
                (self.resolve_path(name), Ty::Unknown, None, Origin::Accessor)
            }
            _ => {
                if record {
                    self.walk(function);
                }
                (Target::Unknown, Ty::Unknown, None, Origin::Held)
            }
        }
    }

    fn resolve_path(&self, path: &str) -> Target {
        let path: String = path.chars().filter(|c| !c.is_whitespace()).collect();
        let path = path.split("::<").next().unwrap_or(&path).to_owned();
        let segments: Vec<&str> = path.split("::").filter(|s| !s.is_empty()).collect();
        let Some(last) = segments.last() else {
            return Target::Unknown;
        };
        if segments.len() == 1 {
            if last.starts_with(|c: char| c.is_ascii_uppercase()) {
                return Target::External;
            }
            if let Some(node) = self.world.free.get(&format!("{}::{last}", self.module)) {
                return Target::Function(node.id.clone());
            }
            for (alias, full) in &self.uses {
                if alias == last {
                    return self.resolve_absolute(full);
                }
            }
            for (alias, full) in &self.uses {
                if alias == "*" {
                    if let Some(node) = self
                        .world
                        .free
                        .get(&format!("{}::{last}", self.absolute(full)))
                    {
                        return Target::Function(node.id.clone());
                    }
                }
            }
            return if self.world.free_names.contains(*last) {
                Target::Unknown
            } else {
                Target::External
            };
        }
        let owner = segments[segments.len() - 2];
        if owner == "Self" {
            return match &self.owner {
                Some(own) => self.world.method(own, last, &self.crate_name),
                None => Target::Unknown,
            };
        }
        if owner.starts_with(|c: char| c.is_ascii_uppercase()) {
            let owner = simple(owner);
            if self.world.types.contains_key(&owner) || self.world.methods.contains_key(&owner) {
                return self.world.method(&owner, last, &self.crate_name);
            }
            return Target::External;
        }
        let first = segments[0];
        if let Some((_, full)) = self.uses.iter().find(|(alias, _)| alias == first) {
            let rest = segments[1..].join("::");
            return self.resolve_absolute(&format!("{full}::{rest}"));
        }
        self.resolve_absolute(&path)
    }

    fn absolute(&self, path: &str) -> String {
        let mut segments: Vec<&str> = path.split("::").collect();
        let mut base: Vec<String> = Vec::new();
        match segments.first().copied() {
            Some("crate") => {
                base.push(self.crate_name.clone());
                segments.remove(0);
            }
            Some("self") => {
                base.extend(self.module.split("::").map(str::to_owned));
                segments.remove(0);
            }
            Some("super") => {
                base.extend(self.module.split("::").map(str::to_owned));
                while segments.first() == Some(&"super") {
                    base.pop();
                    segments.remove(0);
                }
            }
            _ => {}
        }
        base.extend(segments.iter().map(|s| (*s).to_owned()));
        base.join("::")
    }

    fn resolve_absolute(&self, path: &str) -> Target {
        let absolute = self.absolute(path);
        let segments: Vec<&str> = absolute.split("::").collect();
        if let Some(node) = self.world.free.get(&absolute) {
            return Target::Function(node.id.clone());
        }
        if let Some(node) = self.world.free.get(&format!("{}::{absolute}", self.module)) {
            return Target::Function(node.id.clone());
        }
        if let Some(node) = self
            .world
            .free
            .get(&format!("{}::{absolute}", self.crate_name))
        {
            return Target::Function(node.id.clone());
        }
        if segments.len() >= 2 {
            let owner = segments[segments.len() - 2];
            if owner.starts_with(|c: char| c.is_ascii_uppercase())
                && self.world.types.contains_key(owner)
            {
                return self
                    .world
                    .method(owner, segments[segments.len() - 1], &self.crate_name);
            }
        }
        if self.world.crates.contains(segments[0]) {
            Target::Unknown
        } else {
            Target::External
        }
    }

    /// A call carrying an HTTP status literal and a machine-readable code.
    fn exit(
        &self,
        function: Syntax<'_>,
        arguments: Syntax<'_>,
    ) -> Option<(String, Option<String>)> {
        let callee = text(function, self.content);
        let args = named(arguments);
        let mut statuses = Vec::new();
        let mut direct_code = None;
        for arg in &args {
            match arg.kind() {
                "integer_literal" => {
                    if let Ok(value) = text(*arg, self.content)
                        .trim_end_matches(|c: char| c.is_ascii_alphabetic())
                        .parse::<i64>()
                    {
                        if is_status(value) {
                            statuses.push(value.to_string());
                        }
                    }
                }
                "scoped_identifier" => {
                    if let Some(code) = status_constant(text(*arg, self.content)) {
                        statuses.push(code.to_string());
                    }
                }
                "string_literal" if direct_code.is_none() => {
                    let value = literal(*arg, self.content);
                    if is_code(&value) {
                        direct_code = Some(value);
                    }
                }
                _ => {}
            }
        }
        let errorish = {
            let lower = callee.to_ascii_lowercase();
            [
                "error", "status", "response", "reply", "abort", "fail", "reject", "problem",
            ]
            .iter()
            .any(|w| lower.contains(w))
        };
        if statuses.is_empty() && errorish && direct_code.is_some() {
            if let Some(first) = args
                .first()
                .filter(|a| matches!(a.kind(), "match_expression" | "if_expression"))
            {
                let mut found = BTreeSet::new();
                collect_status_literals(*first, self.content, &mut found);
                statuses.extend(found.into_iter().map(|v| v.to_string()));
            }
        }
        if statuses.is_empty() {
            return None;
        }
        let code = direct_code.or_else(|| {
            let mut codes = Vec::new();
            for arg in &args {
                collect_codes(*arg, self.content, &mut codes, 0);
            }
            codes.pop()
        });
        if code.is_none() && !errorish {
            return None;
        }
        Some((statuses.join("/"), code))
    }

    fn route(&mut self, syntax: Syntax<'_>, arguments: Syntax<'_>, verb: &str) {
        let args = named(arguments);
        let mut paths = Vec::new();
        let mut method = String::new();
        for arg in &args {
            match arg.kind() {
                "string_literal" | "raw_string_literal" => {
                    let value = literal(*arg, self.content);
                    if looks_like_route(&value) {
                        paths.push((value, None));
                    } else if let Some(m) = http_method(&value) {
                        method = m;
                    }
                }
                "scoped_identifier" | "identifier" => {
                    let value = text(*arg, self.content);
                    if let Some(m) = http_method(value) {
                        method = m;
                    }
                }
                _ => {}
            }
        }
        let mut handler = None;
        for arg in &args {
            if matches!(arg.kind(), "string_literal" | "raw_string_literal") {
                continue;
            }
            if let Some((found, verb_method)) = self.handler(*arg, 0) {
                handler = Some(found);
                if method.is_empty() {
                    if let Some(m) = verb_method {
                        method = m;
                    }
                }
                break;
            }
        }
        if HTTP_VERBS.contains(&verb) && method.is_empty() {
            method = verb.to_ascii_uppercase();
        }
        if paths.is_empty() {
            if let Some(loop_paths) = self.loop_paths.last() {
                paths = loop_paths.clone();
            }
        }
        if paths.is_empty() {
            return;
        }
        let evidence = self.evidence(syntax);
        for (path, path_method) in paths {
            let (label, id) = match &handler {
                Some((label, id)) => (Some(label.clone()), id.clone()),
                None => (None, None),
            };
            self.routes.push(Route {
                method: path_method.unwrap_or_else(|| method.clone()),
                internal: is_internal_path(&path),
                path,
                handler: label,
                handler_id: id,
                evidence: evidence.clone(),
            });
        }
    }

    /// Find the handler named by a route argument: a function path, a type
    /// whose single trait method serves requests, or a verb wrapper.
    fn handler(&self, syntax: Syntax<'_>, depth: usize) -> Option<(HandlerRef, Option<String>)> {
        if depth > 4 {
            return None;
        }
        match syntax.kind() {
            "struct_expression" => {
                let name = simple(text(syntax.child_by_field_name("name")?, self.content));
                let id = self.world.resolve_type_entry(&name);
                Some(((self.entry_label(&name, id.as_deref()), id), None))
            }
            "call_expression" => {
                let function = syntax.child_by_field_name("function")?;
                let arguments = syntax.child_by_field_name("arguments")?;
                let name = text(function, self.content);
                let last = name
                    .rsplit("::")
                    .next()
                    .unwrap_or(name)
                    .rsplit('.')
                    .next()
                    .unwrap_or(name);
                let inner = named(arguments);
                if HTTP_VERBS.contains(&last) {
                    let found = inner.first().and_then(|a| self.handler(*a, depth + 1))?;
                    return Some((found.0, Some(last.to_ascii_uppercase())));
                }
                if matches!(
                    name,
                    "Arc::new" | "Box::new" | "Rc::new" | "std::sync::Arc::new"
                ) {
                    return inner.first().and_then(|a| self.handler(*a, depth + 1));
                }
                let segments: Vec<_> = name.split("::").collect();
                if segments.len() >= 2
                    && segments[segments.len() - 2].starts_with(|c: char| c.is_ascii_uppercase())
                {
                    let owner = simple(segments[segments.len() - 2]);
                    if self.world.types.contains_key(&owner) {
                        let id = self.world.resolve_type_entry(&owner);
                        return Some(((self.entry_label(&owner, id.as_deref()), id), None));
                    }
                }
                None
            }
            "identifier" | "scoped_identifier" => {
                let name = text(syntax, self.content);
                if let (Ty::Named(owner, _), _) = self.lookup(name) {
                    if self.world.methods.contains_key(&owner) {
                        let id = self.world.resolve_type_entry(&owner);
                        return Some(((self.entry_label(&owner, id.as_deref()), id), None));
                    }
                }
                let last = simple(name);
                if last.starts_with(|c: char| c.is_ascii_uppercase())
                    && self.world.methods.contains_key(&last)
                {
                    let id = self.world.resolve_type_entry(&last);
                    return Some(((self.entry_label(&last, id.as_deref()), id), None));
                }
                match self.resolve_path(name) {
                    Target::Function(id) => {
                        let label = self
                            .world
                            .by_id
                            .get(id.as_str())
                            .map(|n| self.world.label(n))?;
                        Some(((label, Some(id)), None))
                    }
                    _ => None,
                }
            }
            "reference_expression" | "parenthesized_expression" => named(syntax)
                .into_iter()
                .find_map(|child| self.handler(child, depth + 1)),
            _ => None,
        }
    }

    fn entry_label(&self, name: &str, id: Option<&str>) -> String {
        id.and_then(|id| self.world.by_id.get(id))
            .map(|n| self.world.label(n))
            .unwrap_or_else(|| name.to_owned())
    }
}

/// Name of an enum variant arm; `Ok`, `Err`, `Some`, wildcards and literals
/// carry no domain meaning and are omitted.
fn arm_label(pattern: &str) -> Option<String> {
    let head = pattern
        .split(['(', '{', ' ', '|'])
        .next()
        .unwrap_or(pattern)
        .trim();
    let name = head.rsplit("::").next().unwrap_or(head);
    if !name.starts_with(|c: char| c.is_ascii_uppercase())
        || matches!(name, "Ok" | "Err" | "Some" | "None" | "Self")
    {
        return None;
    }
    Some(compact(name, 32))
}

fn is_sink(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower
        .split('_')
        .any(|w| matches!(w, "error" | "err" | "errors" | "reject" | "fail"))
}

fn literal(syntax: Syntax<'_>, content: &str) -> String {
    let raw = text(syntax, content);
    let raw = raw.trim_start_matches(['r', 'b', '#']);
    raw.trim_matches('#').trim_matches('"').to_owned()
}

fn looks_like_route(value: &str) -> bool {
    value.starts_with('/')
        && value.len() < 256
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/_-{}*:.<>".contains(c))
}

fn http_method(value: &str) -> Option<String> {
    let last = value.rsplit("::").next()?;
    matches!(
        last,
        "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS"
    )
    .then(|| last.to_owned())
}

fn collect_route_literals(
    syntax: Syntax<'_>,
    content: &str,
    output: &mut Vec<(String, Option<String>)>,
) {
    if syntax.kind() == "tuple_expression" {
        let parts = named(syntax);
        let path = parts
            .iter()
            .find(|p| p.kind() == "string_literal")
            .map(|p| literal(*p, content))
            .filter(|p| looks_like_route(p));
        if let Some(path) = path {
            let method = parts.iter().find_map(|p| http_method(text(*p, content)));
            output.push((path, method));
            return;
        }
    }
    if syntax.kind() == "string_literal" {
        let value = literal(syntax, content);
        if looks_like_route(&value) {
            output.push((value, None));
        }
        return;
    }
    for child in named(syntax) {
        collect_route_literals(child, content, output);
    }
}

fn collect_status_literals(syntax: Syntax<'_>, content: &str, output: &mut BTreeSet<i64>) {
    if syntax.kind() == "integer_literal" {
        if let Ok(value) = text(syntax, content).parse::<i64>() {
            if is_status(value) {
                output.insert(value);
            }
        }
        return;
    }
    for child in named(syntax) {
        collect_status_literals(child, content, output);
    }
}

fn collect_codes(syntax: Syntax<'_>, content: &str, output: &mut Vec<String>, depth: usize) {
    if depth > 3 || syntax.kind() == "closure_expression" {
        return;
    }
    if syntax.kind() == "string_literal" {
        let value = literal(syntax, content);
        if is_code(&value) {
            output.push(value);
        }
        return;
    }
    for child in named(syntax) {
        collect_codes(child, content, output, depth + 1);
    }
}

fn status_constant(path: &str) -> Option<u16> {
    let name = path.rsplit("::").next()?;
    if !path.contains("StatusCode") {
        return None;
    }
    Some(match name {
        "OK" => 200,
        "CREATED" => 201,
        "ACCEPTED" => 202,
        "NO_CONTENT" => 204,
        "BAD_REQUEST" => 400,
        "UNAUTHORIZED" => 401,
        "FORBIDDEN" => 403,
        "NOT_FOUND" => 404,
        "METHOD_NOT_ALLOWED" => 405,
        "CONFLICT" => 409,
        "GONE" => 410,
        "PAYLOAD_TOO_LARGE" => 413,
        "UNSUPPORTED_MEDIA_TYPE" => 415,
        "UNPROCESSABLE_ENTITY" => 422,
        "TOO_MANY_REQUESTS" => 429,
        "INTERNAL_SERVER_ERROR" => 500,
        "NOT_IMPLEMENTED" => 501,
        "BAD_GATEWAY" => 502,
        "SERVICE_UNAVAILABLE" => 503,
        "GATEWAY_TIMEOUT" => 504,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn use_trees_expand_aliases_groups_and_globs() {
        let mut found = use_paths("crate::{pool::{Pool, Health as State}, context::Context}");
        found.extend(use_paths("super::*"));
        assert!(found.contains(&("Pool".into(), "crate::pool::Pool".into())));
        assert!(found.contains(&("State".into(), "crate::pool::Health".into())));
        assert!(found.contains(&("Context".into(), "crate::context::Context".into())));
        assert!(found.contains(&("*".into(), "super".into())));
    }

    #[test]
    fn declared_types_strip_wrappers_and_keep_trait_objects() {
        assert_eq!(
            deref(parse_ty("Arc<Engine>", None)),
            Ty::Named("Engine".into(), vec![])
        );
        assert_eq!(
            parse_ty("&'a mut dyn Reader + Send", None),
            Ty::Dyn("Reader".into())
        );
        assert_eq!(
            element(&parse_ty("BTreeMap<String, Group>", None)),
            Ty::Named("Group".into(), vec![])
        );
        assert_eq!(parse_ty("T", None), Ty::Unknown);
    }
}
