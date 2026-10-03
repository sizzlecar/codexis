use codexis::frontend::{rust::RustFrontend, FileContext, LanguageFrontend};
use codexis::model::{Evidence, FileFacts};
use codexis::source::{allowed_path, GitSource, SourceFile, SourceProvider, WorkingTreeSource};
use std::{fs, path::Path, process::Command};
use tempfile::TempDir;

fn write(root: &Path, path: &str, content: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn project_evidence_includes_docs_interfaces_deployment_and_python() {
    let root = TempDir::new().unwrap();
    let included = [
        "README.md",
        "docs/architecture.rst",
        "docs/contracts.txt",
        "config/service.yaml",
        "config/settings.json",
        "migrations/schema.sql",
        "api/service.proto",
        "Dockerfile",
        "Dockerfile.production",
        "compose.yml",
        ".github/workflows/check.yml",
        ".circleci/config.yml",
        ".cargo/config",
        "pyproject.toml",
        "setup.cfg",
        "setup.py",
        "requirements-dev.txt",
        "app/main.py",
        "app/interfaces.pyi",
        "Cargo.lock",
    ];
    for path in included {
        write(root.path(), path, "project evidence\n");
        assert!(allowed_path(path), "missing evidence: {path}");
    }
    for path in [
        "events.txt",
        "package-lock.json",
        "pnpm-lock.yaml",
        "vendor/schema.sql",
        "node_modules/config.json",
        ".venv/lib/site.py",
        "app/__pycache__/test.py",
        ".git/README.md",
        "vendor/.cargo/config",
    ] {
        write(root.path(), path, "omitted\n");
        assert!(!allowed_path(path), "unwanted evidence: {path}");
    }
    write(root.path(), ".gitignore", "ignored.yaml\n");
    write(root.path(), "ignored.yaml", "ignored: true\n");
    let sources = WorkingTreeSource {
        root: root.path().into(),
    }
    .snapshot()
    .unwrap();
    for path in included {
        assert!(sources.files.contains_key(path), "not captured: {path}");
    }
    assert!(!sources.files.contains_key("ignored.yaml"));
    assert!(!sources.files.contains_key("events.txt"));
    assert!(!sources.files.contains_key(".venv/lib/site.py"));
}

#[test]
fn evidence_capture_retains_size_and_symlink_limits() {
    let root = TempDir::new().unwrap();
    let big = fs::File::create(root.path().join("large.json")).unwrap();
    big.set_len(32 * 1024 * 1024 + 1).unwrap();
    #[cfg(unix)]
    {
        let outside = TempDir::new().unwrap();
        write(outside.path(), "external.md", "outside\n");
        std::os::unix::fs::symlink(
            outside.path().join("external.md"),
            root.path().join("link.md"),
        )
        .unwrap();
        let sources = WorkingTreeSource {
            root: root.path().into(),
        }
        .snapshot()
        .unwrap();
        assert!(!sources.files.contains_key("link.md"));
    }
    let sources = WorkingTreeSource {
        root: root.path().into(),
    }
    .snapshot()
    .unwrap();
    assert!(!sources.files.contains_key("large.json"));
    assert!(sources
        .diagnostics
        .iter()
        .any(|d| d.code == "source_too_large"));
}

#[test]
fn git_capture_uses_the_same_expanded_evidence_paths() {
    let root = TempDir::new().unwrap();
    for (path, contents) in [
        ("README.md", "architecture"),
        (".github/workflows/ci.yml", "jobs: {}"),
        ("app/main.py", "def main(): pass"),
        ("api/service.proto", "syntax = \"proto3\";"),
    ] {
        write(root.path(), path, contents);
    }
    for args in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Evidence Test",
            "-c",
            "user.email=evidence@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    ] {
        let output = Command::new("git")
            .arg("-C")
            .arg(root.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let git = GitSource {
        root: root.path().into(),
        revision: "HEAD".into(),
    }
    .snapshot()
    .unwrap();
    let worktree = WorkingTreeSource {
        root: root.path().into(),
    }
    .snapshot()
    .unwrap();
    assert_eq!(
        git.files.keys().collect::<Vec<_>>(),
        worktree.files.keys().collect::<Vec<_>>()
    );
}

fn rust_facts(content: &str) -> (SourceFile, FileFacts) {
    let file = SourceFile::new("src/lib.rs".into(), content.into());
    let context = FileContext {
        path: file.path.clone(),
        package: "demo".into(),
        unit: "demo:lib".into(),
        module: "demo".into(),
        conditions: Vec::new(),
        is_test: false,
        linked: true,
    };
    let facts = RustFrontend.parse(&file, &context).unwrap();
    assert!(facts.diagnostics.is_empty(), "{:?}", facts.diagnostics);
    (file, facts)
}

#[test]
fn rust_data_facts_preserve_declared_types_state_candidates_and_test_context() {
    let (file, facts) = rust_facts(
        r#"
/// Request payload and its state.
#[derive(Clone)]
pub struct Payload { pub id: u64, state: State }
pub enum State { Ready, Running(Payload), Done { result: String } }
pub struct Tuple(pub String, #[cfg(test)] usize);
pub type ResultData = Result<Payload, String>;
pub fn handle(input: &Payload, flag: bool) -> ResultData { helper(); todo!() }
fn helper() {}
#[cfg(test)]
mod tests { struct TestData { test_only: String } }
"#,
    );
    let payload = facts.nodes.iter().find(|n| n.name == "Payload").unwrap();
    assert!(payload.attributes["rust.doc"].contains("Request payload"));
    let docs: Vec<Evidence> =
        serde_json::from_str(&payload.attributes["rust.doc_evidence"]).unwrap();
    assert_eq!(
        &file.content[docs[0].start_byte..docs[0].end_byte],
        "/// Request payload and its state.\n"
    );
    let id = facts
        .nodes
        .iter()
        .find(|n| n.qualified_name == "demo::Payload::id")
        .unwrap();
    assert_eq!(id.kind, "field");
    assert_eq!(id.attributes["rust.declared_type"], "u64");
    assert_eq!(id.parent.as_deref(), Some(payload.id.as_str()));
    assert_eq!(id.visibility, "pub");
    let running = facts
        .nodes
        .iter()
        .find(|n| n.qualified_name == "demo::State::Running")
        .unwrap();
    assert_eq!(running.kind, "variant");
    assert_eq!(running.attributes["rust.state_candidate"], "true");
    assert!(facts
        .nodes
        .iter()
        .any(|n| n.qualified_name == "demo::State::Running::0" && n.kind == "field"));
    assert!(facts
        .nodes
        .iter()
        .any(|n| n.qualified_name == "demo::State::Done::result" && n.kind == "field"));
    let tuple = facts
        .nodes
        .iter()
        .find(|n| n.qualified_name == "demo::Tuple::0")
        .unwrap();
    assert_eq!(tuple.visibility, "pub");
    assert_eq!(
        &file.content[tuple.evidence.start_byte..tuple.evidence.end_byte],
        "pub String"
    );
    assert!(facts
        .nodes
        .iter()
        .any(|n| n.qualified_name == "demo::Tuple::1" && n.is_test));
    assert!(facts
        .nodes
        .iter()
        .any(|n| n.name == "test_only" && n.is_test));
    let handle = facts.nodes.iter().find(|n| n.name == "handle").unwrap();
    assert_eq!(handle.attributes["rust.return_type"], "ResultData");
    assert_eq!(
        handle.attributes["rust.parameters"],
        "(input: &Payload, flag: bool)"
    );
    let uses: Vec<_> = facts
        .edges
        .iter()
        .filter(|e| e.kind == "type_usage")
        .collect();
    assert!(uses
        .iter()
        .any(|e| e.source == handle.id && e.target_name == "&Payload"));
    for edge in uses {
        assert!(edge.target.is_none());
        assert_eq!(edge.resolution, "unresolved");
        assert_eq!(
            edge.target_name,
            file.content[edge.evidence.start_byte..edge.evidence.end_byte]
        );
        assert_eq!(edge.evidence.content_hash, file.hash);
    }
    assert_eq!(
        facts
            .edges
            .iter()
            .filter(|e| e.kind == "calls" && e.source == handle.id)
            .count(),
        1
    );
}

#[test]
fn explicit_route_syntax_has_precise_evidence_and_no_runtime_claim() {
    let (file, facts) = rust_facts(
        r##"
fn router() -> Router {
    Router::new().route("/users", get(list_users)).route(r#"/users/{id}"#, post(update_user))
}
fn list_users() {}
fn update_user() {}
fn other() { nested_route(); route("/not-method", handler); custom.route(dynamic_path, handler); }
"##,
    );
    let router = facts.nodes.iter().find(|n| n.name == "router").unwrap();
    let routes: Vec<serde_json::Value> =
        serde_json::from_str(&router.attributes["rust.route_bindings"]).unwrap();
    assert_eq!(routes.len(), 2);
    for route in routes {
        assert_eq!(route["resolution"], "unresolved");
        let path: Evidence = serde_json::from_value(route["path_evidence"].clone()).unwrap();
        assert_eq!(
            route["path_literal"],
            file.content[path.start_byte..path.end_byte]
        );
        let handler: Evidence = serde_json::from_value(route["handler_evidence"].clone()).unwrap();
        assert_eq!(
            route["handler_expression"],
            file.content[handler.start_byte..handler.end_byte]
        );
    }
    assert!(!facts
        .nodes
        .iter()
        .find(|n| n.name == "other")
        .unwrap()
        .attributes
        .contains_key("rust.route_bindings"));
    assert!(facts.edges.iter().all(|e| e.target.is_none()));
}
