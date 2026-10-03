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
fn cli(root: &Path, cache: &Path, locale: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_codexis"))
        .env_remove("CODEXIS_LOCALE")
        .arg("--project")
        .arg(root)
        .arg("--cache-dir")
        .arg(cache)
        .arg("--locale")
        .arg(locale)
        .args(args)
        .output()
        .unwrap()
}
fn text(root: &Path, cache: &Path, locale: &str, args: &[&str]) -> String {
    let output = cli(root, cache, locale, args);
    assert!(
        matches!(output.status.code(), Some(0 | 3)),
        "{locale} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn both_locales_cover_cli_reports_and_preserve_source_and_human_content() {
    let root = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    write(root.path(), "Cargo.toml", "[package]\nname='locale-demo'\nversion='0.1.0'\nedition='2021'\ndescription='Original English project description'\n");
    write(root.path(), "src/lib.rs", "//! 原始模块说明 Original module documentation\n/// 原始函数说明 Original function documentation\npub fn run() -> u32 { 7 }\n");
    write(
        root.path(),
        "README.md",
        "# Original README\n用户原文 Preserve the original documentation.\n",
    );
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "locale-fixture@example.invalid"],
        vec!["config", "user.name", "Locale fixture"],
        vec!["add", "."],
        vec!["commit", "-qm", "initial"],
    ] {
        let output = Command::new("git")
            .current_dir(root.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let mut snapshot_id = None;
    let mut context = None;
    for (locale, overview, reading, title, help_title, source_title, empty) in [
        (
            "zh-CN",
            "项目概览",
            "从这里开始读",
            "能力与意图",
            "用法：",
            "已保存源码：",
            "尚未保存结论",
        ),
        (
            "en",
            "Project overview",
            "Start reading here",
            "Capabilities and intent",
            "Usage:",
            "Stored source:",
            "No conclusions saved",
        ),
    ] {
        let help = text(root.path(), cache.path(), locale, &["--help"]);
        assert!(help.contains(help_title) && help.contains("--locale"));
        assert!(help.contains(if locale == "en" {
            "Interface and report language"
        } else {
            "界面与报告语言"
        }));
        let analyze_help = text(root.path(), cache.path(), locale, &["analyze", "--help"]);
        assert!(analyze_help.contains(if locale == "en" {
            "Capture source"
        } else {
            "读取项目源码"
        }));
        let analyzed: Value = serde_json::from_str(&text(
            root.path(),
            cache.path(),
            locale,
            &["analyze", "--format", "json"],
        ))
        .unwrap();
        assert_eq!(analyzed["locale"], locale);
        if let Some(ref expected) = snapshot_id {
            assert_eq!(&analyzed["snapshot_id"], expected);
        }
        if let Some(ref expected) = context {
            assert_eq!(&analyzed["analysis_context"], expected);
        }
        snapshot_id = Some(analyzed["snapshot_id"].clone());
        context = Some(analyzed["analysis_context"].clone());
        let dimensions = analyzed["data"]["understanding"]["dimensions"]
            .as_array()
            .unwrap();
        assert_eq!(dimensions.len(), 8);
        assert_eq!(dimensions[0]["title"], title);
        assert!(analyzed
            .to_string()
            .contains("Original English project description"));
        assert!(analyzed
            .to_string()
            .contains("原始模块说明 Original module documentation"));

        let compact = text(root.path(), cache.path(), locale, &["analyze", "--plain"]);
        assert!(compact.contains(overview) && compact.contains(reading));
        assert!(compact.contains("Original English project description"));
        let inspection = text(
            root.path(),
            cache.path(),
            locale,
            &["inspect", "locale_demo::run"],
        );
        assert!(inspection.contains(source_title));
        assert!(inspection.contains("pub fn run() -> u32 { 7 }"));
        let understand = text(
            root.path(),
            cache.path(),
            locale,
            &[
                "understand",
                "--dimension",
                "intent",
                "--format",
                "markdown",
            ],
        );
        assert!(understand.contains(if locale == "en" {
            "# Codexis Project Understanding and Review"
        } else {
            "# Codexis 项目理解与审阅"
        }));
        assert!(understand.contains("Original module documentation"));
        assert!(understand.contains(if locale == "en" {
            "Basis:"
        } else {
            "依据："
        }));
        // Each locale gets an independent knowledge store for the empty state.
        let empty_cache = TempDir::new().unwrap();
        text(
            root.path(),
            empty_cache.path(),
            locale,
            &["analyze", "--format", "json"],
        );
        assert!(text(root.path(), empty_cache.path(), locale, &["knowledge"]).contains(empty));
        let review = text(
            root.path(),
            cache.path(),
            locale,
            &["review", "--base", "HEAD", "--head", "HEAD"],
        );
        assert!(review.contains(if locale == "en" {
            "Batch change summary"
        } else {
            "批次改动摘要"
        }));
        let review_md = text(
            root.path(),
            cache.path(),
            locale,
            &[
                "review", "--base", "HEAD", "--head", "HEAD", "--format", "markdown",
            ],
        );
        assert!(review_md.contains(if locale == "en" {
            "## Review Checklist"
        } else {
            "## 重点核查清单"
        }));
    }
    let claim = "用户结论 Keep my own words exactly";
    let remembered: Value = serde_json::from_str(&text(
        root.path(),
        cache.path(),
        "zh-CN",
        &[
            "remember",
            "locale_demo::run",
            "--claim",
            claim,
            "--format",
            "json",
        ],
    ))
    .unwrap();
    assert_eq!(remembered["data"]["record"]["claim"], claim);
    let knowledge = text(
        root.path(),
        cache.path(),
        "en",
        &["knowledge", "--format", "markdown"],
    );
    assert!(knowledge.contains(claim) && knowledge.contains("State:"));

    let env_help = Command::new(env!("CARGO_BIN_EXE_codexis"))
        .env("CODEXIS_LOCALE", "en")
        .arg("--help")
        .output()
        .unwrap();
    assert!(
        env_help.status.success()
            && String::from_utf8(env_help.stdout)
                .unwrap()
                .contains("Usage:")
    );
    let override_help = Command::new(env!("CARGO_BIN_EXE_codexis"))
        .env("CODEXIS_LOCALE", "en")
        .args(["--locale=zh-CN", "--help"])
        .output()
        .unwrap();
    assert!(
        override_help.status.success()
            && String::from_utf8(override_help.stdout)
                .unwrap()
                .contains("用法：")
    );
    assert_eq!(
        cli(root.path(), cache.path(), "unsupported", &["--help"])
            .status
            .code(),
        Some(2)
    );
}
