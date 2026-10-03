//! Explicit architecture rules are checked only against known-target relations.
use crate::{
    index::Index,
    model::{Edge, Evidence, Node, Snapshot},
};
use anyhow::Result;
use globset::Glob;
use serde_json::{json, Value};

pub fn evaluate(index: &Index, snapshot: &Snapshot) -> Result<Value> {
    let hashes = index.file_hashes(&snapshot.id)?;
    let Some(hash) = hashes.get("codexis.toml") else {
        return Ok(
            json!({"items":[],"rules":0,"total":0,"truncated":false,"summary":crate::localize!("尚未声明架构约束；可在 codexis.toml 配置 architecture.forbidden。","No architecture constraints declared; configure architecture.forbidden in codexis.toml.")}),
        );
    };
    let content = index.content(hash)?;
    let evidence = Evidence {
        path: "codexis.toml".into(),
        content_hash: hash.clone(),
        start_byte: 0,
        end_byte: content.len(),
        start_line: 1,
        start_column: 0,
        end_line: content.lines().count().max(1),
        end_column: 0,
    };
    let config: toml::Value = match toml::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            return Ok(
                json!({"items":[{"id":"constraint_config","title":crate::localize!("架构约束配置需要修正","Fix the architecture constraint configuration"),"summary":e.to_string(),"basis":"configuration parse error","evidence":[evidence],"node_ids":[]}],"rules":0,"total":1,"truncated":false}),
            )
        }
    };
    let rules = config
        .get("architecture")
        .and_then(|a| a.get("forbidden"))
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut compiled = Vec::new();
    let mut findings = Vec::new();
    for (position, rule) in rules.iter().enumerate() {
        let from = rule.get("from").and_then(toml::Value::as_str).unwrap_or("");
        let to = rule.get("to").and_then(toml::Value::as_str).unwrap_or("");
        match (Glob::new(from),Glob::new(to)) {
            (Ok(a),Ok(b)) if !from.is_empty()&&!to.is_empty()=>compiled.push((position,a.compile_matcher(),b.compile_matcher(),rule.get("reason").and_then(toml::Value::as_str).unwrap_or(crate::localize!("用户声明的禁止依赖方向","Dependency direction forbidden by a user constraint")))),
            _=>findings.push(json!({"id":format!("constraint_config:{position}"),"title":crate::localize!("架构约束路径模式无效","Invalid architecture constraint path pattern"),"summary":format!("from={from}, to={to}"),"basis":"configuration parse error","evidence":[evidence],"node_ids":[]})),
        }
    }
    let mut stmt=index.connection.prepare("SELECT e.data,s.data,t.data FROM edges e JOIN nodes s ON s.snapshot=e.snapshot AND s.id=e.source JOIN nodes t ON t.snapshot=e.snapshot AND t.id=e.target WHERE e.snapshot=?1 AND e.kind IN ('calls','imports','type_usage') ORDER BY s.path,t.path,e.id")?;
    let rows = stmt.query_map([&snapshot.id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut total = findings.len();
    let mut checked = 0;
    for row in rows {
        crate::source::check_cancelled()?;
        let (edge, source, target) = row?;
        let edge: Edge = serde_json::from_str(&edge)?;
        let source: Node = serde_json::from_str(&source)?;
        let target: Node = serde_json::from_str(&target)?;
        if !matches!(
            edge.resolution.as_str(),
            "resolved" | "semantic" | "static" | "interface" | "virtual"
        ) {
            continue;
        }
        checked += 1;
        for (position, from, to, reason) in &compiled {
            if !from.is_match(&source.evidence.path) || !to.is_match(&target.evidence.path) {
                continue;
            }
            total += 1;
            if findings.len() < 100 {
                findings.push(json!({"id":format!("constraint:{position}:{}",edge.id),"title":crate::localize!("架构约束冲突：{} → {}","Architecture constraint conflict: {} → {}",source.qualified_name,target.qualified_name),"summary":reason,"basis":"explicit user constraint and indexed known-target relation","priority":"high","dimensions":["architecture","verification"],"rule":position,"conditions":edge.conditions,"resolution":edge.resolution,"evidence":[evidence,edge.evidence,target.evidence],"node_ids":[source.id,target.id],"snapshot_id":snapshot.id}));
            }
        }
    }
    Ok(
        json!({"items":findings,"rules":rules.len(),"total":total,"truncated":total>100,"checked_relations":checked,"summary":crate::localize!("{} 条声明约束；{checked} 条已知目标关系；{total} 项需核查。","{} declared constraints; {checked} known-target relations; {total} review items.",rules.len()),"limitations":[crate::localize!("只检查已有确定目标的静态关系；未解析调用、动态行为和未索引代码不在检查范围。","Only indexed known-target static relations are checked; unresolved calls, dynamic behavior, and unindexed code remain outside the scope.")]}),
    )
}
