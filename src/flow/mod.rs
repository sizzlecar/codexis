//! A deterministic map of how a project serves its entries, built only from
//! stored source: route registrations, the main call path in source order,
//! literal exits, configuration reads, shared state and declared dependencies.
//! Targets are resolved through declared types; anything else stays unknown.
mod python;
mod rust;
pub mod view;

use crate::{
    frontend::FrontendCache,
    index::Index,
    model::{identity, Evidence, Node, Snapshot},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

pub const FLOW_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), ":flow:17");
const MAX_STEPS: usize = 64;
const MAX_INLINE_DEPTH: usize = 5;
const REACH_LIMIT: usize = 400;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FlowMap {
    pub language: String,
    pub routes: Vec<Route>,
    pub trunk: Option<Trunk>,
    pub trunks: Vec<TrunkSummary>,
    pub errors: Vec<ErrorExit>,
    pub config: Vec<ConfigField>,
    pub shared: Vec<SharedState>,
    pub external: Vec<External>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Route {
    pub method: String,
    pub path: String,
    pub handler: Option<String>,
    pub handler_id: Option<String>,
    pub internal: bool,
    pub evidence: Evidence,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trunk {
    pub id: String,
    pub label: String,
    pub routes: usize,
    pub evidence: Evidence,
    pub steps: Vec<Step>,
    pub unresolved: usize,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// A function whose body is expanded below it.
    Inline,
    Call,
    /// A trait or interface method with several implementations.
    Dispatch,
    /// A call whose target is not determined but which can end the request.
    Unresolved,
    Loop,
    /// A condition that ends the request without a preceding call.
    Guard,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Step {
    pub kind: StepKind,
    pub depth: usize,
    pub label: String,
    pub target: Option<String>,
    pub candidates: usize,
    pub arm: Option<String>,
    pub in_loop: bool,
    pub evidence: Evidence,
    pub exits: Vec<Exit>,
    pub reads: Vec<String>,
    pub state: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Exit {
    pub status: String,
    pub code: Option<String>,
    pub evidence: Evidence,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrunkSummary {
    pub id: String,
    pub label: String,
    pub routes: usize,
    pub reach: usize,
    pub evidence: Evidence,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorExit {
    pub status: String,
    pub code: Option<String>,
    pub function: String,
    pub on_trunk: bool,
    pub evidence: Evidence,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReadMode {
    /// Obtained through an accessor call at the time of use.
    Current,
    /// Supplied by the caller as a parameter.
    Passed,
    /// Held in a field or local value.
    Held,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigRead {
    pub function: String,
    pub mode: ReadMode,
    pub evidence: Evidence,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigField {
    pub path: String,
    pub ty: String,
    pub evidence: Evidence,
    pub reads: Vec<ConfigRead>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SharedField {
    pub name: String,
    pub ty: String,
    pub evidence: Evidence,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateAccess {
    pub function: String,
    pub field: String,
    pub op: String,
    pub evidence: Evidence,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SharedState {
    pub name: String,
    /// Held in a field of another type, so one instance outlives a call.
    pub held: bool,
    pub evidence: Evidence,
    pub fields: Vec<SharedField>,
    pub accesses: Vec<StateAccess>,
    pub on_trunk: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct External {
    pub category: String,
    pub packages: Vec<String>,
}

/// Language facts in a neutral form. Events of one function are sorted by
/// source position; call targets are already resolved or explicitly unknown.
pub(crate) struct Program {
    pub functions: HashMap<String, Function>,
    pub routes: Vec<Route>,
    pub mains: Vec<String>,
    /// Public functions, used as a starting point when a library has no
    /// route registrations or program entry.
    pub public: Vec<String>,
    pub config: Vec<ConfigField>,
    pub shared: Vec<SharedState>,
}

pub(crate) struct Function {
    pub label: String,
    /// Sends or wraps an error rather than doing work (by name).
    pub sink: bool,
    pub owner: Option<String>,
    pub evidence: Evidence,
    pub events: Vec<Event>,
}

pub(crate) struct Event {
    /// Ordering position (a call's method name).
    pub at: usize,
    /// Full syntax range (a call's whole expression).
    pub start: usize,
    pub end: usize,
    pub evidence: Evidence,
    pub arm: Option<String>,
    /// Innermost enclosing `if` condition and its start.
    pub guard: Option<(String, usize, Evidence)>,
    pub kind: EventKind,
}

pub(crate) enum EventKind {
    Call {
        label: String,
        target: Target,
        /// Combinators such as map_err never anchor an exit or a step.
        anchor: bool,
    },
    Exit {
        status: String,
        code: Option<String>,
    },
    Loop {
        header: String,
    },
    Read {
        path: String,
        mode: ReadMode,
    },
    Touch {
        state: String,
        field: String,
        op: String,
    },
    /// Construction of an enum variant; becomes an exit when the variant is
    /// mapped to an error status elsewhere (`impl IntoResponse`).
    Construct {
        owner: String,
        variant: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Function(String),
    Dispatch(Vec<String>),
    External,
    Unknown,
}

pub fn build(index: &Index, snapshot: &Snapshot) -> Result<FlowMap> {
    let key = identity(&[FLOW_VERSION, &snapshot.id]);
    if let Some(record) = index.load_record(&key)? {
        if let Ok(map) = serde_json::from_str(&record) {
            return Ok(map);
        }
    }
    let map = compute(index, snapshot)?;
    index.store_record(&key, &serde_json::to_string(&map)?)?;
    Ok(map)
}

fn compute(index: &Index, snapshot: &Snapshot) -> Result<FlowMap> {
    let nodes: Vec<Node> = index.nodes_of_kinds(
        &snapshot.id,
        &[
            "module", "function", "type", "trait", "impl", "field", "variable",
        ],
        trim,
    )?;
    let program = match snapshot.context.language.as_str() {
        "rust" => rust::program(index, &nodes)?,
        "python" => python::program(index, &nodes)?,
        _ => {
            return Ok(FlowMap {
                language: snapshot.context.language.clone(),
                ..FlowMap::default()
            })
        }
    };
    Ok(assemble(snapshot, program))
}

/// Keep only what the flow map reads, so a whole-project pass stays small:
/// declared types, impl and trait owners, derives, route attributes and
/// Python class signatures. Test code is dropped.
fn trim(node: &mut Node) -> bool {
    const KEEP: &[&str] = &[
        "rust.parameter_types",
        "rust.return_type",
        "rust.declared_type",
        "rust.self_type",
        "rust.trait",
        "rust.attributes",
        "decorators",
        "parameters",
        "return_type",
        "python.initializer",
        "python.declared_type",
    ];
    if node.is_test {
        return false;
    }
    node.attributes
        .retain(|key, _| KEEP.contains(&key.as_str()));
    if let Some(attributes) = node.attributes.get_mut("rust.attributes") {
        *attributes = attributes
            .lines()
            .filter(|line| line.contains("derive") || line.contains("(\"/"))
            .collect::<Vec<_>>()
            .join("\n");
    }
    let python_signature =
        node.language == "python" && matches!(node.kind.as_str(), "type" | "field");
    if !python_signature {
        node.signature = String::new();
    }
    node.fingerprint = String::new();
    node.stable_key = String::new();
    node.conditions = Vec::new();
    true
}

fn assemble(snapshot: &Snapshot, program: Program) -> FlowMap {
    let mut routes = program.routes.clone();
    let mut seen = HashSet::new();
    routes.retain(|r| seen.insert((r.method.clone(), r.path.clone())));
    routes.sort_by(|a, b| {
        (a.internal, &a.evidence.path, a.evidence.start_byte).cmp(&(
            b.internal,
            &b.evidence.path,
            b.evidence.start_byte,
        ))
    });
    let mut expander = Expander::new(&program);
    let mut handlers: BTreeMap<String, usize> = BTreeMap::new();
    for route in routes.iter().filter(|r| !r.internal) {
        if let Some(id) = &route.handler_id {
            *handlers.entry(id.clone()).or_default() += 1;
        }
    }
    let mut candidates: Vec<(String, usize, usize)> = handlers
        .iter()
        .filter(|(id, _)| program.functions.contains_key(*id))
        .map(|(id, count)| (id.clone(), *count, expander.reach(id)))
        .collect();
    for route in routes.iter().filter(|r| r.internal) {
        if let Some(id) = &route.handler_id {
            if program.functions.contains_key(id) && !candidates.iter().any(|c| &c.0 == id) {
                candidates.push((id.clone(), 0, expander.reach(id)));
            }
        }
    }
    for id in &program.mains {
        if program.functions.contains_key(id) && !candidates.iter().any(|c| &c.0 == id) {
            candidates.push((id.clone(), 0, expander.reach(id)));
        }
    }
    if candidates.is_empty() {
        for id in &program.public {
            if program.functions.contains_key(id) {
                candidates.push((id.clone(), 0, expander.reach(id)));
            }
        }
        candidates.retain(|c| c.2 > 0);
    }
    candidates.sort_by(|a, b| (b.1, b.2, &a.0).cmp(&(a.1, a.2, &b.0)));
    let trunk = candidates.first().map(|(id, count, _)| {
        expander.expand(id, 0, false, None);
        let function = &program.functions[id];
        Trunk {
            id: id.clone(),
            label: function.label.clone(),
            routes: *count,
            evidence: function.evidence.clone(),
            steps: std::mem::take(&mut expander.steps),
            unresolved: expander.unresolved,
            truncated: expander.truncated,
        }
    });
    let trunk_functions = expander.visited.clone();
    let trunks = candidates
        .iter()
        .skip(1)
        .take(60)
        .map(|(id, count, reach)| TrunkSummary {
            id: id.clone(),
            label: program.functions[id].label.clone(),
            routes: *count,
            reach: *reach,
            evidence: program.functions[id].evidence.clone(),
        })
        .collect();
    let mut errors = Vec::new();
    let mut reads: BTreeMap<String, Vec<ConfigRead>> = BTreeMap::new();
    let mut accesses: BTreeMap<String, Vec<StateAccess>> = BTreeMap::new();
    let mut touched = BTreeSet::new();
    for (id, function) in &program.functions {
        let on_trunk = trunk_functions.contains(id);
        for event in &function.events {
            match &event.kind {
                EventKind::Exit { status, code } => errors.push(ErrorExit {
                    status: status.clone(),
                    code: code.clone(),
                    function: function.label.clone(),
                    on_trunk,
                    evidence: event.evidence.clone(),
                }),
                EventKind::Read { path, mode } => {
                    reads.entry(path.clone()).or_default().push(ConfigRead {
                        function: function.label.clone(),
                        mode: *mode,
                        evidence: event.evidence.clone(),
                    })
                }
                EventKind::Touch { state, field, op } => {
                    if on_trunk {
                        touched.insert(state.clone());
                    }
                    accesses
                        .entry(state.clone())
                        .or_default()
                        .push(StateAccess {
                            function: function.label.clone(),
                            field: field.clone(),
                            op: op.clone(),
                            evidence: event.evidence.clone(),
                        })
                }
                _ => {}
            }
        }
        if on_trunk {
            if let Some(owner) = &function.owner {
                touched.insert(owner.clone());
            }
        }
    }
    errors.sort_by(|a, b| {
        (
            !a.on_trunk,
            &a.status,
            &a.code,
            &a.evidence.path,
            a.evidence.start_byte,
        )
            .cmp(&(
                !b.on_trunk,
                &b.status,
                &b.code,
                &b.evidence.path,
                b.evidence.start_byte,
            ))
    });
    let mut config = program.config;
    for field in &mut config {
        if let Some(found) = reads.remove(&field.path) {
            field.reads = found;
            field.reads.sort_by(|a, b| {
                (&a.evidence.path, a.evidence.start_byte)
                    .cmp(&(&b.evidence.path, b.evidence.start_byte))
            });
        }
    }
    let mut shared = program.shared;
    for state in &mut shared {
        if let Some(mut found) = accesses.remove(&state.name) {
            found.sort_by(|a, b| {
                (&a.evidence.path, a.evidence.start_byte)
                    .cmp(&(&b.evidence.path, b.evidence.start_byte))
            });
            state.accesses = found;
        }
        state.on_trunk = touched.contains(&state.name);
    }
    shared.sort_by(|a, b| {
        (
            !(a.on_trunk && a.held),
            !a.held,
            std::cmp::Reverse(a.accesses.len()),
            &a.name,
        )
            .cmp(&(
                !(b.on_trunk && b.held),
                !b.held,
                std::cmp::Reverse(b.accesses.len()),
                &b.name,
            ))
    });
    FlowMap {
        language: snapshot.context.language.clone(),
        routes,
        trunk,
        trunks,
        errors,
        config,
        shared,
        external: external(snapshot),
    }
}

/// An error status and its machine-readable code, if any.
type ExitCode = (String, Option<String>);

/// Expands the trunk in source order. A callee is shown inline only when it
/// carries most of the remaining work (the largest reachable call set); other
/// callees are single steps summarizing their own exits, reads and state.
struct Expander<'a> {
    program: &'a Program,
    reach: HashMap<String, HashSet<String>>,
    steps: Vec<Step>,
    path: Vec<String>,
    visited: HashSet<String>,
    unresolved: usize,
    truncated: bool,
}

#[derive(Default)]
struct Attached {
    exits: Vec<Exit>,
    reads: Vec<String>,
    state: Vec<String>,
}

impl Attached {
    fn merge(&mut self, other: Attached) {
        for exit in other.exits {
            if !self
                .exits
                .iter()
                .any(|e| e.status == exit.status && e.code == exit.code)
            {
                self.exits.push(exit);
            }
        }
        for read in other.reads {
            if !self.reads.contains(&read) {
                self.reads.push(read);
            }
        }
        for state in other.state {
            if !self.state.contains(&state) {
                self.state.push(state);
            }
        }
    }
    fn is_empty(&self) -> bool {
        self.exits.is_empty() && self.reads.is_empty() && self.state.is_empty()
    }
}

impl<'a> Expander<'a> {
    fn new(program: &'a Program) -> Self {
        Self {
            program,
            reach: HashMap::new(),
            steps: Vec::new(),
            path: Vec::new(),
            visited: HashSet::new(),
            unresolved: 0,
            truncated: false,
        }
    }

    fn reach_set(&mut self, id: &str) -> &HashSet<String> {
        if !self.reach.contains_key(id) {
            let mut seen = HashSet::new();
            let mut queue = VecDeque::from([id.to_owned()]);
            while let Some(next) = queue.pop_front() {
                let Some(function) = self.program.functions.get(&next) else {
                    continue;
                };
                for event in &function.events {
                    if let EventKind::Call {
                        target: Target::Function(target),
                        ..
                    } = &event.kind
                    {
                        if target != id && seen.len() < REACH_LIMIT && seen.insert(target.clone()) {
                            queue.push_back(target.clone());
                        }
                    }
                }
            }
            self.reach.insert(id.to_owned(), seen);
        }
        &self.reach[id]
    }

    fn reach(&mut self, id: &str) -> usize {
        self.reach_set(id).len()
    }

    /// Callees that only build an error (for example `Error::deadline()`) are
    /// exits of their caller, not steps of the flow.
    fn exit_helper(&self, id: &str) -> Option<Vec<ExitCode>> {
        let function = self.program.functions.get(id)?;
        let mut exits = Vec::new();
        for event in &function.events {
            match &event.kind {
                // Only an unconditional error construction is a helper.
                EventKind::Exit { .. } if event.guard.is_some() || event.arm.is_some() => {
                    return None
                }
                EventKind::Exit { status, code } => exits.push((status.clone(), code.clone())),
                EventKind::Call {
                    target: Target::Function(_) | Target::Dispatch(_),
                    ..
                }
                | EventKind::Call { anchor: true, .. }
                | EventKind::Loop { .. } => return None,
                _ => {}
            }
        }
        (!exits.is_empty()).then_some(exits)
    }

    /// Exits, reads and state touched by a callee and its own callees.
    fn summary(&self, id: &str, depth: usize, seen: &mut HashSet<String>) -> Attached {
        let mut result = Attached::default();
        if depth > 2
            || !seen.insert(id.to_owned())
            || (depth > 0 && self.visited.contains(id))
            || self.path.iter().any(|p| p == id)
        {
            return result;
        }
        let Some(function) = self.program.functions.get(id) else {
            return result;
        };
        for event in &function.events {
            match &event.kind {
                EventKind::Exit { status, code } => result.merge(Attached {
                    exits: vec![Exit {
                        status: status.clone(),
                        code: code.clone(),
                        evidence: event.evidence.clone(),
                    }],
                    ..Attached::default()
                }),
                EventKind::Read { path, .. } if depth == 0 => result.reads.push(path.clone()),
                EventKind::Touch { state, .. } if depth == 0 => {
                    if !result.state.contains(state) {
                        result.state.push(state.clone())
                    }
                }
                EventKind::Call {
                    target: Target::Function(target),
                    ..
                } => {
                    let nested = self.summary(target, depth + 1, seen);
                    result.merge(nested);
                }
                _ => {}
            }
        }
        if depth == 0 {
            if let Some(owner) = &function.owner {
                if self.program.shared.iter().any(|s| &s.name == owner)
                    && !result.state.contains(owner)
                {
                    result.state.push(owner.clone());
                }
            }
        }
        result.reads.dedup();
        result
    }

    fn choose_spine(&mut self, callees: &[String]) -> Option<String> {
        let mut best: Option<(String, usize)> = None;
        for id in callees {
            let size = self.reach(id);
            if size >= 2 && best.as_ref().is_none_or(|(_, s)| size > *s) {
                best = Some((id.clone(), size));
            }
        }
        let (mut spine, mut size) = best?;
        // Prefer a shared core: when the largest callee itself reaches a
        // sibling doing most of the same work, that sibling is the trunk.
        loop {
            let set = self.reach_set(&spine).clone();
            let next = callees
                .iter()
                .filter(|c| **c != spine && set.contains(*c))
                .map(|c| (c.clone(), self.reach(c)))
                .filter(|(_, s)| *s * 10 >= size * 6)
                .max_by_key(|(_, s)| *s);
            match next {
                Some((id, s)) => {
                    spine = id;
                    size = s;
                }
                None => return Some(spine),
            }
        }
    }

    fn push(&mut self, step: Step) -> bool {
        if self.steps.len() >= MAX_STEPS {
            self.truncated = true;
            return false;
        }
        self.steps.push(step);
        true
    }

    fn expand(&mut self, id: &str, depth: usize, in_loop: bool, call_site: Option<&Event>) {
        let program = self.program;
        let Some(function) = program.functions.get(id) else {
            return;
        };
        let events = &function.events;
        self.path.push(id.to_owned());
        self.visited.insert(id.to_owned());
        let helpers: Vec<Option<Vec<ExitCode>>> = events
            .iter()
            .map(|event| match &event.kind {
                EventKind::Call {
                    target: Target::Function(target),
                    ..
                } => self.exit_helper(target),
                _ => None,
            })
            .collect();
        let anchors: Vec<usize> = events
            .iter()
            .enumerate()
            .filter(|(i, e)| {
                helpers[*i].is_none() && matches!(e.kind, EventKind::Call { anchor: true, .. })
            })
            .map(|(i, _)| i)
            .collect();
        // An exit belongs to the call that finished just before it, unless an
        // `if` condition sits in between; reads and state belong to the call
        // whose arguments contain them.
        let preceding = |position: usize| {
            anchors
                .iter()
                .copied()
                .filter(|a| events[*a].end <= position)
                .max_by_key(|a| (events[*a].end, events[*a].at))
        };
        let containing = |position: usize| {
            anchors
                .iter()
                .copied()
                .filter(|a| {
                    events[*a].start <= position
                        && position < events[*a].end
                        && events[*a].at != position
                })
                .max_by_key(|a| events[*a].start)
        };
        let mut attached: HashMap<usize, Attached> = HashMap::new();
        let mut guards: BTreeMap<usize, (String, Evidence, Attached)> = BTreeMap::new();
        let mut header = Attached::default();
        for (position, event) in events.iter().enumerate() {
            let exits: Vec<Exit> = match (&event.kind, &helpers[position]) {
                (EventKind::Exit { status, code }, _) => vec![Exit {
                    status: status.clone(),
                    code: code.clone(),
                    evidence: event.evidence.clone(),
                }],
                (EventKind::Call { .. }, Some(found)) => found
                    .iter()
                    .map(|(status, code)| Exit {
                        status: status.clone(),
                        code: code.clone(),
                        evidence: event.evidence.clone(),
                    })
                    .collect(),
                _ => vec![],
            };
            if !exits.is_empty() {
                let anchor = preceding(event.start);
                let guarded = event
                    .guard
                    .as_ref()
                    .filter(|(_, start, _)| anchor.is_none_or(|a| events[a].end <= *start));
                let item = Attached {
                    exits,
                    ..Attached::default()
                };
                match (guarded, anchor) {
                    (Some((label, start, evidence)), _) => guards
                        .entry(*start)
                        .or_insert_with(|| (label.clone(), evidence.clone(), Attached::default()))
                        .2
                        .merge(item),
                    (None, Some(anchor)) => attached.entry(anchor).or_default().merge(item),
                    (None, None) => header.merge(item),
                }
                continue;
            }
            let item = match &event.kind {
                EventKind::Read { path, .. } => Attached {
                    reads: vec![path.clone()],
                    ..Attached::default()
                },
                EventKind::Touch { state, .. } => Attached {
                    state: vec![state.clone()],
                    ..Attached::default()
                },
                _ => continue,
            };
            match containing(event.at).or_else(|| preceding(event.at)) {
                Some(anchor) => attached.entry(anchor).or_default().merge(item),
                None => header.merge(item),
            }
        }
        let callees: Vec<String> = anchors
            .iter()
            .filter_map(|a| match &events[*a].kind {
                EventKind::Call {
                    target: Target::Function(target),
                    ..
                } if !self.path.contains(target) => Some(target.clone()),
                _ => None,
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let others = callees.iter().filter(|c| self.reach(c) > 0).count();
        let spine = if depth < MAX_INLINE_DEPTH {
            self.choose_spine(&callees)
                .filter(|_| depth < 2 || others <= 3)
        } else {
            None
        };
        let entry = Step {
            kind: StepKind::Inline,
            depth,
            label: function.label.clone(),
            target: Some(id.to_owned()),
            candidates: 0,
            arm: call_site.and_then(|e| e.arm.clone()),
            in_loop,
            evidence: call_site
                .map(|e| e.evidence.clone())
                .unwrap_or_else(|| function.evidence.clone()),
            exits: header.exits,
            reads: header.reads,
            state: header.state,
        };
        if !self.push(entry) {
            self.path.pop();
            return;
        }
        let loops: Vec<(usize, usize, String, Evidence)> = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Loop { header } => {
                    Some((e.at, e.end, header.clone(), e.evidence.clone()))
                }
                _ => None,
            })
            .collect();
        enum Item {
            Call(usize),
            Guard(usize),
        }
        let mut items: Vec<(usize, Item)> = anchors
            .iter()
            .map(|a| (events[*a].at, Item::Call(*a)))
            .collect();
        items.extend(guards.keys().map(|start| (*start, Item::Guard(*start))));
        items.sort_by_key(|(position, _)| *position);
        let mut opened: Vec<usize> = Vec::new();
        let mut shown_spine = false;
        let mut shown: HashSet<String> = HashSet::new();
        for (position, item) in items {
            let inside: Vec<usize> = loops
                .iter()
                .enumerate()
                .filter(|(_, (start, end, _, _))| position > *start && position < *end)
                .map(|(i, _)| i)
                .collect();
            let step_in_loop = in_loop || !inside.is_empty();
            let index = match item {
                Item::Guard(start) => {
                    let Some((label, evidence, extra)) = guards.remove(&start) else {
                        continue;
                    };
                    self.open_loops(&loops, &inside, &mut opened, depth, in_loop);
                    let step = Step {
                        kind: StepKind::Guard,
                        depth,
                        label,
                        target: None,
                        candidates: 0,
                        arm: None,
                        in_loop: step_in_loop,
                        evidence,
                        exits: extra.exits,
                        reads: vec![],
                        state: vec![],
                    };
                    if !self.push(step) {
                        break;
                    }
                    continue;
                }
                Item::Call(index) => index,
            };
            let event = &events[index];
            let EventKind::Call { label, target, .. } = &event.kind else {
                continue;
            };
            let extra = attached.remove(&index).unwrap_or_default();
            let (kind, step_target, candidates, summary) = match target {
                Target::Function(target) if spine.as_deref() == Some(target) && !shown_spine => {
                    shown_spine = true;
                    shown.insert(target.clone());
                    self.open_loops(&loops, &inside, &mut opened, depth, in_loop);
                    if self.steps.len() >= MAX_STEPS {
                        self.truncated = true;
                        break;
                    }
                    let before = self.steps.len();
                    self.expand(target, depth + 1, step_in_loop, Some(event));
                    if let Some(step) = self.steps.get_mut(before) {
                        let mut merged = Attached {
                            exits: std::mem::take(&mut step.exits),
                            reads: std::mem::take(&mut step.reads),
                            state: std::mem::take(&mut step.state),
                        };
                        merged.merge(extra);
                        step.exits = merged.exits;
                        step.reads = merged.reads;
                        step.state = merged.state;
                    }
                    continue;
                }
                Target::Function(target) => {
                    if shown.contains(target) && extra.exits.is_empty() {
                        continue;
                    }
                    let sink = program.functions.get(target).is_some_and(|f| f.sink);
                    let mut summary = self.summary(target, 0, &mut HashSet::new());
                    if sink && extra.exits.is_empty() && summary.exits.is_empty() {
                        continue;
                    }
                    if self.reach(target) < 2 && summary.is_empty() && extra.is_empty() {
                        continue;
                    }
                    summary.merge(extra);
                    shown.insert(target.clone());
                    self.visited.insert(target.clone());
                    (StepKind::Call, Some(target.clone()), 0, summary)
                }
                Target::Dispatch(candidates) => (StepKind::Dispatch, None, candidates.len(), extra),
                Target::External if !extra.exits.is_empty() => (StepKind::Call, None, 0, extra),
                Target::Unknown => {
                    self.unresolved += 1;
                    if extra.exits.is_empty() {
                        continue;
                    }
                    (StepKind::Unresolved, None, 0, extra)
                }
                Target::External => continue,
            };
            self.open_loops(&loops, &inside, &mut opened, depth, in_loop);
            let step = Step {
                kind,
                depth,
                label: label.clone(),
                target: step_target,
                candidates,
                arm: event.arm.clone(),
                in_loop: step_in_loop,
                evidence: event.evidence.clone(),
                exits: summary.exits,
                reads: summary.reads,
                state: summary.state,
            };
            if !self.push(step) {
                break;
            }
        }
        self.path.pop();
    }

    fn open_loops(
        &mut self,
        loops: &[(usize, usize, String, Evidence)],
        inside: &[usize],
        opened: &mut Vec<usize>,
        depth: usize,
        in_loop: bool,
    ) {
        for index in inside {
            if opened.contains(index) {
                continue;
            }
            opened.push(*index);
            let (_, _, header, evidence) = &loops[*index];
            self.push(Step {
                kind: StepKind::Loop,
                depth,
                label: header.clone(),
                target: None,
                candidates: 0,
                arm: None,
                in_loop,
                evidence: evidence.clone(),
                exits: vec![],
                reads: vec![],
                state: vec![],
            });
        }
    }
}

/// Declared dependencies grouped by the external system they connect to.
fn external(snapshot: &Snapshot) -> Vec<External> {
    let mut groups: BTreeMap<&'static str, BTreeSet<String>> = BTreeMap::new();
    for package in &snapshot.project.packages {
        for dependency in &package.dependencies {
            if dependency.kind == "dev" {
                continue;
            }
            let name = dependency.package.to_ascii_lowercase().replace('_', "-");
            if let Some(category) = category(&name) {
                groups
                    .entry(category)
                    .or_default()
                    .insert(dependency.package.clone());
            }
        }
    }
    if ["mysql", "postgres", "sqlite"]
        .iter()
        .any(|k| groups.contains_key(k))
    {
        groups.remove("sql");
    }
    let order = [
        "mysql",
        "postgres",
        "sqlite",
        "sql",
        "redis",
        "mongodb",
        "elasticsearch",
        "kafka",
        "rabbitmq",
        "nats",
        "mq",
        "nacos",
        "etcd",
        "consul",
        "object_storage",
        "grpc",
        "websocket",
        "http_client",
        "model_api",
        "metrics",
        "task_queue",
    ];
    order
        .iter()
        .filter_map(|key| {
            groups.remove(key).map(|packages| External {
                category: (*key).into(),
                packages: packages.into_iter().collect(),
            })
        })
        .collect()
}

fn category(name: &str) -> Option<&'static str> {
    Some(match name {
        "sqlx-mysql"
        | "mysql"
        | "mysql-async"
        | "mysql-common"
        | "pymysql"
        | "mysqlclient"
        | "aiomysql"
        | "asyncmy"
        | "mysql-connector-python" => "mysql",
        "tokio-postgres" | "postgres" | "sqlx-postgres" | "psycopg" | "psycopg2"
        | "psycopg2-binary" | "asyncpg" => "postgres",
        "rusqlite" | "sqlx-sqlite" | "aiosqlite" => "sqlite",
        "sqlx" | "sqlx-core" | "diesel" | "sea-orm" | "sqlalchemy" | "sqlmodel"
        | "tortoise-orm" | "peewee" | "django" => "sql",
        "redis" | "fred" | "deadpool-redis" | "bb8-redis" | "aioredis" | "redis-py" => "redis",
        "mongodb" | "pymongo" | "motor" => "mongodb",
        "elasticsearch" | "opensearch-py" => "elasticsearch",
        "rdkafka" | "kafka" | "kafka-python" | "aiokafka" | "confluent-kafka" => "kafka",
        "lapin" | "pika" | "aio-pika" | "amqp" | "kombu" => "rabbitmq",
        "async-nats" | "nats" | "nats-py" => "nats",
        "rocketmq" | "rocketmq-client-python" | "pulsar" | "pulsar-client" => "mq",
        "nacos-sdk" | "nacos-sdk-python" | "nacos" => "nacos",
        "etcd-client" | "etcd-rs" | "etcd3" => "etcd",
        "consul" | "python-consul" => "consul",
        "opendal"
        | "aws-sdk-s3"
        | "rusoto-s3"
        | "object-store"
        | "rust-s3"
        | "s3"
        | "boto3"
        | "aioboto3"
        | "minio"
        | "oss2"
        | "google-cloud-storage" => "object_storage",
        "tonic" | "grpcio" | "grpclib" => "grpc",
        "tokio-tungstenite" | "tungstenite" | "async-tungstenite" | "websockets"
        | "websocket-client" => "websocket",
        "reqwest" | "ureq" | "isahc" | "surf" | "awc" | "requests" | "httpx" | "aiohttp"
        | "urllib3" => "http_client",
        "openai"
        | "anthropic"
        | "async-openai"
        | "google-genai"
        | "google-generativeai"
        | "langchain"
        | "pydantic-ai"
        | "litellm" => "model_api",
        "prometheus" | "prometheus-client" | "metrics" | "opentelemetry" | "statsd" => "metrics",
        "celery" | "rq" | "dramatiq" | "apalis" => "task_queue",
        _ => return None,
    })
}

/// Short display path: the last two path components, enough to open source.
pub fn short_path(path: &str) -> String {
    let parts: Vec<_> = path.rsplit('/').take(2).collect();
    match parts.as_slice() {
        [file, dir] if matches!(*file, "mod.rs" | "lib.rs" | "main.rs" | "__init__.py") => {
            format!("{dir}/{file}")
        }
        [file, ..] => (*file).to_owned(),
        [] => path.to_owned(),
    }
}

pub(crate) fn evidence_at(
    path: &str,
    hash: &str,
    start_byte: usize,
    end_byte: usize,
    start: (usize, usize),
    end: (usize, usize),
) -> Evidence {
    Evidence {
        path: path.to_owned(),
        content_hash: hash.to_owned(),
        start_byte,
        end_byte,
        start_line: start.0 + 1,
        start_column: start.1,
        end_line: end.0 + 1,
        end_column: end.1,
    }
}

/// Collapse whitespace so multi-line callee expressions fit one line.
pub(crate) fn compact(text: &str, limit: usize) -> String {
    let mut out = String::new();
    let mut space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if space
            && !out.is_empty()
            && !matches!(c, '.' | ')' | ']' | ',' | '?')
            && !out.ends_with(['.', '(', '['])
        {
            out.push(' ');
        }
        space = false;
        out.push(c);
    }
    if out.chars().count() > limit {
        let mut clipped: String = out.chars().take(limit.saturating_sub(1)).collect();
        clipped.push('…');
        clipped
    } else {
        out
    }
}

pub(crate) fn is_internal_path(path: &str) -> bool {
    let first = path.trim_start_matches('/').split('/').next().unwrap_or("");
    matches!(
        first,
        "health"
            | "healthz"
            | "ready"
            | "readyz"
            | "live"
            | "livez"
            | "internal"
            | "metrics"
            | "debug"
            | "status"
            | "ping"
            | "_internal"
    )
}

/// Error statuses only: successful and redirect responses are not early exits.
pub(crate) fn is_status(value: i64) -> bool {
    (400..=599).contains(&value)
}

/// Snake-case identifiers such as `rate_limited` are machine codes;
/// sentences and display text are not.
pub(crate) fn is_code(value: &str) -> bool {
    let len = value.len();
    (3..=64).contains(&len)
        && value.starts_with(|c: char| c.is_ascii_lowercase())
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.')
        && (value.contains('_') || value.contains('.') || len <= 24)
}
