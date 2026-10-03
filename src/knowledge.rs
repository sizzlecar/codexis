//! Human conclusions are append-only records bound to stored evidence.
use crate::{
    analysis::now_ms,
    index::Index,
    model::{identity, Evidence, Snapshot},
};
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::BTreeSet;

fn prepare(index: &Index) -> Result<()> {
    index.connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS knowledge_records (
        id TEXT NOT NULL, revision INTEGER NOT NULL, snapshot TEXT NOT NULL,
        dimension TEXT NOT NULL, title TEXT NOT NULL, claim TEXT NOT NULL,
        state TEXT NOT NULL, evidence TEXT NOT NULL, updated INTEGER NOT NULL,
        absent_paths TEXT NOT NULL DEFAULT '[]',
        PRIMARY KEY(id, revision));",
    )?;
    let columns = index
        .connection
        .prepare("PRAGMA table_info(knowledge_records)")?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !columns.iter().any(|c| c == "absent_paths") {
        index.connection.execute_batch(
            "ALTER TABLE knowledge_records ADD COLUMN absent_paths TEXT NOT NULL DEFAULT '[]'",
        )?;
    }
    Ok(())
}

pub struct Remember<'a> {
    pub query: &'a str,
    pub dimension: &'a str,
    pub claim: &'a str,
    pub state: &'a str,
    pub paths: &'a [String],
    pub record_id: Option<&'a str>,
}
pub fn remember(index: &Index, snapshot: &Snapshot, request: Remember<'_>) -> Result<Value> {
    let Remember {
        query,
        dimension,
        claim,
        state,
        paths,
        record_id,
    } = request;
    if snapshot.completeness.stale {
        bail!("cannot record a conclusion against a stale snapshot; run analyze first");
    }
    if !["confirmed", "question"].contains(&state) {
        bail!("knowledge state must be confirmed or question");
    }
    if claim.trim().is_empty() || claim.chars().count() > 4096 {
        bail!("a claim must contain 1–4096 characters");
    }
    if !crate::understanding::DIMENSION_IDS.contains(&dimension) {
        bail!("unknown dimension: {dimension}");
    }
    prepare(index)?;
    let hashes = index.file_hashes(&snapshot.id)?;
    let mut evidence = Vec::<Evidence>::new();
    let mut absent_paths = Vec::new();
    let mut evidence_paths: BTreeSet<String> = paths.iter().cloned().collect();
    let candidates = index.find_nodes(&snapshot.id, query, 2)?;
    let (title, subject) = if candidates.len() == 1 {
        let node = &candidates[0];
        evidence.push(node.evidence.clone());
        // Include direct relations and their known definitions, so a change to
        // the referenced interface also invalidates a saved conclusion.
        for incoming in [false, true] {
            for edge in index.connected_edges(&snapshot.id, &node.id, incoming, 1001)? {
                evidence_paths.insert(edge.evidence.path.clone());
                let related = if incoming {
                    Some(edge.source.as_str())
                } else {
                    edge.target.as_deref()
                };
                if let Some(target) = related {
                    if let Some(target) = index.find_nodes(&snapshot.id, target, 1)?.first() {
                        evidence_paths.insert(target.evidence.path.clone());
                    }
                }
            }
        }
        (node.qualified_name.clone(), node.stable_key.clone())
    } else if hashes.contains_key(query) {
        evidence_paths.insert(query.into());
        (query.into(), query.into())
    } else if let Some(id) = record_id {
        let stored = index.connection.query_row(
            "SELECT title,evidence FROM knowledge_records WHERE id=?1 ORDER BY revision DESC LIMIT 1",
            [id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .optional()?.context("knowledge record does not exist")?;
        let previous: Vec<Evidence> = serde_json::from_str(&stored.1)?;
        evidence_paths.extend(previous.into_iter().map(|e| e.path));
        (stored.0, id.into())
    } else {
        bail!(
            "remember requires one symbol ID or a snapshot file path; use inspect to disambiguate"
        );
    };
    evidence_paths.extend(
        hashes
            .keys()
            .filter(|p| {
                matches!(
                    p.as_str(),
                    "Cargo.toml" | "Cargo.lock" | "pyproject.toml" | "codexis.toml"
                )
            })
            .cloned(),
    );
    for path in evidence_paths {
        let Some(hash) = hashes.get(&path) else {
            if record_id.is_some() {
                absent_paths.push(path);
                continue;
            }
            bail!("evidence file is missing from this snapshot: {path}");
        };
        if evidence.iter().any(|e| e.path == path) {
            continue;
        }
        let content = index.content(hash)?;
        evidence.push(Evidence {
            path,
            content_hash: hash.clone(),
            start_byte: 0,
            end_byte: content.len(),
            start_line: 1,
            start_column: 0,
            end_line: content.lines().count().max(1),
            end_column: 0,
        });
    }
    evidence.sort_by(|a, b| a.path.cmp(&b.path));
    let id = record_id
        .map(str::to_owned)
        .unwrap_or_else(|| identity(&["knowledge:1", dimension, &subject]));
    if let Some(existing) = record_id {
        let exists: bool = index.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM knowledge_records WHERE id=?1)",
            [existing],
            |r| r.get(0),
        )?;
        if !exists {
            bail!("knowledge record does not exist: {existing}");
        }
    }
    let revision: u64 = index.connection.query_row(
        "SELECT COALESCE(MAX(revision),0)+1 FROM knowledge_records WHERE id=?1",
        [&id],
        |r| r.get(0),
    )?;
    index.connection.execute("INSERT INTO knowledge_records(id,revision,snapshot,dimension,title,claim,state,evidence,updated,absent_paths) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![id,revision,snapshot.id,dimension,title,claim,state,serde_json::to_string(&evidence)?,now_ms(),serde_json::to_string(&absent_paths)?])?;
    Ok(
        json!({"kind":"knowledge","record": list(index,snapshot,Some(&id),false)?[0],"history_preserved":true}),
    )
}

pub fn list(
    index: &Index,
    snapshot: &Snapshot,
    id: Option<&str>,
    history: bool,
) -> Result<Vec<Value>> {
    prepare(index)?;
    let hashes = index.file_hashes(&snapshot.id)?;
    let mut stmt = index.connection.prepare("SELECT id,revision,snapshot,dimension,title,claim,state,evidence,updated,absent_paths
        FROM knowledge_records k WHERE (?1 IS NULL OR id=?1) AND (?2 OR revision=(SELECT MAX(revision) FROM knowledge_records WHERE id=k.id)) ORDER BY updated DESC,id,revision DESC")?;
    let rows = stmt.query_map(params![id, history], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, u64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, String>(7)?,
            r.get::<_, u64>(8)?,
            r.get::<_, String>(9)?,
        ))
    })?;
    let mut values = Vec::new();
    for row in rows {
        let (
            id,
            revision,
            recorded_snapshot,
            dimension,
            title,
            claim,
            state,
            encoded,
            updated,
            absent,
        ) = row?;
        let evidence: Vec<Evidence> = serde_json::from_str(&encoded)?;
        let absent_paths: Vec<String> = serde_json::from_str(&absent)?;
        let mut changed: Vec<_> = evidence
            .iter()
            .filter(|e| hashes.get(&e.path) != Some(&e.content_hash))
            .map(|e| e.path.clone())
            .collect();
        changed.extend(
            absent_paths
                .iter()
                .filter(|p| hashes.contains_key(*p))
                .cloned(),
        );
        let valid = changed.is_empty() && !snapshot.completeness.stale;
        values.push(json!({"id":id,"revision":revision,"recorded_snapshot":recorded_snapshot,"dimension":dimension,
            "title":title,"summary":claim,"claim":claim,"state":if valid {state.as_str()} else {"needs_review"},
            "previous_state":state,"basis":if state=="question" {"user_question"} else {"user_confirmed"},"valid":valid,"changed_evidence":changed,"evidence":evidence,
            "absent_evidence":absent_paths,"updated_at_ms":updated,"evidence_scope":"selected source files, direct relations and project configuration; not all transitive behavior"}));
    }
    Ok(values)
}
