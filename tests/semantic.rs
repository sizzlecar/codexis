use codexis::analysis;
use codexis::index::Index;
use codexis::model::AnalysisContext;
use codexis::source::{git, GitSource, SourceProvider, WorkingTreeSource};
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn write(root: &Path, path: &str, content: &str) {
    let file = root.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, content).unwrap();
}

#[test]
#[ignore = "requires the rust-analyzer component; run cargo test --test semantic -- --ignored"]
fn real_rust_analyzer_resolves_aliases_reexports_methods_and_trait_interfaces() {
    let root = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    write(
        root.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['base','app']\nresolver='2'\n",
    );
    write(
        root.path(),
        "base/Cargo.toml",
        "[package]\nname='base'\nversion='0.1.0'\nedition='2021'\n",
    );
    write(root.path(), "base/src/lib.rs", "pub fn service() {}\n");
    write(root.path(),"app/Cargo.toml","[package]\nname='app'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nbase={path='../base'}\n");
    write(
        root.path(),
        "app/build.rs",
        "fn main() { std::fs::write(\"build-script-ran\", b\"unexpected\").unwrap(); }\n",
    );
    write(
        root.path(),
        "app/src/lib.rs",
        r#"
use base::service as alias;
pub use base::service as exported;
mod inner { pub fn entry() { super::alias(); } }
mod unrelated { pub fn service() {} }
pub struct Unit;
impl Unit { pub fn execute(&self) {} }
impl std::fmt::Display for Unit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("unit") }
}
pub trait Engine { fn infer(&self); }
impl Engine for Unit { fn infer(&self) {} }
pub struct Other;
impl Engine for Other { fn infer(&self) {} }
pub fn cycle_a() { cycle_b(); }
pub fn cycle_b() { cycle_a(); }
pub fn dynamic(engine: &dyn Engine) { engine.infer(); }
pub fn orchestrate() {
    let _message = "😀你好"; alias();
    exported();
    inner::entry();
    let unit = Unit;
    unit.execute();
    let _ = unit.to_string();
    dynamic(&unit);
}
"#,
    );
    let output = Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let sources = WorkingTreeSource {
        root: root.path().into(),
    }
    .snapshot()
    .unwrap();
    let mut index = Index::open(root.path(), Some(cache.path())).unwrap();
    let mut context = AnalysisContext::rust();
    context.analysis = "semantic".into();
    context.semantic_timeout_secs = 90;
    let snapshot = analysis::analyze(&mut index, &sources, context, true).unwrap();
    assert_eq!(
        snapshot.completeness.status, "complete",
        "{:#?}",
        snapshot.diagnostics
    );
    let nodes = index.all_nodes(&snapshot.id).unwrap();
    let edges = index.all_edges(&snapshot.id).unwrap();
    let name = |id: &str| {
        nodes
            .iter()
            .find(|n| n.id == id)
            .unwrap()
            .qualified_name
            .clone()
    };
    for (call, expected) in [
        ("alias", "base::service"),
        ("exported", "base::service"),
        ("inner::entry", "app::inner::entry"),
        ("unit.execute", "app::<Unit>::execute"),
    ] {
        let edge = edges
            .iter()
            .find(|e| e.target_name == call && name(&e.source) == "app::orchestrate")
            .unwrap();
        assert_eq!(
            edge.resolution, "semantic",
            "{edge:#?}\n{:#?}",
            snapshot.diagnostics
        );
        assert_eq!(name(edge.target.as_deref().unwrap()), expected);
    }
    let dynamic = edges
        .iter()
        .find(|e| e.target_name == "engine.infer")
        .unwrap();
    assert_eq!(dynamic.resolution, "interface", "{dynamic:#?}");
    assert_eq!(
        name(dynamic.target.as_deref().unwrap()),
        "app::Engine::infer"
    );
    let navigation = edges
        .iter()
        .find(|e| e.target_name == "unit.to_string")
        .unwrap();
    assert!(
        navigation.target.is_none(),
        "a navigation redirect to fmt is not a direct call edge: {navigation:?}"
    );
    assert!(!root.path().join("app/build-script-ran").exists());
    let after = WorkingTreeSource {
        root: root.path().into(),
    }
    .snapshot()
    .unwrap();
    assert_eq!(
        sources.content_id(),
        after.content_id(),
        "semantic analysis must not modify the source workspace"
    );
    let cycle = codexis::query::trace(&index, &snapshot, "cycle_a", false, 10, 100).unwrap();
    assert_eq!(cycle.data["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(cycle.data["edges"].as_array().unwrap().len(), 2);
    assert!(!cycle.completeness.truncated);
    let callers = codexis::query::trace(&index, &snapshot, "base::service", true, 2, 100).unwrap();
    assert!(callers.data["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["qualified_name"] == "app::orchestrate"));
    assert!(callers.data["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["qualified_name"] == "app::inner::entry"));
    let bounded = codexis::query::trace(&index, &snapshot, "orchestrate", false, 1, 2).unwrap();
    assert!(bounded.completeness.truncated);
    assert!(bounded.data["nodes"].as_array().unwrap().len() <= 2);
    let caller = index
        .find_nodes(&snapshot.id, "orchestrate", 1)
        .unwrap()
        .remove(0);
    codexis::marks::set(&index, &snapshot, &caller, "seen", "Reviewed direct calls").unwrap();
    let dynamic_caller = index
        .find_nodes(&snapshot.id, "dynamic", 1)
        .unwrap()
        .remove(0);
    codexis::marks::set(
        &index,
        &snapshot,
        &dynamic_caller,
        "seen",
        "Interface-only dispatch",
    )
    .unwrap();
    git(root.path(), &["init", "-q"]).unwrap();
    git(root.path(), &["config", "user.name", "Semantic fixture"]).unwrap();
    git(
        root.path(),
        &["config", "user.email", "fixture@example.invalid"],
    )
    .unwrap();
    git(root.path(), &["add", "."]).unwrap();
    git(root.path(), &["commit", "-qm", "base"]).unwrap();
    let historical_source = GitSource {
        root: root.path().into(),
        revision: "HEAD".into(),
    }
    .snapshot()
    .unwrap();
    let mut semantic_context = AnalysisContext::rust();
    semantic_context.analysis = "semantic".into();
    semantic_context.semantic_timeout_secs = 90;
    let historical = analysis::analyze(
        &mut index,
        &historical_source,
        semantic_context.clone(),
        false,
    )
    .unwrap();
    assert_eq!(
        historical.completeness.status, "complete",
        "{:?}",
        historical.diagnostics
    );
    assert_eq!(
        historical.stats.resolved_calls,
        snapshot.stats.resolved_calls
    );
    write(
        root.path(),
        "base/src/lib.rs",
        "pub fn service() -> i32 { 42 }\n",
    );
    let changed_source = WorkingTreeSource {
        root: root.path().into(),
    }
    .snapshot()
    .unwrap();
    let changed =
        analysis::analyze(&mut index, &changed_source, semantic_context.clone(), false).unwrap();
    assert_eq!(
        changed.completeness.status, "complete",
        "{:?}",
        changed.diagnostics
    );
    let unchanged_caller = index
        .find_nodes(&changed.id, "orchestrate", 1)
        .unwrap()
        .remove(0);
    assert_eq!(caller.fingerprint, unchanged_caller.fingerprint);
    let mark = codexis::marks::get(&index, &changed, &unchanged_caller).unwrap();
    let current_dynamic = index
        .find_nodes(&changed.id, "dynamic", 1)
        .unwrap()
        .remove(0);
    assert_eq!(
        codexis::marks::get(&index, &changed, &current_dynamic).unwrap()["state"],
        "needs_review",
        "unknown runtime dispatch requires conservative snapshot invalidation"
    );
    assert_eq!(
        mark["state"], "needs_review",
        "callee change must invalidate caller review"
    );
    let review = codexis::review::compare(&index, &historical, &changed, 20, 0).unwrap();
    let service_change = review.data["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["after"]["qualified_name"] == "base::service")
        .unwrap();
    assert_eq!(service_change["priority"], "interface");
    for side in ["previous_impacts", "current_impacts"] {
        let impacts = service_change[side].as_array().unwrap();
        assert!(
            impacts
                .iter()
                .any(|i| i["caller"]["qualified_name"] == "app::orchestrate"),
            "{impacts:?}"
        );
        assert!(impacts.iter().all(|i| i["path"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["resolution"] == "semantic")));
    }
    semantic_context.semantic_request_limit = 1;
    let limited = analysis::analyze(&mut index, &changed_source, semantic_context, false).unwrap();
    assert_eq!(limited.completeness.status, "partial");
    assert!(
        limited
            .diagnostics
            .iter()
            .any(|d| d.code == "semantic_budget"),
        "{:?}",
        limited.diagnostics
    );
}
