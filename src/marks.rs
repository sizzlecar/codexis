use crate::analysis::now_ms;
use crate::index::Index;
use crate::model::{identity, Node, Snapshot};
use anyhow::{bail, Result};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

fn evidence_fingerprint(index: &Index, snapshot: &Snapshot, node: &Node) -> Result<String> {
    let mut dependencies = vec![
        node.fingerprint.clone(),
        node.evidence.content_hash.clone(),
        serde_json::to_string(&snapshot.context)?,
    ];
    let mut incomplete = false;
    for incoming in [false, true] {
        let edges = index.connected_edges(&snapshot.id, &node.id, incoming, 1001)?;
        if edges.len() > 1000 {
            incomplete = true;
        }
        for edge in edges {
            if matches!(
                edge.resolution.as_str(),
                "interface" | "virtual" | "ambiguous" | "navigation_only"
            ) {
                incomplete = true;
            }
            dependencies.push(serde_json::to_string(&edge)?);
            let related = if incoming {
                Some(edge.source.as_str())
            } else {
                edge.target.as_deref()
            };
            if let Some(id) = related {
                if let Some(other) = index.find_nodes(&snapshot.id, id, 1)?.first() {
                    dependencies.push(other.fingerprint.clone());
                }
            } else if edge.resolution != "external" {
                incomplete = true;
            }
        }
    }
    if incomplete || snapshot.completeness.status != "complete" {
        dependencies.push(snapshot.id.clone());
    }
    let files = index.file_hashes(&snapshot.id)?;
    for (path, hash) in files.iter().filter(|(p, _)| {
        p.ends_with("Cargo.toml")
            || p.ends_with("Cargo.lock")
            || p.contains(".cargo/")
            || p.ends_with("pyproject.toml")
            || p.ends_with("requirements.txt")
            || p.ends_with("setup.cfg")
            || p.ends_with("codexis.toml")
    }) {
        dependencies.extend([path.clone(), hash.clone()]);
    }
    dependencies.sort();
    Ok(identity(
        &dependencies.iter().map(String::as_str).collect::<Vec<_>>(),
    ))
}

pub fn set(
    index: &Index,
    snapshot: &Snapshot,
    node: &Node,
    state: &str,
    note: &str,
) -> Result<Value> {
    if !["unread", "seen", "question"].contains(&state) {
        bail!("mark state must be unread, seen or question");
    }
    if note.chars().count() > 4096 {
        bail!("review note must not exceed 4096 characters");
    }
    if snapshot.completeness.stale {
        bail!("cannot mark a stale working snapshot; refresh analyze or select an immutable historical snapshot");
    }
    let fingerprint = evidence_fingerprint(index, snapshot, node)?;
    index.connection.execute("INSERT OR REPLACE INTO marks(stable_key,snapshot,fingerprint,state,note,updated) VALUES(?1,?2,?3,?4,?5,?6)",params![node.stable_key,snapshot.id,fingerprint,state,note,now_ms()])?;
    Ok(
        json!({"kind":"mark","node_id":node.id,"qualified_name":node.qualified_name,"mark":get(index,snapshot,node)?}),
    )
}

pub fn get(index: &Index, snapshot: &Snapshot, node: &Node) -> Result<Value> {
    let stored: Option<(String, String, String, String, u64)> = index
        .connection
        .query_row(
            "SELECT snapshot,fingerprint,state,note,updated FROM marks WHERE stable_key=?1",
            [&node.stable_key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    if let Some((recorded_snapshot, fingerprint, state, note, updated)) = stored {
        let valid = !snapshot.completeness.stale
            && evidence_fingerprint(index, snapshot, node)? == fingerprint;
        Ok(
            json!({"state":if valid {state.as_str()} else {"needs_review"},"previous_state":state,"note":note,
            "valid":valid,"recorded_snapshot":recorded_snapshot,"updated_at_ms":updated,"evidence_scope":"node and direct relations"}),
        )
    } else {
        Ok(json!({"state":"unread","valid":true,"evidence_scope":"node and direct relations"}))
    }
}
