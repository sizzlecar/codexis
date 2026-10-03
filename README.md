# Codexis

Evidence-backed understanding and review of large Rust and Python projects, from the terminal.

面向 AI 持续生成大量代码的场景，帮助开发者建立项目认知、理解改动、沿证据核查并更新已有认识。支持中文和英文界面，Java 已退出支持范围。

## Run / 运行

```sh
cargo build --release
target/release/codexis analyze /path/to/project
target/release/codexis --locale en analyze /path/to/project
```

普通终端进入工作台。主页围绕认知基线、改动批次、证据核查和认知更新组织操作。同一对象可在八个维度切换：能力与意图、架构与边界、行为与控制、数据与状态、配置与运行、变更与演进、验证与约束、认知与阅读。

Use the workbench to explore intent, architecture, behavior, data, runtime configuration, changes, verification, and knowledge. Select an object and keep it in context while switching views.

`↑↓` / `j k` 选择，Enter 进入，`b` 返回，`g` 回主页，`/` 搜索，`o` 看固定快照源码。`d` 打开维度，`1–8` 切换，`x` 清除筛选，`p` 输入当前问题，`c/v` 保存结论或疑问，`h` 查看认知历史，`q` 退出，`?` 查看帮助。宽终端左右布局，窄终端上下布局。

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

Findings use stored source, declarations, and indexed relationships. Static calls are not runtime order, types are not complete dataflow, and related tests do not prove coverage or successful execution. Missing responsibilities remain unknown. Model-generated explanations are not connected yet.

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
