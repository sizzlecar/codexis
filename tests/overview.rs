use serde_json::Value;
use std::{fs, path::Path, process::Command};
use tempfile::TempDir;

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn fixture() -> TempDir {
    let root = TempDir::new().unwrap();
    write(
        root.path(),
        "Cargo.toml",
        "[workspace]\nmembers=['app','core']\nresolver='2'\n",
    );
    write(root.path(), "app/Cargo.toml", "[package]\nname='reader-app'\nversion='0.1.0'\nedition='2021'\ndescription='Command line application'\n[dependencies]\nengine={package='reader-core',path='../core'}\n");
    write(root.path(), "core/Cargo.toml", "[package]\nname='reader-core'\nversion='0.1.0'\nedition='2021'\ndescription='Shared execution primitives'\n");
    write(
        root.path(),
        "app/src/main.rs",
        "fn main() { reader_core::run(); }\n",
    );
    write(root.path(), "app/examples/demo.rs", "fn main() {}\n");
    write(root.path(), "app/build.rs", "fn main() {}\n");
    write(root.path(), "core/src/lib.rs", "pub fn run() {}\n");
    for n in 0..40 {
        write(
            root.path(),
            &format!("core/fixtures/case_{n}.rs"),
            "fn sample() {}\n",
        );
    }
    root
}

fn cli(root: &Path, cache: &Path, extra: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_codexis"))
        .arg("--cache-dir")
        .arg(cache)
        .arg("analyze")
        .arg(root)
        .args(extra)
        .output()
        .unwrap()
}

#[test]
fn one_command_shows_reading_guide_without_debug_wall() {
    let root = fixture();
    let cache = TempDir::new().unwrap();
    let output = cli(root.path(), cache.path(), &[]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    for needed in [
        "静态分析 · 不调用模型",
        "主干  reader_app::main",
        "src/main.rs:1",
        "外部系统",
        "--verbose",
        "codexis flow --view",
    ] {
        assert!(text.contains(needed), "missing {needed}\n{text}");
    }
    for noise in [
        "Next queries",
        "Capabilities and boundaries",
        "Diagnostics (",
        "Unresolved calls",
        "unlinked_source:",
        "tree-sitter",
        " — ID ",
        "examples/demo.rs",
        "app/build.rs",
        "fixtures/case_",
    ] {
        assert!(!text.contains(noise), "unexpected {noise}\n{text}");
    }
    assert!(text.lines().count() <= 60, "{} lines", text.lines().count());
    assert!(!String::from_utf8(output.stderr)
        .unwrap()
        .contains("Indexing"));
    let detailed = cli(root.path(), cache.path(), &["--verbose"]);
    assert!(detailed.status.success());
    assert!(String::from_utf8(detailed.stdout)
        .unwrap()
        .contains("unlinked_source:"));
    let json = cli(root.path(), cache.path(), &["--format", "json"]);
    assert!(json.status.success());
    let json: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(json["diagnostics"].as_array().unwrap().len(), 40);
    assert!(json["data"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["evidence"]["path"] == "app/build.rs"));
    assert_eq!(
        json["data"]["reading_guide"]["packages"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let snapshot = json["snapshot_id"].as_str().unwrap();
    assert!(!text.contains(snapshot));
}

#[test]
fn library_project_has_a_source_start_without_a_main_function() {
    let root = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    write(
        root.path(),
        "Cargo.toml",
        "[package]\nname='library-only'\nversion='0.1.0'\n",
    );
    write(
        root.path(),
        "src/lib.rs",
        "pub trait Engine { fn run(&self); }\n",
    );
    let output = cli(root.path(), cache.path(), &[]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("库入口 src/lib.rs"), "{text}");
    assert!(!text.contains("<symbol>"));
}

#[test]
fn partial_analysis_remains_obvious_in_compact_output() {
    let root = fixture();
    let cache = TempDir::new().unwrap();
    write(root.path(), "app/src/main.rs", "fn main() { invalid rust (");
    let output = cli(root.path(), cache.path(), &[]);
    assert_eq!(output.status.code(), Some(3));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("分析不完整") && text.contains("app/src/main.rs"),
        "{text}"
    );
    assert!(!text.contains("unlinked_source:"));
    assert!(text.lines().count() <= 60);
    let markdown = cli(root.path(), cache.path(), &["--format", "markdown"]);
    assert_eq!(markdown.status.code(), Some(3));
    let markdown = String::from_utf8(markdown.stdout).unwrap();
    assert!(markdown.contains("分析不完整") && markdown.contains("~~~text"));
}
