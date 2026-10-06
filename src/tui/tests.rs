use super::{
    browser::{Action, Browser},
    clip,
    jobs::{Completed, Request},
};
use crate::{
    analysis,
    index::Index,
    model::{AnalysisContext, Snapshot},
    query, review,
    source::{SourceProvider, WorkingTreeSource},
};
use std::{fs, path::Path};
use tempfile::TempDir;
use unicode_width::UnicodeWidthStr;

fn write(root: &Path, path: &str, content: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn fixture() -> (TempDir, TempDir, Index, Snapshot) {
    let root = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    write(root.path(), "Cargo.toml", "[package]\nname='browser-demo'\nversion='0.1.0'\nedition='2021'\ndescription='A source-backed example'\n");
    write(root.path(), "src/main.rs", "fn main() { greet(); }\nfn greet() {}\npub struct Config;\ntrait Engine { fn infer(&self); }\nimpl Engine for Config { fn infer(&self) {} }\n");
    write(root.path(), "examples/not_main_entry.rs", "fn main() {}\n");
    write(root.path(), "build.rs", "fn main() {}\n");
    let mut index = Index::open(root.path(), Some(cache.path())).unwrap();
    let source = WorkingTreeSource {
        root: root.path().into(),
    }
    .snapshot()
    .unwrap();
    let snapshot = analysis::analyze(&mut index, &source, AnalysisContext::rust(), false).unwrap();
    (root, cache, index, snapshot)
}

fn browser(index: &Index, snapshot: Snapshot) -> Browser<'_> {
    let guide = query::overview(index, &snapshot).unwrap().data["reading_guide"].clone();
    Browser::new(index, snapshot, guide).unwrap()
}

#[test]
fn flow_home_opens_source_and_keeps_explore_reachable() {
    let (_root, _cache, index, snapshot) = fixture();
    let mut ui = browser(&index, snapshot.clone());
    assert_eq!(ui.page.title, "脉络图");
    assert!(ui.detail.is_empty());
    assert!(ui
        .page
        .rows
        .iter()
        .any(|r| r.text.contains("browser_demo::main")));
    assert!(!ui.page.rows.iter().any(|r| r.text.contains(&snapshot.id)));
    assert!(
        ui.page.items.len() >= 2,
        "rows with source positions are selectable"
    );
    assert_eq!(ui.page.items.len(), ui.page.row_items.len());
    ui.select(1).unwrap();
    ui.enter().unwrap();
    assert!(ui.page.title.starts_with("源码"));
    ui.back().unwrap();
    assert_eq!(ui.page.title, "脉络图");
    assert_eq!(ui.page.selected, 1);
    ui.open(Action::Explore).unwrap();
    assert!(ui.page.items[0].label.contains("项目解释"));
    ui.select(1).unwrap();
    ui.enter().unwrap();
    assert_eq!(ui.page.title, "入口与调用");
    assert_eq!(ui.page.items.len(), 1, "exclude example/build entry points");
    ui.enter().unwrap();
    assert!(ui.page.title.ends_with("::main"));
    assert!(ui.detail.iter().any(|l| l.contains("greet();")));
    ui.back().unwrap();
    ui.back().unwrap();
    assert_eq!(ui.page.title, "深入分析");
    ui.open(Action::FlowView("errors".into())).unwrap();
    assert_eq!(ui.page.title, "错误码");
    ui.back().unwrap();
    ui.back().unwrap();
    assert_eq!(ui.page.title, "脉络图");
}

#[test]
fn types_search_relations_and_source_have_real_evidence() {
    let (_root, _cache, index, snapshot) = fixture();
    let mut ui = browser(&index, snapshot);
    ui.open(Action::Symbols {
        package: String::new(),
        path: String::new(),
        types: true,
        offset: 0,
    })
    .unwrap();
    assert!(ui.page.items.iter().any(|i| i.label.contains("Config")));
    assert!(ui.page.items.iter().any(|i| i.label.contains("Engine")));
    ui.open(Action::Search {
        query: "main".into(),
        offset: 0,
    })
    .unwrap();
    assert!(!ui.page.items.is_empty());
    let main = ui
        .page
        .items
        .iter()
        .position(|i| i.hint.contains("src/main.rs"))
        .unwrap();
    ui.page.selected = main;
    ui.enter().unwrap();
    ui.select(1).unwrap();
    ui.enter().unwrap();
    assert!(matches!(
        ui.page.items[0].action,
        Action::Job(Request::Semantic(_))
    ));
    let edge = ui
        .page
        .items
        .iter()
        .position(|i| i.label.contains("greet"))
        .unwrap();
    ui.page.selected = edge;
    ui.enter().unwrap();
    assert!(ui.page.intro.iter().any(|l| l.contains("未定位")));
    ui.enter().unwrap();
    assert!(ui
        .detail
        .iter()
        .any(|l| l.contains("fn main() { greet(); }")));
    ui.open(Action::Search {
        query: "%".into(),
        offset: 0,
    })
    .unwrap();
    assert!(
        ui.page.items.is_empty(),
        "literal search does not treat % as a wildcard"
    );
}

#[test]
fn mark_menu_preserves_notes_and_rejects_changed_source() {
    let (root, _cache, index, snapshot) = fixture();
    let mut ui = browser(&index, snapshot.clone());
    let node = index
        .find_nodes(&snapshot.id, "greet", 1)
        .unwrap()
        .pop()
        .unwrap();
    ui.open(Action::Node {
        snapshot: snapshot.id.clone(),
        id: node.id.clone(),
    })
    .unwrap();
    ui.mark(&snapshot.id, &node.id, "question", Some("检查空输入"))
        .unwrap();
    ui.open(Action::Marks {
        snapshot: snapshot.id.clone(),
        id: node.id.clone(),
    })
    .unwrap();
    ui.enter().unwrap();
    assert!(ui.page.intro.iter().any(|l| l.contains("已读")));
    assert!(ui.page.intro.iter().any(|l| l.contains("检查空输入")));
    write(
        root.path(),
        "src/main.rs",
        "fn main() {}\nfn greet() { panic!(); }\n",
    );
    assert!(ui.mark(&snapshot.id, &node.id, "seen", None).is_err());
}

#[test]
fn review_drills_into_both_pinned_versions_and_diff() {
    let (root, _cache, mut index, before) = fixture();
    write(
        root.path(),
        "src/main.rs",
        "fn main() { greet(); }\nfn greet() { println!(\"changed\"); }\n",
    );
    let source = WorkingTreeSource {
        root: root.path().into(),
    }
    .snapshot()
    .unwrap();
    let after = analysis::analyze(&mut index, &source, AnalysisContext::rust(), false).unwrap();
    let report = review::compare(&index, &before, &after, 50, 0).unwrap();
    let mut ui = browser(&index, before.clone());
    ui.completed(Completed::Review(Box::new(report))).unwrap();
    assert!(ui.page.intro.iter().any(|l| l.contains("未计算调用影响")));
    let change = ui
        .page
        .items
        .iter()
        .position(|i| i.label.contains("greet"))
        .unwrap();
    ui.page.selected = change;
    ui.enter().unwrap();
    assert_eq!(ui.page.items.len(), 2);
    for (i, expected) in [&before.id, &after.id].iter().enumerate() {
        match &ui.page.items[i].action {
            Action::Node { snapshot, .. } => assert_eq!(snapshot, *expected),
            _ => panic!("expected pinned node"),
        }
    }
    ui.enter().unwrap();
    assert!(ui.detail.iter().any(|l| l.contains("fn greet() {}")));
    ui.back().unwrap();
    ui.select(1).unwrap();
    ui.enter().unwrap();
    assert!(ui
        .detail
        .iter()
        .any(|l| l.contains("println!(\"changed\")")));
    ui.back().unwrap();
    ui.back().unwrap();
    assert!(ui.page.items.iter().any(|i| i.label.starts_with("diff")));
}

#[test]
fn long_lists_are_paged_without_losing_symbols() {
    let (root, _cache, mut index, _) = fixture();
    let definitions = (0..205)
        .map(|i| format!("pub struct Entry{i:03};\n"))
        .collect::<String>();
    write(root.path(), "src/main.rs", &definitions);
    let source = WorkingTreeSource {
        root: root.path().into(),
    }
    .snapshot()
    .unwrap();
    let snapshot = analysis::analyze(&mut index, &source, AnalysisContext::rust(), false).unwrap();
    let mut ui = browser(&index, snapshot);
    ui.open(Action::Symbols {
        package: String::new(),
        path: String::new(),
        types: true,
        offset: 0,
    })
    .unwrap();
    assert_eq!(ui.page.items.len(), 101);
    ui.select(isize::MAX).unwrap();
    ui.enter().unwrap();
    assert!(ui.page.items[0].label.ends_with("Entry100"));
    ui.select(isize::MAX).unwrap();
    ui.enter().unwrap();
    assert_eq!(ui.page.items.len(), 5);
    ui.back().unwrap();
    assert_eq!(ui.page.selected, 100);
}

#[test]
fn terminal_text_is_safe_and_uses_display_cells() {
    assert_eq!(clip("中文abc", 5, 0), "中文a");
    assert_eq!(clip("中文abc", 3, 2), "文a");
    assert_eq!(clip("\x1b[2Jhi\r\n\x07\u{202e}", 80, 0), "[2Jhi");
    for width in 0..100 {
        assert!(clip("hello 中文 😀 e\u{301}", width, 0).width() <= width);
    }
}

#[test]
fn background_review_handles_git_and_rejects_stale_semantic_input() {
    use super::jobs::Job;
    use std::{
        process::Command,
        thread,
        time::{Duration, Instant},
    };
    fn finish(job: &mut Job) -> Result<Completed, String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(result) = job.poll() {
                return result;
            }
            assert!(Instant::now() < deadline, "worker did not finish");
            thread::sleep(Duration::from_millis(10));
        }
    }
    let (root, cache, index, snapshot) = fixture();
    let hashes = index.file_hashes(&snapshot.id).unwrap();
    let request = Request::Review {
        base: "HEAD".into(),
        head: None,
        semantic: false,
    };
    let mut missing = Job::start(
        request.clone(),
        snapshot.clone(),
        Some(cache.path().into()),
        hashes.clone(),
    );
    assert!(
        finish(&mut missing).is_err(),
        "non-Git project should return a recoverable error"
    );
    for args in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Codexis test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "fixture",
        ],
    ] {
        assert!(Command::new("git")
            .args(args)
            .current_dir(root.path())
            .status()
            .unwrap()
            .success());
    }
    write(
        root.path(),
        "src/main.rs",
        "fn main() {}\nfn greet() { panic!(); }\n",
    );
    let mut job = Job::start(
        request,
        snapshot.clone(),
        Some(cache.path().into()),
        hashes.clone(),
    );
    let result = finish(&mut job).unwrap();
    let mut ui = browser(&index, snapshot.clone());
    ui.completed(result).unwrap();
    assert_eq!(ui.page.title, "改动批次 · 摘要与核查");
    assert!(ui.page.items.iter().any(|i| i.label.contains("分组")));
    let mut stale = Job::start(
        Request::Semantic("browser-demo".into()),
        snapshot,
        Some(cache.path().into()),
        hashes,
    );
    assert!(finish(&mut stale).err().unwrap().contains("源码已变化"));
}
