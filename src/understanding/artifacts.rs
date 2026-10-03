use super::{item, short, stable_id};
use crate::{
    index::Index,
    model::{Evidence, Snapshot},
};
use anyhow::Result;
use serde_json::{json, Value};
use std::path::Path;

pub(super) struct Artifact {
    pub path: String,
    pub kind: &'static str,
    pub content: String,
    pub evidence: Evidence,
}

impl Artifact {
    pub fn value(&self) -> Value {
        json!({"path":self.path,"kind":self.kind,"evidence":self.evidence})
    }

    pub fn item(&self) -> Value {
        let (text, evidence) = self.excerpt();
        let mut value = item(
            stable_id("artifact", &self.path),
            &self.path,
            &text,
            "stored artifact excerpt",
            &[evidence],
            &[],
        );
        value["artifact_kind"] = json!(self.kind);
        value["path"] = json!(self.path);
        value
    }

    pub fn excerpt(&self) -> (String, Evidence) {
        let mut offset = 0;
        let mut fallback = None;
        for line in self.content.split_inclusive('\n') {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with("```") && !trimmed.starts_with("~~~") {
                let end = offset + line.trim_end_matches(['\n', '\r']).len();
                if self.kind == "documentation"
                    && (trimmed.starts_with('#')
                        || trimmed.chars().all(|c| matches!(c, '=' | '-' | '*' | ' ')))
                {
                    fallback.get_or_insert_with(|| {
                        (
                            short(trimmed, 400),
                            span(
                                &self.path,
                                &self.evidence.content_hash,
                                &self.content,
                                offset,
                                end,
                            ),
                        )
                    });
                    offset += line.len();
                    continue;
                }
                return (
                    short(trimmed, 400),
                    span(
                        &self.path,
                        &self.evidence.content_hash,
                        &self.content,
                        offset,
                        end,
                    ),
                );
            }
            offset += line.len();
        }
        fallback.unwrap_or_else(|| {
            (
                crate::localize!(
                    "空文件；没有可展示的声明。",
                    "Empty file; no declaration to display."
                )
                .into(),
                self.evidence.clone(),
            )
        })
    }

    pub fn evidence_for(&self, text: &str) -> Evidence {
        self.content
            .find(text)
            .map(|start| {
                span(
                    &self.path,
                    &self.evidence.content_hash,
                    &self.content,
                    start,
                    start + text.len(),
                )
            })
            .unwrap_or_else(|| self.evidence.clone())
    }
}

pub(super) fn load(index: &Index, snapshot: &Snapshot) -> Result<Vec<Artifact>> {
    let mut artifacts = Vec::new();
    for (path, hash) in index.file_hashes(&snapshot.id)? {
        let Some(kind) = classify(&path) else {
            continue;
        };
        crate::source::check_cancelled()?;
        let content = index.content(&hash)?;
        let end = content
            .char_indices()
            .nth(512)
            .map_or(content.len(), |(i, _)| i);
        let evidence = span(&path, &hash, &content, 0, end);
        artifacts.push(Artifact {
            path,
            kind,
            content,
            evidence,
        });
    }
    Ok(artifacts)
}

fn classify(path: &str) -> Option<&'static str> {
    let p = Path::new(path);
    let name = p.file_name()?.to_str()?.to_ascii_lowercase();
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(
        name.as_str(),
        "cargo.toml" | "pyproject.toml" | "setup.cfg" | "setup.py" | "requirements.txt" | "pipfile"
    ) || (name.starts_with("requirements") && ext == "txt")
    {
        return Some("manifest");
    }
    if matches!(
        name.as_str(),
        "cargo.lock" | "poetry.lock" | "uv.lock" | "pipfile.lock"
    ) {
        return Some("lockfile");
    }
    if matches!(ext.as_str(), "md" | "markdown" | "rst" | "adoc")
        || name.starts_with("readme")
        || (ext == "txt" && (path.starts_with("docs/") || path.contains("/docs/")))
    {
        return Some("documentation");
    }
    if matches!(ext.as_str(), "sql" | "proto" | "graphql" | "gql") {
        return Some("schema");
    }
    if name == "dockerfile"
        || name.starts_with("dockerfile.")
        || matches!(
            name.as_str(),
            "makefile" | "containerfile" | "jenkinsfile" | "justfile"
        )
        || name.contains("compose")
        || path.contains(".github/workflows/")
        || path.contains(".circleci/")
        || name.starts_with(".gitlab-ci")
        || path.starts_with("deploy/")
        || path.contains("/deploy/")
        || matches!(ext.as_str(), "sh" | "dockerfile")
    {
        return Some("deployment");
    }
    if matches!(
        ext.as_str(),
        "toml" | "yaml" | "yml" | "json" | "xml" | "ini" | "cfg" | "env"
    ) || name == ".env"
        || name.starts_with(".env.")
        || path.contains(".cargo/config")
    {
        return Some("configuration");
    }
    None
}

pub(super) fn span(path: &str, hash: &str, content: &str, start: usize, end: usize) -> Evidence {
    let start = start.min(content.len());
    let end = end.min(content.len()).max(start);
    let position = |byte: usize| {
        let before = &content[..byte];
        let line = before.bytes().filter(|b| *b == b'\n').count() + 1;
        let column = before
            .rsplit_once('\n')
            .map_or(before.len(), |(_, tail)| tail.len())
            + 1;
        (line, column)
    };
    let (start_line, start_column) = position(start);
    let (end_line, end_column) = position(end);
    Evidence {
        path: path.into(),
        content_hash: hash.into(),
        start_byte: start,
        end_byte: end,
        start_line,
        start_column,
        end_line,
        end_column,
    }
}
