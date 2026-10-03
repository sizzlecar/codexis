//! Connect snapshot evidence, model explanations and the terminal workbench.
use crate::{
    explanation::{self, ContextEvidence, EvidenceKind, ExplanationContext, Provider},
    index::Index,
    model::{identity, Evidence, Report, Snapshot},
};
use anyhow::{bail, Result};
use rusqlite::params;
use serde_json::{json, Value};
use std::{collections::BTreeMap, time::Duration};

fn admitted(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let lower = name.to_ascii_lowercase();
    !path
        .split('/')
        .any(|p| p.starts_with('.') || matches!(p, "runtime-data" | "vendor" | "target"))
        && (path.ends_with(".rs")
            || path.ends_with(".py")
            || path.ends_with(".pyi")
            || matches!(name, "Cargo.toml" | "pyproject.toml")
            || matches!(
                lower.as_str(),
                "readme" | "readme.md" | "readme.rst" | "readme.markdown"
            ))
}

/// The model reads only admitted files from this immutable snapshot. Runtime
/// data, credentials, project instructions and planning documents are excluded.
pub fn context(index: &Index, snapshot: &Snapshot, baseline: &Value) -> Result<ExplanationContext> {
    let mut evidence = Vec::new();
    let mut ids = BTreeMap::new();
    let mut bytes = 0;
    for (path, hash) in index.file_hashes(&snapshot.id)? {
        if !admitted(&path) {
            continue;
        }
        let content = index.content(&hash)?;
        if content.is_empty() {
            continue;
        }
        bytes += content.len();
        if bytes > 64 * 1024 * 1024 || evidence.len() >= 4096 {
            bail!(crate::localize!("解释素材超过 64 MiB / 4096 文件限制；尚未生成业务基线。", "Explanation material exceeds 64 MiB / 4096 files; no business baseline was generated."));
        }
        let id = format!("file:{}", &identity(&[&path, &hash])[..24]);
        ids.insert(path.clone(), id.clone());
        let span = Evidence {
            path: path.clone(),
            content_hash: hash,
            start_byte: 0,
            end_byte: content.len(),
            start_line: 1,
            start_column: 0,
            end_line: content.bytes().filter(|b| *b == b'\n').count() + 1,
            end_column: content.rsplit('\n').next().unwrap_or("").len(),
        };
        let kind = if path.ends_with(".rs") || path.ends_with(".py") || path.ends_with(".pyi") {
            EvidenceKind::Source
        } else {
            EvidenceKind::Declared
        };
        evidence.push(ContextEvidence {
            id,
            evidence: span,
            content,
            kind,
        });
    }
    let mut facts = json!({
        "purpose":baseline["purpose"],
        "entries":baseline["entries"].as_array().into_iter().flatten().take(6).map(|entry| json!({"name":entry["name"],"signature":entry["signature"],"evidence":entry["evidence"]})).collect::<Vec<_>>(),
        "source_context":baseline["source_context"],
        "source_context_truncated":baseline["source_context_truncated"],
        "source_context_scope":baseline["source_context_scope"],
        "questions":baseline["questions"].as_array().into_iter().flatten().take(3).map(|question| json!({"question":question["question"],"why":question["why"]})).collect::<Vec<_>>()
    });
    for snippet in facts["source_context"].as_array_mut().into_iter().flatten() {
        if let Some(id) = snippet["evidence"]["path"]
            .as_str()
            .and_then(|p| ids.get(p))
        {
            snippet["id"] = json!(format!(
                "{id}#L{}-L{}",
                snippet["evidence"]["start_line"], snippet["evidence"]["end_line"]
            ));
        }
    }
    Ok(ExplanationContext {
        snapshot_id: snapshot.id.clone(),
        locale: crate::i18n::current().tag().into(),
        project_name: std::path::Path::new(&snapshot.project_root)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        evidence,
        facts,
    })
}

fn enrich(index: &Index, context: &ExplanationContext, value: &mut Value) -> Result<()> {
    match value {
        Value::Array(items) => {
            for item in items {
                enrich(index, context, item)?;
            }
        }
        Value::Object(fields) => {
            if let Some(ids) = fields.get("evidence_ids").and_then(Value::as_array) {
                let spans = ids
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|id| explanation::resolve_evidence(context, id))
                    .collect::<Result<Vec<_>>>()?;
                let mut nodes = Vec::new();
                for span in &spans {
                    let mut statement = index.connection.prepare("SELECT id FROM nodes WHERE snapshot=?1 AND path=?2 AND start<?3 AND end>?4 AND kind IN ('function','type','trait') ORDER BY end-start,id LIMIT 4")?;
                    nodes.extend(
                        statement
                            .query_map(
                                params![
                                    context.snapshot_id,
                                    span.path,
                                    span.end_byte,
                                    span.start_byte
                                ],
                                |r| r.get::<_, String>(0),
                            )?
                            .collect::<rusqlite::Result<Vec<_>>>()?,
                    );
                }
                nodes.sort();
                nodes.dedup();
                nodes.truncate(12);
                fields.insert("evidence".into(), json!(spans));
                fields.insert("node_ids".into(), json!(nodes));
            }
            for field in fields.values_mut() {
                enrich(index, context, field)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn attach(index: &Index, snapshot: &Snapshot, baseline: &mut Value) -> Result<()> {
    if let Some(explanation) = explanation::load(index, &snapshot.id, crate::i18n::current().tag())?
    {
        let context = context(index, snapshot, baseline)?;
        let mut value = serde_json::to_value(explanation)?;
        enrich(index, &context, &mut value)?;
        baseline["explanation"] = value;
        baseline["status"] = json!("interpreted");
        baseline["missing_business_explanation"] = json!(false);
        baseline["interpretation_notice"] = json!(crate::localize!("这是模型对固定快照的解释；引用可核查，不等同人工确认或运行验证。", "This is a model interpretation of a fixed snapshot. Citations can be inspected; they do not imply human confirmation or runtime verification."));
    }
    Ok(())
}

pub fn data(
    index: &Index,
    snapshot: &Snapshot,
    understanding: &Value,
    guide: &Value,
) -> Result<Value> {
    let mut baseline = crate::baseline::build(index, snapshot, understanding, guide)?;
    attach(index, snapshot, &mut baseline)?;
    Ok(baseline)
}

pub fn report(
    index: &Index,
    snapshot: &Snapshot,
    generate: bool,
    model: Option<&str>,
    timeout_secs: u64,
) -> Result<Report<Value>> {
    let overview = crate::query::overview(index, snapshot)?;
    let mut baseline = overview.data["baseline"].clone();
    if generate {
        if snapshot.completeness.stale {
            bail!(crate::localize!(
                "源码已变化；先 analyze 更新快照，再生成认知解释。",
                "Source changed; analyze again before generating a project explanation."
            ));
        }
        let context = context(index, snapshot, &baseline)?;
        let provider = Provider {
            model: model.map(str::to_owned),
            timeout: Duration::from_secs(timeout_secs),
            ..Provider::default()
        };
        explanation::generate(index, &context, &provider, &crate::source::CANCELLED)?;
        attach(index, snapshot, &mut baseline)?;
    }
    Ok(Report::new(
        snapshot,
        json!({"kind":"baseline","project_root":snapshot.project_root,"baseline":baseline}),
    ))
}
