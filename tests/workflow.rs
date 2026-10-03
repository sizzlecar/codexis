use codexis::{
    analysis,
    index::Index,
    marks,
    model::AnalysisContext,
    query, review,
    source::{git, GitSource, SourceProvider, WorkingTreeSource},
};
use serde_json::Value;
use std::{fs, path::Path, process::Command};
use tempfile::TempDir;

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn repo() -> TempDir {
    let root = TempDir::new().unwrap();
    git(root.path(), &["init", "-q"]).unwrap();
    git(root.path(), &["config", "user.name", "Codexis fixture"]).unwrap();
    git(
        root.path(),
        &["config", "user.email", "fixture@example.invalid"],
    )
    .unwrap();
    write(
        root.path(),
        "Cargo.toml",
        "[package]\nname='sample'\nversion='0.1.0'\nedition='2021'\n[workspace]\n",
    );
    write(
        root.path(),
        "src/lib.rs",
        "pub fn original() -> i32 { 1 }\npub fn removed() {}\n",
    );
    write(root.path(), "src/moved.rs", "pub fn unchanged() {}\n");
    git(root.path(), &["add", "."]).unwrap();
    git(root.path(), &["commit", "-qm", "base"]).unwrap();
    root
}

fn capture(index: &mut Index, root: &Path) -> codexis::model::Snapshot {
    let source = WorkingTreeSource { root: root.into() }.snapshot().unwrap();
    analysis::analyze(index, &source, AnalysisContext::rust(), false).unwrap()
}

#[test]
fn git_review_reads_both_sides_staged_unstaged_untracked_and_deleted_evidence() {
    let root = repo();
    let cache = TempDir::new().unwrap();
    let mut index = Index::open(root.path(), Some(cache.path())).unwrap();
    let base_source = GitSource {
        root: root.path().into(),
        revision: "HEAD".into(),
    }
    .snapshot()
    .unwrap();
    write(
        root.path(),
        "src/lib.rs",
        "pub fn original() -> i32 { 2 }\n",
    );
    fs::rename(
        root.path().join("src/moved.rs"),
        root.path().join("src/renamed.rs"),
    )
    .unwrap();
    git(root.path(), &["add", "."]).unwrap();
    git(root.path(), &["commit", "-qm", "change and move"]).unwrap();
    let committed_source = GitSource {
        root: root.path().into(),
        revision: "HEAD".into(),
    }
    .snapshot()
    .unwrap();
    let base = analysis::analyze(&mut index, &base_source, AnalysisContext::rust(), false).unwrap();
    let head = analysis::analyze(
        &mut index,
        &committed_source,
        AnalysisContext::rust(),
        false,
    )
    .unwrap();
    let pair = review::compare(&index, &base, &head, 50, 0).unwrap();
    assert_eq!(pair.data["total_changed_files"], 3);
    let deleted = pair.data["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["before"]["name"] == "removed")
        .unwrap();
    assert_eq!(deleted["state"], "removed");
    assert_eq!(deleted["mark_snapshot"], base.id);
    let hash = deleted["before"]["evidence"]["content_hash"]
        .as_str()
        .unwrap();
    assert!(index.content(hash).unwrap().contains("fn removed"));
    write(
        root.path(),
        "src/lib.rs",
        "pub fn original() -> i32 { 3 }\n",
    );
    git(root.path(), &["add", "src/lib.rs"]).unwrap();
    write(
        root.path(),
        "src/lib.rs",
        "pub fn original() -> i32 { 4 }\n",
    );
    write(root.path(), "src/new.rs", "pub fn untracked() {}\n");
    let status = git(root.path(), &["status", "--porcelain=v1", "-z"])
        .unwrap()
        .stdout;
    let staged = git(root.path(), &["show", ":src/lib.rs"]).unwrap().stdout;
    let working = capture(&mut index, root.path());
    let report = review::compare(&index, &head, &working, 50, 0).unwrap();
    let files = report.data["files"].as_array().unwrap();
    assert!(files
        .iter()
        .any(|f| f["path"] == "src/new.rs" && f["state"] == "added"));
    let modified = files.iter().find(|f| f["path"] == "src/lib.rs").unwrap();
    assert!(modified["diff"].as_str().unwrap().contains("{ 4 }"));
    assert!(!modified["diff"].as_str().unwrap().contains("{ 3 }"));
    assert_eq!(
        status,
        git(root.path(), &["status", "--porcelain=v1", "-z"])
            .unwrap()
            .stdout
    );
    assert_eq!(
        staged,
        git(root.path(), &["show", ":src/lib.rs"]).unwrap().stdout
    );
    let first = review::compare(&index, &head, &working, 1, 0).unwrap();
    let second = review::compare(&index, &head, &working, 1, 1).unwrap();
    assert!(first.completeness.truncated);
    assert_ne!(
        first.data["files"][0]["path"],
        second.data["files"][0]["path"]
    );
}

#[test]
fn marks_preserve_notes_and_invalidate_changed_or_stale_evidence() {
    let root = repo();
    let cache = TempDir::new().unwrap();
    let mut index = Index::open(root.path(), Some(cache.path())).unwrap();
    let first = capture(&mut index, root.path());
    let node = index
        .find_nodes(&first.id, "original", 2)
        .unwrap()
        .remove(0);
    marks::set(&index, &first, &node, "question", "确认边界：输入为空？").unwrap();
    assert_eq!(
        marks::get(&index, &first, &node).unwrap()["state"],
        "question"
    );
    let unchanged = capture(&mut index, root.path());
    assert_eq!(
        marks::get(&index, &unchanged, &node).unwrap()["state"],
        "question"
    );
    write(
        root.path(),
        "src/lib.rs",
        "pub fn original() -> i32 { 77 }\n",
    );
    let mut stale = first.clone();
    query::check_freshness(&index, &mut stale).unwrap();
    assert!(marks::set(&index, &stale, &node, "seen", "").is_err());
    let next = capture(&mut index, root.path());
    let changed = index.find_nodes(&next.id, "original", 2).unwrap().remove(0);
    let mark = marks::get(&index, &next, &changed).unwrap();
    assert_eq!(mark["state"], "needs_review");
    assert_eq!(mark["previous_state"], "question");
    assert_eq!(mark["note"], "确认边界：输入为空？");
    marks::set(&index, &next, &changed, "seen", "done").unwrap();
    assert_eq!(
        marks::get(&index, &next, &changed).unwrap()["state"],
        "seen"
    );
    let different = repo();
    let mut separate = Index::open(different.path(), Some(cache.path())).unwrap();
    assert_ne!(separate.path, index.path);
    let other = capture(&mut separate, different.path());
    let other_node = separate
        .find_nodes(&other.id, "original", 1)
        .unwrap()
        .remove(0);
    assert_eq!(
        marks::get(&separate, &other, &other_node).unwrap()["state"],
        "unread"
    );
}

fn cli(root: &Path, cache: &Path, arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_codexis"))
        .arg("--project")
        .arg(root)
        .arg("--cache-dir")
        .arg(cache)
        .args(arguments)
        .output()
        .unwrap()
}

#[test]
fn all_six_commands_exit_codes_formats_and_explicit_overwrite() {
    let root = repo();
    let cache = TempDir::new().unwrap();
    for argument in ["--help", "--version"] {
        assert!(cli(root.path(), cache.path(), &[argument]).status.success());
    }
    for args in [
        vec!["analyze", "--format", "json"],
        vec!["map", "--format", "json"],
        vec!["inspect", "original", "--format", "json"],
        vec!["trace", "original", "--format", "json"],
        vec![
            "mark", "original", "--state", "seen", "--note", "OK", "--format", "json",
        ],
        vec!["review", "--base", "HEAD", "--worktree", "--format", "json"],
    ] {
        let output = cli(root.path(), cache.path(), &args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["schema_version"], 1);
        assert!(report["snapshot_id"].is_string());
        assert!(report["analysis_context"].is_object());
        assert!(report["completeness"]["capabilities"].is_array());
        assert!(!output.stdout.contains(&0x1b));
    }
    for args in [
        vec!["map", "--limit", "0"],
        vec!["trace", "original", "--depth", "33"],
        vec!["review", "--base", "HEAD"],
        vec!["review", "--base", "HEAD", "--head", "HEAD", "--worktree"],
        vec!["analyze", "--semantic-request-limit", "0"],
    ] {
        assert_eq!(
            cli(root.path(), cache.path(), &args).status.code(),
            Some(2),
            "{args:?}"
        );
    }
    assert_eq!(
        cli(root.path(), cache.path(), &["inspect", "missing-symbol"])
            .status
            .code(),
        Some(1)
    );
    let report_file = cache.path().join("report.md");
    let output_args = [
        "inspect",
        "original",
        "--format",
        "markdown",
        "--output",
        report_file.to_str().unwrap(),
    ];
    assert!(cli(root.path(), cache.path(), &output_args)
        .status
        .success());
    let content = fs::read_to_string(&report_file).unwrap();
    assert!(content.starts_with("# Codexis"));
    assert!(content.contains("快照：") && content.contains("~~~text"));
    assert_eq!(
        cli(root.path(), cache.path(), &output_args).status.code(),
        Some(1)
    );
    assert_eq!(fs::read_to_string(&report_file).unwrap(), content);
    let mut overwrite = output_args.to_vec();
    overwrite.push("--overwrite");
    assert!(cli(root.path(), cache.path(), &overwrite).status.success());
    write(
        root.path(),
        "src/lib.rs",
        "pub fn original() { invalid syntax\n",
    );
    let partial = cli(root.path(), cache.path(), &["analyze", "--format", "json"]);
    assert_eq!(partial.status.code(), Some(3));
    let partial: Value = serde_json::from_slice(&partial.stdout).unwrap();
    assert_eq!(partial["completeness"]["status"], "partial");
    assert_eq!(
        cli(root.path(), cache.path(), &["map"]).status.code(),
        Some(3)
    );
}

#[test]
fn unchanged_conditional_duplicates_are_not_reported_as_changes() {
    let root = repo();
    write(
        root.path(),
        "src/lib.rs",
        "#[cfg(feature=\"a\")]\nfn same() {}\n#[cfg(not(feature=\"a\"))]\nfn same() {}\n",
    );
    let cache = TempDir::new().unwrap();
    let mut index = Index::open(root.path(), Some(cache.path())).unwrap();
    let first = capture(&mut index, root.path());
    write(root.path(), "src/unrelated.rs", "fn other() {}\n");
    let second = capture(&mut index, root.path());
    let review = review::compare(&index, &first, &second, 20, 0).unwrap();
    assert_eq!(review.data["total_changes"], 1);
    assert_eq!(review.data["changes"][0]["after"]["name"], "other");
}

#[test]
fn nested_git_project_only_captures_its_own_sources() {
    let root = repo();
    write(root.path(), "nested/src/lib.rs", "pub fn nested() {}\n");
    git(root.path(), &["add", "."]).unwrap();
    git(root.path(), &["commit", "-qm", "nested"]).unwrap();
    let source = GitSource {
        root: root.path().join("nested"),
        revision: "HEAD".into(),
    }
    .snapshot()
    .unwrap();
    assert_eq!(source.files.len(), 1);
    assert!(source.files.contains_key("src/lib.rs"));
    assert!(!source.files.contains_key("Cargo.toml"));
}

#[test]
fn source_mutation_and_publish_failure_preserve_the_prior_snapshot() {
    let root = repo();
    let cache = TempDir::new().unwrap();
    let mut index = Index::open(root.path(), Some(cache.path())).unwrap();
    let first = capture(&mut index, root.path());
    let source = WorkingTreeSource {
        root: root.path().into(),
    }
    .snapshot()
    .unwrap();
    write(
        root.path(),
        "src/lib.rs",
        "pub fn altered_during_scan() {}\n",
    );
    assert!(analysis::analyze(&mut index, &source, AnalysisContext::rust(), false).is_err());
    assert_eq!(index.snapshot(None).unwrap().id, first.id);
    let changed = WorkingTreeSource {
        root: root.path().into(),
    }
    .snapshot()
    .unwrap();
    index.connection.execute_batch("CREATE TRIGGER simulate_disk_failure BEFORE INSERT ON snapshots BEGIN SELECT RAISE(ABORT, 'simulated publish failure'); END;").unwrap();
    assert!(analysis::analyze(&mut index, &changed, AnalysisContext::rust(), false).is_err());
    assert_eq!(index.snapshot(None).unwrap().id, first.id);
    assert_eq!(index.all_nodes(&first.id).unwrap().len(), first.stats.nodes);
    index
        .connection
        .execute_batch("DROP TRIGGER simulate_disk_failure")
        .unwrap();
    let recovered =
        analysis::analyze(&mut index, &changed, AnalysisContext::rust(), false).unwrap();
    assert_eq!(
        recovered.stats.parsed_files, 0,
        "syntax cache survives a failed publication"
    );
    assert_ne!(recovered.id, first.id);
}

#[test]
fn first_bulk_publication_failure_restores_indexes_and_can_retry() {
    let root = repo();
    let cache = TempDir::new().unwrap();
    let mut index = Index::open(root.path(), Some(cache.path())).unwrap();
    index.connection.execute_batch("CREATE TRIGGER first_failure BEFORE INSERT ON snapshots BEGIN SELECT RAISE(ABORT, 'first publish failure'); END;").unwrap();
    let source = WorkingTreeSource {
        root: root.path().into(),
    }
    .snapshot()
    .unwrap();
    assert!(analysis::analyze(&mut index, &source, AnalysisContext::rust(), false).is_err());
    assert!(index.snapshot(None).is_err());
    for name in [
        "nodes_lookup_name",
        "nodes_lookup_qualified",
        "nodes_file",
        "nodes_package",
        "edges_read_source",
        "edges_read_target",
    ] {
        let count: u32 = index
            .connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name=?1",
                [name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "rollback lost {name}");
    }
    index
        .connection
        .execute_batch("DROP TRIGGER first_failure")
        .unwrap();
    let snapshot = analysis::analyze(&mut index, &source, AnalysisContext::rust(), false).unwrap();
    assert_eq!(snapshot.stats.parsed_files, 0);
    assert_eq!(snapshot.completeness.status, "complete");
    let original = index
        .find_nodes(&snapshot.id, "original", 1)
        .unwrap()
        .pop()
        .unwrap();
    marks::set(
        &index,
        &snapshot,
        &original,
        "seen",
        "survives index migration",
    )
    .unwrap();
    index.connection.execute_batch("CREATE INDEX nodes_name ON nodes(snapshot,name); CREATE INDEX edges_source ON edges(snapshot,source,kind);").unwrap();
    drop(index);
    let index = Index::open(root.path(), Some(cache.path())).unwrap();
    assert_eq!(index.snapshot(None).unwrap().id, snapshot.id);
    assert_eq!(
        marks::get(&index, &snapshot, &original).unwrap()["state"],
        "seen"
    );
    assert_eq!(
        index.find_nodes(&snapshot.id, "original", 1).unwrap()[0].id,
        original.id
    );
}

#[test]
fn omitted_non_utf8_source_is_explicitly_partial() {
    let root = repo();
    let cache = TempDir::new().unwrap();
    fs::write(root.path().join("src/unreadable.rs"), [0xff, 0xfe]).unwrap();
    let mut index = Index::open(root.path(), Some(cache.path())).unwrap();
    let snapshot = capture(&mut index, root.path());
    assert_eq!(snapshot.completeness.status, "partial");
    assert!(snapshot
        .diagnostics
        .iter()
        .any(|d| d.code == "non_utf8_source" && d.path.as_deref() == Some("src/unreadable.rs")));
}

#[test]
fn absent_semantic_binary_returns_partial_not_fabricated_edges() {
    let root = repo();
    let cache = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_codexis"))
        .arg("--project")
        .arg(root.path())
        .arg("--cache-dir")
        .arg(cache.path())
        .args(["analyze", "--analysis", "semantic", "--format", "json"])
        .env("PATH", cache.path())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["code"] == "semantic_unavailable"));
    assert_eq!(report["data"]["stats"]["resolved_calls"], 0);
}

#[test]
fn report_escaping_literal_queries_stable_ordering_and_conflicting_arguments() {
    let root = repo();
    let cache = TempDir::new().unwrap();
    assert!(cli(root.path(), cache.path(), &["analyze"])
        .status
        .success());
    let first = cli(root.path(), cache.path(), &["map", "--format", "json"]).stdout;
    assert_eq!(
        first,
        cli(root.path(), cache.path(), &["map", "--format", "json"]).stdout
    );
    assert_eq!(
        cli(root.path(), cache.path(), &["inspect", "%"])
            .status
            .code(),
        Some(1)
    );
    let marked = cli(
        root.path(),
        cache.path(),
        &[
            "mark",
            "original",
            "--state",
            "question",
            "--note",
            "~~~\n<script>alert(1)</script>\n~~~\n\u{1b}[31m",
        ],
    );
    assert!(marked.status.success());
    let markdown = cli(
        root.path(),
        cache.path(),
        &["inspect", "original", "--format", "markdown"],
    );
    let markdown = String::from_utf8(markdown.stdout).unwrap();
    assert!(
        markdown.contains("~~~~text"),
        "source/notes cannot break the Markdown fence"
    );
    assert!(!markdown.contains('\u{1b}'));
    assert!(markdown.ends_with("~~~~\n"));
    let json = cli(
        root.path(),
        cache.path(),
        &["inspect", "original", "--format", "json"],
    );
    let parsed: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert!(parsed["data"]["mark"]["note"]
        .as_str()
        .unwrap()
        .contains("<script>"));
    assert!(!json.stdout.contains(&0x1b), "JSON controls are escaped");
    assert_eq!(
        cli(
            root.path(),
            cache.path(),
            &["analyze", cache.path().to_str().unwrap()]
        )
        .status
        .code(),
        Some(2)
    );
    assert_eq!(
        cli(
            root.path(),
            cache.path(),
            &["analyze", "--snapshot", "ignored"]
        )
        .status
        .code(),
        Some(2)
    );
    assert_eq!(
        cli(root.path(), cache.path(), &["map", "--overwrite"])
            .status
            .code(),
        Some(2)
    );
}

#[cfg(unix)]
#[test]
fn ctrl_c_during_analysis_preserves_completed_snapshot_and_allows_restart() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    let root = repo();
    let cache = TempDir::new().unwrap();
    let mut index = Index::open(root.path(), Some(cache.path())).unwrap();
    let first = capture(&mut index, root.path());
    let file = (0..300)
        .map(|i| format!("pub fn added_{i}() {{}}\n"))
        .collect::<String>();
    for n in 0..30 {
        write(root.path(), &format!("src/add_{n}.rs"), &file);
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_codexis"))
        .arg("--project")
        .arg(root.path())
        .arg("--cache-dir")
        .arg(cache.path())
        .args(["analyze", "--format", "json", "--verbose"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let errors = child.stderr.take().unwrap();
    let mut reader = BufReader::new(errors);
    let mut line = String::new();
    loop {
        assert!(
            reader.read_line(&mut line).unwrap() != 0,
            "analysis ended before cancellation point"
        );
        if line.contains("Indexing ") {
            break;
        }
        line.clear();
    }
    let signal = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(signal.success());
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        output.stdout.is_empty(),
        "no incomplete report may be published after cancellation"
    );
    assert_eq!(index.snapshot(None).unwrap().id, first.id);
    let resumed = capture(&mut index, root.path());
    assert_eq!(resumed.completeness.status, "complete");
    assert_ne!(resumed.id, first.id);
}
