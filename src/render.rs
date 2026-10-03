use crate::model::Report;
use serde_json::Value;
use std::fmt::Write;

macro_rules! line {
    ($out:expr, $zh:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        writeln!($out, "{}", crate::localize!($zh, $en $(, $arg)*))
    };
}

fn label(value: &str) -> &str {
    match value {
        "overview" => crate::localize!("项目概览", "Project overview"),
        "baseline" => crate::localize!("业务认知基线", "Business understanding baseline"),
        "understanding" => {
            crate::localize!("八维项目理解", "Eight dimensions of project understanding")
        }
        "review" => crate::localize!("改动审阅", "Change review"),
        "map" => crate::localize!("项目结构", "Project structure"),
        "inspect" => crate::localize!("源码核查", "Source inspection"),
        "trace" => crate::localize!("调用路径", "Call paths"),
        "knowledge" => crate::localize!("认知记录", "Knowledge records"),
        "mark" => crate::localize!("阅读记录", "Reading record"),
        "candidates" => crate::localize!("候选符号", "Symbol candidates"),
        "complete" => crate::localize!("完整", "complete"),
        "partial" => crate::localize!("不完整", "partial"),
        "syntax" => crate::localize!("语法", "syntax"),
        "semantic" => crate::localize!("语义", "semantic"),
        "unread" => crate::localize!("未读", "unread"),
        "seen" => crate::localize!("已读", "seen"),
        "question" => crate::localize!("疑问", "question"),
        "confirmed" => crate::localize!("已确认", "confirmed"),
        "needs_review" => crate::localize!("需复核", "needs review"),
        "added" => crate::localize!("新增", "added"),
        "removed" => crate::localize!("删除", "removed"),
        "modified" => crate::localize!("修改", "modified"),
        "implementation" => crate::localize!("实现", "implementation"),
        "interface" => crate::localize!("接口", "interface"),
        "callers" => crate::localize!("调用者", "callers"),
        "callees" => crate::localize!("被调用者", "callees"),
        "package" => crate::localize!("包", "package"),
        "module" => crate::localize!("模块", "module"),
        "function" => crate::localize!("函数", "function"),
        "type" => crate::localize!("类型", "type"),
        "field" => crate::localize!("字段", "field"),
        "variable" => crate::localize!("变量", "variable"),
        "trait" => "trait",
        "resolved" => crate::localize!("已解析", "resolved"),
        "unresolved" => crate::localize!("未解析", "unresolved"),
        "external" => crate::localize!("外部边界", "external"),
        "calls" => crate::localize!("调用", "calls"),
        "imports" => crate::localize!("导入", "imports"),
        "writes" => crate::localize!("写入", "writes"),
        "type_usage" => crate::localize!("类型引用", "type usage"),
        "user_confirmed" => crate::localize!("开发者确认", "Human confirmation"),
        "source" => crate::localize!("源码事实", "Source fact"),
        "declared" => crate::localize!("文档声明", "Documented declaration"),
        "interpretation" => crate::localize!("解释", "Interpretation"),
        "high" => crate::localize!("高", "high"),
        "medium" => crate::localize!("中", "medium"),
        "low" => crate::localize!("低", "low"),
        "normal" => crate::localize!("普通", "normal"),
        "dev" => crate::localize!("开发", "development"),
        "build" => crate::localize!("构建", "build"),
        "test" => crate::localize!("测试", "test"),
        _ => value,
    }
}

fn clean(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

fn field(value: &Value, name: &str) -> String {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(clean)
        .unwrap_or_default()
}

fn location(node: &Value) -> String {
    let evidence = &node["evidence"];
    format!("{}:{}", field(evidence, "path"), evidence["start_line"])
}

fn baseline_evidence(out: &mut String, value: &Value) -> anyhow::Result<()> {
    for evidence in value["evidence"].as_array().into_iter().flatten() {
        let path = field(evidence, "path");
        let start = evidence["start_line"].as_u64().unwrap_or(0);
        let end = evidence["end_line"].as_u64().unwrap_or(start);
        if end > start {
            writeln!(out, "    {path}:{start}-{end}")?;
        } else {
            writeln!(out, "    {path}:{start}")?;
        }
    }
    Ok(())
}

fn baseline_claim(out: &mut String, title: &str, claim: &Value) -> anyhow::Result<()> {
    line!(out, "  {}：{}", "  {}: {}", title, field(claim, "text"))?;
    line!(
        out,
        "    依据：{}",
        "    Basis: {}",
        label(claim["basis"].as_str().unwrap_or_default())
    )?;
    baseline_evidence(out, claim)
}

pub(crate) fn baseline_text(out: &mut String, baseline: &Value) -> anyhow::Result<()> {
    line!(out, "\n业务认知基线", "\nBusiness understanding baseline")?;
    if let Some(explanation) = baseline
        .get("explanation")
        .filter(|value| value.is_object())
    {
        let notice = field(baseline, "interpretation_notice");
        if !notice.is_empty() {
            writeln!(out, "{notice}")?;
        } else {
            line!(
                out,
                "这是模型对固定快照的解释；引用可核查，不等同人工确认或运行验证。",
                "This is a model interpretation of a fixed snapshot. Citations can be inspected; they do not imply human confirmation or runtime verification."
            )?;
        }
        baseline_claim(
            out,
            crate::localize!("项目目的", "Project purpose"),
            &explanation["purpose"],
        )?;
        line!(out, "\n核心场景", "\nCore scenario")?;
        let scenario = &explanation["scenario"];
        baseline_claim(out, crate::localize!("目标", "Goal"), &scenario["goal"])?;
        baseline_claim(out, crate::localize!("输入", "Input"), &scenario["input"])?;
        baseline_claim(out, crate::localize!("输出", "Output"), &scenario["output"])?;
        for (position, step) in scenario["steps"]
            .as_array()
            .into_iter()
            .flatten()
            .take(5)
            .enumerate()
        {
            line!(out, "\n步骤 {}", "\nStep {}", position + 1)?;
            baseline_claim(out, crate::localize!("环节", "Action"), &step["title"])?;
            baseline_claim(out, crate::localize!("输入", "Input"), &step["input"])?;
            baseline_claim(out, crate::localize!("输出", "Output"), &step["output"])?;
            baseline_claim(
                out,
                crate::localize!("职责", "Responsibility"),
                &step["responsibility"],
            )?;
        }
        line!(out, "\n状态与边界", "\nState and boundary")?;
        baseline_claim(
            out,
            crate::localize!("关键状态", "Key state"),
            &explanation["key_state"],
        )?;
        baseline_claim(
            out,
            crate::localize!("边界", "Boundary"),
            &explanation["boundary"],
        )?;
        line!(out, "\n下一步阅读", "\nNext reading")?;
        baseline_claim(
            out,
            crate::localize!("阅读目标", "Reading target"),
            &explanation["reading"]["target"],
        )?;
        baseline_claim(
            out,
            crate::localize!("阅读原因", "Why read it"),
            &explanation["reading"]["why"],
        )?;
        if explanation["questions"]
            .as_array()
            .is_some_and(|questions| !questions.is_empty())
        {
            line!(out, "\n待回答问题", "\nOpen questions")?;
            for question in explanation["questions"]
                .as_array()
                .into_iter()
                .flatten()
                .take(2)
            {
                baseline_claim(out, crate::localize!("问题", "Question"), question)?;
            }
        }
    } else {
        line!(
            out,
            "尚未形成业务认知基线。",
            "A business understanding baseline has not been established."
        )?;
        let purpose = &baseline["purpose"];
        let original = field(purpose, "text");
        if original.is_empty() {
            line!(
                out,
                "快照中尚未找到文档目的声明。",
                "No documented purpose statement was found in the snapshot."
            )?;
        } else {
            line!(out, "\n文档目的原文：", "\nOriginal documented purpose:")?;
            writeln!(out, "{original}")?;
            line!(out, "  依据：{}", "  Basis: {}", label("declared"))?;
            baseline_evidence(out, purpose)?;
        }
        line!(out, "\n待回答问题", "\nOpen questions")?;
        for question in baseline["questions"]
            .as_array()
            .into_iter()
            .flatten()
            .take(2)
        {
            writeln!(out, "  - {}", field(question, "question"))?;
            let why = field(question, "why");
            if !why.is_empty() {
                line!(out, "    原因：{}", "    Why: {}", why)?;
            }
            baseline_evidence(out, question)?;
        }
        line!(
            out,
            "\n生成业务解释：codexis baseline --generate",
            "\nGenerate a business explanation: codexis baseline --generate"
        )?;
    }
    Ok(())
}

pub fn render(report: &Report<Value>, format: &str) -> anyhow::Result<String> {
    render_with_detail(report, format, false)
}

pub fn render_with_detail(
    report: &Report<Value>,
    format: &str,
    verbose: bool,
) -> anyhow::Result<String> {
    if format == "json" {
        return Ok(serde_json::to_string_pretty(report)? + "\n");
    }
    if format == "markdown" {
        if matches!(
            report.data["kind"].as_str(),
            Some("overview" | "understanding" | "review" | "knowledge")
        ) {
            return markdown_report(report);
        }
        // A dynamically sized fence keeps arbitrary source/notes/filenames
        // literal, including embedded fences and HTML, without corrupting code.
        let plain = render_with_detail(report, "text", verbose)?;
        let longest = plain.split(|c| c != '~').map(str::len).max().unwrap_or(0);
        let fence = "~".repeat(3.max(longest.saturating_add(1)));
        return Ok(format!(
            "# Codexis — {}\n\n{fence}text\n{plain}{fence}\n",
            label(report.data["kind"].as_str().unwrap_or_default())
        ));
    }
    if !verbose && report.data["kind"] == "overview" {
        return compact_overview(report);
    }
    if report.data["kind"] == "baseline" {
        let mut out = String::new();
        line!(
            out,
            "Codexis — 业务认知基线",
            "Codexis — Business understanding baseline"
        )?;
        line!(out, "快照：{}", "Snapshot: {}", report.snapshot_id)?;
        if report.completeness.stale {
            line!(
                out,
                "源码已变化，以下解释对应已保存快照。",
                "Source changed; the explanation below refers to the stored snapshot."
            )?;
        }
        baseline_text(
            &mut out,
            report.data.get("baseline").unwrap_or(&report.data),
        )?;
        return Ok(out);
    }
    if report.data["kind"] == "understanding" {
        return understanding_text(report);
    }
    if report.data["kind"] == "knowledge" {
        return knowledge_text(report);
    }
    let markdown = format == "markdown";
    let mut out = String::new();
    writeln!(
        out,
        "{}Codexis — {}",
        if markdown { "# " } else { "" },
        label(report.data["kind"].as_str().unwrap_or_default())
    )?;
    line!(out, "快照：{}", "Snapshot: {}", report.snapshot_id)?;
    line!(
        out,
        "分析：{} / {} | {}{}",
        "Analysis: {} / {} | {}{}",
        report.analysis_context.language,
        label(&report.analysis_context.analysis),
        label(&report.completeness.status),
        if report.completeness.stale {
            crate::localize!(
                "（源码已变化；引用已保存证据）",
                " (source changed; stored evidence)"
            )
        } else {
            ""
        }
    )?;
    let data = &report.data;
    match data["kind"].as_str().unwrap_or("") {
        "overview" => {
            if !data["baseline"].is_null() {
                baseline_text(&mut out, &data["baseline"])?;
            }
            let stats = &data["stats"];
            line!(
                out,
                "\n{} 个源码文件 · {} 行 · {} 个符号 · {} 条关系",
                "\n{} source files · {} lines · {} symbols · {} relations",
                stats["source_files"],
                stats["source_lines"],
                stats["nodes"],
                stats["edges"]
            )?;
            line!(
                out,
                "解析 {} · 复用 {} · 语法错误 {} · {} 毫秒",
                "Parsed {} · reused {} · syntax errors {} · {} ms",
                stats["parsed_files"],
                stats["reused_files"],
                stats["failed_files"],
                stats["elapsed_ms"]
            )?;
            line!(out, "\n包：", "\nPackages:")?;
            for package in data["project"]["packages"].as_array().into_iter().flatten() {
                line!(
                    out,
                    "- {} ({}) — {} 个编译单元，{} 个声明依赖",
                    "- {} ({}) — {} compilation units, {} declared dependencies",
                    field(package, "name"),
                    field(package, "root"),
                    package["units"].as_array().map_or(0, Vec::len),
                    package["dependencies"].as_array().map_or(0, Vec::len)
                )?;
            }
            line!(
                out,
                "\n阅读入口（共 {} 个，最多显示 30 个）：",
                "\nReading entry points (showing at most 30 of {}):",
                data["entry_points_total"]
            )?;
            line!(
                out,
                "已索引但未关联单元的文件：{} · 带诊断跳过的文件：{}",
                "Indexed but unlinked files: {} · skipped with diagnostics: {}",
                data["source_accounting"]["unlinked_indexed_files"],
                data["source_accounting"]["skipped_files_with_diagnostics"]
            )?;
            for node in data["entries"].as_array().into_iter().flatten() {
                writeln!(
                    out,
                    "- {} — {} — ID {}",
                    field(node, "qualified_name"),
                    location(node),
                    field(node, "id")
                )?;
            }
            line!(out, "\n后续查询：", "\nNext queries:")?;
            for command in data["next_queries"].as_array().into_iter().flatten() {
                writeln!(out, "    {}", command.as_str().unwrap_or_default())?;
            }
        }
        "map" => {
            line!(
                out,
                "\n{}结构（共 {} 项）：",
                "\n{} map ({} total):",
                label(data["level"].as_str().unwrap_or_default()),
                data["total"]
            )?;
            for item in data["items"].as_array().into_iter().flatten() {
                if data["level"] == "package" {
                    let package = &item["package"];
                    writeln!(
                        out,
                        "\n- {} — {}",
                        field(package, "name"),
                        field(package, "root")
                    )?;
                    for dep in package["dependencies"].as_array().into_iter().flatten() {
                        line!(
                            out,
                            "    -> {} [{}；声明依赖{}]",
                            "    -> {} [{}; declared{}]",
                            field(dep, "alias"),
                            field(dep, "kind"),
                            dep["condition"]
                                .as_str()
                                .map(|s| format!("; {}", clean(s)))
                                .unwrap_or_default()
                        )?;
                    }
                } else {
                    writeln!(
                        out,
                        "- {} — {} — ID {}",
                        field(item, "qualified_name"),
                        location(item),
                        field(item, "id")
                    )?;
                }
            }
        }
        "candidates" => {
            line!(out, "\n选择符号 ID：", "\nChoose a symbol ID:")?;
            for node in data["candidates"].as_array().into_iter().flatten() {
                writeln!(
                    out,
                    "- {} [{}] — {}\n    {}",
                    field(node, "qualified_name"),
                    label(node["kind"].as_str().unwrap_or_default()),
                    location(node),
                    field(node, "id")
                )?;
            }
        }
        "inspect" => {
            let node = &data["node"];
            writeln!(
                out,
                "\n{} [{}]\n{}\n{}",
                field(node, "qualified_name"),
                label(node["kind"].as_str().unwrap_or_default()),
                location(node),
                field(node, "signature")
            )?;
            line!(out, "\n已保存源码：", "\nStored source:")?;
            for line in data["source"].as_array().into_iter().flatten() {
                writeln!(out, "    {:>6}  {}", line["line"], field(line, "text"))?;
            }
            line!(out, "\n出向关系：", "\nOutgoing relations:")?;
            for edge in data["outgoing"].as_array().into_iter().flatten() {
                write_edge(&mut out, edge)?;
            }
            line!(out, "\n入向关系：", "\nIncoming relations:")?;
            for edge in data["incoming"].as_array().into_iter().flatten() {
                write_edge(&mut out, edge)?;
            }
            line!(
                out,
                "\n阅读状态：{} — {}",
                "\nReading state: {} — {}",
                label(data["mark"]["state"].as_str().unwrap_or_default()),
                field(&data["mark"], "note")
            )?;
        }
        "trace" => {
            line!(
                out,
                "\n{} — {}（深度 {}，最多 {} 个节点）",
                "\n{} — {} (depth {}, max {} nodes)",
                field(&data["root"], "qualified_name"),
                label(data["direction"].as_str().unwrap_or_default()),
                data["depth"],
                data["limit"]
            )?;
            for edge in data["edges"].as_array().into_iter().flatten() {
                write_edge(&mut out, edge)?;
            }
            writeln!(out, "\n{}", field(data, "next"))?;
        }
        "review" => {
            write_batch(&mut out, &data["batch"])?;
            line!(
                out,
                "\n旧版本：{}\n新版本：{}",
                "\nBase: {}\nHead: {}",
                field(data, "base_revision"),
                field(data, "head_revision")
            )?;
            line!(
                out,
                "旧快照：{}\n新快照：{}",
                "Base snapshot: {}\nHead snapshot: {}",
                field(data, "base_snapshot"),
                field(data, "head_snapshot")
            )?;
            line!(
                out,
                "\n{} 个定义发生变化 · {} 个文件发生变化",
                "\n{} changed definitions · {} changed files",
                data["total_changes"],
                data["total_changed_files"]
            )?;
            writeln!(out, "{}", field(data, "impact_scope"))?;
            line!(
                out,
                "影响分析：{}",
                "Impact analysis: {}",
                field(data, "impact_analysis")
            )?;
            for change in data["changes"].as_array().into_iter().flatten() {
                let node = if change["after"].is_null() {
                    &change["before"]
                } else {
                    &change["after"]
                };
                writeln!(
                    out,
                    "\n- {} [{} / {}] — {}",
                    field(node, "qualified_name"),
                    label(change["state"].as_str().unwrap_or_default()),
                    label(change["priority"].as_str().unwrap_or_default()),
                    location(node)
                )?;
                line!(
                    out,
                    "    ID {} · 阅读状态 {}",
                    "    ID {} · mark {}",
                    field(node, "id"),
                    label(change["mark"]["state"].as_str().unwrap_or_default())
                )?;
                for reason in change["reasons"].as_array().into_iter().flatten() {
                    writeln!(out, "    {}", clean(reason.as_str().unwrap_or_default()))?;
                }
                for side in ["previous_impacts", "current_impacts"] {
                    for impact in change[side].as_array().into_iter().flatten() {
                        line!(
                            out,
                            "    潜在调用者（{}）：{} — {}",
                            "    Potential caller ({}): {} — {}",
                            if side == "previous_impacts" {
                                crate::localize!("旧版本", "previous version")
                            } else {
                                crate::localize!("新版本", "current version")
                            },
                            field(&impact["caller"], "qualified_name"),
                            location(&impact["caller"])
                        )?;
                    }
                }
                if change["impacts_truncated"] == true {
                    line!(
                        out,
                        "    影响路径已截断；使用 trace 聚焦查询。",
                        "    Impact paths truncated; use trace for a narrower query."
                    )?;
                }
            }
            line!(out, "\n文件证据：", "\nFile evidence:")?;
            for file in data["files"].as_array().into_iter().flatten() {
                writeln!(
                    out,
                    "\n{} [{}]",
                    field(file, "path"),
                    label(file["state"].as_str().unwrap_or_default())
                )?;
                for line in field(file, "diff").lines() {
                    writeln!(out, "    {line}")?;
                }
                if file["diff_truncated"] == true {
                    line!(
                        out,
                        "    文件差异已截断（最多 160 行，每行 800 个字符）。",
                        "    Diff truncated (160 lines / 800 characters per line)."
                    )?;
                }
            }
            writeln!(out, "\n{}", field(data, "next"))?;
        }
        "mark" => {
            writeln!(
                out,
                "\n{} — {}\n{}",
                field(data, "qualified_name"),
                label(data["mark"]["state"].as_str().unwrap_or_default()),
                field(&data["mark"], "note")
            )?;
        }
        _ => {
            writeln!(out, "\n{}", serde_json::to_string_pretty(data)?)?;
        }
    }
    if report.completeness.truncated {
        line!(
            out,
            "\n结果已截断。{}",
            "\nResult is truncated. {}",
            report
                .completeness
                .next_cursor
                .as_ref()
                .map(|c| crate::localize!(
                    "用 --cursor {} 继续查询",
                    "Continue with --cursor {}",
                    c
                ))
                .unwrap_or_else(|| crate::localize!(
                    "增加查询预算，或缩小范围。",
                    "Increase the query budget or choose a narrower scope."
                )
                .into())
        )?;
    }
    line!(out, "\n分析能力与边界：", "\nCapabilities and boundaries:")?;
    for capability in &report.completeness.capabilities {
        writeln!(
            out,
            "- {} ({})",
            clean(&capability.name),
            clean(&capability.provider)
        )?;
        for limitation in &capability.limitations {
            writeln!(out, "    {}", clean(limitation))?;
        }
    }
    line!(
        out,
        "快照中未解析的调用：{}",
        "Unresolved calls in snapshot: {}",
        report.completeness.unresolved
    )?;
    if !report.diagnostics.is_empty() {
        line!(
            out,
            "\n诊断（共 {} 项）：",
            "\nDiagnostics ({} total):",
            report.diagnostics.len()
        )?;
        for diagnostic in report.diagnostics.iter().take(20) {
            writeln!(
                out,
                "- {}: {} {}",
                clean(&diagnostic.code),
                clean(&diagnostic.message),
                diagnostic.path.as_deref().map(clean).unwrap_or_default()
            )?;
        }
        if report.diagnostics.len() > 20 {
            line!(
                out,
                "    用 --format json 查看全部诊断。",
                "    Use --format json for all diagnostics."
            )?;
        }
    }
    Ok(out)
}

fn single_line(text: &str, limit: usize) -> String {
    let text = clean(text).split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= limit {
        text
    } else {
        text.chars().take(limit).collect::<String>() + "…"
    }
}

fn understanding_text(report: &Report<Value>) -> anyhow::Result<String> {
    let mut out = String::new();
    line!(
        out,
        "Codexis · 八维项目理解\n快照：{} · {} / {}",
        "Codexis · Eight dimensions of project understanding\nSnapshot: {} · {} / {}",
        report.snapshot_id,
        report.analysis_context.language,
        label(&report.completeness.status)
    )?;
    for dimension in report.data["understanding"]["dimensions"]
        .as_array()
        .into_iter()
        .flatten()
    {
        line!(
            out,
            "\n{} · {} 项\n{}",
            "\n{} · {} items\n{}",
            field(dimension, "title"),
            dimension["total"],
            field(dimension, "summary")
        )?;
        if dimension["items"].as_array().is_none_or(Vec::is_empty) {
            line!(
                out,
                "  当前范围暂无记录。",
                "  No records in the current scope."
            )?;
        }
        for item in dimension["items"].as_array().into_iter().flatten().take(20) {
            writeln!(
                out,
                "  {}\n    {}",
                field(item, "title"),
                single_line(&field(item, "summary"), 260)
            )?;
            line!(
                out,
                "    依据：{}",
                "    Basis: {}",
                label(item["basis"].as_str().unwrap_or_default())
            )?;
            for e in item["evidence"].as_array().into_iter().flatten().take(3) {
                writeln!(out, "    {}:{}", field(e, "path"), e["start_line"])?;
            }
        }
        let visible = dimension["items"].as_array().map_or(0, Vec::len);
        if visible > 20 || dimension["truncated"] == true {
            line!(out,"  当前文本最多展示 20 项；JSON 保留有界结果，缩小 --scope 可聚焦。","  Text shows up to 20 items; JSON retains bounded results. Narrow --scope to focus.")?;
        }
        for limit in dimension["limitations"].as_array().into_iter().flatten() {
            line!(
                out,
                "  范围：{}",
                "  Scope: {}",
                clean(limit.as_str().unwrap_or_default())
            )?;
        }
    }
    if report.completeness.stale {
        line!(
            out,
            "\n注意：源码已变化，结果引用旧快照。",
            "\nSource changed; these results refer to a stored snapshot."
        )?;
    }
    Ok(out)
}

fn knowledge_text(report: &Report<Value>) -> anyhow::Result<String> {
    let mut out = String::new();
    line!(
        out,
        "Codexis · 认知记录\n快照：{}",
        "Codexis · Knowledge records\nSnapshot: {}",
        report.snapshot_id
    )?;
    let records: Vec<_> = if let Some(records) = report.data["records"].as_array() {
        records.iter().collect()
    } else {
        report.data.get("record").into_iter().collect()
    };
    if records.is_empty() {
        line!(
            out,
            "尚未保存结论；使用 remember <符号或文件> --claim <结论>。",
            "No conclusions saved; use remember <symbol-or-file> --claim <conclusion>."
        )?;
    }
    for record in records {
        line!(
            out,
            "\n{} · {} · 修订 {}\n{}\nID {}",
            "\n{} · {} · revision {}\n{}\nID {}",
            field(record, "title"),
            label(record["state"].as_str().unwrap_or_default()),
            record["revision"],
            field(record, "claim"),
            field(record, "id")
        )?;
        for path in record["changed_evidence"].as_array().into_iter().flatten() {
            line!(
                out,
                "  证据已变化：{}",
                "  Evidence changed: {}",
                clean(path.as_str().unwrap_or_default())
            )?;
        }
        for e in record["evidence"].as_array().into_iter().flatten().take(5) {
            writeln!(out, "  {}:{}", field(e, "path"), e["start_line"])?;
        }
    }
    Ok(out)
}

fn write_batch(out: &mut String, batch: &Value) -> anyhow::Result<()> {
    if batch.is_null() {
        return Ok(());
    }
    line!(
        out,
        "\n批次改动摘要\n{}",
        "\nBatch change summary\n{}",
        field(batch, "summary")
    )?;
    for group in batch["groups"].as_array().into_iter().flatten().take(10) {
        writeln!(
            out,
            "  {} · {}",
            field(group, "title"),
            field(group, "summary")
        )?;
    }
    line!(
        out,
        "\n重点核查 · {} 项",
        "\nReview priorities · {} items",
        batch["checklist_total"]
    )?;
    if batch["checklist_total"] == 0 {
        line!(
            out,
            "  当前批次暂无待核查项。",
            "  No review items in this batch."
        )?;
    }
    for question in batch["checklist"].as_array().into_iter().flatten().take(15) {
        writeln!(
            out,
            "  [{}] {}\n    {}",
            label(question["priority"].as_str().unwrap_or_default()),
            field(question, "title"),
            field(question, "summary")
        )?;
    }
    if batch["truncated"] == true {
        line!(
            out,
            "  批次聚合结果已截断；总数和边界见 JSON。",
            "  Batch aggregation was truncated; JSON includes totals and limits."
        )?;
    }
    Ok(())
}

fn md(text: &str) -> String {
    let mut value = clean(text);
    for c in ['\\', '`', '*', '_', '[', ']', '<', '>'] {
        value = value.replace(c, &format!("\\{c}"));
    }
    value
}
fn path_link(path: &str) -> String {
    let mut out = String::new();
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"/._-".contains(&byte) {
            out.push(byte as char)
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}
fn md_item(out: &mut String, item: &Value) -> anyhow::Result<()> {
    line!(
        out,
        "\n### {}\n\n{}\n\n依据：{}",
        "\n### {}\n\n{}\n\nBasis: {}",
        md(&field(item, "title")),
        md(&field(item, "summary")),
        md(label(item["basis"].as_str().unwrap_or_default()))
    )?;
    for evidence in item["evidence"].as_array().into_iter().flatten().take(8) {
        let path = field(evidence, "path");
        line!(
            out,
            "\n- [{}:{}]({}) · 内容标识 `{}`",
            "\n- [{}:{}]({}) · Content hash `{}`",
            md(&path),
            evidence["start_line"],
            path_link(&path),
            field(evidence, "content_hash")
        )?;
    }
    if item["truncated"] == true {
        line!(
            out,
            "\n此项的路径或成员已截断。",
            "\nPaths or members in this item were truncated."
        )?;
    }
    Ok(())
}
fn markdown_report(report: &Report<Value>) -> anyhow::Result<String> {
    let mut out = String::new();
    line!(out,"# Codexis 项目理解与审阅\n\n快照：`{}`\n\n语言：{}；分析：{}；状态：{}。","# Codexis Project Understanding and Review\n\nSnapshot: `{}`\n\nLanguage: {}; analysis: {}; status: {}.",report.snapshot_id,report.analysis_context.language,label(&report.analysis_context.analysis),label(&report.completeness.status))?;
    if report.completeness.status == "partial" {
        line!(out,"\n~~~text\n分析不完整，部分源码或关系未能解析。\n~~~","\n~~~text\nAnalysis is partial; some source or relationships could not be resolved.\n~~~")?;
    }
    if report.completeness.stale {
        line!(
            out,
            "\n源码已变化，以下证据属于旧快照。",
            "\nSource changed; the evidence below belongs to a stored snapshot."
        )?;
    }
    line!(out,"\n源码链接用于位置导航；核查结论时应使用记录的快照与内容标识。","\nSource links help navigate locations; verify conclusions against the recorded snapshot and content hashes.")?;
    match report.data["kind"].as_str().unwrap_or_default() {
        "overview" | "understanding" => {
            if report.data["kind"] == "overview" && !report.data["baseline"].is_null() {
                let mut plain = String::new();
                baseline_text(&mut plain, &report.data["baseline"])?;
                let longest = plain.split(|c| c != '~').map(str::len).max().unwrap_or(0);
                let fence = "~".repeat(3.max(longest.saturating_add(1)));
                writeln!(out, "\n{fence}text\n{plain}{fence}")?;
            }
            for dimension in report.data["understanding"]["dimensions"]
                .as_array()
                .into_iter()
                .flatten()
            {
                line!(
                    out,
                    "\n## {}\n\n{}\n\n共 {} 项。",
                    "\n## {}\n\n{}\n\n{} items in total.",
                    md(&field(dimension, "title")),
                    md(&field(dimension, "summary")),
                    dimension["total"]
                )?;
                for item in dimension["items"].as_array().into_iter().flatten().take(30) {
                    md_item(&mut out, item)?;
                }
                if dimension["truncated"] == true
                    || dimension["items"].as_array().is_some_and(|i| i.len() > 30)
                {
                    line!(out,"\n本报告每维最多展示 30 项；聚焦查询或 JSON 可查看有界详情。","\nThis report shows up to 30 items per dimension; use focused queries or JSON for bounded details.")?;
                }
                for limit in dimension["limitations"].as_array().into_iter().flatten() {
                    line!(
                        out,
                        "\n范围：{}",
                        "\nScope: {}",
                        md(limit.as_str().unwrap_or_default())
                    )?;
                }
            }
        }
        "review" => {
            line!(
                out,
                "\n## 批次改动摘要\n\n{}",
                "\n## Batch Change Summary\n\n{}",
                md(&field(&report.data["batch"], "summary"))
            )?;
            for group in report.data["batch"]["groups"]
                .as_array()
                .into_iter()
                .flatten()
                .take(30)
            {
                md_item(&mut out, group)?;
            }
            line!(out, "\n## 重点核查清单\n", "\n## Review Checklist\n")?;
            for item in report.data["batch"]["checklist"]
                .as_array()
                .into_iter()
                .flatten()
                .take(30)
            {
                md_item(&mut out, item)?;
            }
            line!(
                out,
                "\n旧版本快照：`{}`；新版本快照：`{}`。",
                "\nBase snapshot: `{}`; head snapshot: `{}`.",
                field(&report.data, "base_snapshot"),
                field(&report.data, "head_snapshot")
            )?;
            line!(out, "\n## 文件差异\n", "\n## File Differences\n")?;
            for file in report.data["files"].as_array().into_iter().flatten() {
                let diff = field(file, "diff");
                let longest = diff.split(|c| c != '~').map(str::len).max().unwrap_or(0);
                let fence = "~".repeat(3.max(longest + 1));
                writeln!(
                    out,
                    "\n### {}\n\n{fence}diff\n{diff}\n{fence}",
                    md(&field(file, "path"))
                )?;
            }
        }
        "knowledge" => {
            let records: Vec<_> = if let Some(records) = report.data["records"].as_array() {
                records.iter().collect()
            } else {
                report.data.get("record").into_iter().collect()
            };
            if records.is_empty() {
                line!(out, "\n尚未保存结论。", "\nNo conclusions saved.")?;
            }
            for item in records {
                md_item(&mut out, item)?;
                line!(
                    out,
                    "\n状态：{}；修订：{}。",
                    "\nState: {}; revision: {}.",
                    md(label(item["state"].as_str().unwrap_or_default())),
                    item["revision"]
                )?;
            }
        }
        _ => {}
    }
    Ok(out)
}

fn compact_overview(report: &Report<Value>) -> anyhow::Result<String> {
    let data = &report.data;
    let guide = &data["reading_guide"];
    let stats = &data["stats"];
    let mut out = String::new();
    let root = field(data, "project_root");
    let name = std::path::Path::new(&root)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    line!(
        out,
        "Codexis · {} · 项目概览",
        "Codexis · {} · Project overview",
        name
    )?;
    line!(
        out,
        "{} · {} 个包 · {} 个源码文件 · {} 行",
        "{} · {} packages · {} source files · {} lines",
        report.analysis_context.language,
        guide["packages_total"],
        stats["source_files"],
        stats["source_lines"]
    )?;
    if report.completeness.status == "partial" {
        line!(
            out,
            "注意：分析不完整，部分关系或源码未能解析。",
            "Analysis is partial; some relationships or source could not be resolved."
        )?;
    }
    if report.completeness.stale {
        line!(
            out,
            "注意：源码已变化，以下是旧快照。",
            "Source changed; the results below refer to a stored snapshot."
        )?;
    }
    if !data["baseline"].is_null() {
        baseline_text(&mut out, &data["baseline"])?;
    }
    line!(
        out,
        "\n八个分析维度",
        "\nEight dimensions of project understanding"
    )?;
    for dimension in data["understanding"]["dimensions"]
        .as_array()
        .into_iter()
        .flatten()
    {
        line!(
            out,
            "  {} · {} 项",
            "  {} · {} items",
            field(dimension, "title"),
            dimension["total"]
        )?;
    }
    line!(
        out,
        "\n主要子系统 · 模块与目录聚合",
        "\nMain subsystems · Grouped by module and directory"
    )?;
    for component in data["understanding"]["architecture"]["components"]
        .as_array()
        .into_iter()
        .flatten()
        .take(4)
    {
        writeln!(
            out,
            "  {} · {}",
            field(component, "title"),
            single_line(&field(component, "summary"), 86)
        )?;
    }
    line!(out, "\n从这里开始读", "\nStart reading here")?;
    for start in guide["starts"].as_array().into_iter().flatten() {
        let kind = match start["kind"].as_str() {
            Some("program") => crate::localize!("程序入口", "Program entry"),
            Some("library") => crate::localize!("库入口", "Library entry"),
            _ => crate::localize!("源码起点", "Source starting point"),
        };
        writeln!(
            out,
            "  {kind}  {} · {}",
            field(start, "name"),
            location(start)
        )?;
    }
    line!(
        out,
        "\n模块组成 · 描述来自项目清单",
        "\nModules · Descriptions from project manifests"
    )?;
    for package in guide["packages"].as_array().into_iter().flatten() {
        let description = package["description"]["text"]
            .as_str()
            .unwrap_or(crate::localize!(
                "未提供模块描述",
                "No module description provided"
            ));
        line!(
            out,
            "  {} · {} 个文件 · {}",
            "  {} · {} files · {}",
            field(package, "name"),
            package["source_files"],
            single_line(description, 86)
        )?;
    }
    if guide["packages_omitted"].as_u64().unwrap_or(0) > 0 {
        line!(
            out,
            "  另有 {} 个包；完整信息保留在 JSON 中。",
            "  {} additional packages; JSON retains the full details.",
            guide["packages_omitted"]
        )?;
    }
    line!(
        out,
        "\n内部依赖 · 声明依赖，不代表执行顺序",
        "\nInternal dependencies · Declared dependencies, not execution order"
    )?;
    let mut groups = std::collections::BTreeMap::<String, Vec<String>>::new();
    for edge in guide["dependencies"].as_array().into_iter().flatten() {
        let suffix = if edge["optional"] == true {
            crate::localize!("（可选）", " (optional)")
        } else if edge["condition"]
            .as_str()
            .is_some_and(|c| !c.starts_with("declared-version="))
        {
            crate::localize!("（条件）", " (conditional)")
        } else {
            ""
        };
        groups
            .entry(field(edge, "source"))
            .or_default()
            .push(format!("{}{suffix}", field(edge, "target")));
    }
    if groups.is_empty() {
        line!(
            out,
            "  未发现可关联的项目内部依赖。",
            "  No linkable internal project dependencies found."
        )?;
    }
    for (source, targets) in groups.iter().take(6) {
        let extra = if targets.len() > 4 {
            crate::localize!("，另 {} 项", ", {} more", targets.len() - 4)
        } else {
            String::new()
        };
        writeln!(
            out,
            "  {source} → {}{extra}",
            targets
                .iter()
                .take(4)
                .cloned()
                .collect::<Vec<_>>()
                .join(" / ")
        )?;
    }
    if groups.len() > 6 {
        line!(
            out,
            "  其余 {} 组依赖可在交互视图中展开。",
            "  {} additional dependency groups can be expanded in the interactive view.",
            groups.len() - 6
        )?;
    }
    let unlinked = data["source_accounting"]["unlinked_indexed_files"]
        .as_u64()
        .unwrap_or(0);
    line!(out, "\n阅读范围", "\nReading scope")?;
    if report.analysis_context.analysis == "syntax" {
        line!(out, "  当前展示结构与声明依赖；函数调用目标尚未解析。", "  This view shows structure and declared dependencies; function call targets have not been resolved.")?;
    } else {
        line!(
            out,
            "  已解析 {} 条内部调用；动态分派不代表唯一运行时实现。",
            "  {} internal calls resolved; dynamic dispatch does not establish a unique runtime implementation.",
            stats["resolved_calls"]
        )?;
    }
    if unlinked > 0 {
        line!(
            out,
            "  {} 个文件已索引但未关联编译单元，关联状态见完整报告。",
            "  {} files were indexed without a compilation unit; the full report retains their linking status.",
            unlinked,
        )?;
    }
    let important: Vec<_> = report
        .diagnostics
        .iter()
        .filter(|d| d.code != "unlinked_source" && d.code != "semantic_scope")
        .collect();
    for diagnostic in important.iter().take(3) {
        let message = match diagnostic.code.as_str() {
            "rust_parse_error" | "python_parse_error" => crate::localize!(
                "源码存在语法错误，已保留可读取的定义。",
                "Source contains syntax errors; readable definitions were retained."
            ),
            "semantic_unavailable" => crate::localize!(
                "调用解析后端不可用，仍可阅读结构和源码。",
                "Call resolution is unavailable; structure and source remain readable."
            ),
            "non_utf8_source" | "source_too_large" => crate::localize!(
                "文件未能读取，分析范围不完整。",
                "A file could not be read; the analysis scope is incomplete."
            ),
            _ => &diagnostic.message,
        };
        writeln!(
            out,
            "  {} {}",
            diagnostic
                .path
                .as_deref()
                .map(|p| single_line(p, 100))
                .unwrap_or_default(),
            single_line(message, 130)
        )?;
    }
    if important.len() > 3 {
        line!(
            out,
            "  另有 {} 项分析限制。",
            "  {} additional analysis limitations.",
            important.len() - 3
        )?;
    }
    line!(
        out,
        "  详细诊断：--verbose；完整机器可读结果：--format json。",
        "  Detailed diagnostics: --verbose; complete machine-readable results: --format json."
    )?;
    Ok(out)
}

fn write_edge(out: &mut String, edge: &Value) -> std::fmt::Result {
    writeln!(
        out,
        "- {} {} [{}] — {}",
        label(edge["kind"].as_str().unwrap_or_default()),
        field(edge, "target_name"),
        label(edge["resolution"].as_str().unwrap_or_default()),
        location(edge)
    )
}
