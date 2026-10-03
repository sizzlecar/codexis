use super::jobs::{Completed, Request};
use crate::{
    index::Index,
    knowledge, marks,
    model::{Edge, Evidence, Node, Report, Snapshot},
    query,
};
use anyhow::{bail, Context, Result};
use rusqlite::params;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

const PAGE_SIZE: usize = 100;

#[derive(Clone, Debug)]
pub(super) enum Action {
    Home,
    Baseline,
    Dimensions,
    Dimension(String),
    Aggregate {
        dimension: String,
        value: Arc<Value>,
    },
    Trace {
        snapshot: String,
        id: String,
    },
    BatchSection {
        report: Arc<Report<Value>>,
        section: String,
    },
    Knowledge {
        history: Option<String>,
    },
    KnowledgeRecord(Arc<Value>),
    Packages,
    Package(String),
    Entries,
    Files(String),
    Symbols {
        package: String,
        path: String,
        types: bool,
        offset: usize,
    },
    Node {
        snapshot: String,
        id: String,
    },
    Relations {
        snapshot: String,
        id: String,
        incoming: bool,
        offset: usize,
    },
    Edge {
        snapshot: String,
        edge: Box<Edge>,
        incoming: bool,
    },
    Source(Evidence),
    Diff {
        path: String,
        before: Option<Evidence>,
        after: Option<Evidence>,
    },
    Search {
        query: String,
        offset: usize,
    },
    ReviewMenu,
    Changes(Arc<Report<Value>>),
    Change {
        before: String,
        after: String,
        value: Box<Value>,
    },
    Text {
        title: String,
        lines: Vec<String>,
    },
    Status,
    Job(Request),
    Marks {
        snapshot: String,
        id: String,
    },
    Mark {
        snapshot: String,
        id: String,
        state: String,
    },
}

#[derive(Clone)]
pub(super) struct Item {
    pub label: String,
    pub hint: String,
    pub action: Action,
    pub detail: Vec<String>,
    pub evidence: Option<Evidence>,
}

impl Item {
    fn new(label: impl Into<String>, hint: impl Into<String>, action: Action) -> Self {
        Self {
            label: label.into(),
            hint: hint.into(),
            action,
            detail: vec![],
            evidence: None,
        }
    }
    fn node(node: &Node, snapshot: &str) -> Self {
        Self {
            label: node.qualified_name.clone(),
            hint: format!(
                "{} · {}:{}",
                kind(&node.kind),
                node.evidence.path,
                node.evidence.start_line
            ),
            action: Action::Node {
                snapshot: snapshot.into(),
                id: node.id.clone(),
            },
            detail: vec![
                node.signature.clone(),
                crate::localize!("可见性：{}", "Visibility: {}", node.visibility),
                conditions(&node.conditions),
            ],
            evidence: Some(node.evidence.clone()),
        }
    }
    fn source(label: impl Into<String>, evidence: Evidence) -> Self {
        Self {
            label: label.into(),
            hint: format!("{}:{}", evidence.path, evidence.start_line),
            action: Action::Source(evidence.clone()),
            detail: vec![],
            evidence: Some(evidence),
        }
    }

    fn aggregate(value: &Value, dimension: &str) -> Self {
        let mut item = Self::new(
            s(&value["title"]),
            s(&value["summary"]),
            Action::Aggregate {
                dimension: dimension.into(),
                value: Arc::new(value.clone()),
            },
        );
        item.detail = value_lines(value);
        item.evidence = evidence_values(value).into_iter().next();
        item
    }
}

#[derive(Clone, Debug)]
pub(super) struct Focus {
    pub title: String,
    pub node_ids: Vec<String>,
    pub paths: Vec<String>,
    by_path: bool,
}

#[derive(Clone)]
pub(super) struct Page {
    pub action: Action,
    pub title: String,
    pub intro: Vec<String>,
    pub items: Vec<Item>,
    pub selected: usize,
    pub scroll: usize,
    pub horizontal: usize,
    pub evidence: Option<Evidence>,
    pub node: Option<(String, String)>,
}

impl Page {
    fn new(action: Action, title: impl Into<String>) -> Self {
        Self {
            action,
            title: title.into(),
            intro: vec![],
            items: vec![],
            selected: 0,
            scroll: 0,
            horizontal: 0,
            evidence: None,
            node: None,
        }
    }
}

pub(super) struct Browser<'a> {
    pub index: &'a Index,
    pub snapshot: Snapshot,
    guide: Value,
    understanding: Value,
    review: Option<Arc<Report<Value>>>,
    hashes: BTreeMap<String, String>,
    semantics: BTreeMap<String, Snapshot>,
    pub page: Page,
    history: Vec<(Page, Option<Focus>)>,
    pub focus: Option<Focus>,
    pub question: String,
    pub dimension: String,
    pub detail: Vec<String>,
    pub status: String,
}

impl<'a> Browser<'a> {
    pub fn new(index: &'a Index, snapshot: Snapshot, guide: Value) -> Result<Self> {
        let hashes = index.file_hashes(&snapshot.id)?;
        let understanding = query::understanding_data(index, &snapshot)?;
        let mut browser = Self {
            index,
            snapshot,
            guide,
            understanding,
            review: None,
            hashes,
            semantics: BTreeMap::new(),
            page: Page::new(
                Action::Home,
                crate::localize!("项目概览", "Project overview"),
            ),
            history: vec![],
            focus: None,
            question: String::new(),
            dimension: "architecture".into(),
            detail: vec![],
            status: String::new(),
        };
        browser.page = browser.build(Action::Home)?;
        browser.preview()?;
        Ok(browser)
    }

    pub fn breadcrumb(&self) -> String {
        self.history
            .iter()
            .map(|(p, _)| p.title.as_str())
            .chain(std::iter::once(self.page.title.as_str()))
            .collect::<Vec<_>>()
            .join(" › ")
    }

    pub fn open(&mut self, action: Action) -> Result<Option<Request>> {
        if let Action::Job(request) = action {
            return Ok(Some(request));
        }
        if let Action::Mark {
            snapshot,
            id,
            state,
        } = action
        {
            self.mark(&snapshot, &id, &state, None)?;
            self.back()?;
            self.refresh()?;
            return Ok(None);
        }
        let previous_focus = self.focus.clone();
        match &action {
            Action::Aggregate { dimension, value } => {
                self.dimension = dimension.clone();
                let node_ids = array(&value["node_ids"])
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                let mut paths = evidence_values(value)
                    .into_iter()
                    .map(|e| e.path)
                    .collect::<Vec<_>>();
                paths.extend(
                    array(&value["paths"])
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned),
                );
                paths.sort();
                paths.dedup();
                if !node_ids.is_empty() || !paths.is_empty() {
                    self.focus = Some(Focus {
                        title: s(&value["title"]).into(),
                        by_path: node_ids.is_empty() || value["paths"].is_array(),
                        node_ids,
                        paths,
                    });
                }
            }
            Action::Dimension(dimension) => self.dimension = dimension.clone(),
            Action::Node { snapshot, id } if self.focus.is_none() && self.is_current(snapshot) => {
                let node = self.node(snapshot, id)?;
                self.focus = Some(Focus {
                    title: node.qualified_name,
                    node_ids: vec![id.clone()],
                    paths: vec![node.evidence.path],
                    by_path: false,
                });
            }
            _ => {}
        }
        let page = match self.build(action) {
            Ok(page) => page,
            Err(error) => {
                self.focus = previous_focus;
                return Err(error);
            }
        };
        self.history
            .push((std::mem::replace(&mut self.page, page), previous_focus));
        self.preview()?;
        Ok(None)
    }

    pub fn enter(&mut self) -> Result<Option<Request>> {
        let Some(item) = self.page.items.get(self.page.selected) else {
            return Ok(None);
        };
        self.open(item.action.clone())
    }

    pub fn back(&mut self) -> Result<()> {
        if let Some((page, focus)) = self.history.pop() {
            self.page = page;
            self.focus = focus;
            self.refresh()?;
        }
        Ok(())
    }

    pub fn refresh(&mut self) -> Result<()> {
        let (selected, scroll, horizontal) =
            (self.page.selected, self.page.scroll, self.page.horizontal);
        self.page = self.build(self.page.action.clone())?;
        self.page.selected = selected.min(self.page.items.len().saturating_sub(1));
        self.page.scroll = scroll;
        self.page.horizontal = horizontal;
        self.preview()
    }

    pub fn home(&mut self) -> Result<()> {
        self.focus = None;
        self.page = self.build(Action::Home)?;
        self.history.clear();
        self.preview()
    }

    pub fn clear_focus(&mut self) -> Result<()> {
        self.focus = None;
        self.refresh()
    }

    pub fn selected_record(&self) -> Option<Arc<Value>> {
        if let Action::KnowledgeRecord(value) = &self.page.action {
            return Some(value.clone());
        }
        self.page
            .items
            .get(self.page.selected)
            .and_then(|item| match &item.action {
                Action::KnowledgeRecord(value) => Some(value.clone()),
                _ => None,
            })
    }

    pub fn remember(&mut self, claim: &str, state: &str) -> Result<()> {
        let mut snapshot = self.snapshot.clone();
        query::check_freshness(self.index, &mut snapshot)?;
        let record = self.selected_record();
        let mut paths = self
            .focus
            .as_ref()
            .map(|f| f.paths.clone())
            .unwrap_or_default();
        if let Some(record) = &record {
            paths.extend(evidence_values(record).into_iter().map(|e| e.path));
        }
        let query = self
            .selected_node()
            .map(|(_, id)| id)
            .or_else(|| {
                self.focus
                    .as_ref()
                    .and_then(|f| f.node_ids.first().cloned())
            })
            .or_else(|| self.selected_evidence().map(|e| e.path))
            .or_else(|| paths.first().cloned())
            .context(crate::localize!(
                "先选择一个有源码证据的对象，再保存认知",
                "Select an object with source evidence before saving knowledge"
            ))?;
        let dimension = record
            .as_ref()
            .and_then(|r| r["dimension"].as_str())
            .unwrap_or(&self.dimension);
        let record_id = record.as_ref().and_then(|r| r["id"].as_str());
        let saved = knowledge::remember(
            self.index,
            &snapshot,
            knowledge::Remember {
                query: &query,
                dimension,
                claim,
                state,
                paths: &paths,
                record_id,
            },
        )?;
        self.status = crate::localize!(
            "已保存认知；原有历史版本保留。",
            "Knowledge saved; previous revisions are preserved."
        )
        .into();
        self.open(Action::KnowledgeRecord(Arc::new(saved["record"].clone())))?;
        Ok(())
    }

    pub fn select(&mut self, delta: isize) -> Result<()> {
        self.page.selected = self
            .page
            .selected
            .saturating_add_signed(delta)
            .min(self.page.items.len().saturating_sub(1));
        self.page.scroll = 0;
        self.page.horizontal = 0;
        self.preview()
    }

    pub fn selected_evidence(&self) -> Option<Evidence> {
        self.page
            .items
            .get(self.page.selected)
            .and_then(|i| i.evidence.clone())
            .or_else(|| self.page.evidence.clone())
    }

    pub fn selected_node(&self) -> Option<(String, String)> {
        self.page.node.clone().or_else(|| {
            self.page
                .items
                .get(self.page.selected)
                .and_then(|i| match &i.action {
                    Action::Node { snapshot, id } => Some((snapshot.clone(), id.clone())),
                    _ => None,
                })
        })
    }

    pub fn mark(
        &mut self,
        snapshot: &str,
        id: &str,
        state: &str,
        note: Option<&str>,
    ) -> Result<()> {
        let mut snapshot = self.index.snapshot(Some(snapshot))?;
        query::check_freshness(self.index, &mut snapshot)?;
        let node = self.node(&snapshot.id, id)?;
        let previous = marks::get(self.index, &snapshot, &node)?;
        marks::set(
            self.index,
            &snapshot,
            &node,
            state,
            note.unwrap_or_else(|| previous["note"].as_str().unwrap_or("")),
        )?;
        self.status = crate::localize!(
            "已记录：{} · {}",
            "Recorded: {} · {}",
            state_label(state),
            node.name
        );
        Ok(())
    }

    pub fn completed(&mut self, result: Completed) -> Result<()> {
        match result {
            Completed::Semantic { package, snapshot } => {
                self.understanding = query::understanding_data(self.index, &snapshot)?;
                self.status = crate::localize!(
                    "{package} 调用解析{}；仍不代表完整运行时路径",
                    "{package} call resolution {}; runtime paths may still be incomplete",
                    if snapshot.completeness.status == "complete" {
                        crate::localize!("完成", "complete")
                    } else {
                        crate::localize!("部分完成", "partially complete")
                    }
                );
                self.semantics.insert(package, *snapshot);
                let selection = self.page.selected;
                self.page = self.build(self.page.action.clone())?;
                self.page.selected = selection.min(self.page.items.len().saturating_sub(1));
                self.preview()?;
            }
            Completed::Review(report) => {
                self.review = Some(Arc::new((*report).clone()));
                self.status = crate::localize!(
                    "比较完成；Enter 查看变更前后与源码差异",
                    "Comparison complete; Enter to inspect old/new source and diffs"
                )
                .into();
                self.open(Action::Changes(Arc::new(*report)))?;
            }
        }
        Ok(())
    }

    fn matches_focus(&self, value: &Value) -> bool {
        let Some(focus) = &self.focus else {
            return true;
        };
        if array(&value["node_ids"])
            .iter()
            .filter_map(Value::as_str)
            .any(|id| focus.node_ids.iter().any(|known| known == id))
        {
            return true;
        }
        if s(&value["title"]) == focus.title {
            return true;
        }
        if !focus.by_path && !focus.node_ids.is_empty() {
            return false;
        }
        evidence_values(value)
            .iter()
            .any(|e| focus.paths.iter().any(|path| paths_related(path, &e.path)))
            || array(&value["paths"])
                .iter()
                .filter_map(Value::as_str)
                .any(|path| focus.paths.iter().any(|known| paths_related(known, path)))
    }

    fn linked_node(&self, id: &str) -> Result<Option<(String, Node)>> {
        let mut snapshots = vec![self.snapshot.id.clone()];
        if let Some(report) = &self.review {
            snapshots.insert(0, s(&report.data["head_snapshot"]).into());
            snapshots.push(s(&report.data["base_snapshot"]).into());
        }
        for snapshot in snapshots {
            if snapshot.is_empty() {
                continue;
            }
            if let Some(node) = self.index.find_nodes(&snapshot, id, 1)?.pop() {
                return Ok(Some((snapshot, node)));
            }
        }
        Ok(None)
    }

    fn architecture_lines(&self, component: Option<&str>) -> Vec<String> {
        let architecture = &self.understanding["architecture"];
        let name = |id: &str| {
            array(&architecture["components"])
                .iter()
                .find(|v| s(&v["id"]) == id)
                .map(|v| s(&v["title"]))
                .unwrap_or(id)
                .to_owned()
        };
        let mut lines = vec![
            String::new(),
            crate::localize!(
                "子系统协作 · 已定位的静态关系",
                "Subsystem collaboration · established static relations"
            )
            .into(),
        ];
        let related: Vec<_> = array(&architecture["dependencies"])
            .iter()
            .filter(|edge| {
                component.is_none_or(|id| s(&edge["source"]) == id || s(&edge["target"]) == id)
            })
            .collect();
        for edge in related.iter().take(12) {
            lines.push(crate::localize!(
                "{} → {} · {} {} · {}条",
                "{} → {} · {} {} · {} relations",
                name(s(&edge["source"])),
                name(s(&edge["target"])),
                kind(s(&edge["kind"])),
                resolution(s(&edge["resolution"])),
                edge["count"]
            ));
        }
        if related.is_empty() {
            lines.push(crate::localize!("未记录已定位的跨子系统关系；可进入架构维度和源码继续核查。", "No established cross-subsystem relations; inspect architecture and source for more evidence.").into());
        }
        if related.len() > 12 {
            lines.push(crate::localize!(
                "还有 {} 组关系；架构维度可查看逐条证据。",
                "{} more relation groups; inspect their evidence in Architecture.",
                related.len() - 12
            ));
        }
        if component.is_none() {
            for edge in array(&architecture["declared_dependencies"]).iter().take(6) {
                lines.push(crate::localize!(
                    "清单：{} → {} [{}]",
                    "Manifest: {} → {} [{}]",
                    s(&edge["source_name"]),
                    s(&edge["target_name"]),
                    s(&edge["kind"])
                ));
            }
        }
        lines
    }

    fn scenario_items(&self, page: &mut Page, value: &Value, snapshot: &str) -> Result<()> {
        let root = value["root"]["id"].as_str().or_else(|| {
            array(&value["nodes"])
                .first()
                .and_then(|n| n["id"].as_str())
        });
        let Some(root) = root else { return Ok(()) };
        let edges: Vec<Edge> = array(&value["edges"])
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect();
        let nodes: BTreeSet<_> = array(&value["nodes"])
            .iter()
            .filter_map(|n| n["id"].as_str())
            .collect();
        let seed = self.node(snapshot, root)?;
        let mut item = Item::node(&seed, snapshot);
        item.label = format!("● {}", seed.qualified_name);
        page.items.push(item);
        self.tree_branch(
            page,
            snapshot,
            root,
            &edges,
            &nodes,
            &mut BTreeSet::new(),
            &mut BTreeSet::new(),
            "",
            0,
        )?;
        if value["truncated"] == true {
            page.intro.push(
                crate::localize!(
                    "场景已达到深度、节点或边数上限；继续进入定义展开下一段。",
                    "Scenario limit reached; open a definition to explore the next neighborhood."
                )
                .into(),
            );
        }
        for route in array(&value["route_bindings"]) {
            page.intro.push(crate::localize!(
                "路由声明语法：{} → {}",
                "Route declaration syntax: {} → {}",
                s(&route["path_literal"]),
                s(&route["handler_expression"])
            ));
            if let Ok(evidence) = serde_json::from_value::<Evidence>(route["evidence"].clone()) {
                page.items.push(Item::source(
                    crate::localize!("查看路由注册语法", "Inspect route registration syntax"),
                    evidence,
                ));
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn tree_branch(
        &self,
        page: &mut Page,
        snapshot: &str,
        id: &str,
        edges: &[Edge],
        nodes: &BTreeSet<&str>,
        seen: &mut BTreeSet<String>,
        ancestors: &mut BTreeSet<String>,
        prefix: &str,
        depth: usize,
    ) -> Result<()> {
        if depth >= 3 {
            return Ok(());
        }
        seen.insert(id.into());
        ancestors.insert(id.into());
        let outgoing: Vec<_> = edges.iter().filter(|edge| edge.source == id).collect();
        for (position, edge) in outgoing.iter().enumerate() {
            let last = position + 1 == outgoing.len();
            let branch = if last { "└─" } else { "├─" };
            let name = edge
                .target
                .as_deref()
                .and_then(|target| self.node(snapshot, target).ok())
                .map(|n| n.qualified_name)
                .unwrap_or_else(|| edge.target_name.clone());
            let state = match edge.target.as_deref() {
                Some(target) if ancestors.contains(target) => crate::localize!("↻ 循环", "↻ Cycle"),
                Some(target) if seen.contains(target) => {
                    crate::localize!("↪ 已展开", "↪ Already expanded")
                }
                Some(target) if !nodes.contains(target) => {
                    crate::localize!("… 范围边界", "… Scope boundary")
                }
                Some(_) => resolution(&edge.resolution),
                None if edge.resolution == "external" => {
                    crate::localize!("外部边界", "External boundary")
                }
                None => crate::localize!("? 未知目标", "? Unknown target"),
            };
            let mut item = Item::new(
                format!("{prefix}{branch} {name} [{state}]"),
                format!("{}:{}", edge.evidence.path, edge.evidence.start_line),
                Action::Edge {
                    snapshot: snapshot.into(),
                    edge: Box::new((*edge).clone()),
                    incoming: false,
                },
            );
            item.evidence = Some(edge.evidence.clone());
            item.detail = vec![
                crate::localize!(
                    "关系：{} · {}",
                    "Relation: {} · {}",
                    kind(&edge.kind),
                    resolution(&edge.resolution)
                ),
                conditions(&edge.conditions),
                crate::localize!(
                    "Enter 查看关系发生处与可定位定义。",
                    "Enter to inspect the relation site and established definition."
                )
                .into(),
            ];
            page.items.push(item);
            if let Some(target) = edge.target.as_deref().filter(|target| {
                nodes.contains(target) && !seen.contains(*target) && !ancestors.contains(*target)
            }) {
                let next_prefix = format!("{prefix}{}", if last { "   " } else { "│  " });
                self.tree_branch(
                    page,
                    snapshot,
                    target,
                    edges,
                    nodes,
                    seen,
                    ancestors,
                    &next_prefix,
                    depth + 1,
                )?;
            }
        }
        ancestors.remove(id);
        Ok(())
    }

    fn batch_links(&self, page: &mut Page, value: &Value) -> Result<()> {
        let Some(report) = &self.review else {
            return Ok(());
        };
        let mut paths: BTreeSet<_> = evidence_values(value).into_iter().map(|e| e.path).collect();
        paths.extend(
            array(&value["paths"])
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned),
        );
        for change in array(&value["changes"]) {
            if let (Some(snapshot), Some(id)) =
                (change["snapshot_id"].as_str(), change["node_id"].as_str())
            {
                if let Ok(node) = self.node(snapshot, id) {
                    page.items.push(Item::node(&node, snapshot));
                }
            }
            for side in ["before", "after"] {
                for evidence in array(&change[side])
                    .iter()
                    .filter_map(|v| serde_json::from_value::<Evidence>(v.clone()).ok())
                {
                    paths.insert(evidence.path.clone());
                    page.items.push(Item::source(
                        crate::localize!(
                            "{}证据 · {}:{}",
                            "{} evidence · {}:{}",
                            if side == "before" {
                                crate::localize!("旧版", "Old")
                            } else {
                                crate::localize!("新版", "New")
                            },
                            evidence.path,
                            evidence.start_line
                        ),
                        evidence,
                    ));
                }
            }
        }
        for change in array(&report.data["changes"]) {
            let node = if change["after"].is_null() {
                &change["before"]
            } else {
                &change["after"]
            };
            if paths.contains(s(&node["evidence"]["path"]))
                || array(&value["node_ids"]).iter().any(|id| id == &node["id"])
            {
                page.items.push(Item::new(
                    crate::localize!(
                        "前后定义 · {}",
                        "Old/new definition · {}",
                        s(&node["qualified_name"])
                    ),
                    s(&change["state"]),
                    Action::Change {
                        before: s(&report.data["base_snapshot"]).into(),
                        after: s(&report.data["head_snapshot"]).into(),
                        value: Box::new(change.clone()),
                    },
                ));
            }
        }
        for file in array(&report.data["files"])
            .iter()
            .filter(|f| paths.contains(s(&f["path"])))
        {
            page.items.push(Item::new(
                format!("diff · {}", s(&file["path"])),
                crate::localize!("固定比较版本", "Pinned comparison revisions"),
                Action::Text {
                    title: format!("diff · {}", s(&file["path"])),
                    lines: s(&file["diff"]).lines().map(str::to_owned).collect(),
                },
            ));
        }
        let before_hashes = self.index.file_hashes(s(&report.data["base_snapshot"]))?;
        let after_hashes = self.index.file_hashes(s(&report.data["head_snapshot"]))?;
        for path in paths.iter().filter(|path| {
            !array(&report.data["files"])
                .iter()
                .any(|file| s(&file["path"]) == path.as_str())
        }) {
            let evidence = |hash: &String| Evidence {
                path: path.clone(),
                content_hash: hash.clone(),
                start_byte: 0,
                end_byte: 0,
                start_line: 1,
                end_line: 1,
                start_column: 0,
                end_column: 0,
            };
            let before = before_hashes.get(path).map(evidence);
            let after = after_hashes.get(path).map(evidence);
            if before.as_ref().map(|e| &e.content_hash) != after.as_ref().map(|e| &e.content_hash) {
                page.items.push(Item::new(
                    format!("diff · {path}"),
                    crate::localize!(
                        "全批次固定版本；按需读取",
                        "Pinned batch revisions; loaded on demand"
                    ),
                    Action::Diff {
                        path: path.clone(),
                        before,
                        after,
                    },
                ));
            }
        }
        Ok(())
    }

    fn node(&self, snapshot: &str, id: &str) -> Result<Node> {
        self.index
            .find_nodes(snapshot, id, 1)?
            .pop()
            .context(crate::localize!(
                "当前快照中找不到该定义",
                "Definition not found in this snapshot"
            ))
    }

    fn resolved_snapshot(&self, snapshot: &str, node: &Node) -> String {
        if !self.is_current(snapshot) {
            return snapshot.into();
        }
        let package = self
            .snapshot
            .project
            .packages
            .iter()
            .find(|p| p.id == node.package);
        package
            .and_then(|p| self.semantics.get(&p.name))
            .map(|s| s.id.clone())
            .unwrap_or_else(|| snapshot.into())
    }

    fn is_current(&self, snapshot: &str) -> bool {
        snapshot == self.snapshot.id || self.semantics.values().any(|s| s.id == snapshot)
    }

    fn source_evidence(&self, path: &str) -> Result<Option<Evidence>> {
        Ok(self.hashes.get(path).map(|hash| Evidence {
            path: path.into(),
            content_hash: hash.clone(),
            start_byte: 0,
            end_byte: 0,
            start_line: 1,
            end_line: 1,
            start_column: 0,
            end_column: 0,
        }))
    }

    fn build(&self, action: Action) -> Result<Page> {
        let mut page = Page::new(action.clone(), "");
        match action {
            Action::Home => {
                page.title = crate::localize!(
                    "项目概览 · 认知工作台",
                    "Project overview · Knowledge workbench"
                )
                .into();
                page.intro = vec![
                    crate::localize!("建立认知基线 → 理解改动批次 → 沿证据核查 → 更新项目认知。", "Establish a baseline → Understand a change batch → Check evidence → Update knowledge.").into(),
                    crate::localize!("{} 个子系统 · {} 组已定位依赖 · 八个联动视角", "{} subsystems · {} established dependency groups · Eight linked perspectives",
                        self.understanding["architecture"]["total"],
                        self.understanding["architecture"]["dependencies_total"]
                    ),
                    crate::localize!("先选择对象，再按 d / 1–8 切换视角；p 保存当前问题，x 回到项目全景。", "Select an object, then d / 1–8 to switch perspectives; p to keep a question, x for the full project.").into(),
                ];
                let mut baseline = Item::new(
                    crate::localize!("建立认知基线", "Establish a baseline"),
                    crate::localize!(
                        "项目能力、子系统职责与协作关系",
                        "Project capabilities, subsystem responsibilities and collaboration"
                    ),
                    Action::Baseline,
                );
                for component in array(&self.understanding["architecture"]["components"])
                    .iter()
                    .take(16)
                {
                    baseline.detail.push(format!(
                        "{} · {}",
                        s(&component["title"]),
                        s(&component["summary"])
                    ));
                }
                for package in array(&self.guide["packages"]) {
                    if let Some(description) = package["description"]["text"].as_str() {
                        baseline.detail.push(description.into());
                    }
                }
                page.items = vec![
                    baseline,
                    Item::new(
                        crate::localize!("入口与调用", "Entries and calls"),
                        crate::localize!(
                            "从程序入口深入场景、路径与未知边界",
                            "Explore scenarios, paths and unknown boundaries from declared entries"
                        ),
                        Action::Entries,
                    ),
                    Item::new(
                        crate::localize!("理解改动批次", "Understand a change batch"),
                        crate::localize!(
                            "功能分组、边界变化与前后源码",
                            "Functional groups, boundary changes and old/new source"
                        ),
                        self.review
                            .as_ref()
                            .map(|r| Action::Changes(r.clone()))
                            .unwrap_or(Action::ReviewMenu),
                    ),
                    Item::new(
                        crate::localize!("沿证据核查", "Check the evidence"),
                        crate::localize!(
                            "核查清单、测试关联与关键场景",
                            "Review checklist, test associations and key scenarios"
                        ),
                        self.review
                            .as_ref()
                            .map(|r| Action::BatchSection {
                                report: r.clone(),
                                section: "checklist".into(),
                            })
                            .unwrap_or(Action::Dimension("verification".into())),
                    ),
                    Item::new(
                        crate::localize!("更新项目认知", "Update project knowledge"),
                        crate::localize!(
                            "结论、疑问、需复核记录与历史",
                            "Conclusions, questions, outdated records and history"
                        ),
                        Action::Knowledge { history: None },
                    ),
                    Item::new(
                        crate::localize!("八维视角", "Eight perspectives"),
                        crate::localize!(
                            "围绕同一对象切换观察维度",
                            "Switch perspectives around the same object"
                        ),
                        Action::Dimensions,
                    ),
                    Item::new(
                        crate::localize!("构建包与文件", "Build packages and files"),
                        crate::localize!(
                            "下钻源码与清单声明的依赖",
                            "Explore source and manifest dependencies"
                        ),
                        Action::Packages,
                    ),
                    Item::new(
                        crate::localize!("分析范围与状态", "Analysis scope and status"),
                        crate::localize!(
                            "查看范围、未知和截断",
                            "Inspect scope, unknowns and truncation"
                        ),
                        Action::Status,
                    ),
                ];
            }
            Action::Baseline => {
                page.title = crate::localize!("项目认知基线", "Project baseline").into();
                page.intro =
                    vec![crate::localize!("按源码模块聚合子系统；职责引用文档，结构数量保留事实来源。", "Subsystems follow source modules; responsibilities quote documentation, with structural facts kept explicit.").into()];
                for component in array(&self.understanding["architecture"]["components"]) {
                    page.items.push(Item::aggregate(component, "architecture"));
                }
                page.intro.extend(self.architecture_lines(None));
                if self.understanding["architecture"]["truncated"] == true {
                    page.intro.push(
                        crate::localize!(
                            "子系统列表已截断；可进入构建包与文件继续阅读。",
                            "Subsystem list truncated; continue through build packages and files."
                        )
                        .into(),
                    );
                }
                page.items.push(Item::new(
                    crate::localize!("项目声明与文档", "Project declarations and documentation"),
                    crate::localize!(
                        "能力、目的和模块职责证据",
                        "Evidence for capabilities, intent and module responsibilities"
                    ),
                    Action::Dimension("intent".into()),
                ));
                page.items.push(Item::new(
                    crate::localize!("构建包与文件", "Build packages and files"),
                    crate::localize!(
                        "进一步浏览完整定义",
                        "Browse the complete set of definitions"
                    ),
                    Action::Packages,
                ));
            }
            Action::Dimensions => {
                page.title = crate::localize!("八维视角", "Eight perspectives").into();
                page.intro = vec![crate::localize!("切换视角保留当前对象与问题；x 清除对象筛选。", "Perspective switches keep the current object and question; x clears the object filter.").into()];
                for (position, dimension) in
                    array(&self.understanding["dimensions"]).iter().enumerate()
                {
                    let mut item = Item::new(
                        format!("{}  {}", position + 1, s(&dimension["title"])),
                        s(&dimension["summary"]),
                        Action::Dimension(s(&dimension["id"]).into()),
                    );
                    item.detail = array(&dimension["limitations"])
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect();
                    page.items.push(item);
                }
            }
            Action::Dimension(id) => {
                if id == "knowledge" {
                    return self.build(Action::Knowledge { history: None });
                }
                if id == "changes" {
                    return self.build(
                        self.review
                            .as_ref()
                            .map(|r| Action::Changes(r.clone()))
                            .unwrap_or(Action::ReviewMenu),
                    );
                }
                let dimension = array(&self.understanding["dimensions"])
                    .iter()
                    .find(|d| s(&d["id"]) == id)
                    .context(crate::localize!(
                        "未找到分析维度",
                        "Analysis perspective not found"
                    ))?;
                page.title = s(&dimension["title"]).into();
                page.intro = vec![s(&dimension["summary"]).into()];
                for value in array(&dimension["items"])
                    .iter()
                    .filter(|value| self.matches_focus(value))
                {
                    page.items.push(Item::aggregate(value, &id));
                }
                page.intro.extend(
                    array(&dimension["limitations"])
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|v| crate::localize!("范围：{v}", "Scope: {v}", v = v)),
                );
                if dimension["truncated"] == true {
                    page.intro.push(crate::localize!("共有 {} 项，当前展示已截断；可按对象缩小范围。", "{} items in total; displayed results are truncated. Narrow the scope by object.",
                        dimension["total"]
                    ));
                }
                if page.items.is_empty() {
                    page.intro.push(
                        crate::localize!("当前对象没有此维度的直接关联证据；x 清除筛选，或 / 搜索其他对象。", "No directly associated evidence for this object in this perspective; x clears the filter, / searches other objects.").into(),
                    );
                }
            }
            Action::Aggregate { dimension, value } => {
                page.title = s(&value["title"]).into();
                page.intro = value_lines(&value);
                if dimension == "architecture" {
                    page.intro
                        .extend(self.architecture_lines(value["id"].as_str()));
                }
                if dimension == "behavior" {
                    self.scenario_items(&mut page, &value, &self.snapshot.id)?;
                }
                for (position, evidence) in evidence_values(&value).into_iter().enumerate() {
                    page.items.push(Item::source(
                        crate::localize!(
                            "证据 {} · {}:{}",
                            "Evidence {} · {}:{}",
                            position + 1,
                            evidence.path,
                            evidence.start_line
                        ),
                        evidence,
                    ));
                }
                for id in array(&value["node_ids"]).iter().filter_map(Value::as_str) {
                    if let Some((snapshot, node)) = self.linked_node(id)? {
                        page.items.push(Item::node(&node, &snapshot));
                    }
                }
                for id in array(&value["component_ids"])
                    .iter()
                    .filter_map(Value::as_str)
                {
                    if let Some(component) =
                        array(&self.understanding["architecture"]["components"])
                            .iter()
                            .find(|v| s(&v["id"]) == id)
                    {
                        page.items.push(Item::aggregate(component, "architecture"));
                    }
                }
                if dimension == "changes" {
                    self.batch_links(&mut page, &value)?;
                }
                page.items.push(Item::new(
                    crate::localize!(
                        "围绕此对象切换八维",
                        "Explore this object across eight perspectives"
                    ),
                    crate::localize!(
                        "d 或 1–8；保留对象、问题和阅读位置",
                        "d or 1–8; keep the object, question and reading position"
                    ),
                    Action::Dimensions,
                ));
                page.intro
                    .push(crate::localize!("c 保存结论 · v 保存疑问；来源与事实需要自行核查。", "c saves a conclusion · v saves a question; check the sources and facts first.").into());
            }
            Action::Trace { snapshot, id } => {
                let node = self.node(&snapshot, &id)?;
                let actual = self.resolved_snapshot(&snapshot, &node);
                let snapshot_value = self.index.snapshot(Some(&actual))?;
                let report = query::trace(self.index, &snapshot_value, &id, false, 3, 60)?;
                let mut value = report.data;
                value["truncated"] = Value::Bool(report.completeness.truncated);
                page.title = crate::localize!("调用场景 · {}", "Call scenario · {}", node.name);
                page.node = Some((actual.clone(), id));
                page.evidence = Some(node.evidence.clone());
                page.intro =
                    vec![crate::localize!("有界调用邻域：最多 3 层、60 个定义。每个未知目标保留源码证据。", "Bounded call neighborhood: up to 3 levels and 60 definitions. Unknown targets retain source evidence.").into()];
                self.scenario_items(&mut page, &value, &actual)?;
            }
            Action::BatchSection { report, section } => {
                page.title = batch_section_title(&section).into();
                page.intro = vec![format!(
                    "{} → {}",
                    s(&report.data["base_revision"]),
                    s(&report.data["head_revision"])
                )];
                for value in array(&report.data["batch"][&section])
                    .iter()
                    .filter(|v| self.matches_focus(v))
                {
                    let mut value = value.clone();
                    if value["title"].is_null() {
                        value["title"] = Value::String(s(&value["summary"]).to_owned());
                    }
                    page.items.push(Item::aggregate(&value, "changes"));
                }
                if page.items.is_empty() {
                    page.intro
                        .push(crate::localize!("当前范围没有此类已提取记录；可查看符号变化和文件 diff。", "No extracted records of this kind in the current scope; inspect symbol changes and file diffs.").into());
                }
            }
            Action::Knowledge { history } => {
                page.title = if history.is_some() {
                    crate::localize!("认知记录历史", "Knowledge record history")
                } else {
                    crate::localize!("认知与阅读", "Knowledge and reading")
                }
                .into();
                page.intro = vec![
                    crate::localize!("已确认结论与疑问绑定证据；证据变化显示需复核，历史保留。", "Confirmed conclusions and questions are bound to evidence. Changed evidence requires review; history is preserved.").into(),
                    crate::localize!("选中对象后 c 保存结论 / v 保存疑问；h 查看历史。", "Select an object, then c to save a conclusion / v to save a question; h opens history.").into(),
                ];
                for value in knowledge::list(
                    self.index,
                    &self.snapshot,
                    history.as_deref(),
                    history.is_some(),
                )? {
                    if history.is_none() && !self.matches_focus(&value) {
                        continue;
                    }
                    let mut item = Item::new(
                        format!(
                            "[{}] {} · v{}",
                            knowledge_state(s(&value["state"])),
                            s(&value["title"]),
                            value["revision"]
                        ),
                        s(&value["claim"]),
                        Action::KnowledgeRecord(Arc::new(value.clone())),
                    );
                    item.detail = value_lines(&value);
                    item.evidence = evidence_values(&value).into_iter().next();
                    page.items.push(item);
                }
                if page.items.is_empty() {
                    page.intro
                        .push(crate::localize!("还没有关联的认知记录；从子系统或具体定义开始，核查后保存。", "No associated knowledge records yet; start from a subsystem or definition, verify it, then save.").into());
                }
            }
            Action::KnowledgeRecord(value) => {
                page.title = crate::localize!("认知 · {}", "Knowledge · {}", s(&value["title"]));
                page.intro = value_lines(&value);
                for evidence in evidence_values(&value) {
                    page.items.push(Item::source(
                        crate::localize!(
                            "记录证据 · {}:{}",
                            "Recorded evidence · {}:{}",
                            evidence.path,
                            evidence.start_line
                        ),
                        evidence,
                    ));
                }
                for path in array(&value["changed_evidence"])
                    .iter()
                    .filter_map(Value::as_str)
                {
                    if let Some(evidence) = self.source_evidence(path)? {
                        page.items.push(Item::source(
                            crate::localize!(
                                "当前证据 · {path}",
                                "Current evidence · {path}",
                                path = path
                            ),
                            evidence,
                        ));
                    }
                }
                page.items.push(Item::new(
                    crate::localize!("查看记录历史", "View record history"),
                    crate::localize!(
                        "保留原结论与历次证据",
                        "Preserve previous conclusions and their evidence"
                    ),
                    Action::Knowledge {
                        history: value["id"].as_str().map(str::to_owned),
                    },
                ));
                page.intro
                    .push(crate::localize!("c 确认 / 改写当前结论 · v 改为疑问；生成新版本。", "c confirms / revises this conclusion · v records a question; both create a new revision.").into());
            }
            Action::Packages => {
                page.title = crate::localize!("模块与依赖", "Modules and dependencies").into();
                page.intro = vec![crate::localize!(
                    "箭头表示清单声明的依赖，不代表调用或业务分层。",
                    "Arrows represent manifest dependencies, not calls or business layering."
                )
                .into()];
                let mut packages: Vec<_> = self.snapshot.project.packages.iter().collect();
                packages.sort_by_key(|p| &p.name);
                for p in packages {
                    let summary = array(&self.guide["packages"])
                        .iter()
                        .find(|v| v["id"] == p.id);
                    let mut item = Item::new(
                        &p.name,
                        if p.root.is_empty() { "." } else { &p.root },
                        Action::Package(p.id.clone()),
                    );
                    if let Some(summary) = summary {
                        item.detail = vec![
                            summary["description"]["text"]
                                .as_str()
                                .unwrap_or(crate::localize!(
                                    "清单未声明职责",
                                    "No responsibility declared in the manifest"
                                ))
                                .into(),
                            crate::localize!(
                                "{} 个内部模块声明依赖它",
                                "{} internal modules declare a dependency on it",
                                summary["dependents"]
                            ),
                        ];
                    }
                    for d in &p.dependencies {
                        item.detail.push(format!(
                            "{} → {} [{}{}{}]",
                            p.name,
                            d.package,
                            d.kind,
                            if d.optional {
                                crate::localize!(" · 可选", " · optional")
                            } else {
                                ""
                            },
                            d.condition
                                .as_ref()
                                .map(|c| format!(" · {c}"))
                                .unwrap_or_default()
                        ));
                    }
                    page.items.push(item);
                }
            }
            Action::Package(id) => {
                let p = self
                    .snapshot
                    .project
                    .packages
                    .iter()
                    .find(|p| p.id == id)
                    .context(crate::localize!("未找到模块", "Module not found"))?;
                page.title = p.name.clone();
                page.intro = vec![
                    crate::localize!("目录：{}", "Directory: {}", if p.root.is_empty() { "." } else { &p.root }),
                    crate::localize!("构建入口、源码文件及声明依赖；测试/可选/条件依赖均单独标识。", "Build entries, source files and declared dependencies; test, optional and conditional dependencies are labeled separately.").into(),
                ];
                page.items.push(Item::new(
                    crate::localize!("源码文件", "Source files"),
                    crate::localize!("按文件浏览定义", "Browse definitions by file"),
                    Action::Files(id.clone()),
                ));
                page.items.push(Item::new(
                    crate::localize!("类型与接口", "Types and interfaces"),
                    crate::localize!(
                        "此模块的类型、接口和实现块",
                        "Types, interfaces and implementation blocks in this module"
                    ),
                    Action::Symbols {
                        package: id.clone(),
                        path: String::new(),
                        types: true,
                        offset: 0,
                    },
                ));
                for unit in &p.units {
                    if let Some(evidence) = self.source_evidence(&unit.source)? {
                        page.items.push(Item::source(
                            format!("{} · {}", kind(&unit.kind), unit.name),
                            evidence,
                        ));
                    }
                }
                for d in &p.dependencies {
                    let label = format!(
                        "→ {} [{}{}]",
                        d.package,
                        d.kind,
                        if d.optional {
                            crate::localize!(", 可选", ", optional")
                        } else {
                            ""
                        }
                    );
                    let detail = vec![
                        crate::localize!(
                            "来源：项目清单声明；不是已确认的调用关系。",
                            "Source: project manifest declarations; no established call relation."
                        )
                        .into(),
                        crate::localize!(
                            "条件：{}",
                            "Condition: {}",
                            d.condition
                                .as_deref()
                                .unwrap_or(crate::localize!("无显式条件", "No explicit condition"))
                        ),
                        crate::localize!("解析状态：{}", "Resolution: {}", d.resolution),
                    ];
                    let target = d.path.as_ref().and_then(|path| {
                        self.snapshot
                            .project
                            .packages
                            .iter()
                            .find(|p| &p.root == path)
                    });
                    let action = target
                        .map(|p| Action::Package(p.id.clone()))
                        .unwrap_or_else(|| Action::Text {
                            title: label.clone(),
                            lines: detail.clone(),
                        });
                    let mut item =
                        Item::new(label, d.condition.clone().unwrap_or_default(), action);
                    item.detail = detail;
                    page.items.push(item);
                }
            }
            Action::Entries => {
                page.title = crate::localize!("入口与调用", "Entries and calls").into();
                page.intro = vec![
                    crate::localize!("程序入口与库入口；不把 example、build.rs 当作业务入口。", "Program and library entries; examples and build.rs are excluded from business entries.").into(),
                    crate::localize!("路由与 Python 入口保留显式声明；候选不证明运行时实际注册。", "Routes and Python entries retain explicit declarations; candidates do not prove runtime registration.").into(),
                ];
                let mut stmt = self.index.connection.prepare("SELECT data FROM nodes WHERE snapshot=?1 AND name='main' AND kind='function' ORDER BY path,start")?;
                let rows = stmt.query_map([&self.snapshot.id], |r| r.get::<_, String>(0))?;
                for row in rows {
                    let n: Node = serde_json::from_str(&row?)?;
                    let entry = self
                        .snapshot
                        .project
                        .packages
                        .iter()
                        .flat_map(|p| &p.units)
                        .any(|u| {
                            u.id == n.unit
                                && u.kind == "bin"
                                && n.qualified_name == format!("{}::main", u.name.replace('-', "_"))
                        });
                    if !n.is_test && entry {
                        page.items.push(Item::node(&n, &self.snapshot.id));
                    }
                }
                if let Some(behavior) = array(&self.understanding["dimensions"])
                    .iter()
                    .find(|v| s(&v["id"]) == "behavior")
                {
                    for value in array(&behavior["items"]).iter().filter(|v| {
                        matches!(
                            s(&v["entry_kind"]),
                            "python_main"
                                | "python_script"
                                | "http_route"
                                | "bin"
                                | "route_registration_syntax"
                        )
                    }) {
                        page.items.push(Item::aggregate(value, "behavior"));
                    }
                }
                for p in &self.snapshot.project.packages {
                    for u in p.units.iter().filter(|u| u.kind == "lib") {
                        page.items.push(Item::new(
                            crate::localize!("{} · 库入口", "{} · Library entry", p.name),
                            &u.source,
                            Action::Symbols {
                                package: p.id.clone(),
                                path: u.source.clone(),
                                types: false,
                                offset: 0,
                            },
                        ));
                    }
                }
                if page.items.is_empty() {
                    page.intro
                        .push(crate::localize!("未识别到程序/库入口；可通过模块或 / 搜索开始阅读。", "No program/library entry found; start from a module or use / to search.").into());
                }
            }
            Action::Files(package) => {
                page.title = crate::localize!("源码文件", "Source files").into();
                let mut stmt = self.index.connection.prepare("SELECT path,COUNT(*) FROM nodes WHERE snapshot=?1 AND package=?2 GROUP BY path ORDER BY path")?;
                let rows = stmt.query_map(params![self.snapshot.id, package], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, usize>(1)?))
                })?;
                for row in rows {
                    let (path, count) = row?;
                    let mut item = Item::new(
                        &path,
                        crate::localize!("{count} 个定义", "{count} definitions", count = count),
                        Action::Symbols {
                            package: package.clone(),
                            path: path.clone(),
                            types: false,
                            offset: 0,
                        },
                    );
                    item.evidence = self.source_evidence(&path)?;
                    page.items.push(item);
                }
                page.intro.push(crate::localize!(
                    "{} 个包含已索引定义的文件；不代表所有资源文件。",
                    "{} files contain indexed definitions; this excludes other resource files.",
                    page.items.len()
                ));
            }
            Action::Symbols {
                package,
                path,
                types,
                offset,
            } => {
                page.title = if types {
                    crate::localize!("类型与接口", "Types and interfaces").into()
                } else {
                    path.clone()
                };
                let mut stmt = self.index.connection.prepare("SELECT data FROM nodes WHERE snapshot=?1 AND (?2='' OR package=?2) AND (?3='' OR path=?3) AND (?4=0 OR kind IN ('type','trait','impl')) ORDER BY qualified,path,start LIMIT ?5 OFFSET ?6")?;
                let nodes = stmt
                    .query_map(
                        params![
                            self.snapshot.id,
                            package,
                            path,
                            types,
                            PAGE_SIZE + 1,
                            offset
                        ],
                        |r| r.get::<_, String>(0),
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let more = nodes.len() > PAGE_SIZE;
                for row in nodes.into_iter().take(PAGE_SIZE) {
                    page.items.push(Item::node(
                        &serde_json::from_str::<Node>(&row)?,
                        &self.snapshot.id,
                    ));
                }
                page.intro = vec![crate::localize!("第 {} 页 · 类型/实现块是静态定义，不代表运行时绑定。", "Page {} · Types and implementation blocks are static declarations, not runtime bindings.",
                    offset / PAGE_SIZE + 1
                )];
                if !path.is_empty() {
                    page.evidence = self.source_evidence(&path)?;
                }
                if more {
                    page.items.push(Item::new(
                        crate::localize!("下一页 →", "Next page →"),
                        crate::localize!(
                            "继续浏览，不省略后续定义",
                            "Continue browsing the remaining definitions"
                        ),
                        Action::Symbols {
                            package,
                            path,
                            types,
                            offset: offset + PAGE_SIZE,
                        },
                    ));
                }
            }
            Action::Node { snapshot, id } => {
                let node = self.node(&snapshot, &id)?;
                let actual = self.resolved_snapshot(&snapshot, &node);
                let profile = self.index.snapshot(Some(&actual))?;
                let mark = marks::get(self.index, &profile, &node)?;
                page.title = node.qualified_name.clone();
                page.node = Some((actual.clone(), id.clone()));
                page.evidence = Some(node.evidence.clone());
                page.intro = vec![
                    node.signature.clone(),
                    format!(
                        "{}:{} · {} · {}",
                        node.evidence.path,
                        node.evidence.start_line,
                        kind(&node.kind),
                        state_label(s(&mark["state"]))
                    ),
                    conditions(&node.conditions),
                ];
                if let Some(note) = mark["note"].as_str().filter(|n| !n.is_empty()) {
                    page.intro.push(crate::localize!(
                        "笔记：{note}",
                        "Note: {note}",
                        note = note
                    ));
                }
                if profile.context.analysis != "semantic" {
                    page.intro.push(
                        crate::localize!(
                            "当前为结构索引：调用目标尚未解析；可在下面解析本模块。",
                            "Syntax index: call targets are unresolved; resolve this module below."
                        )
                        .into(),
                    );
                } else {
                    page.intro.push(crate::localize!("语义范围：{} · {}；接口调用不等于具体运行时实现", "Semantic scope: {} · {}; interface calls do not identify a unique runtime implementation",
                        profile.context.scope.as_deref().unwrap_or(crate::localize!("工作区", "Workspace")),
                        if profile.completeness.status == "complete" {
                            crate::localize!("已完成", "Complete")
                        } else {
                            crate::localize!("部分完成", "partially complete")
                        }
                    ));
                }
                page.items.push(Item::source(
                    crate::localize!("查看定义源码", "View definition source"),
                    node.evidence.clone(),
                ));
                page.items.push(Item::new(
                    crate::localize!("它调用谁 / 引用了谁 →", "What it calls / references →"),
                    crate::localize!(
                        "展开出向关系，每条都能查看证据",
                        "Expand outgoing relations and inspect their evidence"
                    ),
                    Action::Relations {
                        snapshot: actual.clone(),
                        id: id.clone(),
                        incoming: false,
                        offset: 0,
                    },
                ));
                page.items.push(Item::new(
                    crate::localize!("← 谁调用 / 引用了它", "← What calls / references it"),
                    crate::localize!("仅列出已定位到该定义的关系；空不代表无人使用", "Only relations with established targets are listed; an empty list does not prove no usage"),
                    Action::Relations {
                        snapshot: actual.clone(),
                        id: id.clone(),
                        incoming: true,
                        offset: 0,
                    },
                ));
                let package = self
                    .snapshot
                    .project
                    .packages
                    .iter()
                    .find(|p| p.id == node.package);
                if let Some(p) = package.filter(|_| self.is_current(&snapshot)) {
                    page.items.push(Item::new(
                        crate::localize!("解析 / 更新本模块调用", "Resolve / update calls in this module"),
                        crate::localize!("{} · 后台执行，需要本地语言服务；不构建项目", "{} · Runs in the background using a local language service; no project build", p.name),
                        Action::Job(Request::Semantic(p.name.clone())),
                    ));
                }
                let mut stmt = self.index.connection.prepare(
                    "SELECT data FROM nodes WHERE snapshot=?1 AND path=?2 ORDER BY start,id",
                )?;
                let rows = stmt.query_map(params![actual, node.evidence.path], |r| {
                    r.get::<_, String>(0)
                })?;
                for row in rows {
                    let child: Node = serde_json::from_str(&row?)?;
                    if child.parent.as_deref() == Some(&id) {
                        page.items.push(Item::node(&child, &actual));
                    }
                }
                page.items.push(Item::new(
                    crate::localize!("标记阅读状态", "Set reading status"),
                    crate::localize!(
                        "已读 / 有疑问 / 未读；n 编辑笔记",
                        "Read / Question / Unread; n edits notes"
                    ),
                    Action::Marks {
                        snapshot: actual,
                        id,
                    },
                ));
                page.items.push(Item::new(
                    crate::localize!("展开有界调用场景", "Explore a bounded call scenario"),
                    crate::localize!(
                        "3 层 / 60 个定义；循环和未知明确标识",
                        "3 levels / 60 definitions; cycles and unknowns are labeled"
                    ),
                    Action::Trace {
                        snapshot: page.node.as_ref().unwrap().0.clone(),
                        id: page.node.as_ref().unwrap().1.clone(),
                    },
                ));
                page.items.push(Item::new(
                    crate::localize!(
                        "同一对象的八维视角",
                        "Eight perspectives of the same object"
                    ),
                    crate::localize!(
                        "d 或 1–8 切换；c 保存结论 / v 保存疑问",
                        "d or 1–8 switches perspectives; c saves conclusions / v saves questions"
                    ),
                    Action::Dimensions,
                ));
            }
            Action::Relations {
                snapshot,
                id,
                incoming,
                offset,
            } => {
                let node = self.node(&snapshot, &id)?;
                let actual = self.resolved_snapshot(&snapshot, &node);
                page.title = format!(
                    "{} {}",
                    if incoming {
                        crate::localize!("谁引用", "Referenced by")
                    } else {
                        crate::localize!("引用了谁", "References")
                    },
                    node.name
                );
                page.node = Some((actual.clone(), id.clone()));
                page.evidence = Some(node.evidence.clone());
                page.intro =
                    vec![crate::localize!("已解析、接口/虚派发与未解析关系分别标识。Enter 查看关系证据。", "Resolved, interface/virtual and unresolved relations are labeled separately. Enter opens evidence.").into()];
                let column = if incoming { "target" } else { "source" };
                let mut stmt = self.index.connection.prepare(&format!("SELECT data FROM edges WHERE snapshot=?1 AND {column}=?2 ORDER BY id LIMIT ?3 OFFSET ?4"))?;
                let rows = stmt
                    .query_map(params![actual, id, PAGE_SIZE + 1, offset], |r| {
                        r.get::<_, String>(0)
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let more = rows.len() > PAGE_SIZE;
                for row in rows.into_iter().take(PAGE_SIZE) {
                    let edge: Edge = serde_json::from_str(&row)?;
                    let name = if incoming {
                        self.node(&actual, &edge.source)?.qualified_name
                    } else {
                        edge.target_name.clone()
                    };
                    let mut item = Item::new(
                        format!("{} {}", if incoming { "←" } else { "→" }, name),
                        format!(
                            "{} · {} · {}:{}",
                            kind(&edge.kind),
                            resolution(&edge.resolution),
                            edge.evidence.path,
                            edge.evidence.start_line
                        ),
                        Action::Edge {
                            snapshot: actual.clone(),
                            edge: Box::new(edge.clone()),
                            incoming,
                        },
                    );
                    item.detail = vec![
                        crate::localize!("关系：{} · {}", "Relation: {} · {}",
                            kind(&edge.kind),
                            resolution(&edge.resolution)
                        ),
                        conditions(&edge.conditions),
                        crate::localize!("Enter 查看调用处 / 目标定义。未解析不自动猜测目标。", "Enter opens the call site / target definition. Unresolved targets are kept unknown.").into(),
                    ];
                    item.evidence = Some(edge.evidence);
                    page.items.push(item);
                }
                if more {
                    page.items.push(Item::new(
                        crate::localize!("下一页 →", "Next page →"),
                        crate::localize!("继续浏览关系", "Continue browsing relations"),
                        Action::Relations {
                            snapshot: actual.clone(),
                            id: id.clone(),
                            incoming,
                            offset: offset + PAGE_SIZE,
                        },
                    ));
                }
                if page.items.is_empty() {
                    page.intro.push(
                        crate::localize!(
                            "未找到已记录的关系；不表示没有调用或影响。",
                            "No indexed relations found; this does not prove no calls or impact."
                        )
                        .into(),
                    );
                }
                if self.index.snapshot(Some(&actual))?.context.analysis != "semantic" {
                    if let Some(p) = self
                        .snapshot
                        .project
                        .packages
                        .iter()
                        .find(|p| p.id == node.package)
                    {
                        if snapshot == self.snapshot.id {
                            page.items.insert(
                                0,
                                Item::new(
                                    crate::localize!(
                                        "解析本模块调用",
                                        "Resolve calls in this module"
                                    ),
                                    crate::localize!(
                                        "{} · 后台运行，可继续浏览",
                                        "{} · Runs in the background; browsing remains available",
                                        p.name
                                    ),
                                    Action::Job(Request::Semantic(p.name.clone())),
                                ),
                            );
                        }
                    }
                }
            }
            Action::Edge {
                snapshot,
                edge,
                incoming,
            } => {
                page.title = crate::localize!("关系 · {}", "Relation · {}", edge.target_name);
                page.evidence = Some(edge.evidence.clone());
                page.intro = vec![
                    format!("{} · {}", kind(&edge.kind), resolution(&edge.resolution)),
                    conditions(&edge.conditions),
                ];
                if matches!(
                    edge.resolution.as_str(),
                    "interface" | "virtual" | "ambiguous" | "navigation_only"
                ) {
                    page.intro
                        .push(crate::localize!("此处只能定位接口/候选定义，不代表运行时一定进入该实现。", "Only an interface/candidate definition is established; runtime execution of that implementation is not guaranteed.").into());
                }
                let next = if incoming {
                    Some(&edge.source)
                } else {
                    edge.target.as_ref()
                };
                if let Some(id) = next {
                    page.items
                        .push(Item::node(&self.node(&snapshot, id)?, &snapshot));
                } else {
                    page.intro.push(
                        crate::localize!(
                            "目标未定位；保留原调用表达式，不生成虚假的调用链。",
                            "Target unresolved; the original call expression is preserved."
                        )
                        .into(),
                    );
                }
                page.items.push(Item::source(
                    crate::localize!("查看关系发生处", "View relation source"),
                    edge.evidence.clone(),
                ));
            }
            Action::Source(evidence) => {
                page.title = crate::localize!(
                    "源码 · {}:{}",
                    "Source · {}:{}",
                    evidence.path,
                    evidence.start_line
                );
                page.intro = self.source_lines(&evidence, true)?;
                page.scroll = evidence.start_line.saturating_sub(5);
                page.evidence = Some(evidence);
            }
            Action::Diff {
                path,
                before,
                after,
            } => {
                let old = before
                    .as_ref()
                    .map(|e| self.index.content(&e.content_hash))
                    .transpose()?
                    .unwrap_or_default();
                let new = after
                    .as_ref()
                    .map(|e| self.index.content(&e.content_hash))
                    .transpose()?
                    .unwrap_or_default();
                page.title = format!("diff · {path}");
                let diff = similar::TextDiff::from_lines(&old, &new)
                    .unified_diff()
                    .context_radius(3)
                    .header(
                        crate::localize!("旧版", "Old"),
                        crate::localize!("新版", "New"),
                    )
                    .to_string();
                page.intro = diff.lines().take(500).map(str::to_owned).collect();
                if diff.lines().count() > 500 {
                    page.intro.push(
                        crate::localize!(
                            "… 差异已截断；进入下方两侧完整源码继续核查。",
                            "… Diff truncated; inspect the complete old/new source below."
                        )
                        .into(),
                    );
                }
                if let Some(evidence) = before {
                    page.items.push(Item::source(
                        crate::localize!("旧版完整源码", "Complete old source"),
                        evidence,
                    ));
                }
                if let Some(evidence) = after {
                    page.items.push(Item::source(
                        crate::localize!("新版完整源码", "Complete new source"),
                        evidence,
                    ));
                }
            }
            Action::Search { query, offset } => {
                page.title = crate::localize!("搜索 · {query}", "Search · {query}", query = query);
                page.intro = vec![crate::localize!("按名称/完整路径匹配符号（大小写不敏感），不是全文搜索。", "Match symbols by name/full path, case-insensitively; source content is not searched.").into()];
                let escaped = query
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_");
                let mut stmt = self.index.connection.prepare("SELECT data FROM nodes WHERE snapshot=?1 AND (name LIKE ?2 ESCAPE '\\' OR qualified LIKE ?2 ESCAPE '\\' OR path LIKE ?2 ESCAPE '\\') ORDER BY CASE WHEN name=?3 THEN 0 ELSE 1 END,qualified,path,start LIMIT ?4 OFFSET ?5")?;
                let rows = stmt
                    .query_map(
                        params![
                            self.snapshot.id,
                            format!("%{escaped}%"),
                            query,
                            PAGE_SIZE + 1,
                            offset
                        ],
                        |r| r.get::<_, String>(0),
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let more = rows.len() > PAGE_SIZE;
                for row in rows.into_iter().take(PAGE_SIZE) {
                    page.items.push(Item::node(
                        &serde_json::from_str::<Node>(&row)?,
                        &self.snapshot.id,
                    ));
                }
                if more {
                    page.items.push(Item::new(
                        crate::localize!("下一页 →", "Next page →"),
                        crate::localize!("继续搜索结果", "Continue search results"),
                        Action::Search {
                            query,
                            offset: offset + PAGE_SIZE,
                        },
                    ));
                }
                if page.items.is_empty() {
                    page.intro.push(
                        crate::localize!(
                            "没有匹配结果，按 / 换个关键词。",
                            "No matches; press / to try another term."
                        )
                        .into(),
                    );
                }
            }
            Action::ReviewMenu => {
                page.title =
                    crate::localize!("改动批次 · 选择范围", "Change batch · Select scope").into();
                page.intro = vec![
                    crate::localize!("只读比较 Git 和工作区，不修改代码、不切换分支。", "Compare Git and working-tree snapshots without modifying code or changing branches.").into(),
                    crate::localize!("结构比较快；语义影响分析更慢，需要本地语言服务。", "Syntax comparison is fast; semantic impact analysis takes longer and needs a local language service.").into(),
                ];
                for (label, base, head, semantic) in [
                    (
                        crate::localize!("当前未提交改动", "Current uncommitted changes"),
                        "HEAD",
                        None,
                        false,
                    ),
                    (
                        crate::localize!("最近一次提交", "Latest commit"),
                        "HEAD~1",
                        Some("HEAD"),
                        false,
                    ),
                    (
                        crate::localize!(
                            "未提交改动 + 潜在调用影响",
                            "Uncommitted changes + potential call impact"
                        ),
                        "HEAD",
                        None,
                        true,
                    ),
                    (
                        crate::localize!(
                            "最近提交 + 潜在调用影响",
                            "Latest commit + potential call impact"
                        ),
                        "HEAD~1",
                        Some("HEAD"),
                        true,
                    ),
                ] {
                    page.items.push(Item::new(
                        label,
                        format!(
                            "{base} → {} · {}",
                            head.unwrap_or(crate::localize!(
                                "工作区（含未跟踪源码）",
                                "Working tree (including untracked source)"
                            )),
                            if semantic {
                                crate::localize!(
                                    "语义分析 · 耗时较长",
                                    "Semantic analysis · may take longer"
                                )
                            } else {
                                crate::localize!("结构比较", "Syntax comparison")
                            }
                        ),
                        Action::Job(Request::Review {
                            base: base.into(),
                            head: head.map(str::to_owned),
                            semantic,
                        }),
                    ));
                }
            }
            Action::Changes(report) => {
                page.title =
                    crate::localize!("改动批次 · 摘要与核查", "Change batch · Summary and review")
                        .into();
                page.intro = vec![
                    format!(
                        "{} → {}",
                        s(&report.data["base_revision"]),
                        s(&report.data["head_revision"])
                    ),
                    crate::localize!("{} 个定义变化 · {} 个文件变化", "{} definition changes · {} file changes",
                        report.data["total_changes"], report.data["total_changed_files"]
                    ),
                    if report.analysis_context.analysis == "semantic" {
                        crate::localize!("潜在影响最多追踪两跳；不代表完整运行时影响。", "Potential impact follows at most two hops; runtime impact may be broader.")
                    } else {
                        crate::localize!("当前仅比较结构；未计算调用影响。返回可选“+ 潜在调用影响”。", "Syntax comparison only; call impact has not been computed. Go back to select + potential call impact.")
                    }
                    .into(),
                ];
                if report.completeness.status != "complete" {
                    page.intro
                        .push(crate::localize!("分析部分完成；缺失关系可能隐藏其他影响。", "Analysis partially complete; missing relations may hide further impact.").into());
                }
                let before = s(&report.data["base_snapshot"]).to_owned();
                let after = s(&report.data["head_snapshot"]).to_owned();
                for section in [
                    "groups",
                    "checklist",
                    "boundary_changes",
                    "knowledge_updates",
                    "verification",
                ] {
                    let count = array(&report.data["batch"][section]).len();
                    page.items.push(Item::new(
                        batch_section_title(section),
                        crate::localize!(
                            "{count} 项 · 全批次聚合",
                            "{count} items · Full batch aggregation",
                            count = count
                        ),
                        Action::BatchSection {
                            report: report.clone(),
                            section: section.into(),
                        },
                    ));
                }
                for change in array(&report.data["changes"]) {
                    let node: Node = serde_json::from_value(if change["after"].is_null() {
                        change["before"].clone()
                    } else {
                        change["after"].clone()
                    })?;
                    let mut item = Item::new(
                        format!(
                            "[{}] {}",
                            change_state(s(&change["state"])),
                            node.qualified_name
                        ),
                        format!(
                            "{}:{} · {}",
                            node.evidence.path,
                            node.evidence.start_line,
                            s(&change["priority"])
                        ),
                        Action::Change {
                            before: before.clone(),
                            after: after.clone(),
                            value: Box::new(change.clone()),
                        },
                    );
                    item.detail = change_reasons(change);
                    item.evidence = Some(node.evidence);
                    page.items.push(item);
                }
                for file in array(&report.data["files"]) {
                    let mut lines = s(&file["diff"])
                        .lines()
                        .map(str::to_owned)
                        .collect::<Vec<_>>();
                    if file["diff_truncated"] == true {
                        lines.push(crate::localize!("… 差异已截断（最多 160 行）；请查看两侧完整源码。", "… Diff truncated at 160 lines; inspect the complete old/new source.").into());
                    }
                    page.items.push(Item::new(
                        format!("diff · {}", s(&file["path"])),
                        change_state(s(&file["state"])),
                        Action::Text {
                            title: format!("diff · {}", s(&file["path"])),
                            lines,
                        },
                    ));
                }
                if let Some(cursor) = &report.completeness.next_cursor {
                    page.items.push(Item::new(
                        crate::localize!("下一页 →", "Next page →"),
                        crate::localize!(
                            "继续查看变更 / 文件差异",
                            "Continue changes / file diffs"
                        ),
                        Action::Job(Request::ReviewPage {
                            before,
                            after,
                            offset: cursor.parse()?,
                        }),
                    ));
                }
                if page.items.is_empty() {
                    page.intro.push(
                        crate::localize!(
                            "比较范围内没有已支持源码/清单的变更。",
                            "No supported source or manifest changes within the comparison scope."
                        )
                        .into(),
                    );
                }
            }
            Action::Change {
                before,
                after,
                value,
            } => {
                page.title = crate::localize!("变更详情", "Change details").into();
                page.intro = change_reasons(&value);
                for (label, side, snapshot) in [
                    (crate::localize!("变更前", "Before"), "before", &before),
                    (crate::localize!("变更后", "After"), "after", &after),
                ] {
                    if !value[side].is_null() {
                        let node: Node = serde_json::from_value(value[side].clone())?;
                        let mut item = Item::node(&node, snapshot);
                        item.label = format!("{label} · {}", node.qualified_name);
                        page.items.push(item);
                    }
                }
                for (field, label, snapshot) in [
                    (
                        "previous_impacts",
                        crate::localize!("之前的潜在调用方", "Previous potential caller"),
                        &before,
                    ),
                    (
                        "current_impacts",
                        crate::localize!("当前潜在调用方", "Current potential caller"),
                        &after,
                    ),
                ] {
                    for impact in array(&value[field]) {
                        let node: Node = serde_json::from_value(impact["caller"].clone())?;
                        let mut item = Item::node(&node, snapshot);
                        item.label = format!("{label} · {}", node.qualified_name);
                        for hop in array(&impact["path"]) {
                            item.detail.push(crate::localize!(
                                "影响路径证据：{}:{} · {}",
                                "Impact path evidence: {}:{} · {}",
                                s(&hop["evidence"]["path"]),
                                hop["evidence"]["start_line"],
                                resolution(s(&hop["resolution"]))
                            ));
                        }
                        page.items.push(item);
                    }
                }
                page.intro
                    .push(crate::localize!("未列出调用方不表示无影响；结构比较不会计算调用影响。", "An empty caller list does not prove no impact; syntax comparison does not compute call impact.").into());
                if value["impacts_truncated"] == true {
                    page.intro.push(
                        crate::localize!(
                            "潜在影响列表已截断，最多两跳 / 30 个调用方。",
                            "Potential impact truncated at two hops / 30 callers."
                        )
                        .into(),
                    );
                }
            }
            Action::Text { title, lines } => {
                page.title = title;
                page.intro = lines;
            }
            Action::Status => {
                page.title = crate::localize!("分析范围与状态", "Analysis scope and status").into();
                page.intro = vec![
                    crate::localize!("结构快照：{} · {}", "Syntax snapshot: {} · {}",
                        self.snapshot.context.language, self.snapshot.completeness.status
                    ),
                    crate::localize!("源码文件 {} · 行数 {} · 定义 {}", "Source files {} · Lines {} · Definitions {}",
                        self.snapshot.stats.source_files,
                        self.snapshot.stats.source_lines,
                        self.snapshot.stats.nodes
                    ),
                    crate::localize!("基础调用表达式尚未解析；在函数页可按模块补充语义。", "Basic call expressions are unresolved; add semantic analysis per module from a function page.").into(),
                    crate::localize!("不展开宏生成代码，不证明完整业务流程、数据流或测试覆盖。", "Generated macro code is not expanded; complete business paths, data flow and test coverage are not established.").into(),
                    crate::localize!("接口/虚派发只保留可证明的定义，不猜测唯一运行时实现。", "Interface/virtual dispatch retains established definitions without assuming a unique runtime implementation.").into(),
                    crate::localize!("源码是固定快照；文件变化后请退出并重新打开。", "Source is pinned to a snapshot; reopen after changing files.").into(),
                    String::new(),
                ];
                for snapshot in std::iter::once(&self.snapshot).chain(self.semantics.values()) {
                    page.intro.push(format!(
                        "{} · {} · {}",
                        snapshot.context.analysis,
                        snapshot
                            .context
                            .scope
                            .as_deref()
                            .unwrap_or(crate::localize!("工作区", "Workspace")),
                        snapshot.completeness.status
                    ));
                    for diagnostic in &snapshot.diagnostics {
                        page.intro.push(format!(
                            "{} · {} {}",
                            diagnostic.code,
                            diagnostic.message,
                            diagnostic.path.as_deref().unwrap_or("")
                        ));
                    }
                }
            }
            Action::Marks { snapshot, id } => {
                page.title = crate::localize!("阅读状态", "Reading status").into();
                page.node = Some((snapshot.clone(), id.clone()));
                page.intro =
                    vec![crate::localize!("记录绑定源码和直接关系；证据变化后会变成“需复核”。n 可编辑笔记。", "Records bind source and direct relations; changed evidence requires review. n edits notes.").into()];
                for state in ["seen", "question", "unread"] {
                    page.items.push(Item::new(
                        state_label(state),
                        crate::localize!("保留已有笔记", "Keep existing notes"),
                        Action::Mark {
                            snapshot: snapshot.clone(),
                            id: id.clone(),
                            state: state.into(),
                        },
                    ));
                }
            }
            Action::Job(_) | Action::Mark { .. } => bail!(crate::localize!(
                "动作不能作为页面展示",
                "This action cannot be displayed as a page"
            )),
        }
        Ok(page)
    }

    pub fn preview(&mut self) -> Result<()> {
        self.detail = self.page.intro.clone();
        if let Some(item) = self.page.items.get(self.page.selected) {
            self.detail.extend([
                String::new(),
                item.label.clone(),
                item.hint.clone(),
                String::new(),
            ]);
            self.detail.extend(item.detail.clone());
            if let Some(evidence) = &item.evidence {
                self.detail.extend([
                    String::new(),
                    crate::localize!(
                        "源码证据 · {}:{}",
                        "Source evidence · {}:{}",
                        evidence.path,
                        evidence.start_line
                    ),
                ]);
                self.detail.extend(self.source_lines(evidence, false)?);
            }
        }
        Ok(())
    }

    fn source_lines(&self, evidence: &Evidence, full: bool) -> Result<Vec<String>> {
        let content = self.index.content(&evidence.content_hash)?;
        let start = if full {
            0
        } else {
            evidence.start_line.saturating_sub(4)
        };
        Ok(content
            .lines()
            .enumerate()
            .skip(start)
            .take(if full { usize::MAX } else { 28 })
            .map(|(i, line)| {
                format!(
                    "{} {:>5}  {}",
                    if i + 1 == evidence.start_line {
                        "▶"
                    } else {
                        " "
                    },
                    i + 1,
                    line.replace('\t', "    ")
                )
            })
            .collect())
    }
}

fn s(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}
fn array(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn evidence_values(value: &Value) -> Vec<Evidence> {
    let mut result = if value["evidence"].is_array() {
        array(&value["evidence"])
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect::<Vec<_>>()
    } else {
        serde_json::from_value::<Evidence>(value["evidence"].clone())
            .ok()
            .into_iter()
            .collect()
    };
    let mut seen = BTreeSet::new();
    result.retain(|e| {
        seen.insert((
            e.path.clone(),
            e.content_hash.clone(),
            e.start_byte,
            e.end_byte,
        ))
    });
    result
}
fn paths_related(a: &str, b: &str) -> bool {
    a == b
        || a.strip_prefix(b).is_some_and(|tail| tail.starts_with('/'))
        || b.strip_prefix(a).is_some_and(|tail| tail.starts_with('/'))
}
fn knowledge_state(value: &str) -> &str {
    match value {
        "confirmed" => crate::localize!("已确认", "Confirmed"),
        "question" => crate::localize!("有疑问", "Question"),
        "needs_review" => crate::localize!("需复核", "Needs review"),
        _ => value,
    }
}
fn batch_section_title(value: &str) -> &str {
    match value {
        "groups" => crate::localize!("功能与子系统分组", "Functional and subsystem groups"),
        "checklist" => crate::localize!("重点核查清单", "Review checklist"),
        "boundary_changes" => crate::localize!("架构边界变化", "Architecture boundary changes"),
        "knowledge_updates" => crate::localize!("需要更新的认知", "Knowledge requiring updates"),
        "verification" => crate::localize!("相关验证证据", "Related verification evidence"),
        _ => value,
    }
}
fn value_lines(value: &Value) -> Vec<String> {
    let mut lines = vec![s(&value["summary"]).into()];
    if let Some(basis) = value["basis"].as_str().filter(|v| !v.is_empty()) {
        lines.push(crate::localize!(
            "来源：{basis}",
            "Source: {basis}",
            basis = basis
        ));
    }
    if let Some(state) = value["state"].as_str() {
        lines.push(crate::localize!(
            "状态：{}",
            "Status: {}",
            knowledge_state(state)
        ));
    }
    if let Some(priority) = value["priority"].as_str() {
        lines.push(crate::localize!(
            "核查优先级：{priority}",
            "Review priority: {priority}",
            priority = priority
        ));
    }
    if let Some(return_type) = value["return_type"].as_str() {
        lines.push(crate::localize!(
            "返回声明：{return_type}",
            "Return declaration: {return_type}",
            return_type = return_type
        ));
    }
    if let Some(parameters) = value["parameters"].as_str() {
        lines.push(crate::localize!(
            "输入声明：{parameters}",
            "Parameter declarations: {parameters}",
            parameters = parameters
        ));
    }
    for member in array(&value["members"]) {
        lines.push(format!(
            "{} · {}{}",
            kind(s(&member["kind"])),
            s(&member["name"]),
            member["declared_type"]
                .as_str()
                .map(|v| format!(" : {v}"))
                .unwrap_or_default()
        ));
    }
    for usage in array(&value["type_usages"]) {
        lines.push(crate::localize!(
            "类型使用：{} · {}",
            "Type usage: {} · {}",
            s(&usage["target_name"]),
            resolution(s(&usage["resolution"]))
        ));
    }
    for (field, label) in [
        (
            "unknown_breakpoints",
            crate::localize!("未知断点", "Unknown breakpoint"),
        ),
        (
            "external_boundaries",
            crate::localize!("外部边界", "External boundary"),
        ),
    ] {
        for boundary in array(&value[field]) {
            lines.push(format!(
                "{label}：{} · {}",
                s(&boundary["target_name"]),
                resolution(s(&boundary["resolution"]))
            ));
        }
    }
    for path in array(&value["changed_evidence"])
        .iter()
        .filter_map(Value::as_str)
    {
        lines.push(crate::localize!(
            "需复核证据：{path}",
            "Changed evidence: {path}",
            path = path
        ));
    }
    for field in [
        "truncated",
        "node_ids_truncated",
        "paths_truncated",
        "members_truncated",
        "evidence_truncated",
        "type_usages_truncated",
    ] {
        if value[field] == true {
            lines.push(
                crate::localize!(
                    "部分关联已截断；可进入定义或文件继续阅读。",
                    "Some associations are truncated; continue through definitions or files."
                )
                .into(),
            );
            break;
        }
    }
    lines
}
fn conditions(values: &[String]) -> String {
    if values.is_empty() {
        String::new()
    } else {
        crate::localize!(
            "源码条件（非已执行路径）：{}",
            "Source conditions (not executed paths): {}",
            values.join(" / ")
        )
    }
}
fn kind(value: &str) -> &str {
    match value {
        "function" => crate::localize!("函数", "Function"),
        "type" => crate::localize!("类型", "Type"),
        "field" => crate::localize!("字段", "Field"),
        "variant" => crate::localize!("枚举变体", "Enum variant"),
        "type_usage" => crate::localize!("声明类型使用", "Declared type usage"),
        "trait" => crate::localize!("接口", "Interface"),
        "impl" => crate::localize!("实现块", "Implementation"),
        "module" => crate::localize!("模块", "Module"),
        "calls" | "call" => crate::localize!("调用", "Call"),
        "bin" => crate::localize!("程序入口", "Program entry"),
        "lib" => crate::localize!("库入口", "Library entry"),
        "example" => crate::localize!("示例", "Example"),
        "build" => crate::localize!("构建脚本", "Build script"),
        "test" => crate::localize!("测试", "Test"),
        other => other,
    }
}
fn resolution(value: &str) -> &str {
    match value {
        "resolved" | "exact" => crate::localize!("已解析", "Resolved"),
        "unresolved" => crate::localize!("未解析", "Unresolved"),
        "interface" => crate::localize!(
            "接口（非唯一实现）",
            "Interface (non-unique implementation)"
        ),
        "virtual" => crate::localize!("虚派发", "Virtual dispatch"),
        "ambiguous" => crate::localize!("存在歧义", "Ambiguous"),
        "navigation_only" => crate::localize!("仅可导航", "Navigation only"),
        "external" => crate::localize!("外部目标", "External target"),
        "syntactic" | "syntax" => crate::localize!("语法关系", "Syntax relation"),
        other => other,
    }
}
pub(super) fn state_label(value: &str) -> &str {
    match value {
        "seen" => crate::localize!("已读", "Read"),
        "question" => crate::localize!("有疑问", "Question"),
        "unread" => crate::localize!("未读", "Unread"),
        "needs_review" => crate::localize!("需复核", "Needs review"),
        other => other,
    }
}
fn change_state(value: &str) -> &str {
    match value {
        "added" => crate::localize!("新增", "Added"),
        "removed" => crate::localize!("删除", "Removed"),
        "modified" => crate::localize!("修改", "Modified"),
        other => other,
    }
}
fn change_reasons(value: &Value) -> Vec<String> {
    array(&value["reasons"])
        .iter()
        .map(|r| {
            match s(r) {
                "definition or signature changed" => {
                    crate::localize!("定义或签名改变", "Definition or signature changed")
                }
                "public interface requires review" => crate::localize!(
                    "公开接口改变，优先审阅",
                    "Public interface changed; prioritize review"
                ),
                "implementation changed" => crate::localize!("实现改变", "Implementation changed"),
                "conditional compilation changed" => {
                    crate::localize!("条件编译改变", "Conditional compilation changed")
                }
                "definition removed; inspect previous callers" => crate::localize!(
                    "定义已删除，检查之前的调用方",
                    "Definition removed; inspect previous callers"
                ),
                "definition added" => crate::localize!("新增定义", "Definition added"),
                other => other,
            }
            .into()
        })
        .collect()
}
