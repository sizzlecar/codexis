//! Read-only Ferrum acceptance runner. Reports and caches use a new directory
//! outside the analyzed repository. Optional semantic probes reuse measure.rs.
use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use codexis::understanding::DIMENSION_IDS;
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

#[derive(Parser)]
#[command(about = "Validate Ferrum with stored reports and read-only Git checks")]
struct Args {
    /// Ferrum checkout; its source and Git state remain unchanged.
    project: PathBuf,
    /// New, nonexistent output directory outside the Ferrum checkout.
    output: PathBuf,
    /// Codexis executable; defaults to the example's adjacent build directory.
    #[arg(long)]
    binary: Option<PathBuf>,
    /// Rust measurement executable; defaults to the adjacent measure example.
    #[arg(long)]
    measure: Option<PathBuf>,
    #[arg(long, default_value = "en", value_parser = ["zh-CN", "en"])]
    locale: String,
    /// Run the four bounded rust-analyzer profiles from the previous harness.
    #[arg(long)]
    semantic: bool,
    /// Also run historical acceptance assertions (macOS; implies --semantic).
    #[arg(long)]
    legacy: bool,
    #[arg(long, default_value = "HEAD~2")]
    base: String,
    #[arg(long, default_value = "HEAD~1")]
    head: String,
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create new artifact {}", path.display()))?
        .write_all(bytes)?;
    Ok(())
}

fn git(project: &Path, arguments: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(arguments)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()?;
    ensure!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

struct Runner {
    project: PathBuf,
    output: PathBuf,
    binary: PathBuf,
    measure: PathBuf,
    locale: String,
    legacy: bool,
    metrics: Vec<Value>,
}

impl Runner {
    fn report(&mut self, name: &str, arguments: &[&str]) -> Result<Value> {
        println!("{name}: {}", arguments.join(" "));
        let report_path = self.output.join(format!("{name}.json"));
        let metrics_path = self.output.join(format!("{name}.metrics.json"));
        let cache = self.output.join("cache");
        let mut cli = vec![
            OsString::from("--cache-dir"),
            cache.into_os_string(),
            OsString::from("--project"),
            self.project.clone().into_os_string(),
            OsString::from("--locale"),
            OsString::from(&self.locale),
            OsString::from("--format"),
            OsString::from("json"),
            OsString::from("--output"),
            report_path.clone().into_os_string(),
        ];
        cli.extend(arguments.iter().map(OsString::from));
        let legacy_time = self.legacy && matches!(name, "cold" | "warm" | "map" | "inspect");
        let mut command = if legacy_time {
            let mut command = Command::new("/usr/bin/time");
            command
                .args(["-l", "-o"])
                .arg(self.output.join(format!("{name}.time.txt")))
                .arg(&self.binary);
            command
        } else {
            let mut command = Command::new(&self.measure);
            command.arg(&metrics_path).arg(&self.binary);
            command
        };
        let started = Instant::now();
        let output = command
            .args(&cli)
            .output()
            .with_context(|| format!("run {name}"))?;
        write_new(
            &self.output.join(format!("{name}.stdout.txt")),
            &output.stdout,
        )?;
        write_new(
            &self.output.join(format!("{name}.stderr.txt")),
            &output.stderr,
        )?;
        if legacy_time {
            write_new(
                &metrics_path,
                &serde_json::to_vec_pretty(&json!({
                    "elapsed_ms":started.elapsed().as_millis(),
                    "exit_code":output.status.code(),
                    "method":"Rust wall clock; exact legacy /usr/bin/time report stored separately"
                }))?,
            )?;
        }
        let metrics: Value = serde_json::from_slice(&fs::read(&metrics_path)?)?;
        self.metrics.push(json!({"step":name,"metrics":metrics}));
        ensure!(
            output.status.success(),
            "{name} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&fs::read(&report_path)?)
            .with_context(|| format!("read report {}", report_path.display()))
    }

    fn profile(&mut self, name: &str, scope: &str) -> Result<()> {
        let report = self.report(
            name,
            &[
                "analyze",
                "--language",
                "rust",
                "--analysis",
                "semantic",
                "--scope",
                scope,
                "--semantic-timeout-secs",
                "180",
                "--semantic-request-limit",
                "5000",
            ],
        )?;
        ensure!(
            report["analysis_context"]["analysis"] == "semantic",
            "{name}: expected semantic analysis"
        );
        if self.legacy {
            ensure!(
                report["completeness"]["status"] == "complete",
                "{name}: legacy acceptance requires complete semantic analysis"
            );
        }
        Ok(())
    }

    fn validate(&mut self, args: &Args) -> Result<()> {
        let rustc = Command::new("rustc").arg("--version").output()?;
        ensure!(rustc.status.success(), "rustc --version failed");
        write_new(&self.output.join("rustc.txt"), &rustc.stdout)?;
        if args.semantic || args.legacy {
            let analyzer = Command::new("rust-analyzer")
                .arg("--version")
                .output()
                .context("optional semantic probes require local rust-analyzer")?;
            ensure!(analyzer.status.success(), "rust-analyzer --version failed");
            write_new(&self.output.join("rust-analyzer.txt"), &analyzer.stdout)?;
        }
        let cold = self.report("cold", &["analyze", "--language", "rust"])?;
        ensure!(
            cold["completeness"]["status"] == "complete"
                && cold["data"]["stats"]["failed_files"] == 0,
            "cold structural analysis was incomplete"
        );
        let warm = self.report("warm", &["analyze", "--language", "rust"])?;
        ensure!(
            warm["data"]["stats"]["parsed_files"] == 0
                && warm["data"]["stats"]["reused_files"] == warm["data"]["stats"]["source_files"],
            "warm analysis did not reuse every parsed source file"
        );
        let map = self.report("map", &["map"])?;
        ensure!(
            map["data"]["total"].as_u64().unwrap_or(0) > 0,
            "package map is empty"
        );
        let all = self.report("understand", &["understand"])?;
        let dimensions = all["data"]["understanding"]["dimensions"]
            .as_array()
            .context("missing dimensions")?;
        ensure!(
            dimensions.len() == DIMENSION_IDS.len(),
            "expected all eight dimensions"
        );
        for dimension in DIMENSION_IDS {
            let report = self.report(
                &format!("understand-{dimension}"),
                &["understand", "--dimension", dimension],
            )?;
            let selected = report["data"]["understanding"]["dimensions"]
                .as_array()
                .context("missing selected dimension")?;
            ensure!(
                selected.len() == 1 && selected[0]["id"] == dimension,
                "dimension selection failed: {dimension}"
            );
        }
        let scope = "crates/ferrum-server";
        let scoped = self.report("understand-scope", &["understand", "--scope", scope])?;
        ensure!(scoped["data"]["scope"] == scope, "scope was not preserved");
        ensure!(
            !scoped["data"]["understanding"]["architecture"]["components"]
                .as_array()
                .context("missing scoped architecture")?
                .is_empty(),
            "Ferrum server scope produced no components"
        );
        let inspect = self.report("inspect", &["inspect", "LlmInferenceEngine"])?;
        ensure!(
            inspect["data"]["kind"] == "inspect"
                && inspect["data"]["node"]["name"] == "LlmInferenceEngine",
            "interface inspect did not identify LlmInferenceEngine"
        );
        let syntax = self.report(
            "syntax-trace",
            &[
                "trace",
                "handle_chat_completions_sync",
                "--depth",
                "2",
                "--limit",
                "30",
            ],
        )?;
        ensure!(
            syntax["data"]["kind"] == "trace"
                && !syntax["data"]["edges"]
                    .as_array()
                    .context("missing trace edges")?
                    .is_empty(),
            "syntax trace did not expose the chat call neighborhood"
        );
        let history = self.report(
            "history",
            &[
                "review", "--base", &args.base, "--head", &args.head, "--limit", "20",
            ],
        )?;
        ensure!(
            history["data"]["kind"] == "review"
                && history["data"]["base_revision"] != history["data"]["head_revision"],
            "history review did not compare distinct revisions"
        );
        ensure!(
            history["data"]["batch"]["groups"].is_array()
                && history["data"]["batch"]["checklist"].is_array(),
            "history review is missing batch groups or checklist"
        );
        if args.semantic || args.legacy {
            self.profile("server", "ferrum-server")?;
            let chat = self.report(
                "chat-trace",
                &[
                    "trace",
                    "handle_chat_completions_sync",
                    "--depth",
                    "2",
                    "--limit",
                    "30",
                ],
            )?;
            ensure!(
                chat["data"]["kind"] == "trace",
                "chat semantic trace is missing"
            );
            self.profile("cli", "ferrum::command_future")?;
            self.report(
                "cli-trace",
                &[
                    "trace",
                    "ferrum::command_future",
                    "--depth",
                    "1",
                    "--limit",
                    "100",
                ],
            )?;
            self.profile(
                "prefix",
                "crates/ferrum-engine/src/continuous_engine/inner/prefix_restore.rs",
            )?;
            self.report(
                "prefix-trace",
                &[
                    "trace",
                    "restore_admitted_prefixes",
                    "--depth",
                    "1",
                    "--limit",
                    "100",
                ],
            )?;
            let implementation =
                "ferrum_engine::continuous_engine::<ContinuousBatchEngine as LlmInferenceEngine>";
            self.profile("engine", implementation)?;
            self.report("engine-impl", &["inspect", implementation])?;
        }
        Ok(())
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.legacy && !cfg!(target_os = "macos") {
        bail!(
            "--legacy uses macOS /usr/bin/time -l; use the default or --semantic on this platform"
        );
    }
    let project = args.project.canonicalize().context("open Ferrum project")?;
    ensure!(project.is_dir(), "project must be a directory");
    let executable = std::env::current_exe()?;
    let examples = executable
        .parent()
        .context("example executable has no parent")?;
    let binary = args
        .binary
        .clone()
        .unwrap_or_else(|| examples.parent().unwrap_or(examples).join("codexis"));
    let measure = args
        .measure
        .clone()
        .unwrap_or_else(|| examples.join("measure"));
    ensure!(
        binary.is_file() && measure.is_file(),
        "build the Rust tools first: cargo build --release --bin codexis --examples"
    );
    let binary = binary.canonicalize()?;
    let measure = measure.canonicalize()?;
    let output = if args.output.is_absolute() {
        args.output.clone()
    } else {
        std::env::current_dir()?.join(&args.output)
    };
    ensure!(
        !output.exists(),
        "output directory must be new: {}",
        output.display()
    );
    let parent = output.parent().context("output directory needs a parent")?;
    let output = parent
        .canonicalize()
        .context("output parent directory must already exist")?
        .join(output.file_name().context("invalid output directory")?);
    ensure!(
        !output.starts_with(&project),
        "output directory must be outside the analyzed project"
    );
    fs::create_dir(&output)
        .context("create new validation directory; existing directories are not reused")?;
    let before = git(&project, &["status", "--porcelain=v1", "-z"])?;
    let revision = git(&project, &["rev-parse", "HEAD"])?;
    write_new(&output.join("git-status-before.bin"), &before)?;
    write_new(&output.join("revision.txt"), &revision)?;
    let mut runner = Runner {
        project,
        output,
        binary,
        measure,
        locale: args.locale.clone(),
        legacy: args.legacy,
        metrics: Vec::new(),
    };
    let mut validation = runner.validate(&args);
    // Capture and compare repository state even if a report fails.
    let after = git(&runner.project, &["status", "--porcelain=v1", "-z"])?;
    let after_revision = git(&runner.project, &["rev-parse", "HEAD"])?;
    write_new(&runner.output.join("git-status-after.bin"), &after)?;
    write_new(&runner.output.join("revision-after.txt"), &after_revision)?;
    if args.legacy && validation.is_ok() && before == after && revision == after_revision {
        validation = (|| -> Result<()> {
            let result = Command::new("cargo")
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .args([
                    "test",
                    "--test",
                    "ferrum_acceptance",
                    "--locked",
                    "--",
                    "--ignored",
                ])
                .env("CODEXIS_FERRUM_ROOT", &runner.project)
                .env("CODEXIS_FERRUM_ARTIFACTS", &runner.output)
                .output()?;
            write_new(
                &runner.output.join("legacy-acceptance.stdout.txt"),
                &result.stdout,
            )?;
            write_new(
                &runner.output.join("legacy-acceptance.stderr.txt"),
                &result.stderr,
            )?;
            ensure!(
                result.status.success(),
                "legacy Ferrum acceptance assertions failed; inspect legacy-acceptance.stdout.txt"
            );
            Ok(())
        })();
    }
    write_new(
        &runner.output.join("validation.json"),
        &serde_json::to_vec_pretty(&json!({
            "passed":validation.is_ok() && before == after && revision == after_revision,
            "semantic_requested":args.semantic || args.legacy,"legacy_requested":args.legacy,
            "project":runner.project,"revision":String::from_utf8_lossy(&revision).trim(),
            "git_unchanged":before == after && revision == after_revision,
            "error":validation.as_ref().err().map(|error|format!("{error:#}")),"steps":runner.metrics,
        }))?,
    )?;
    ensure!(
        before == after && revision == after_revision,
        "project Git status or HEAD changed during validation"
    );
    validation?;
    println!(
        "PASS: reports and read-only Git checks in {}",
        runner.output.display()
    );
    Ok(())
}
