use crate::{
    analysis,
    index::Index,
    marks,
    model::{AnalysisContext, Report},
    query, render, review,
    source::{GitSource, SourceProvider, WorkingTreeSource},
};
use anyhow::{bail, Context, Result};
use clap::{Arg, ArgAction, Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use serde_json::Value;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;

#[derive(Parser)]
#[command(version, about="Evidence-backed project maps and code review", long_about=None)]
struct Cli {
    /// Interface and report language; independent from the analyzed source language.
    #[arg(long, global = true, default_value = "zh-CN", value_parser = ["zh-CN", "en"])]
    locale: String,
    #[arg(long, global = true)]
    project: Option<PathBuf>,
    #[arg(long, global = true)]
    cache_dir: Option<PathBuf>,
    #[arg(long, global = true, value_enum, default_value = "text")]
    format: Format,
    #[arg(long, global = true)]
    output: Option<PathBuf>,
    #[arg(long, global = true, requires = "output")]
    overwrite: bool,
    #[arg(long, global = true)]
    snapshot: Option<String>,
    /// Print a compact report instead of opening the terminal browser.
    #[arg(long, global = true)]
    plain: bool,
    /// Include technical details and analysis progress (disables the browser).
    #[arg(long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Text,
    Markdown,
    Json,
}
#[derive(Clone, Copy, ValueEnum)]
enum Analysis {
    Syntax,
    Semantic,
}
#[derive(Clone, Copy, ValueEnum)]
enum Language {
    Auto,
    Rust,
    Python,
}

#[derive(Clone, Copy, ValueEnum)]
enum Dimension {
    All,
    Intent,
    Architecture,
    Behavior,
    Data,
    Runtime,
    Changes,
    Verification,
    Knowledge,
}
impl Dimension {
    fn id(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Intent => "intent",
            Self::Architecture => "architecture",
            Self::Behavior => "behavior",
            Self::Data => "data",
            Self::Runtime => "runtime",
            Self::Changes => "changes",
            Self::Verification => "verification",
            Self::Knowledge => "knowledge",
        }
    }
}
#[derive(Clone, Copy, ValueEnum)]
enum FlowView {
    Home,
    Errors,
    Config,
    State,
    Trunks,
}
impl FlowView {
    fn id(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Errors => "errors",
            Self::Config => "config",
            Self::State => "state",
            Self::Trunks => "trunks",
        }
    }
}
#[derive(Clone, Copy, ValueEnum)]
enum Level {
    Package,
    Module,
}
#[derive(Clone, Copy, ValueEnum)]
enum Direction {
    Callers,
    Callees,
}

#[derive(Clone, Copy, ValueEnum)]
enum MarkState {
    Unread,
    Seen,
    Question,
}

#[derive(Args)]
struct AnalysisOptions {
    /// Select the language explicitly for mixed repositories.
    #[arg(long, value_enum, default_value = "auto")]
    language: Language,
    #[arg(long, value_enum, default_value = "syntax")]
    analysis: Analysis,
    #[arg(long)]
    scope: Option<String>,
    #[arg(long)]
    target: Option<String>,
    #[arg(long, value_delimiter = ',')]
    features: Vec<String>,
    #[arg(long)]
    no_default_features: bool,
    #[arg(long, default_value_t = 180, value_parser = clap::value_parser!(u64).range(1..=3600))]
    semantic_timeout_secs: u64,
    #[arg(long, default_value = "5000", value_parser = parse_request_limit)]
    semantic_request_limit: usize,
}

impl AnalysisOptions {
    fn context(&self, source: &crate::source::SourceSet) -> Result<AnalysisContext> {
        let language = match self.language {
            Language::Auto => {
                let rust = source.files.contains_key("Cargo.toml");
                let python = source.files.contains_key("pyproject.toml")
                    || source.files.contains_key("setup.py")
                    || source.files.contains_key("setup.cfg");
                match (rust, python) {
                    (true, false) => Language::Rust,
                    (false, true) => Language::Python,
                    (true, true) => return Err(UsageError(crate::localize!("混合项目需要指定 --language rust 或 --language python", "mixed project roots require --language rust or --language python")).into()),
                    (false, false) => match (
                        source.files.keys().any(|p| p.ends_with(".rs")),
                        source.files.keys().any(|p| p.ends_with(".py") || p.ends_with(".pyi")),
                    ) {
                        (true, false) => Language::Rust,
                        (false, true) => Language::Python,
                        _ => return Err(UsageError(crate::localize!("无法确定唯一源码语言；请指定 --language rust 或 --language python", "cannot infer one source language; use --language rust or --language python")).into()),
                    },
                }
            }
            chosen => chosen,
        };
        if matches!(language, Language::Python)
            && (self.target.is_some() || !self.features.is_empty() || self.no_default_features)
        {
            return Err(UsageError(crate::localize!(
                "--target、--features 和 --no-default-features 仅支持 Rust",
                "--target, --features and --no-default-features are Rust-only options"
            ))
            .into());
        }
        let mut context = if matches!(language, Language::Python) {
            AnalysisContext::python()
        } else {
            AnalysisContext::rust()
        };
        context.analysis = match self.analysis {
            Analysis::Syntax => "syntax",
            Analysis::Semantic => "semantic",
        }
        .into();
        context.scope = self.scope.clone();
        context.target = self.target.clone();
        context.features = self.features.clone();
        context.features.sort();
        context.features.dedup();
        context.no_default_features = self.no_default_features;
        context.semantic_timeout_secs = self.semantic_timeout_secs;
        context.semantic_request_limit = self.semantic_request_limit;
        Ok(context)
    }
}

#[derive(Subcommand)]
enum Command {
    /// Read a saved business baseline or generate one by reading snapshot source with local Codex.
    Baseline {
        #[arg(long)]
        generate: bool,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=3600))]
        timeout_secs: u64,
    },
    /// Explore the eight dimensions of project understanding in a stored snapshot.
    Understand {
        #[arg(long, value_enum, default_value = "all")]
        dimension: Dimension,
        /// Filter findings by title, file path, or symbol.
        #[arg(long)]
        scope: Option<String>,
    },
    /// Save a human conclusion or question with evidence and revision history.
    Remember {
        query: String,
        #[arg(long)]
        claim: String,
        #[arg(long, value_enum, default_value = "intent")]
        dimension: Dimension,
        #[arg(long, default_value="confirmed", value_parser=["confirmed","question"])]
        state: String,
        #[arg(long)]
        evidence: Vec<String>,
        /// Revise an existing conclusion and retain its previous versions.
        #[arg(long)]
        id: Option<String>,
    },
    /// List saved conclusions, evidence changes, or the history of one conclusion.
    Knowledge {
        id: Option<String>,
        #[arg(long)]
        history: bool,
    },
    /// Capture source, update the index and generate a project overview.
    Analyze {
        path: Option<PathBuf>,
        #[command(flatten)]
        options: AnalysisOptions,
    },
    /// Show the static flow map: entries, trunk, exits, configuration and shared state.
    Flow {
        #[arg(long, value_enum, default_value = "home")]
        view: FlowView,
    },
    /// Query package or module structure in a stored snapshot.
    Map {
        #[arg(long, value_enum, default_value = "package")]
        level: Level,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long, default_value = "100", value_parser = parse_limit)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        cursor: usize,
    },
    /// Find a symbol and inspect stored source evidence.
    Inspect {
        query: String,
        #[arg(long, default_value = "20", value_parser = parse_limit)]
        limit: usize,
    },
    /// Explore a bounded neighborhood of call relationships.
    Trace {
        query: String,
        #[arg(long, value_enum, default_value = "callees")]
        direction: Direction,
        #[arg(long, default_value = "2", value_parser = parse_depth)]
        depth: usize,
        #[arg(long, default_value = "100", value_parser = parse_limit)]
        limit: usize,
    },
    /// Compare two commits, or a commit with the current working tree (including untracked source).
    Review {
        #[arg(long)]
        base: String,
        #[arg(
            long,
            required_unless_present = "worktree",
            conflicts_with = "worktree"
        )]
        head: Option<String>,
        #[arg(long)]
        worktree: bool,
        #[command(flatten)]
        options: AnalysisOptions,
        #[arg(long, default_value = "50", value_parser = parse_limit)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        cursor: usize,
    },
    /// Record evidence-bound reading state and an optional note for one symbol.
    Mark {
        query: String,
        #[arg(long, value_enum)]
        state: MarkState,
        #[arg(long, default_value = "", value_parser = parse_note)]
        note: String,
    },
}

pub fn run() -> i32 {
    // Clap exits while displaying help, so select the presentation locale before
    // constructing or parsing its command. The locale never enters cache keys.
    let mut arguments: Vec<_> = std::env::args_os().collect();
    let mut selected = std::env::var("CODEXIS_LOCALE").unwrap_or_else(|_| "zh-CN".into());
    let mut explicit = false;
    for (position, argument) in arguments.iter().enumerate().skip(1) {
        if argument == "--" {
            break;
        }
        if argument == "--locale" {
            if let Some(value) = arguments.get(position + 1).and_then(|s| s.to_str()) {
                selected = value.into();
                explicit = true;
            }
        } else if let Some(value) = argument.to_str().and_then(|s| s.strip_prefix("--locale=")) {
            selected = value.into();
            explicit = true;
        }
    }
    if let Err(error) = crate::i18n::set_locale(&selected) {
        eprintln!("error: {error}");
        return 2;
    }
    if !explicit {
        arguments.insert(1, "--locale".into());
        arguments.insert(2, crate::i18n::current().tag().into());
    }
    let matches = localized_command(Cli::command()).get_matches_from(arguments);
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
    debug_assert_eq!(cli.locale, crate::i18n::current().tag());
    if let Err(error) =
        ctrlc::set_handler(|| crate::source::CANCELLED.store(true, Ordering::Relaxed))
    {
        eprintln!(
            "{}",
            crate::localize!(
                "警告：无法安装取消处理程序：{}",
                "warning: cannot install cancellation handler: {}",
                error
            )
        );
    }
    match execute(&cli) {
        Ok((report, code)) => match if interactive(&cli) {
            open_browser(&cli, &report)
        } else {
            write_report(&cli, &report)
        } {
            Ok(()) => code,
            Err(error) => {
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe)
                {
                    return 0;
                }
                eprintln!("{}", crate::localize!("错误：{:#}", "error: {:#}", error));
                1
            }
        },
        Err(error) => {
            eprintln!("{}", crate::localize!("错误：{:#}", "error: {:#}", error));
            if error.is::<UsageError>() {
                2
            } else {
                1
            }
        }
    }
}

fn localized_command(mut command: clap::Command) -> clap::Command {
    let name = command.get_name().to_owned();
    let about = match name.as_str() {
        "codexis" => crate::localize!("基于源码证据理解大型项目、核查改动", "Understand large projects and review changes with source evidence"),
        "analyze" => crate::localize!("读取项目源码、更新索引并生成项目总览", "Capture source, update the index and generate a project overview"),
        "baseline" => crate::localize!("阅读项目业务认知基线；可调用本机 Codex 阅读源码生成解释", "Read the project's business baseline; optionally use local Codex to read source and explain it"),
        "understand" => crate::localize!("从八个维度探索已保存的项目快照", "Explore the eight dimensions of project understanding in a stored snapshot"),
        "flow" => crate::localize!("查看静态脉络图：入口、主干、错误出口、配置与共享状态", "Show the static flow map: entries, trunk, exits, configuration and shared state"),
        "map" => crate::localize!("查看包或模块的结构", "Query package or module structure in a stored snapshot"),
        "inspect" => crate::localize!("查找符号并核查已保存的源码证据", "Find a symbol and inspect stored source evidence"),
        "trace" => crate::localize!("沿调用关系探索有界路径与未知断点", "Explore a bounded neighborhood of call relationships"),
        "review" => crate::localize!("比较两个提交，或提交与工作区（含未跟踪源码）", "Compare two commits, or a commit with the current working tree (including untracked source)"),
        "remember" => crate::localize!("保存带证据的结论或疑问，并保留修订历史", "Save a human conclusion or question with evidence and revision history"),
        "knowledge" => crate::localize!("查看认知记录、证据变化或修订历史", "List saved conclusions, evidence changes, or the history of one conclusion"),
        "mark" => crate::localize!("保存符号的阅读状态和备注", "Record evidence-bound reading state and an optional note for one symbol"),
        _ => "",
    };
    command = command.about(about)
        .subcommand_help_heading(crate::localize!("命令", "Commands"))
        .disable_help_flag(true)
        .disable_help_subcommand(true)
        .help_template(crate::localize!(
            "{before-help}{name} {version}\n{about-with-newline}\n用法：{usage}\n\n{all-args}{after-help}",
            "{before-help}{name} {version}\n{about-with-newline}\nUsage: {usage}\n\n{all-args}{after-help}"
        ));
    let args: Vec<_> = command
        .get_arguments()
        .map(|arg| {
            (
                arg.get_id().to_string(),
                arg.get_long().is_some() || arg.get_short().is_some(),
                arg.get_default_values()
                    .iter()
                    .filter(|_| arg.get_action().takes_values())
                    .map(|v| v.to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
                arg.get_possible_values()
                    .iter()
                    .filter(|v| arg.get_action().takes_values() && !v.is_hide_set())
                    .map(|v| v.get_name().to_owned())
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    for (id, option, defaults, possible) in args {
        let help = match id.as_str() {
            "locale" => crate::localize!("界面与报告语言（与源码语言独立）；支持环境变量 CODEXIS_LOCALE", "Interface and report language, independent from source language; supports CODEXIS_LOCALE"),
            "project" | "path" => crate::localize!("待分析项目目录", "Project directory to analyze"),
            "cache_dir" => crate::localize!("独立的索引和认知记录缓存目录", "Directory for the index and knowledge records"),
            "format" => crate::localize!("报告输出格式", "Report output format"),
            "output" => crate::localize!("将报告写入指定文件", "Write the report to this file"),
            "overwrite" => crate::localize!("允许覆盖已有输出文件", "Allow replacing an existing output file"),
            "snapshot" => crate::localize!("查询指定的已保存快照", "Query this stored snapshot"),
            "plain" => crate::localize!("输出简洁报告", "Print a compact report"),
            "verbose" => crate::localize!("包含技术详情与分析进度", "Include technical details and analysis progress"),
            "language" => crate::localize!("源码语言；混合仓库需明确指定 Rust 或 Python", "Source language; explicitly choose Rust or Python for mixed repositories"),
            "analysis" => crate::localize!("语法分析或语义增强", "Syntax analysis or semantic enrichment"),
            "scope" => crate::localize!("按包、模块、文件或符号聚焦", "Focus by package, module, file or symbol"),
            "target" => crate::localize!("Rust 目标平台", "Rust target platform"),
            "features" => crate::localize!("Rust feature 列表，以逗号分隔", "Comma-separated Rust features"),
            "no_default_features" => crate::localize!("禁用 Rust 默认 feature", "Disable default Rust features"),
            "semantic_timeout_secs" => crate::localize!("语义分析时间预算（秒）", "Semantic analysis time budget in seconds"),
            "semantic_request_limit" => crate::localize!("语义分析查询数量预算", "Semantic analysis request budget"),
            "generate" => crate::localize!("调用本机 Codex 阅读快照源码并保存业务解释", "Use local Codex to read snapshot source and save a business explanation"),
            "model" => crate::localize!("生成解释使用的 Codex 模型", "Codex model used to generate the explanation"),
            "timeout_secs" => crate::localize!("生成解释的时间预算（秒）", "Explanation generation time budget in seconds"),
            "dimension" => crate::localize!("项目理解维度", "Dimension of project understanding"),
            "query" => crate::localize!("符号名、符号 ID 或文件路径", "Symbol name, symbol ID or file path"),
            "claim" => crate::localize!("开发者确认的结论或疑问（原文保存）", "Human conclusion or question, stored verbatim"),
            "state" if name == "remember" => crate::localize!("认知状态：已确认或疑问", "Knowledge state: confirmed or question"),
            "state" => crate::localize!("阅读状态：未读、已读或疑问", "Reading state: unread, seen or question"),
            "evidence" => crate::localize!("关联的源码或文档路径，可重复指定", "Source or documentation evidence path; may be repeated"),
            "id" if name == "remember" => crate::localize!("修订已有记录并保留历史", "Revise an existing record and retain its history"),
            "id" => crate::localize!("只查看指定认知记录", "Show this knowledge record"),
            "history" => crate::localize!("显示记录的修订历史", "Show the record revision history"),
            "level" => crate::localize!("按包或模块查看结构", "View structure by package or module"),
            "view" => crate::localize!("脉络图视图：首屏、错误码、配置项、共享状态或其他主干", "Flow view: home, error codes, configuration, shared state or other trunks"),
            "limit" => crate::localize!("单次查询的最大结果数量", "Maximum results for this query"),
            "cursor" => crate::localize!("继续查询的位置", "Position from which to continue the query"),
            "direction" => crate::localize!("沿调用者或被调用者展开", "Expand callers or callees"),
            "depth" => crate::localize!("最大调用展开深度", "Maximum call expansion depth"),
            "base" => crate::localize!("改动前的 Git 提交", "Git revision before the change"),
            "head" => crate::localize!("改动后的 Git 提交", "Git revision after the change"),
            "worktree" => crate::localize!("将当前工作区作为新版本", "Use the current working tree as the new version"),
            "note" => crate::localize!("阅读备注（原文保存）", "Reading note, stored verbatim"),
            _ => "",
        };
        let mut description = help.to_owned();
        if !crate::i18n::is_english() {
            if !defaults.is_empty() {
                description.push_str(&format!(" [默认：{}]", defaults.join(", ")));
            }
            if !possible.is_empty() {
                description.push_str(&format!(" [可选值：{}]", possible.join(", ")));
            }
        }
        command = command.mut_arg(&id, |arg| {
            let hide_value_details = !crate::i18n::is_english() && arg.get_action().takes_values();
            arg.help(description)
                .hide_default_value(hide_value_details)
                .hide_possible_values(hide_value_details)
                .help_heading(crate::localize!(
                    if option { "选项" } else { "参数" },
                    if option { "Options" } else { "Arguments" }
                ))
        });
    }
    command = command.arg(
        Arg::new("help")
            .short('h')
            .long("help")
            .action(ArgAction::Help)
            .help(crate::localize!("显示帮助", "Print help"))
            .help_heading(crate::localize!("选项", "Options")),
    );
    if name == "codexis" {
        command = command.disable_version_flag(true).arg(
            Arg::new("version")
                .short('V')
                .long("version")
                .action(ArgAction::Version)
                .help(crate::localize!("显示版本", "Print version"))
                .help_heading(crate::localize!("选项", "Options")),
        );
    }
    let subcommands: Vec<_> = command
        .get_subcommands()
        .map(|sub| sub.get_name().to_owned())
        .collect();
    for sub in subcommands {
        command = command.mut_subcommand(&sub, localized_command);
    }
    command
}

fn open_browser(cli: &Cli, report: &Report<Value>) -> Result<()> {
    let root = PathBuf::from(
        report.data["project_root"]
            .as_str()
            .context(crate::localize!("缺少项目根目录", "missing project root"))?,
    );
    let index = Index::open(&root, cli.cache_dir.as_deref())?;
    let snapshot = index.snapshot(Some(&report.snapshot_id))?;
    crate::tui::run(
        &index,
        snapshot,
        report.data["reading_guide"].clone(),
        cli.cache_dir.clone(),
    )
}

fn interactive(cli: &Cli) -> bool {
    matches!(cli.command, Command::Analyze { .. })
        && matches!(cli.format, Format::Text)
        && !cli.plain
        && !cli.verbose
        && cli.output.is_none()
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::env::var("TERM").is_ok_and(|term| term != "dumb")
}

#[derive(Debug)]
struct UsageError(&'static str);
impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for UsageError {}

fn parse_note(value: &str) -> std::result::Result<String, String> {
    if value.chars().count() > 4096 {
        Err(crate::localize!(
            "阅读备注不能超过 4096 个字符",
            "review note must not exceed 4096 characters"
        )
        .into())
    } else {
        Ok(value.into())
    }
}

fn bounded(value: &str, min: usize, max: usize) -> std::result::Result<usize, String> {
    value
        .parse::<usize>()
        .ok()
        .filter(|n| (min..=max).contains(n))
        .ok_or_else(|| {
            crate::localize!(
                "请输入 {} 到 {} 之间的数字",
                "expected a number between {} and {}",
                min,
                max
            )
        })
}

fn parse_limit(value: &str) -> std::result::Result<usize, String> {
    bounded(value, 1, 10_000)
}
fn parse_request_limit(value: &str) -> std::result::Result<usize, String> {
    bounded(value, 1, 1_000_000)
}
fn parse_depth(value: &str) -> std::result::Result<usize, String> {
    bounded(value, 0, 32)
}

fn report_code(report: &Report<Value>) -> i32 {
    if report.completeness.status == "partial" || report.completeness.stale {
        3
    } else {
        0
    }
}

fn execute(cli: &Cli) -> Result<(Report<Value>, i32)> {
    if cli.snapshot.is_some()
        && matches!(
            cli.command,
            Command::Analyze { .. } | Command::Review { .. }
        )
    {
        return Err(UsageError(crate::localize!("--snapshot 用于查询已有快照，不能与 analyze 或 review 同用", "--snapshot selects an existing query snapshot; do not combine it with analyze or review")).into());
    }
    let positional = match &cli.command {
        Command::Analyze { path, .. } => path.as_ref(),
        _ => None,
    };
    if let (Some(project), Some(path)) = (&cli.project, positional) {
        if project.canonicalize()? != path.canonicalize()? {
            return Err(UsageError(crate::localize!(
                "--project 与 analyze 路径指向不同目录",
                "--project and analyze path refer to different directories"
            ))
            .into());
        }
    }
    let root = cli
        .project
        .as_ref()
        .or(positional)
        .cloned()
        .unwrap_or(std::env::current_dir()?)
        .canonicalize()
        .context(crate::localize!(
            "无法打开项目目录",
            "open project directory"
        ))?;
    let mut index = Index::open(&root, cli.cache_dir.as_deref())?;
    if let Command::Analyze { options, .. } = &cli.command {
        if interactive(cli) {
            eprintln!("{}", crate::localize!("正在读取项目… 首次索引需要几秒，后续会复用缓存。", "Reading project… The first index takes a few seconds; subsequent runs reuse the cache."));
        }
        let source = WorkingTreeSource { root }.snapshot()?;
        let snapshot =
            analysis::analyze(&mut index, &source, options.context(&source)?, cli.verbose)?;
        let report = query::overview(&index, &snapshot)?;
        let code = report_code(&report);
        return Ok((report, code));
    }
    if let Command::Review {
        base,
        head,
        options,
        limit,
        cursor,
        ..
    } = &cli.command
    {
        let base_source = GitSource {
            root: root.clone(),
            revision: base.clone(),
        }
        .snapshot()?;
        let head_source = match head {
            Some(revision) => GitSource {
                root: root.clone(),
                revision: revision.clone(),
            }
            .snapshot()?,
            None => WorkingTreeSource { root: root.clone() }.snapshot()?,
        };
        let before = analysis::analyze(
            &mut index,
            &base_source,
            options.context(&base_source)?,
            cli.verbose,
        )?;
        let after = analysis::analyze(
            &mut index,
            &head_source,
            options.context(&head_source)?,
            cli.verbose,
        )?;
        let report = review::compare(&index, &before, &after, *limit, *cursor)?;
        let code = report_code(&report);
        return Ok((report, code));
    }
    let mut snapshot = index.snapshot(cli.snapshot.as_deref())?;
    query::check_freshness(&index, &mut snapshot)?;
    let report = match &cli.command {
        Command::Baseline {
            generate,
            model,
            timeout_secs,
        } => crate::interpretation::report(
            &index,
            &snapshot,
            *generate,
            model.as_deref(),
            *timeout_secs,
        )?,
        Command::Understand { dimension, scope } => {
            query::understand(&index, &snapshot, dimension.id(), scope.as_deref())?
        }
        Command::Remember {
            query,
            claim,
            dimension,
            state,
            evidence,
            id,
        } => Report::new(
            &snapshot,
            crate::knowledge::remember(
                &index,
                &snapshot,
                crate::knowledge::Remember {
                    query,
                    dimension: dimension.id(),
                    claim,
                    state,
                    paths: evidence,
                    record_id: id.as_deref(),
                },
            )?,
        ),
        Command::Knowledge { id, history } => Report::new(
            &snapshot,
            serde_json::json!({"kind":"knowledge","records":crate::knowledge::list(&index,&snapshot,id.as_deref(),*history)?}),
        ),
        Command::Flow { view } => Report::new(
            &snapshot,
            serde_json::json!({
                "kind": "flow",
                "view": view.id(),
                "header": crate::flow::view::Header::new(&snapshot),
                "flow": crate::flow::build(&index, &snapshot)?,
            }),
        ),
        Command::Map {
            level,
            scope,
            limit,
            cursor,
        } => query::map(
            &index,
            &snapshot,
            match level {
                Level::Package => "package",
                Level::Module => "module",
            },
            scope.as_deref(),
            *limit,
            *cursor,
        )?,
        Command::Inspect {
            query: pattern,
            limit,
        } => query::inspect(&index, &snapshot, pattern, *limit)?,
        Command::Trace {
            query: pattern,
            direction,
            depth,
            limit,
        } => query::trace(
            &index,
            &snapshot,
            pattern,
            matches!(direction, Direction::Callers),
            *depth,
            *limit,
        )?,
        Command::Mark {
            query: pattern,
            state,
            note,
        } => {
            let nodes = index.find_nodes(&snapshot.id, pattern, 2)?;
            if nodes.len() != 1 {
                bail!(
                    "{}",
                    crate::localize!(
                        "mark 需要唯一符号；用 inspect {:?} 选择符号 ID",
                        "mark requires exactly one symbol; use inspect {:?} to choose an ID",
                        pattern
                    )
                );
            }
            Report::new(
                &snapshot,
                marks::set(
                    &index,
                    &snapshot,
                    &nodes[0],
                    match state {
                        MarkState::Unread => "unread",
                        MarkState::Seen => "seen",
                        MarkState::Question => "question",
                    },
                    note,
                )?,
            )
        }
        Command::Analyze { .. } | Command::Review { .. } => unreachable!(),
    };
    let code = report_code(&report);
    Ok((report, code))
}

fn write_report(cli: &Cli, report: &Report<Value>) -> Result<()> {
    let output = render::render_with_detail(
        report,
        match cli.format {
            Format::Text => "text",
            Format::Markdown => "markdown",
            Format::Json => "json",
        },
        cli.verbose,
    )?;
    if let Some(path) = &cli.output {
        let mut options = std::fs::OpenOptions::new();
        options.write(true);
        if cli.overwrite {
            options.create(true).truncate(true);
        } else {
            options.create_new(true);
        }
        let mut file = options.open(path).with_context(|| {
            crate::localize!(
                "无法写入 {}；用 --overwrite 覆盖已有文件",
                "write {}; use --overwrite to replace an existing file",
                path.display()
            )
        })?;
        file.write_all(output.as_bytes())?;
        eprintln!(
            "{}",
            crate::localize!("已保存 {}", "Saved {}", path.display())
        );
    } else {
        std::io::stdout().lock().write_all(output.as_bytes())?;
    }
    Ok(())
}
