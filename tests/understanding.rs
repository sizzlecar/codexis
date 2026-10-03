//! Real captured-source -> frontend -> index -> eight-view integration checks.
use codexis::{
    analysis,
    index::Index,
    model::{AnalysisContext, Evidence},
    source::{SourceProvider, WorkingTreeSource},
    understanding,
};
use serde_json::Value;
use std::{fs, path::Path};
use tempfile::TempDir;

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn dimension<'a>(view: &'a Value, id: &str) -> &'a Value {
    view["dimensions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == id)
        .unwrap()
}

fn verify_evidence(index: &Index, value: &Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                verify_evidence(index, item)
            }
        }
        Value::Object(map) => {
            if let Some(evidence) = map.get("evidence") {
                let spans = if evidence.is_array() {
                    evidence.as_array().unwrap().clone()
                } else {
                    vec![evidence.clone()]
                };
                for span in spans {
                    let span: Evidence = serde_json::from_value(span).unwrap();
                    let content = index.content(&span.content_hash).unwrap();
                    assert!(
                        span.start_byte <= span.end_byte && span.end_byte <= content.len(),
                        "{span:?}"
                    );
                    assert!(
                        content.is_char_boundary(span.start_byte)
                            && content.is_char_boundary(span.end_byte)
                    );
                    assert_eq!(
                        span.start_line,
                        content[..span.start_byte]
                            .bytes()
                            .filter(|b| *b == b'\n')
                            .count()
                            + 1
                    );
                }
            }
            for (key, child) in map {
                if key != "evidence" {
                    verify_evidence(index, child)
                }
            }
        }
        _ => {}
    }
}

#[test]
fn rust_single_package_has_eight_views_and_immutable_source_evidence() {
    let root = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    write(root.path(),"Cargo.toml","[package]\nname='engine'\nversion='0.1.0'\nedition='2021'\ndescription='Task processing engine'\n[features]\nnetwork=[]\n");
    write(
        root.path(),
        "README.md",
        "# Engine\n\nProcesses tasks through src/engine.rs.\n",
    );
    write(
        root.path(),
        "src/main.rs",
        "mod engine; mod storage; mod overflow;\nfn main() { engine::run(); }\nmod nested { pub fn main() {} }\n",
    );
    write(
        root.path(),
        "src/engine.rs",
        r#"//! Coordinates task execution.
pub enum Status { Ready, Running { attempts: u32 }, Done }
pub struct Task { pub state: Status }
pub fn run() { crate::storage::save(); unknown_backend(); }
pub fn accepts(task: Task) -> Status { task.state }
#[cfg(feature="network")]
pub fn network() {}
#[cfg(test)] mod tests { #[test] fn executes() { super::run(); } }
"#,
    );
    write(
        root.path(),
        "src/storage.rs",
        "//! Persists task data.\npub fn save() {}\n",
    );
    let overflow = (0..120)
        .map(|i| format!("pub struct Model{i} {{ pub state:u32 }}\n"))
        .collect::<String>();
    write(root.path(), "src/overflow.rs", &overflow);
    write(root.path(), "config/service.yaml", "port: 8080\n");
    write(
        root.path(),
        "schema/task.sql",
        "CREATE TABLE tasks (id INTEGER PRIMARY KEY);\n",
    );
    write(root.path(), "Dockerfile", "FROM rust:1.85\n");
    let sources = WorkingTreeSource {
        root: root.path().into(),
    }
    .snapshot()
    .unwrap();
    let mut index = Index::open(root.path(), Some(cache.path())).unwrap();
    let snapshot = analysis::analyze(&mut index, &sources, AnalysisContext::rust(), false).unwrap();
    let view = understanding::build(&index, &snapshot).unwrap();
    assert_eq!(
        view["dimensions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        understanding::DIMENSION_IDS
    );
    let components = view["architecture"]["components"].as_array().unwrap();
    assert!(components.iter().any(|c| c["title"] == "engine / engine"));
    assert!(components.iter().any(|c| c["title"] == "engine / storage"));
    assert!(dimension(&view, "intent")["items"]
        .to_string()
        .contains("Coordinates task execution"));
    assert!(dimension(&view, "behavior")["items"]
        .to_string()
        .contains("unknown_backend"));
    let scenarios = dimension(&view, "behavior")["items"].as_array().unwrap();
    assert!(scenarios
        .iter()
        .any(|s| s["title"] == "engine::main" && s["entry_kind"] == "program"));
    assert!(
        scenarios
            .iter()
            .any(|s| s["title"] == "engine::nested::main"
                && s["entry_kind"] == "public_api_candidate")
    );
    assert!(dimension(&view, "data")["items"]
        .to_string()
        .contains("Status"));
    assert!(dimension(&view, "data")["items"]
        .to_string()
        .contains("state_candidate\":true"));
    assert!(dimension(&view, "data")["items"]
        .to_string()
        .contains("schema/task.sql"));
    assert!(dimension(&view, "runtime")["items"]
        .to_string()
        .contains("config/service.yaml"));
    assert!(dimension(&view, "runtime")["items"]
        .to_string()
        .contains("Dockerfile"));
    assert!(dimension(&view, "verification")["items"]
        .to_string()
        .contains("executes"));
    assert_eq!(dimension(&view, "changes")["total"], 0);
    assert_eq!(dimension(&view, "knowledge")["total"], 0);
    assert_eq!(dimension(&view, "data")["truncated"], true);
    let scoped = understanding::build_scoped(&index, &snapshot, Some("Model119")).unwrap();
    assert!(dimension(&scoped, "data")["items"]
        .to_string()
        .contains("Model119"));
    assert_eq!(dimension(&scoped, "data")["truncated"], false);
    verify_evidence(&index, &view);
    // Both cold aggregation and cache hits use the stored snapshot, even after
    // the working README has acquired a contradictory declaration.
    write(
        root.path(),
        "README.md",
        "Replaced live contents, outside this snapshot.\n",
    );
    let cached = understanding::build(&index, &snapshot).unwrap();
    assert_eq!(view, cached);
    index
        .connection
        .execute("DELETE FROM understanding_cache", [])
        .unwrap();
    assert_eq!(view, understanding::build(&index, &snapshot).unwrap());
}

#[test]
fn python_annotations_main_guard_docs_and_tests_join_same_snapshot() {
    let root = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    write(
        root.path(),
        "pyproject.toml",
        "[project]\nname='pipeline'\nversion='0.1.0'\ndescription='Python processing pipeline'\n",
    );
    write(
        root.path(),
        "worker.py",
        r#""""Processes one job."""
class Job:
    state: str
def execute(job: Job) -> str:
    """Executes the provided job."""
    return job.state
if __name__ == "__main__":
    execute(Job())
"#,
    );
    write(
        root.path(),
        "tests/test_worker.py",
        "from worker import execute, Job\ndef test_execution():\n    execute(Job())\n",
    );
    write(
        root.path(),
        "README.md",
        "# Pipeline\n\nworker.py implements the worker entry.\n",
    );
    let sources = WorkingTreeSource {
        root: root.path().into(),
    }
    .snapshot()
    .unwrap();
    let mut index = Index::open(root.path(), Some(cache.path())).unwrap();
    let mut context = AnalysisContext::rust();
    context.language = "python".into();
    context.analysis = "semantic".into();
    let snapshot = analysis::analyze(&mut index, &sources, context, false).unwrap();
    let view = understanding::build(&index, &snapshot).unwrap();
    assert_eq!(view["dimensions"].as_array().unwrap().len(), 8);
    assert!(dimension(&view, "intent")["items"]
        .to_string()
        .contains("Processes one job"));
    assert!(dimension(&view, "behavior")["items"]
        .to_string()
        .contains("python_main"));
    assert!(dimension(&view, "data")["items"]
        .to_string()
        .contains("Job"));
    assert!(dimension(&view, "verification")["items"]
        .to_string()
        .contains("test_execution"));
    assert!(view["architecture"]["dependencies_total"].as_u64().unwrap() > 0);
    assert!(dimension(&view, "verification")["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["related_nodes"]
            .as_array()
            .is_some_and(|nodes| nodes.iter().any(|n| n["name"] == "worker.execute"))));
    verify_evidence(&index, &view);
}
