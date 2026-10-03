# Codexis

Evidence-backed understanding and review of large Rust and Python projects, from the terminal.

面向 AI 持续生成大量代码的场景，帮助开发者建立项目认知、理解改动、沿证据核查并更新已有认识。支持中文和英文界面，Java 已退出支持范围。

## Run / 运行

```sh
cargo build --release
target/release/codexis analyze /path/to/project
target/release/codexis --locale en analyze /path/to/project
```

普通终端进入工作台。首页只保留“理解项目”和“查看改动”，展示一句项目说明。进入“理解项目”后可沿业务步骤阅读源码；“深入分析”提供入口与调用、八维视角、构建包与文件、阅读记录和分析范围。

The home screen offers **Understand the project** and **Review changes**, with a short project description. Follow business steps into source, or choose **Explore further** for entries, eight perspectives, packages and files, reading notes, and analysis scope. Select an object and keep it in context while switching views.

认知基线以一个具体业务场景解释项目目的、协作步骤、关键状态、失败边界和首读理由。首次进入时，选择“用 Codex 生成项目解释”；只有结构索引时会明确提示基线尚未形成。每一步都可打开固定快照中的源码证据。

The baseline explains one concrete scenario: purpose, collaborating steps, state ownership, failure boundaries, and where to read first. Choose **Generate a project explanation with Codex** in the workbench, or run:

```sh
codexis --project /path/to/project baseline --generate --timeout-secs 300
codexis --project /path/to/project baseline
codexis --locale en --project /path/to/project baseline --generate
```

Explanation generation uses an installed, authenticated `codex` CLI and its model service, explicitly on request. `--model` selects a model; `CODEXIS_CODEX_BIN` selects the executable. Codex reads an isolated copy of admitted source, README, and manifests from the stored snapshot. Runtime data, credentials configuration, planning documents, and project instructions are excluded. Explanations retain source/declaration/interpretation labels and line citations, remain separate from human-confirmed knowledge, and are cached by snapshot, locale, provider and context. Generation can take a few minutes; cancel or timeout preserves the previous valid result.

首页用 `↑↓` 选择、Enter 打开、`q` 退出；`?` 查看当前页面的帮助。深入页面保留 `b` 返回、`g` 回主页、`/` 搜索、`o` 看源码、`d` / `1–8` 切换视角、`c/v` 保存结论或疑问等操作，底部只提示当前页面常用按键。深入页面在宽终端左右布局，窄终端上下布局。

`--locale zh-CN|en` controls system text, with Chinese as the default. `CODEXIS_LOCALE` provides an environment default. Source, documentation excerpts, and user notes retain their original language. Pipes, redirects, `--plain`, and exports use noninteractive reports.

## Query and review / 查询与审阅

Install with `cargo install --path .` to use `codexis` directly, or replace it below with `target/release/codexis`.

```sh
codexis --project /path/to/project understand
codexis --project /path/to/project understand --dimension architecture --scope engine
codexis --project /path/to/project inspect SomeSymbol
codexis --project /path/to/project trace SomeSymbol --direction callees --depth 3
codexis --project /path/to/project review --base HEAD --worktree
codexis --project /path/to/project review --base HEAD~1 --head HEAD --analysis semantic
codexis --project /path/to/project remember SomeSymbol --dimension architecture --claim 'This interface separates the subsystems'
codexis --project /path/to/project knowledge
codexis --project /path/to/project knowledge RECORD_ID --history
```

`review` groups a change batch and reports interface changes, dependency boundaries, review questions, artifact changes, and outdated knowledge. Saved conclusions depend on recorded evidence. Use `remember ... --id RECORD_ID` to append a revision and preserve history. Existing `map` and `mark` commands remain available.

`review` 同时提供批次分组、接口与关系变化、重点核查项、文档配置变化及需更新的认知。认知记录绑定证据，修改后显示需复核；更新记录保留历史版本。

```sh
codexis analyze /path/to/project --language python --analysis semantic
codexis analyze /path/to/project --format markdown --output project-understanding.md
codexis --project /path/to/project understand --format json
```

Existing output files require `--overwrite`. JSON includes snapshot, context, locale, completeness, and evidence. Markdown organizes findings by dimension. Use a symbol ID when names are ambiguous.

## Scope / 支持范围

Rust supports Cargo workspaces, packages, and loose source. Structural analysis does not build the target. Semantic enrichment requires local rust-analyzer and rust-src; build scripts, procedural macros, automatic checks, downloads, and toolchain installation are disabled.

Python supports pyproject, static configuration, requirements, src layouts, namespaces, and loose source. It extracts classes, functions, fields, annotations, imports, calls, documentation, and entries. Semantic mode conservatively resolves unique direct functions and import aliases; dynamic dispatch, shadowing, and conditional bindings remain breakpoints. It never executes Python, setup.py, target imports, or builds.

文档、配置、SQL、proto、部署与 CI 文件作为固定证据采集。扫描遵循忽略规则，不跟随符号链接，跳过构建与依赖目录。缓存写用户缓存目录或 `--cache-dir`，待分析项目保持只读。混合根目录需指定 `--language rust|python`，暂不构造跨语言调用图。

Findings use stored source, declarations, and indexed relationships. Static calls are not runtime order, types are not complete dataflow, and related tests do not prove coverage or successful execution. Missing responsibilities remain unknown until supported by evidence; model explanations remain interpretations for review.

Dimension lists show up to 100 items and expose totals and truncation. Use `--scope` to focus on a component, path, or symbol before this limit is applied. Cold analysis materializes the full graph and can use substantial memory and disk space on million-line projects; cached queries reuse the stored snapshot.

Project architecture rules can be declared in `codexis.toml`:

```toml
[architecture]
forbidden = [
  { from = "src/domain/**", to = "src/ui/**", reason = "Domain must not depend on UI" }
]
```

Rules only check indexed relationships with known targets. Unknown calls are not guessed from names. Evidence changes mark knowledge as needing review; confirmation does not automatically prove correctness.

Exit codes: `0` completed within declared scope; `1` execution failure; `2` invalid arguments; `3` partial analysis or stale worktree snapshot. Truncation and unknown relationships are reported separately.

## Development / 开发检查

```sh
cargo fmt --all -- --check
cargo check --all-targets
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
# Optional real rust-analyzer integration
cargo test --test semantic -- --ignored
```

Implementation and new test tooling use Rust. End-to-end checks cover Rust/Python CLI flows, change batches and knowledge updates, bilingual reports, and real terminal interaction. macOS terminal tests use the system PTY utility and clean up their child processes.

The Rust Ferrum harness checks cold/warm analysis, all eight dimensions, scoped understanding, inspect/trace and a history review. Its output directory must be new and outside the analyzed checkout.

```sh
cargo build --release --bin codexis --examples
cargo run --release --example validate_ferrum -- /path/to/ferrum /tmp/new-ferrum-reports
# Optional bounded rust-analyzer profiles
cargo run --release --example validate_ferrum -- /path/to/ferrum /tmp/new-semantic-reports --semantic
# Previous semantic assertions and macOS timing reports
cargo run --release --example validate_ferrum -- /path/to/ferrum /tmp/new-legacy-reports --legacy
```
