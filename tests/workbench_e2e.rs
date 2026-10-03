//! Product workflow exercised through the real executable and Git snapshots.
use codexis::{index::Index, model::Evidence};
use serde_json::Value;
use std::{fs, path::Path, process::Command};
use tempfile::TempDir;

fn write(root: &Path, path: &str, text: &str) {
    let target = root.join(path);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(target, text).unwrap();
}

fn git(root: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn cli(root: &Path, cache: &Path, arguments: &[&str]) -> Value {
    cli_locale(root, cache, "en", arguments)
}

fn cli_locale(root: &Path, cache: &Path, locale: &str, arguments: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_codexis"))
        .args(["--locale", locale, "--format", "json", "--project"])
        .arg(root)
        .arg("--cache-dir")
        .arg(cache)
        .args(arguments)
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "invalid codexis output for {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert!(
        output.status.success()
            || (output.status.code() == Some(3) && report["completeness"]["status"] == "partial"),
        "codexis {arguments:?}: {}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    report
}

fn records(report: &Value) -> &[Value] {
    report["data"]["records"].as_array().unwrap()
}

fn stored_text(index: &Index, value: &Value) -> String {
    let span: Evidence = serde_json::from_value(value.clone()).unwrap();
    let text = index.content(&span.content_hash).unwrap();
    assert!(span.start_byte <= span.end_byte && span.end_byte <= text.len());
    text[span.start_byte..span.end_byte].to_owned()
}

#[test]
fn baseline_change_review_and_knowledge_revision_preserve_both_versions() {
    let project = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = project.path();
    write(root, "Cargo.toml", "[package]\nname='workbench-engine'\nversion='0.1.0'\nedition='2021'\ndescription='Processes jobs'\n");
    write(root, "src/lib.rs", "pub mod engine; pub mod models;\n");
    let models = (0..120)
        .map(|i| format!("pub struct Model{i} {{ pub id:u32 }}\n"))
        .collect::<String>();
    write(root, "src/models.rs", &models);
    write(root, "src/engine.rs", "//! Processes a single job.\npub struct Job { pub id: u32 }\npub fn process(job: Job) -> u32 { job.id }\n#[cfg(test)] mod tests { #[test] fn processes() { super::process(super::Job { id: 1 }); } }\n");
    write(
        root,
        "README.md",
        "# Workbench engine\n\nProcesses one job via src/engine.rs.\n",
    );
    write(root, "config/service.yaml", "workers: 1\n");
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Codexis E2E"]);
    git(
        root,
        &["config", "user.email", "codexis-e2e@example.invalid"],
    );
    git(root, &["add", "."]);
    git(
        root,
        &["-c", "commit.gpgsign=false", "commit", "-qm", "baseline"],
    );

    let baseline = cli(root, cache.path(), &["analyze"]);
    assert_eq!(baseline["completeness"]["status"], "complete");
    assert_eq!(baseline["completeness"]["truncated"], true);
    let baseline_id = baseline["snapshot_id"].as_str().unwrap();
    let views = cli(root, cache.path(), &["understand"]);
    let dimensions = views["data"]["understanding"]["dimensions"]
        .as_array()
        .unwrap();
    let initial_changes = dimensions.iter().find(|d| d["id"] == "changes").unwrap();
    assert_eq!(initial_changes["comparison_available"], false);
    assert_eq!(initial_changes["total"], 0);
    assert_eq!(views["completeness"]["truncated"], true);
    assert_eq!(
        dimensions
            .iter()
            .map(|d| d["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        codexis::understanding::DIMENSION_IDS
    );
    let saved = cli(
        root,
        cache.path(),
        &[
            "remember",
            "process",
            "--dimension",
            "data",
            "--claim",
            "process returns a numeric job id",
            "--evidence",
            "README.md",
            "--evidence",
            "config/service.yaml",
        ],
    );
    let original = &saved["data"]["record"];
    let record_id = original["id"].as_str().unwrap();
    assert_eq!(original["revision"], 1);
    assert_eq!(original["valid"], true);
    assert_eq!(original["recorded_snapshot"], baseline_id);

    write(root, "src/engine.rs", "//! Processes a job as a display string.\npub struct Job { pub id: u64 }\npub fn process(job: Job) -> String { format!(\"job:{}\", job.id) }\n#[cfg(test)] mod tests { #[test] fn processes() { super::process(super::Job { id: 1 }); } }\n");
    write(
        root,
        "README.md",
        "# Workbench engine\n\nProcesses one job into a display string via src/engine.rs.\n",
    );
    write(root, "config/service.yaml", "workers: 4\n");
    let changed = cli(root, cache.path(), &["analyze"]);
    assert_eq!(changed["completeness"]["status"], "complete");
    assert_ne!(changed["snapshot_id"], baseline["snapshot_id"]);
    let knowledge = cli(root, cache.path(), &["knowledge"]);
    let outdated = records(&knowledge)
        .iter()
        .find(|r| r["id"] == record_id)
        .unwrap();
    assert_eq!(outdated["state"], "needs_review");
    assert_eq!(outdated["valid"], false);
    for path in ["src/engine.rs", "README.md", "config/service.yaml"] {
        assert!(outdated["changed_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == path));
    }

    let review = cli(
        root,
        cache.path(),
        &["review", "--base", "HEAD", "--worktree"],
    );
    let batch = &review["data"]["batch"];
    assert!(batch["groups_total"].as_u64().unwrap() >= 2);
    assert_eq!(batch["truncated"], false);
    let artifacts = batch["artifact_changes"].as_array().unwrap();
    for path in ["README.md", "config/service.yaml"] {
        assert!(artifacts.iter().any(|a| a["title"] == path));
    }
    assert!(batch["knowledge_updates"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["id"] == record_id));
    let checklist = batch["checklist"].as_array().unwrap();
    assert!(checklist.iter().any(|r| r["id"] == record_id
        && r["dimensions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d == "knowledge")));
    assert!(checklist
        .iter()
        .any(|r| r["priority"] == "high"
            && r["node_ids"].as_array().is_some_and(|ids| !ids.is_empty())));
    let index = Index::open(root, Some(cache.path())).unwrap();
    let function = batch["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["changes"].as_array().into_iter().flatten())
        .find(|change| {
            change["title"]
                .as_str()
                .is_some_and(|s| s.ends_with("::process"))
        })
        .unwrap();
    assert!(stored_text(&index, &function["before"][0]).contains("-> u32"));
    assert!(stored_text(&index, &function["after"][0]).contains("-> String"));
    // A new process can load the explicitly generated batch for the head.
    let persisted = cli(
        root,
        cache.path(),
        &["understand", "--dimension", "changes"],
    );
    let changes = &persisted["data"]["understanding"]["dimensions"][0];
    assert_eq!(changes["comparison_available"], true);
    assert!(changes["total"].as_u64().unwrap() > 1);
    let restored = &persisted["data"]["understanding"]["change_batch"];
    assert_eq!(restored["base_snapshot"], batch["base_snapshot"]);
    assert_eq!(restored["head_snapshot"], batch["head_snapshot"]);
    assert_eq!(restored["locale"], "en");
    assert!(restored["summary"]
        .as_str()
        .unwrap()
        .contains("review questions"));
    let chinese = cli_locale(
        root,
        cache.path(),
        "zh-CN",
        &["understand", "--dimension", "changes"],
    );
    let chinese_batch = &chinese["data"]["understanding"]["change_batch"];
    assert_eq!(chinese_batch["locale"], "zh-CN");
    assert_eq!(chinese_batch["id"], restored["id"]);
    assert!(chinese_batch["summary"]
        .as_str()
        .unwrap()
        .contains("建议核查"));
    let scoped = cli(
        root,
        cache.path(),
        &[
            "understand",
            "--dimension",
            "changes",
            "--scope",
            "src/engine.rs",
        ],
    );
    let scoped_batch = &scoped["data"]["understanding"]["change_batch"];
    assert!(!scoped_batch["groups"].as_array().unwrap().is_empty());
    assert!(scoped_batch["artifact_changes"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(scoped["data"]["understanding"]["dimensions"][0]["items"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|item| item["evidence"].as_array().into_iter().flatten())
        .all(|e| e["path"] == "src/engine.rs"));

    let revised = cli(
        root,
        cache.path(),
        &[
            "remember",
            "process",
            "--id",
            record_id,
            "--dimension",
            "data",
            "--claim",
            "process returns the formatted job id",
            "--evidence",
            "README.md",
            "--evidence",
            "config/service.yaml",
        ],
    );
    assert_eq!(revised["data"]["record"]["revision"], 2);
    assert_eq!(revised["data"]["record"]["valid"], true);
    let refreshed = cli(
        root,
        cache.path(),
        &["understand", "--dimension", "changes"],
    );
    assert!(
        refreshed["data"]["understanding"]["change_batch"]["knowledge_updates"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        refreshed["data"]["understanding"]["change_batch"]["checklist"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["id"] != record_id)
    );
    let history = cli(root, cache.path(), &["knowledge", record_id, "--history"]);
    let versions = records(&history);
    assert_eq!(versions.len(), 2);
    let old = versions.iter().find(|v| v["revision"] == 1).unwrap();
    let new = versions.iter().find(|v| v["revision"] == 2).unwrap();
    assert_eq!(old["claim"], "process returns a numeric job id");
    assert_eq!(new["claim"], "process returns the formatted job id");
    assert_eq!(old["valid"], false);
    assert_eq!(new["valid"], true);
    assert_eq!(old["recorded_snapshot"], baseline_id);
    let evidence = |record: &Value, path: &str| {
        record["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["path"] == path)
            .unwrap()
            .clone()
    };
    assert_ne!(
        evidence(old, "README.md")["content_hash"],
        evidence(new, "README.md")["content_hash"]
    );
    assert!(stored_text(&index, &evidence(old, "README.md")).contains("Processes one job via"));
    assert!(stored_text(&index, &evidence(new, "README.md")).contains("display string"));
}

#[test]
fn explicit_boundary_rule_follows_a_known_python_call_without_guessing_dynamic_calls() {
    let project = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let root = project.path();
    write(
        root,
        "pyproject.toml",
        "[project]\nname='boundary-engine'\nversion='0.1.0'\n",
    );
    write(root, "reader/__init__.py", "");
    write(root, "reader/service.py", "def persist():\n    return 1\n");
    write(root, "reader/api.py", "from .service import persist\ndef handle():\n    return persist()\ndef dynamic(handler):\n    return handler.persist()\n");
    write(root, "codexis.toml", "[architecture]\nforbidden=[{from='reader/api.py',to='reader/service.py',reason='API cannot call persistence directly'}]\n");
    let analyzed = cli(
        root,
        cache.path(),
        &["analyze", "--language", "python", "--analysis", "semantic"],
    );
    assert_eq!(analyzed["completeness"]["status"], "partial");
    assert_eq!(analyzed["data"]["stats"]["unresolved_calls"], 1);
    let report = cli(
        root,
        cache.path(),
        &["understand", "--dimension", "verification"],
    );
    let findings = &report["data"]["understanding"]["constraints"];
    assert_eq!(findings["rules"], 1);
    assert_eq!(findings["total"], 2);
    let violation = findings["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| {
            v["title"]
                .as_str()
                .is_some_and(|title| title.contains("reader.api.handle"))
        })
        .unwrap();
    assert!(findings["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|v| !v["title"].as_str().unwrap().contains("reader.api.dynamic")));
    assert_eq!(
        violation["basis"],
        "explicit user constraint and indexed known-target relation"
    );
    assert!(violation["title"]
        .as_str()
        .unwrap()
        .contains("reader.api.handle"));
    assert!(violation["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["path"] == "codexis.toml"));
    assert!(violation["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["path"] == "reader/service.py"));
    let architecture = cli(
        root,
        cache.path(),
        &["understand", "--dimension", "architecture"],
    );
    let components = architecture["data"]["understanding"]["architecture"]["components"]
        .as_array()
        .unwrap();
    assert!(components
        .iter()
        .any(|c| c["title"] == "boundary-engine / api"));
    assert!(components
        .iter()
        .any(|c| c["title"] == "boundary-engine / service"));
}
