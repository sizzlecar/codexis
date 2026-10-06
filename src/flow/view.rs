//! Terminal-width text layout shared by the interactive home screen and the
//! plain report. Every line that refers to source carries its evidence.
use super::{short_path, Body, Exit, FlowMap, ReadMode, Step, StepKind};
use crate::model::{Evidence, Snapshot};
use std::collections::BTreeSet;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Title,
    Heading,
    Normal,
    Dim,
    Exit,
    Accent,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub text: String,
    pub tone: Tone,
    pub evidence: Option<Evidence>,
    /// Path of an expandable step from its trunk's root, such as `0/3/1`.
    pub key: Option<String>,
    /// Another trunk this row opens.
    pub link: Option<String>,
}

impl Row {
    fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            text: text.into(),
            tone,
            evidence: None,
            key: None,
            link: None,
        }
    }
    fn at(text: impl Into<String>, tone: Tone, evidence: &Evidence) -> Self {
        Self {
            text: text.into(),
            tone,
            evidence: Some(evidence.clone()),
            key: None,
            link: None,
        }
    }
}

/// Open steps of one trunk, by path from its root.
pub type Expanded = BTreeSet<String>;

/// Rows the trunk section may fill before the default expansion stops in
/// plain output; the terminal uses its own height.
pub const FIRST_SCREEN_ROWS: usize = 30;

fn expandable<'a>(map: &'a FlowMap, step: &Step, stack: &[&str]) -> Option<&'a Body> {
    let target = step.target.as_deref()?;
    if step.inner == 0 || stack.contains(&target) {
        return None;
    }
    map.bodies.get(target)
}

fn step_rows(step: &Step, open: bool, expandable: bool) -> usize {
    if expandable && !open {
        return 1 + step.exits.len().saturating_sub(1).min(3);
    }
    let exits = step.exits.len() + step.inner_exits.len();
    1 + exits.saturating_sub(1).min(3) + usize::from(exits > 4)
}

/// A body whose only step is a call: it just hands over and is opened.
fn thin(body: &Body) -> bool {
    body.steps.len() == 1 && body.steps[0].kind == StepKind::Call && body.steps[0].inner > 0
}

/// Default expansion: pass through thin wrappers, then open whole levels
/// while they fit the first screen. Siblings are always opened together.
pub fn initial(map: &FlowMap, root: &str, budget: usize) -> Expanded {
    let mut expanded = Expanded::new();
    let Some(mut body) = map.bodies.get(root) else {
        return expanded;
    };
    let mut prefix = String::new();
    let mut stack = vec![root];
    while thin(body) {
        let Some(next) = expandable(map, &body.steps[0], &stack) else {
            break;
        };
        expanded.insert(format!("{prefix}0"));
        prefix.push_str("0/");
        stack.push(body.steps[0].target.as_deref().unwrap_or_default());
        body = next;
    }
    let mut rows: usize = body
        .steps
        .iter()
        .map(|s| step_rows(s, false, s.inner > 0))
        .sum();
    let mut frontier: Vec<(String, &Body, Vec<&str>)> = vec![(prefix, body, stack)];
    loop {
        let mut next = Vec::new();
        let mut added = 0;
        let mut keys = Vec::new();
        for (prefix, body, stack) in &frontier {
            for (index, step) in body.steps.iter().enumerate() {
                if let Some(child) = expandable(map, step, stack) {
                    let key = format!("{prefix}{index}");
                    added += child
                        .steps
                        .iter()
                        .map(|s| step_rows(s, false, s.inner > 0))
                        .sum::<usize>();
                    let mut child_stack = stack.clone();
                    child_stack.push(step.target.as_deref().unwrap_or_default());
                    next.push((format!("{key}/"), child, child_stack));
                    keys.push(key);
                }
            }
        }
        if keys.is_empty() || rows + added > budget {
            return expanded;
        }
        rows += added;
        expanded.extend(keys);
        frontier = next;
    }
}

/// Every expandable step down to `depth` levels below the root.
pub fn to_depth(map: &FlowMap, root: &str, depth: usize) -> Expanded {
    fn walk(
        map: &FlowMap,
        id: &str,
        prefix: &str,
        left: usize,
        stack: &mut Vec<String>,
        out: &mut Expanded,
    ) {
        let Some(body) = map.bodies.get(id) else {
            return;
        };
        if left == 0 {
            return;
        }
        for (index, step) in body.steps.iter().enumerate() {
            let refs: Vec<&str> = stack.iter().map(String::as_str).collect();
            if expandable(map, step, &refs).is_some() {
                let key = format!("{prefix}{index}");
                let target = step.target.clone().unwrap_or_default();
                out.insert(key.clone());
                stack.push(target.clone());
                walk(map, &target, &format!("{key}/"), left - 1, stack, out);
                stack.pop();
            }
        }
    }
    let mut out = initial(map, root, 0);
    walk(map, root, "", depth, &mut vec![root.to_owned()], &mut out);
    out
}

fn cells(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn fit(text: &str, width: usize) -> String {
    if cells(text) <= width {
        return text.to_owned();
    }
    let mut out = String::new();
    for c in text.chars() {
        if cells(&out) + cells(c.encode_utf8(&mut [0; 4])) + 1 > width {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

fn pad(text: &str, width: usize) -> String {
    let text = fit(text, width);
    let fill = width.saturating_sub(cells(&text));
    format!("{text}{}", " ".repeat(fill))
}

fn location(evidence: &Evidence) -> String {
    format!("{}:{}", short_path(&evidence.path), evidence.start_line)
}

/// Project identity shown above the map; serializable so reports can render it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Header {
    pub name: String,
    pub revision: String,
    pub language: String,
    pub packages: usize,
    pub files: usize,
    pub lines: usize,
    /// The project's own documented purpose, verbatim (README or manifest).
    #[serde(default)]
    pub description: Option<String>,
    /// Files whose syntax could not be parsed; the map covers the rest.
    #[serde(default)]
    pub failed: Vec<String>,
    #[serde(default)]
    pub stale: bool,
    /// Library roots, the reading start when there is no entry or main.
    #[serde(default)]
    pub libraries: Vec<String>,
}

impl Header {
    pub fn new(snapshot: &Snapshot) -> Self {
        Self {
            name: std::path::Path::new(&snapshot.project_root)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            revision: revision(snapshot),
            language: snapshot.context.language.clone(),
            packages: snapshot.project.packages.len(),
            files: snapshot.stats.source_files,
            lines: snapshot.stats.source_lines,
            description: None,
            failed: snapshot
                .diagnostics
                .iter()
                .filter(|d| d.code.ends_with("_parse_error") || d.code == "syntax_depth_limit")
                .filter_map(|d| d.path.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            stale: snapshot.completeness.stale,
            libraries: snapshot
                .project
                .packages
                .iter()
                .flat_map(|p| p.units.iter())
                .filter(|u| u.kind == "lib")
                .map(|u| u.source.clone())
                .collect(),
        }
    }

    /// Use the documented purpose from a baseline when it is a declaration.
    pub fn described(mut self, baseline: &serde_json::Value) -> Self {
        let purpose = &baseline["purpose"];
        if purpose["status"] == "declared" {
            self.description = purpose["text"]
                .as_str()
                .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
                .filter(|t| !t.is_empty());
        }
        self
    }
}

pub fn revision(snapshot: &Snapshot) -> String {
    let raw = snapshot.source_revision.trim_start_matches("worktree:");
    if raw.len() >= 7 && raw.chars().take(7).all(|c| c.is_ascii_hexdigit()) {
        raw[..7].to_owned()
    } else if raw == "directory" {
        crate::localize!("工作目录", "directory").into()
    } else {
        raw.chars().take(12).collect()
    }
}

pub fn external_label(category: &str) -> &'static str {
    match category {
        "mysql" => "MySQL",
        "postgres" => "PostgreSQL",
        "sqlite" => "SQLite",
        "sql" => crate::localize!("SQL 数据库", "SQL database"),
        "redis" => "Redis",
        "mongodb" => "MongoDB",
        "elasticsearch" => "Elasticsearch",
        "kafka" => "Kafka",
        "rabbitmq" => "RabbitMQ",
        "nats" => "NATS",
        "mq" => crate::localize!("消息队列", "Message queue"),
        "nacos" => "Nacos",
        "etcd" => "etcd",
        "consul" => "Consul",
        "object_storage" => crate::localize!("对象存储", "Object storage"),
        "grpc" => "gRPC",
        "websocket" => "WebSocket",
        "http_client" => crate::localize!("HTTP 客户端", "HTTP client"),
        "model_api" => crate::localize!("模型服务", "Model API"),
        "metrics" => crate::localize!("指标", "Metrics"),
        "task_queue" => crate::localize!("任务队列", "Task queue"),
        _ => "",
    }
}

pub fn read_mode(mode: ReadMode) -> &'static str {
    match mode {
        ReadMode::Current => crate::localize!("读取时取当前值", "current value at use"),
        ReadMode::Passed => crate::localize!("使用调用方传入的值", "value passed by caller"),
        ReadMode::Held => crate::localize!("使用持有的值", "held value"),
    }
}

fn exit_text(status: &str, code: Option<&str>) -> String {
    match code {
        Some(code) => format!("→ {status} {code}"),
        None => format!("→ {status}"),
    }
}

/// One read is shown in full; several are grouped by their top-level section.
fn summarize_reads(reads: &[String]) -> Option<String> {
    let first = reads.first()?;
    if reads.len() == 1 {
        return Some(first.clone());
    }
    let mut sections: Vec<String> = Vec::new();
    for read in reads {
        let mut parts = read.split('.');
        let mut section = parts.next().unwrap_or("").to_owned();
        if parts.next() == Some("*") {
            section.push_str(".*");
        }
        if !sections.contains(&section) {
            sections.push(section);
        }
    }
    Some(if sections.len() <= 2 {
        sections.join(", ")
    } else {
        format!("{}, {} +{}", sections[0], sections[1], sections.len() - 2)
    })
}

/// The home screen: entries, the trunk in source order with exits and
/// configuration reads, shared state and external systems.
pub fn home(header: &Header, map: &FlowMap, width: usize, expanded: &Expanded) -> Vec<Row> {
    let width = width.max(60);
    let mut rows = Vec::new();
    let left = crate::localize!(
        "{}  {}  {} · {} 个包 · {} 个文件 · {} 行",
        "{}  {}  {} · {} packages · {} files · {} lines",
        header.name,
        header.revision,
        language(&header.language),
        header.packages,
        header.files,
        header.lines,
    );
    let right = crate::localize!("静态分析 · 不调用模型", "Static analysis · no model");
    let gap = width.saturating_sub(cells(&left) + cells(right)).max(2);
    rows.push(Row::new(
        format!("{left}{}{right}", " ".repeat(gap)),
        Tone::Title,
    ));
    if let Some(description) = &header.description {
        rows.push(Row::new(fit(description, width), Tone::Dim));
    }
    if header.stale {
        rows.push(Row::new(
            crate::localize!(
                "源码已变化：以下内容属于已保存的快照，重新运行 analyze 更新。",
                "Source changed: this map belongs to the stored snapshot; rerun analyze to update."
            ),
            Tone::Exit,
        ));
    }
    if !header.failed.is_empty() {
        let shown: Vec<_> = header.failed.iter().take(3).map(String::as_str).collect();
        rows.push(Row::new(
            fit(
                &crate::localize!(
                    "分析不完整：{} 个文件无法解析（{}），脉络图只覆盖其余源码。",
                    "Partial analysis: {} files could not be parsed ({}); the map covers the rest.",
                    header.failed.len(),
                    shown.join(", ")
                ),
                width,
            ),
            Tone::Exit,
        ));
    }
    rows.push(Row::new("═".repeat(width), Tone::Dim));
    entries(header, map, width, &mut rows);
    rows.push(Row::new("", Tone::Normal));
    if let Some(trunk) = &map.trunk {
        let mut title = if trunk.routes > 0 {
            crate::localize!(
                "主干  {} · {} 个入口",
                "Trunk {} · {} entries",
                trunk.label,
                trunk.routes
            )
        } else {
            crate::localize!("主干  {}", "Trunk {}", trunk.label)
        };
        // When entries do not share a handler, say how this one was chosen.
        if trunk.routes <= 1 && map.trunks.len() >= 3 {
            title.push_str(&crate::localize!(
                "（{} 个处理函数中可达函数最多的；r 看其他）",
                " (most reaching of {} handlers; r for others)",
                map.trunks.len() + 1
            ));
        }
        tree(
            map,
            &trunk.id,
            &title,
            &trunk.evidence,
            width,
            expanded,
            &mut rows,
        );
    } else {
        rows.push(Row::new(
            crate::localize!(
                "主干  未能确定主干：没有路由处理函数或程序入口可以展开。",
                "Trunk No trunk: no route handler or program entry could be expanded."
            ),
            Tone::Dim,
        ));
    }
    rows.push(Row::new("", Tone::Normal));
    footer(map, width, &mut rows);
    rows
}

/// One trunk on its own page, opened from the entry list.
pub fn trunk_page(map: &FlowMap, root: &str, width: usize, expanded: &Expanded) -> Vec<Row> {
    let width = width.max(60);
    let mut rows = Vec::new();
    let Some(body) = map.bodies.get(root) else {
        rows.push(Row::new(
            crate::localize!(
                "这个入口没有可展开的步骤。",
                "This entry has no expandable steps."
            ),
            Tone::Dim,
        ));
        return rows;
    };
    let routes: Vec<String> = map
        .routes
        .iter()
        .filter(|r| r.handler_id.as_deref() == Some(root))
        .map(|r| format!("{} {}", r.method, r.path))
        .collect();
    let title = if routes.is_empty() {
        crate::localize!("主干  {}", "Trunk {}", body.label)
    } else {
        crate::localize!(
            "主干  {} · {}",
            "Trunk {} · {}",
            body.label,
            routes.join("  ")
        )
    };
    tree(
        map,
        root,
        &title,
        &body.evidence,
        width,
        expanded,
        &mut rows,
    );
    rows
}

fn exit_rows(exits: &[&Exit]) -> Vec<String> {
    exits
        .iter()
        .map(|e| exit_text(&e.status, e.code.as_deref()))
        .collect()
}

fn tree(
    map: &FlowMap,
    root: &str,
    title: &str,
    evidence: &Evidence,
    width: usize,
    expanded: &Expanded,
    rows: &mut Vec<Row>,
) {
    let layout = Layout::new(width);
    let mut heading = pad(title, layout.exit_column);
    if layout.exits_width > 0 {
        heading.push_str(crate::localize!("可能的提前结束", "Early exits"));
    }
    rows.push(Row::at(heading, Tone::Heading, evidence));
    let Some(body) = map.bodies.get(root) else {
        return;
    };
    let exits: Vec<&Exit> = body.exits.iter().collect();
    layout.push(
        rows,
        format!("  ↳ {}", body.label),
        &body.evidence,
        &exit_rows(&exits),
        Tone::Accent,
        None,
        "    ",
    );
    let mut number = 0;
    render_body(
        map,
        root,
        "",
        "    ",
        expanded,
        &layout,
        &mut vec![root.to_owned()],
        &mut number,
        rows,
    );
}

struct Layout {
    exits_width: usize,
    exit_column: usize,
    label_width: usize,
}

impl Layout {
    fn new(width: usize) -> Self {
        let exits_width = if width >= 100 { 34 } else { 0 };
        let exit_column = width.saturating_sub(exits_width);
        Self {
            exits_width,
            exit_column,
            label_width: exit_column.saturating_sub(22).max(24),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &self,
        rows: &mut Vec<Row>,
        left: String,
        evidence: &Evidence,
        exits: &[String],
        tone: Tone,
        key: Option<String>,
        continuation: &str,
    ) {
        let mut line = pad(&left, self.label_width);
        line.push_str("  ");
        line.push_str(&pad(&location(evidence), 20));
        let line = pad(&line, self.exit_column);
        let text = match (exits.first(), self.exits_width) {
            (Some(exit), w) if w > 0 => format!("{line}{}", fit(exit, w)),
            _ => line.trim_end().to_owned(),
        };
        rows.push(Row {
            text,
            tone,
            evidence: Some(evidence.clone()),
            key,
            link: None,
        });
        let rest = if self.exits_width > 0 {
            &exits[exits.len().min(1)..]
        } else {
            exits
        };
        for exit in rest.iter().take(3) {
            let text = if self.exits_width > 0 {
                format!(
                    "{}{}",
                    pad(continuation, self.exit_column),
                    fit(exit, self.exits_width)
                )
            } else {
                format!("{continuation}     {exit}")
            };
            rows.push(Row::new(text, Tone::Exit));
        }
        if rest.len() > 3 {
            rows.push(Row::new(
                format!(
                    "{}{}",
                    pad(continuation, self.exit_column),
                    crate::localize!(
                        "  另 {} 个，按 e 查看",
                        "  {} more, press e",
                        rest.len() - 3
                    )
                ),
                Tone::Dim,
            ));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_body(
    map: &FlowMap,
    id: &str,
    prefix: &str,
    lead: &str,
    expanded: &Expanded,
    layout: &Layout,
    stack: &mut Vec<String>,
    number: &mut usize,
    rows: &mut Vec<Row>,
) {
    let Some(body) = map.bodies.get(id) else {
        return;
    };
    let mut in_loop = false;
    for (index, step) in body.steps.iter().enumerate() {
        let key = format!("{prefix}{index}");
        if step.kind == StepKind::Loop {
            in_loop = true;
            layout.push(
                rows,
                format!("{lead}⟳ {}", step.label),
                &step.evidence,
                &[],
                Tone::Heading,
                None,
                lead,
            );
            continue;
        }
        if !step.in_loop {
            in_loop = false;
        }
        let bar = if in_loop { "│ " } else { "" };
        let refs: Vec<&str> = stack.iter().map(String::as_str).collect();
        let child = expandable(map, step, &refs);
        let open = child.is_some() && expanded.contains(&key);
        let marker = match (child.is_some(), open) {
            (true, false) => "+ ",
            (true, true) => "- ",
            _ => "  ",
        };
        let numbered = if step.kind == StepKind::Guard {
            "   ".to_owned()
        } else {
            *number += 1;
            format!("{:>2} ", *number)
        };
        let mut label = step.label.clone();
        if let Some(arm) = &step.arm {
            label.push_str(&format!(" [{arm}]"));
        }
        match step.kind {
            StepKind::Dispatch => label.push_str(&crate::localize!(
                "  {} 个实现",
                "  {} impls",
                step.candidates
            )),
            StepKind::Unresolved => {
                label.push_str(crate::localize!("  目标未确定", "  target unknown"))
            }
            _ => {}
        }
        if child.is_some() && !open {
            label.push_str(&crate::localize!(" · {} 步", " · {} steps", step.inner));
        }
        let mut left = format!("{lead}{bar}{numbered}{marker}{label}");
        if let Some(reads) = summarize_reads(&step.reads) {
            let config = format!("  {} {reads}", crate::localize!("[配置]", "[cfg]"));
            if cells(&left) + cells(&config) <= layout.label_width {
                left.push_str(&config);
            }
        }
        let mut exits = exit_rows(&step.exits.iter().collect::<Vec<_>>());
        if !open && !step.inner_exits.is_empty() {
            if child.is_some() {
                // A collapsed step summarizes what lies inside on one line.
                let first = &step.inner_exits[0];
                let total = step.inner_exits.len();
                let mut line = exit_text(&first.status, first.code.as_deref());
                if total > 1 {
                    line.push_str(&crate::localize!(" 等 {} 个", " +{} more", total - 1));
                }
                exits.push(line);
            } else {
                exits.extend(exit_rows(&step.inner_exits.iter().collect::<Vec<_>>()));
            }
        }
        let tone = match step.kind {
            StepKind::Guard => Tone::Dim,
            _ => Tone::Normal,
        };
        let continuation = format!("{lead}{bar}");
        layout.push(
            rows,
            left,
            &step.evidence,
            &exits,
            tone,
            child.is_some().then(|| key.clone()),
            &continuation,
        );
        if open {
            let target = step.target.clone().unwrap_or_default();
            stack.push(target.clone());
            render_body(
                map,
                &target,
                &format!("{key}/"),
                &format!("{lead}{bar}   "),
                expanded,
                layout,
                stack,
                number,
                rows,
            );
            stack.pop();
        }
    }
}

fn language(language: &str) -> &str {
    match language {
        "rust" => "Rust",
        "python" => "Python",
        other => other,
    }
}

fn entries(header: &Header, map: &FlowMap, width: usize, rows: &mut Vec<Row>) {
    let head = crate::localize!("入口  ", "Entry ");
    let indent = " ".repeat(cells(head));
    let public: Vec<_> = map.routes.iter().filter(|r| !r.internal).collect();
    let internal: Vec<_> = map.routes.iter().filter(|r| r.internal).collect();
    if public.is_empty() && internal.is_empty() {
        let text = match &map.trunk {
            Some(trunk) => crate::localize!(
                "{head}未发现路由注册；从程序入口 {} 开始",
                "{head}No route registrations found; starting at {}",
                trunk.label
            ),
            None if !header.libraries.is_empty() => crate::localize!(
                "{head}未发现路由注册或程序入口；库入口 {}",
                "{head}No route registrations or program entries; library root {}",
                header.libraries.join("  ")
            ),
            None => crate::localize!(
                "{head}未发现路由注册或程序入口",
                "{head}No route registrations or program entries found",
            ),
        };
        let evidence = map.trunk.as_ref().map(|t| t.evidence.clone());
        rows.push(Row {
            text,
            tone: Tone::Normal,
            evidence,
            key: None,
            link: None,
        });
        return;
    }
    let mut groups: Vec<(String, Vec<&super::Route>)> = Vec::new();
    for route in &public {
        let method = if route.method.is_empty() {
            "ANY".to_owned()
        } else {
            route.method.clone()
        };
        match groups.iter_mut().find(|(m, _)| *m == method) {
            Some((_, routes)) => routes.push(route),
            None => groups.push((method, vec![route])),
        }
    }
    let method_width = groups.iter().map(|(m, _)| m.len()).max().unwrap_or(4) + 1;
    let mut lines: Vec<Row> = Vec::new();
    for (method, routes) in &groups {
        wrap_paths(
            &format!("{method:<method_width$}"),
            routes,
            width - cells(head),
            &mut lines,
        );
    }
    if !internal.is_empty() {
        let mut paths: Vec<&super::Route> = Vec::new();
        for route in &internal {
            if !paths.iter().any(|p| p.path == route.path) {
                paths.push(route);
            }
        }
        wrap_paths(
            &pad(crate::localize!("内部", "int."), method_width),
            &paths,
            width - cells(head),
            &mut lines,
        );
    }
    if lines.len() <= 4 {
        for (i, mut line) in lines.into_iter().enumerate() {
            line.text = format!(
                "{}{}",
                if i == 0 {
                    head.to_owned()
                } else {
                    indent.clone()
                },
                line.text
            );
            rows.push(line);
        }
        return;
    }
    // Many routes: count by method and group by the leading path segments.
    let counts: Vec<String> = groups
        .iter()
        .map(|(m, r)| format!("{m} {}", r.len()))
        .collect();
    rows.push(Row::new(
        format!(
            "{head}{}  {}",
            crate::localize!("{} 个", "{}", public.len()),
            counts.join(" · ")
        ),
        Tone::Normal,
    ));
    let mut prefixes: Vec<(String, usize)> = Vec::new();
    for route in &public {
        let segments: Vec<&str> = route.path.trim_start_matches('/').split('/').collect();
        let depth = if segments
            .first()
            .is_some_and(|s| s.starts_with('v') && s[1..].chars().all(|c| c.is_ascii_digit()))
        {
            2
        } else {
            1
        };
        let prefix = format!(
            "/{}",
            segments
                .iter()
                .take(depth)
                .copied()
                .collect::<Vec<_>>()
                .join("/")
        );
        match prefixes.iter_mut().find(|(p, _)| *p == prefix) {
            Some((_, count)) => *count += 1,
            None => prefixes.push((prefix, 1)),
        }
    }
    prefixes.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut line = String::new();
    for (prefix, count) in &prefixes {
        let piece = format!("{prefix}/… {count}  ");
        if !line.is_empty() && cells(&indent) + cells(&line) + cells(&piece) > width {
            rows.push(Row::new(
                format!("{indent}{}", line.trim_end()),
                Tone::Normal,
            ));
            line.clear();
        }
        line.push_str(&piece);
    }
    if !line.is_empty() {
        rows.push(Row::new(
            format!("{indent}{}", line.trim_end()),
            Tone::Normal,
        ));
    }
    if !internal.is_empty() {
        let paths: BTreeSet<&str> = internal.iter().map(|r| r.path.as_str()).collect();
        rows.push(Row::new(
            fit(
                &format!(
                    "{indent}{}  {}",
                    crate::localize!("内部", "int."),
                    paths.into_iter().collect::<Vec<_>>().join("  ")
                ),
                width,
            ),
            Tone::Normal,
        ));
    }
    rows.push(Row::new(
        crate::localize!(
            "{indent}完整入口清单按 r 查看",
            "{indent}Press r for every entry",
        ),
        Tone::Dim,
    ));
}

fn wrap_paths(prefix: &str, routes: &[&super::Route], width: usize, rows: &mut Vec<Row>) {
    let mut line = String::new();
    let mut evidence = routes.first().map(|r| r.evidence.clone());
    for route in routes {
        let piece = format!("{}  ", route.path);
        if !line.is_empty() && cells(prefix) + cells(&line) + cells(&piece) > width {
            rows.push(Row {
                text: format!("{prefix}{}", line.trim_end()),
                tone: Tone::Normal,
                evidence: evidence.take(),
                key: None,
                link: None,
            });
            line.clear();
            evidence = Some(route.evidence.clone());
        }
        line.push_str(&piece);
    }
    if !line.is_empty() {
        rows.push(Row {
            text: format!("{prefix}{}", line.trim_end()),
            tone: Tone::Normal,
            evidence,
            key: None,
            link: None,
        });
    }
}

fn footer(map: &FlowMap, width: usize, rows: &mut Vec<Row>) {
    let half = width / 2;
    let mut left = vec![Row::new(
        crate::localize!("跨请求共享状态", "Shared state across requests"),
        Tone::Heading,
    )];
    let held: Vec<_> = map.shared.iter().filter(|s| s.held).collect();
    for state in held.iter().take(4) {
        let fields: Vec<_> = state.fields.iter().map(|f| f.name.as_str()).collect();
        left.push(Row::at(
            format!("  {}  {}", state.name, fields.join(" · ")),
            Tone::Normal,
            &state.evidence,
        ));
    }
    if held.len() > 4 {
        left.push(Row::new(
            crate::localize!("  共 {} 个，按 s 查看", "  {} total, press s", held.len()),
            Tone::Dim,
        ));
    }
    if held.is_empty() {
        left.push(Row::new(
            crate::localize!("  未发现", "  none found"),
            Tone::Dim,
        ));
    }
    let mut right = vec![Row::new(
        crate::localize!("外部系统", "External systems"),
        Tone::Heading,
    )];
    for external in &map.external {
        right.push(Row::new(
            format!(
                "  {}  {}",
                external_label(&external.category),
                external.packages.join(" · ")
            ),
            Tone::Normal,
        ));
    }
    if map.external.is_empty() {
        right.push(Row::new(
            crate::localize!("  未发现已知依赖", "  no known dependencies"),
            Tone::Dim,
        ));
    }
    if width < 100 {
        rows.extend(left);
        rows.push(Row::new("", Tone::Normal));
        rows.extend(right);
    } else {
        for i in 0..left.len().max(right.len()) {
            let l = left.get(i);
            let r = right.get(i);
            let text = format!(
                "{}{}",
                pad(l.map_or("", |row| row.text.as_str()), half),
                r.map_or("", |row| row.text.as_str())
            );
            let tone = l.or(r).map_or(Tone::Normal, |row| row.tone);
            rows.push(Row {
                text,
                tone,
                evidence: l.and_then(|row| row.evidence.clone()),
                key: None,
                link: None,
            });
        }
    }
    if let Some(trunk) = &map.trunk {
        if trunk.unresolved > 0 {
            rows.push(Row::new(
                crate::localize!(
                    "主干可达范围内 {} 处调用的目标无法由声明类型确定；只显示其中会提前结束的。",
                    "{} calls within the trunk's reach have targets not determined by declared types; only those with exits are shown.",
                    trunk.unresolved
                ),
                Tone::Dim,
            ));
        }
    }
}

/// Plain-text views used by `codexis flow --view` and the browser lists.
pub fn errors(map: &FlowMap) -> Vec<Row> {
    let mut rows = vec![Row::new(
        crate::localize!(
            "错误出口 · {} 处（主干上的在前）",
            "Error exits · {} (trunk first)",
            map.errors.len()
        ),
        Tone::Heading,
    )];
    for error in &map.errors {
        let code = error.code.as_deref().unwrap_or("—");
        rows.push(Row::at(
            format!(
                "{} {:<7} {}  {}  {}",
                if error.on_trunk { "●" } else { " " },
                error.status,
                pad(code, 30),
                pad(&error.function, 36),
                location(&error.evidence)
            ),
            if error.on_trunk {
                Tone::Normal
            } else {
                Tone::Dim
            },
            &error.evidence,
        ));
    }
    rows
}

pub fn config(map: &FlowMap) -> Vec<Row> {
    let mut rows = vec![
        Row::new(
            crate::localize!(
                "配置项 · {} 个",
                "Configuration · {} fields",
                map.config.len()
            ),
            Tone::Heading,
        ),
        Row::new(
            if map.language == "python" {
                crate::localize!(
                    "根：pydantic BaseSettings 子类；嵌套 BaseModel 字段按路径展开",
                    "Roots: pydantic BaseSettings subclasses; nested BaseModel fields by path"
                )
            } else {
                crate::localize!(
                    "根：源码中由 YAML/TOML/环境变量加载的 Deserialize 类型；读取按声明类型确定",
                    "Roots: Deserialize types loaded from YAML/TOML/environment in source; reads follow declared types"
                )
            },
            Tone::Dim,
        ),
    ];
    for field in &map.config {
        rows.push(Row::at(
            format!(
                "{}  {}  {}",
                pad(&field.path, 44),
                pad(&field.ty, 28),
                crate::localize!("{} 处读取", "{} reads", field.reads.len())
            ),
            if field.reads.is_empty() {
                Tone::Dim
            } else {
                Tone::Normal
            },
            &field.evidence,
        ));
        for read in &field.reads {
            rows.push(Row::at(
                format!(
                    "    {}  {}  {}",
                    pad(&read.function, 36),
                    pad(&location(&read.evidence), 24),
                    read_mode(read.mode)
                ),
                Tone::Dim,
                &read.evidence,
            ));
        }
    }
    rows
}

pub fn state(map: &FlowMap) -> Vec<Row> {
    let mut rows = vec![Row::new(
        crate::localize!(
            "共享状态 · {} 个类型（字段含锁、原子量、ArcSwap、通道等）",
            "Shared state · {} types (fields with locks, atomics, ArcSwap, channels)",
            map.shared.len()
        ),
        Tone::Heading,
    )];
    for state in &map.shared {
        rows.push(Row::at(
            format!(
                "{} {}  {}",
                if state.on_trunk { "●" } else { " " },
                state.name,
                location(&state.evidence)
            ),
            Tone::Normal,
            &state.evidence,
        ));
        for field in &state.fields {
            rows.push(Row::at(
                format!("    {}: {}", field.name, field.ty),
                Tone::Dim,
                &field.evidence,
            ));
        }
        for access in &state.accesses {
            rows.push(Row::at(
                format!(
                    "      {}  .{} {}  {}",
                    pad(&access.function, 36),
                    access.field,
                    access.op,
                    location(&access.evidence)
                ),
                Tone::Dim,
                &access.evidence,
            ));
        }
    }
    rows
}

pub fn trunks(map: &FlowMap) -> Vec<Row> {
    let mut rows = vec![Row::new(
        crate::localize!("入口 · {} 个", "Entries · {}", map.routes.len()),
        Tone::Heading,
    )];
    for route in &map.routes {
        rows.push(Row::at(
            format!(
                "  {:<7} {}  {}  {}",
                if route.method.is_empty() {
                    "ANY"
                } else {
                    route.method.as_str()
                },
                pad(&route.path, 52),
                pad(route.handler.as_deref().unwrap_or("—"), 36),
                location(&route.evidence)
            ),
            if route.internal {
                Tone::Dim
            } else {
                Tone::Normal
            },
            &route.evidence,
        ));
    }
    rows.push(Row::new("", Tone::Normal));
    rows.push(Row::new(
        crate::localize!(
            "其他主干 · {} 个（路由处理函数和程序入口）",
            "Other trunks · {} (route handlers and program entries)",
            map.trunks.len()
        ),
        Tone::Heading,
    ));
    for trunk in &map.trunks {
        let mut row = Row::at(
            format!(
                "  {}  {}  {}",
                pad(&trunk.label, 44),
                pad(
                    &crate::localize!(
                        "{} 个入口 · 可达 {} 个函数",
                        "{} entries · reaches {} functions",
                        trunk.routes,
                        trunk.reach
                    ),
                    30
                ),
                location(&trunk.evidence)
            ),
            Tone::Normal,
            &trunk.evidence,
        );
        row.link = Some(trunk.id.clone());
        rows.push(row);
    }
    rows
}

pub fn text(rows: &[Row]) -> String {
    let mut out = String::new();
    for row in rows {
        out.push_str(row.text.trim_end());
        out.push('\n');
    }
    out
}
