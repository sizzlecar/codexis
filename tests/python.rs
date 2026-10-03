use codexis::{
    frontend::{python::PythonFrontend, LanguageFrontend},
    model::AnalysisContext,
    project::{python::PythonProjectAdapter, ProjectAdapter},
    semantic::{python::PythonSemanticResolver, SemanticResolver},
    source::{SourceProvider, SourceSet, WorkingTreeSource},
};
use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}
fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn fixture() -> TempDir {
    let root = TempDir::new().unwrap();
    write(
        root.path(),
        "pyproject.toml",
        r#"[project]
name = "workflow-demo"
version = "0.1.0"
description = "Processes submissions through a validation boundary"
requires-python = ">=3.10"
dependencies = ["httpx>=0.27; python_version >= '3.10'"]
[project.optional-dependencies]
test = ["pytest>=8"]
"#,
    );
    write(
        root.path(),
        "requirements-dev.txt",
        "pytest>=8\n-r requirements.txt\n",
    );
    write(
        root.path(),
        "README.md",
        "# Workflow\nSubmission processing separates validation, state and HTTP entry points.\n",
    );
    write(
        root.path(),
        "docs/architecture.md",
        "# Architecture\nThe service owns requests; engine validates data before persistence.\n",
    );
    write(root.path(), "config.yaml", "timeout: 15\nbackend: sqlite\n");
    // If discovery accidentally executes setup.py this leaves a visible marker.
    write(
        root.path(),
        "setup.py",
        "from pathlib import Path\nPath('EXECUTED_SETUP').write_text('unsafe')\n",
    );
    write(
        root.path(),
        "src/workflow/engine.py",
        r#""""Validation and state boundary."""
LIMIT: int = 8
class State:
    count: int = 0
    def __init__(self):
        self.items = []
    def save(self, value: str) -> None:
        self.items.append(value)

def validate(value: str) -> str:
    """Reject empty submissions."""
    if not value:
        raise ValueError("empty")
    return value.strip()

def process(value: str, state: State) -> str:
    valid = validate(value)
    state.save(valid)
    return valid
"#,
    );
    write(
        root.path(),
        "src/workflow/service.py",
        r#"from .engine import process as run
import workflow.engine as engine

def submit(value: str, state):
    return run(value, state)

def second(value: str, state):
    return engine.process(value, state)

def shadow(run, value, state):
    return run(value, state)

@app.post("/submit")
async def route(value: str, state):
    return submit(value, state)

if __name__ == "__main__":
    submit("example", None)
"#,
    );
    write(
        root.path(),
        "src/workflow/__main__.py",
        "from .service import submit\nsubmit('example', None)\n",
    );
    write(root.path(), "tests/test_workflow.py", "from workflow.engine import validate\ndef test_valid():\n    assert validate('ok') == 'ok'\n");
    write(root.path(), "legacy/App.java", "class App {}\n");
    git(root.path(), &["init", "-q"]);
    git(
        root.path(),
        &["config", "user.email", "fixture@example.invalid"],
    );
    git(root.path(), &["config", "user.name", "Python fixture"]);
    git(root.path(), &["add", "."]);
    git(root.path(), &["commit", "-qm", "initial"]);
    root
}
fn cli(root: &Path, cache: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_codexis"))
        .arg("--project")
        .arg(root)
        .arg("--cache-dir")
        .arg(cache)
        .arg("--format")
        .arg("json")
        .args(args)
        .output()
        .unwrap()
}
fn report(root: &Path, cache: &Path, args: &[&str]) -> Value {
    let output = cli(root, cache, args);
    assert!(
        matches!(output.status.code(), Some(0 | 3)),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("{args:?}: {e}: {}", String::from_utf8_lossy(&output.stdout)))
}

#[test]
fn python_cli_project_understanding_trace_and_review_workflow() {
    let root = fixture();
    let cache = TempDir::new().unwrap();
    let overview = report(
        root.path(),
        cache.path(),
        &["analyze", "--analysis", "semantic"],
    );
    assert_eq!(overview["analysis_context"]["language"], "python");
    assert_eq!(
        overview["data"]["project"]["packages"][0]["name"],
        "workflow-demo"
    );
    assert!(
        overview["data"]["stats"]["resolved_calls"]
            .as_u64()
            .unwrap()
            >= 5
    );
    assert!(overview["data"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["attributes"]["entry_kind"] == "python_main"));
    assert!(!root.path().join("EXECUTED_SETUP").exists());
    let packages = report(root.path(), cache.path(), &["map"]);
    assert_eq!(
        packages["data"]["items"][0]["package"]["language"],
        "python"
    );
    let modules = report(root.path(), cache.path(), &["map", "--level", "module"]);
    assert!(modules["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["qualified_name"] == "workflow.engine"));

    let inspect = report(
        root.path(),
        cache.path(),
        &["inspect", "workflow.engine.validate"],
    );
    assert_eq!(inspect["data"]["node"]["attributes"]["return_type"], "str");
    assert_eq!(
        inspect["data"]["node"]["attributes"]["doc"],
        "Reject empty submissions."
    );
    let trace = report(
        root.path(),
        cache.path(),
        &["trace", "workflow.service.submit", "--depth", "3"],
    );
    assert!(trace["data"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["qualified_name"] == "workflow.engine.validate"));
    assert!(trace["data"]["edges"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["target_name"] == "state.save" && e["target"].is_null()));
    let shadow = report(
        root.path(),
        cache.path(),
        &["trace", "workflow.service.shadow"],
    );
    assert!(shadow["data"]["edges"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["target"].is_null()));
    let understand = report(root.path(), cache.path(), &["understand"]);
    assert_eq!(
        understand["data"]["understanding"]["dimensions"]
            .as_array()
            .unwrap()
            .len(),
        8
    );

    let marker = report(
        root.path(),
        cache.path(),
        &[
            "mark",
            "workflow.engine.validate",
            "--state",
            "seen",
            "--note",
            "Validation boundary reviewed",
        ],
    );
    assert!(marker["snapshot_id"].is_string());
    let path = root.path().join("src/workflow/engine.py");
    let source = fs::read_to_string(&path).unwrap();
    fs::write(
        path,
        source.replace("return value.strip()", "return value.strip().lower()"),
    )
    .unwrap();
    let review = report(
        root.path(),
        cache.path(),
        &[
            "review",
            "--base",
            "HEAD",
            "--worktree",
            "--analysis",
            "semantic",
        ],
    );
    assert!(review["data"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(
            |change| change["after"]["qualified_name"] == "workflow.engine.validate"
                && change["state"] == "modified"
        ));
    assert!(review["data"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|change| change["current_impacts"]
            .as_array()
            .is_some_and(|impacts| impacts
                .iter()
                .any(|impact| impact["caller"]["qualified_name"] == "workflow.service.submit"))));
    assert!(!root.path().join("EXECUTED_SETUP").exists());
    let rejected = cli(
        root.path(),
        cache.path(),
        &["analyze", "--language", "java"],
    );
    assert_eq!(rejected.status.code(), Some(2));
    let rust_only = cli(
        root.path(),
        cache.path(),
        &["analyze", "--language", "python", "--features", "fake"],
    );
    assert_eq!(rust_only.status.code(), Some(2));
}

#[test]
fn python_static_binding_evidence_and_scope_budget_boundaries() {
    let root = fixture();
    let source: SourceSet = WorkingTreeSource {
        root: root.path().into(),
    }
    .snapshot()
    .unwrap();
    let project = PythonProjectAdapter.discover(&source).unwrap();
    assert!(project.packages[0]
        .dependencies
        .iter()
        .any(|d| d.alias == "httpx" && d.condition.is_some()));
    let plan = PythonFrontend.plan(&source, &project, None).unwrap();
    let mut facts = codexis::model::FileFacts::default();
    for context in plan {
        let parsed = PythonFrontend
            .parse(&source.files[&context.path], &context)
            .unwrap();
        facts.nodes.extend(parsed.nodes);
        facts.edges.extend(parsed.edges);
        facts.diagnostics.extend(parsed.diagnostics);
    }
    assert!(facts
        .nodes
        .iter()
        .any(|n| n.kind == "field" && n.qualified_name == "workflow.engine.State.items"));
    assert!(facts
        .nodes
        .iter()
        .any(|n| n.is_test && n.qualified_name == "tests.test_workflow.test_valid"));
    assert!(facts.nodes.iter().any(|n| n
        .attributes
        .get("entry_kind")
        .is_some_and(|v| v == "http_route")));
    for edge in &facts.edges {
        let file = &source.files[&edge.evidence.path];
        assert_eq!(file.hash, edge.evidence.content_hash);
        assert!(edge.evidence.end_byte <= file.content.len());
        if edge.kind == "calls" {
            assert_eq!(
                &file.content[edge.evidence.start_byte..edge.evidence.end_byte],
                edge.target_name
            );
        }
    }
    let mut context = AnalysisContext::python();
    context.analysis = "semantic".into();
    context.scope = Some("workflow.service.submit".into());
    let outcome = PythonSemanticResolver
        .enrich(&source, &project, &context, &mut facts, false)
        .unwrap();
    assert!(!outcome.partial);
    let submit = facts
        .nodes
        .iter()
        .find(|n| n.qualified_name == "workflow.service.submit")
        .unwrap();
    assert!(facts
        .edges
        .iter()
        .any(|e| e.source == submit.id && e.target_name == "run" && e.target.is_some()));
    assert!(facts
        .edges
        .iter()
        .filter(|e| e.source != submit.id && e.kind == "calls")
        .all(|e| e.target.is_none()));

    context.scope = None;
    context.semantic_request_limit = 1;
    let outcome = PythonSemanticResolver
        .enrich(&source, &project, &context, &mut facts, false)
        .unwrap();
    assert!(
        outcome.partial
            && outcome
                .diagnostics
                .iter()
                .any(|d| d.code == "semantic_budget")
    );
}
