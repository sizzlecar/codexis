use codexis::analysis;
use codexis::frontend::{rust::RustFrontend, LanguageFrontend};
use codexis::index::Index;
use codexis::model::AnalysisContext;
use codexis::project::{CargoAdapter, ProjectAdapter};
use codexis::query;
use codexis::source::{SourceProvider, WorkingTreeSource};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn write(root: &Path, path: &str, content: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    write(dir.path(), "Cargo.toml", "[workspace]\nmembers=['crates/*']\n[workspace.package]\nedition='2021'\n[workspace.dependencies]\nhelper={path='crates/helper'}\n");
    write(
        dir.path(),
        "crates/helper/Cargo.toml",
        "[package]\nname='helper'\nversion='0.1.0'\nedition.workspace=true\n",
    );
    write(
        dir.path(),
        "crates/helper/src/lib.rs",
        "pub fn service() {}\n",
    );
    write(dir.path(), "crates/app/Cargo.toml", "[package]\nname='app'\nversion='0.1.0'\nedition.workspace=true\n[dependencies]\nhelper.workspace=true\n");
    write(
        dir.path(),
        "crates/app/src/lib.rs",
        r#"
pub mod worker;
pub use worker::run as launch;
pub trait Engine { fn infer(&self); }
pub struct First;
pub struct Second;
impl Engine for First { fn infer(&self) {} }
impl Engine for Second { fn infer(&self) {} }
pub fn dynamic(engine: &dyn Engine) { engine.infer(); }
#[cfg(test)]
mod tests { #[test] fn starts() { super::launch(); } }
"#,
    );
    write(
        dir.path(),
        "crates/app/src/worker.rs",
        "use helper::service as invoke;\npub fn run() { invoke(); }\n",
    );
    dir
}

#[test]
fn workspace_inheritance_and_modules_preserve_context() {
    let dir = fixture();
    let sources = WorkingTreeSource {
        root: dir.path().into(),
    }
    .snapshot()
    .unwrap();
    let project = CargoAdapter.discover(&sources).unwrap();
    assert_eq!(project.packages.len(), 2);
    let app = project.packages.iter().find(|p| p.name == "app").unwrap();
    assert_eq!(app.edition, "2021");
    assert_eq!(app.dependencies[0].path.as_deref(), Some("crates/helper"));
    let contexts = RustFrontend.plan(&sources, &project, None).unwrap();
    let worker = contexts
        .iter()
        .find(|c| c.path.ends_with("worker.rs"))
        .unwrap();
    assert!(worker.linked);
    assert_eq!(worker.module, "app::worker");
    let facts = RustFrontend
        .parse(
            &sources.files["crates/app/src/lib.rs"],
            contexts
                .iter()
                .find(|c| c.path == "crates/app/src/lib.rs")
                .unwrap(),
        )
        .unwrap();
    assert!(facts.nodes.iter().any(|n| n.name == "starts" && n.is_test));
    assert_eq!(facts.nodes.iter().filter(|n| n.name == "infer").count(), 3);
    assert!(facts
        .edges
        .iter()
        .filter(|e| e.kind == "calls")
        .all(|e| e.target.is_none()));
    for node in &facts.nodes {
        let file = &sources.files[&node.evidence.path];
        assert!(file
            .content
            .get(node.evidence.start_byte..node.evidence.end_byte)
            .is_some());
        assert_eq!(node.evidence.content_hash, file.hash);
    }
}

#[test]
fn explicit_module_path_and_unicode_locations() {
    let dir = TempDir::new().unwrap();
    write(
        dir.path(),
        "Cargo.toml",
        "[package]\nname='locations'\nversion='0.1.0'\nedition='2021'\n",
    );
    write(dir.path(),"src/lib.rs","#[path = \"special.rs\"]\npub mod renamed;\npub fn f() { let _ = \"你好\"; renamed::go(); }\n");
    write(dir.path(), "src/special.rs", "pub fn go() {}\n");
    let sources = WorkingTreeSource {
        root: dir.path().into(),
    }
    .snapshot()
    .unwrap();
    let project = CargoAdapter.discover(&sources).unwrap();
    let contexts = RustFrontend.plan(&sources, &project, None).unwrap();
    assert!(contexts
        .iter()
        .any(|c| c.module == "locations::renamed" && c.path == "src/special.rs" && c.linked));
    let facts = RustFrontend
        .parse(
            &sources.files["src/lib.rs"],
            contexts.iter().find(|c| c.path == "src/lib.rs").unwrap(),
        )
        .unwrap();
    let call = facts.edges.iter().find(|e| e.kind == "calls").unwrap();
    assert_eq!(
        &sources.files["src/lib.rs"].content[call.evidence.start_byte..call.evidence.end_byte],
        "go"
    );
    assert_eq!(call.evidence.start_line, 3);
}

#[test]
fn cached_analysis_preserves_old_evidence_after_change() {
    let dir = fixture();
    let cache = TempDir::new().unwrap();
    let mut index = Index::open(dir.path(), Some(cache.path())).unwrap();
    let sources = WorkingTreeSource {
        root: dir.path().into(),
    }
    .snapshot()
    .unwrap();
    let first = analysis::analyze(&mut index, &sources, AnalysisContext::rust(), false).unwrap();
    assert!(first.stats.parsed_files > 0);
    let second = analysis::analyze(&mut index, &sources, AnalysisContext::rust(), false).unwrap();
    assert_eq!(second.id, first.id);
    assert_eq!(second.stats.parsed_files, 0);
    assert_eq!(second.stats.reused_files, second.stats.source_files);
    write(
        dir.path(),
        "crates/app/src/worker.rs",
        "pub fn run() { let _changed = true; }\n",
    );
    let changed = WorkingTreeSource {
        root: dir.path().into(),
    }
    .snapshot()
    .unwrap();
    let third = analysis::analyze(&mut index, &changed, AnalysisContext::rust(), false).unwrap();
    assert_ne!(third.id, first.id);
    assert_eq!(third.stats.parsed_files, 1);
    let old = index
        .find_nodes(&first.id, "app::worker::run", 10)
        .unwrap()
        .remove(0);
    assert!(index
        .content(&old.evidence.content_hash)
        .unwrap()
        .contains("invoke()"));
    let mut historical = first.clone();
    query::check_freshness(&index, &mut historical).unwrap();
    assert!(historical.completeness.stale);
}

#[test]
fn ignore_rules_and_unsafe_relative_paths() {
    let dir = TempDir::new().unwrap();
    write(dir.path(), ".gitignore", "ignored/\n");
    write(dir.path(), "src/lib.rs", "pub fn good() {}\n");
    write(dir.path(), "ignored/secret.rs", "pub fn excluded() {}\n");
    write(
        dir.path(),
        "target/build/output.rs",
        "pub fn generated() {}\n",
    );
    let sources = WorkingTreeSource {
        root: dir.path().into(),
    }
    .snapshot()
    .unwrap();
    assert_eq!(
        sources.files.keys().filter(|p| p.ends_with(".rs")).count(),
        1
    );
    assert!(codexis::source::relative_path(Path::new("../escape.rs")).is_err());
    assert!(codexis::source::relative_path(Path::new("/escape.rs")).is_err());
}

#[test]
fn duplicate_symbol_names_require_disambiguation() {
    let dir = fixture();
    let cache = TempDir::new().unwrap();
    let mut index = Index::open(dir.path(), Some(cache.path())).unwrap();
    let sources = WorkingTreeSource {
        root: dir.path().into(),
    }
    .snapshot()
    .unwrap();
    let snapshot = analysis::analyze(&mut index, &sources, AnalysisContext::rust(), false).unwrap();
    let result = query::inspect(&index, &snapshot, "infer", 20).unwrap();
    assert_eq!(result.data["kind"], "candidates");
    assert_eq!(result.data["candidates"].as_array().unwrap().len(), 3);
}

#[test]
fn explicitly_named_binary_does_not_create_a_phantom_main_target() {
    let dir = TempDir::new().unwrap();
    write(dir.path(), "Cargo.toml", "[package]\nname='application-cli'\nversion='0.1.0'\n[[bin]]\nname='application'\npath='src/main.rs'\n");
    write(dir.path(), "src/main.rs", "fn main() {}\n");
    let source = WorkingTreeSource {
        root: dir.path().into(),
    }
    .snapshot()
    .unwrap();
    let project = CargoAdapter.discover(&source).unwrap();
    assert_eq!(project.packages[0].units.len(), 1);
    assert_eq!(project.packages[0].units[0].name, "application");
}

#[test]
fn negated_test_cfg_and_feature_names_are_not_misclassified() {
    let dir = TempDir::new().unwrap();
    write(
        dir.path(),
        "Cargo.toml",
        "[package]\nname='contexts'\nversion='0.1.0'\nedition='2021'\n",
    );
    write(dir.path(),"src/lib.rs","#[cfg(not(test))]\nmod production { pub fn run() {} }\n#[cfg(feature=\"contest\")]\nmod feature { pub fn run() {} }\n#[cfg(test)]\nmod tests { fn run() {} }\n");
    let source = WorkingTreeSource {
        root: dir.path().into(),
    }
    .snapshot()
    .unwrap();
    let project = CargoAdapter.discover(&source).unwrap();
    let contexts = RustFrontend.plan(&source, &project, None).unwrap();
    let facts = RustFrontend
        .parse(&source.files["src/lib.rs"], &contexts[0])
        .unwrap();
    for node in facts.nodes.iter().filter(|n| n.name == "run") {
        assert_eq!(
            node.is_test,
            node.qualified_name == "contexts::tests::run",
            "{node:#?}"
        );
    }
}

#[test]
fn compound_test_cfg_trait_visibility_and_unowned_workspace_source() {
    let root = fixture();
    write(root.path(), "scratch.rs", "pub fn scratch() {}\n");
    write(root.path(), "crates/app/src/lib.rs",
        "pub trait Visible { fn exported(&self); }\n#[cfg(all(test,feature=\"x\"))]\nfn test_only() {}\n#[cfg(any(test,feature=\"x\"))]\nfn also_production() {}\n#[cfg(any())]\nfn disabled() {}\n");
    let cache = TempDir::new().unwrap();
    let mut index = Index::open(root.path(), Some(cache.path())).unwrap();
    let source = WorkingTreeSource {
        root: root.path().into(),
    }
    .snapshot()
    .unwrap();
    let snapshot = analysis::analyze(&mut index, &source, AnalysisContext::rust(), false).unwrap();
    let nodes = index.all_nodes(&snapshot.id).unwrap();
    assert_eq!(
        nodes
            .iter()
            .find(|n| n.name == "exported")
            .unwrap()
            .visibility,
        "pub"
    );
    assert!(
        nodes
            .iter()
            .find(|n| n.name == "test_only")
            .unwrap()
            .is_test
    );
    assert!(
        !nodes
            .iter()
            .find(|n| n.name == "also_production")
            .unwrap()
            .is_test
    );
    assert!(!nodes.iter().find(|n| n.name == "disabled").unwrap().is_test);
    assert!(nodes
        .iter()
        .any(|n| n.name == "scratch" && n.unit.starts_with("unlinked:")));
}

#[test]
fn target_discovery_matches_cargo_metadata_for_custom_and_automatic_targets() {
    use std::collections::BTreeSet;
    use std::process::Command;
    for edition in ["2015", "2021"] {
        let root = TempDir::new().unwrap();
        write(root.path(), "Cargo.toml", &format!(
            "[package]\nname='target-fixture'\nversion='0.1.0'\nedition='{edition}'\n[workspace]\n[[bin]]\nname='custom-tool'\npath='tools/entry.rs'\n[[test]]\nname='smoke'\npath='checks/actual.rs'\n[[example]]\nname='nested'\nrequired-features=['demo']\n[features]\ndemo=[]\n"));
        for path in [
            "src/lib.rs",
            "src/main.rs",
            "src/bin/auto.rs",
            "tools/entry.rs",
            "tests/smoke.rs",
            "checks/actual.rs",
            "examples/nested/main.rs",
            "benches/perf.rs",
        ] {
            write(root.path(), path, "fn main() {}\n");
        }
        let source = WorkingTreeSource {
            root: root.path().into(),
        }
        .snapshot()
        .unwrap();
        let project = CargoAdapter.discover(&source).unwrap();
        let actual: BTreeSet<_> = project.packages[0]
            .units
            .iter()
            .map(|u| (u.kind.clone(), u.name.clone(), u.source.clone()))
            .collect();
        let output = Command::new("cargo")
            .args([
                "metadata",
                "--no-deps",
                "--offline",
                "--format-version",
                "1",
            ])
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let canonical = root.path().canonicalize().unwrap();
        let expected: BTreeSet<_> = metadata["packages"][0]["targets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                let path = Path::new(t["src_path"].as_str().unwrap())
                    .canonicalize()
                    .unwrap();
                (
                    t["kind"][0].as_str().unwrap().to_owned(),
                    t["name"].as_str().unwrap().to_owned(),
                    path.strip_prefix(&canonical)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                )
            })
            .collect();
        assert_eq!(actual, expected, "edition {edition}");
        assert_eq!(
            project.packages[0]
                .units
                .iter()
                .find(|u| u.name == "nested")
                .unwrap()
                .required_features,
            ["demo"]
        );
    }
}

#[test]
fn dependency_aliases_kinds_conditions_and_config_cache_identity() {
    let root = TempDir::new().unwrap();
    write(root.path(), "Cargo.toml", "[package]\nname='deps'\nversion='0.1.0'\nedition='2021'\n[workspace]\n[dependencies]\nrenamed={package='real-name',version='1',optional=true}\n[dev-dependencies]\nhelper='1'\n[build-dependencies]\nbuilder='1'\n[target.'cfg(unix)'.dependencies]\nplatform='1'\n");
    write(root.path(), "src/lib.rs", "pub fn a() {}\n");
    let source = WorkingTreeSource {
        root: root.path().into(),
    }
    .snapshot()
    .unwrap();
    let project = CargoAdapter.discover(&source).unwrap();
    let deps = &project.packages[0].dependencies;
    assert!(deps
        .iter()
        .any(|d| d.alias == "renamed" && d.package == "real-name" && d.optional));
    assert!(deps.iter().any(|d| d.alias == "helper" && d.kind == "dev"));
    assert!(deps
        .iter()
        .any(|d| d.alias == "builder" && d.kind == "build"));
    assert!(deps
        .iter()
        .any(|d| d.alias == "platform" && d.condition.as_deref() == Some("cfg(unix)")));
    let cache = TempDir::new().unwrap();
    let mut index = Index::open(root.path(), Some(cache.path())).unwrap();
    let first = analysis::analyze(&mut index, &source, AnalysisContext::rust(), false).unwrap();
    write(
        root.path(),
        ".cargo/config.toml",
        "[build]\nrustflags=['--cfg','custom_flag']\n",
    );
    let configured = WorkingTreeSource {
        root: root.path().into(),
    }
    .snapshot()
    .unwrap();
    assert!(configured.files.contains_key(".cargo/config.toml"));
    let second =
        analysis::analyze(&mut index, &configured, AnalysisContext::rust(), false).unwrap();
    assert_ne!(first.id, second.id);
    assert_eq!(second.stats.parsed_files, 0);
    let mut context = AnalysisContext::rust();
    context.features.push("demo".into());
    let third = analysis::analyze(&mut index, &configured, context, false).unwrap();
    assert_ne!(second.id, third.id);
    assert_eq!(third.stats.parsed_files, 0);
}
