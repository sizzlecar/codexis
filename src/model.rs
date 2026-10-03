use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: u32 = 1;
pub const ANALYZER_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), ":5");

pub fn digest(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}

pub fn identity(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnalysisContext {
    pub language: String,
    pub analysis: String,
    pub analyzer_version: String,
    pub target: Option<String>,
    pub features: Vec<String>,
    pub no_default_features: bool,
    pub scope: Option<String>,
    pub build_scripts: bool,
    pub proc_macros: bool,
    #[serde(default)]
    pub semantic_timeout_secs: u64,
    #[serde(default)]
    pub semantic_request_limit: usize,
}

impl AnalysisContext {
    pub fn python() -> Self {
        Self {
            language: "python".into(),
            ..Self::rust()
        }
    }

    pub fn rust() -> Self {
        Self {
            language: "rust".into(),
            analysis: "syntax".into(),
            analyzer_version: ANALYZER_VERSION.into(),
            semantic_timeout_secs: 180,
            semantic_request_limit: 5000,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
    pub path: Option<String>,
    pub severity: String,
}

impl Diagnostic {
    pub fn warning(code: &str, message: impl Into<String>, path: Option<&str>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            path: path.map(str::to_owned),
            severity: "warning".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Capability {
    pub name: String,
    pub provider: String,
    pub scope: String,
    pub limitations: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProjectModel {
    pub kind: String,
    pub packages: Vec<Package>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Package {
    pub id: String,
    pub name: String,
    pub root: String,
    pub language: String,
    pub edition: String,
    pub units: Vec<CompilationUnit>,
    pub dependencies: Vec<Dependency>,
    pub features: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompilationUnit {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub source: String,
    pub required_features: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Dependency {
    pub alias: String,
    pub package: String,
    pub kind: String,
    pub path: Option<String>,
    pub condition: Option<String>,
    pub optional: bool,
    pub resolution: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Evidence {
    pub path: String,
    pub content_hash: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub stable_key: String,
    pub name: String,
    pub qualified_name: String,
    pub kind: String,
    pub language: String,
    pub package: String,
    pub unit: String,
    pub parent: Option<String>,
    pub visibility: String,
    pub signature: String,
    pub fingerprint: String,
    pub evidence: Evidence,
    pub conditions: Vec<String>,
    pub is_test: bool,
    pub attributes: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Edge {
    pub id: String,
    pub source: String,
    pub target: Option<String>,
    pub target_name: String,
    pub kind: String,
    pub resolution: String,
    pub evidence: Evidence,
    pub conditions: Vec<String>,
    pub provider: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FileFacts {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AnalysisStats {
    pub discovered_files: usize,
    pub source_files: usize,
    pub parsed_files: usize,
    pub reused_files: usize,
    pub failed_files: usize,
    pub source_bytes: usize,
    pub source_lines: usize,
    pub nodes: usize,
    pub edges: usize,
    pub resolved_calls: usize,
    pub unresolved_calls: usize,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Completeness {
    pub status: String,
    pub capabilities: Vec<Capability>,
    pub unresolved: usize,
    pub truncated: bool,
    pub next_cursor: Option<String>,
    pub stale: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub project_root: String,
    pub source_revision: String,
    pub created_at_ms: u64,
    pub context: AnalysisContext,
    pub project: ProjectModel,
    pub stats: AnalysisStats,
    pub completeness: Completeness,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report<T> {
    pub schema_version: u32,
    #[serde(default)]
    pub locale: String,
    pub snapshot_id: String,
    pub analysis_context: AnalysisContext,
    pub data: T,
    pub diagnostics: Vec<Diagnostic>,
    pub completeness: Completeness,
}

impl<T> Report<T> {
    pub fn new(snapshot: &Snapshot, data: T) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            locale: crate::i18n::current().tag().into(),
            snapshot_id: snapshot.id.clone(),
            analysis_context: snapshot.context.clone(),
            data,
            diagnostics: snapshot.diagnostics.clone(),
            completeness: snapshot.completeness.clone(),
        }
    }
}
