# Codexis

Evidence-backed understanding and review of large Rust and Python projects, from the terminal.

面向 AI 持续生成大量代码的场景，帮助开发者建立项目认知、理解改动、沿证据核查并更新已有认识。支持中文和英文界面，Java 已退出支持范围。

## Run / 运行

```sh
cargo build --release
target/release/codexis analyze /path/to/project
target/release/codexis --locale en analyze /path/to/project
```

普通终端进入工作台，首屏是一张静态脉络图：入口（路由注册）、主干（从处理入口最多的函数出发，按源码顺序展开调用）、每一步可能的提前结束（状态码与错误码）、读取的配置、跨请求共享状态和外部系统。全部来自源码，不调用模型。调用目标只按声明类型确定（参数、字段、带类型或构造出来的局部变量、返回类型、`use`/`import`），确定不了的标为未确定；trait 或接口分派列出实现数量；调用顺序是源码顺序，不是一次真实运行的顺序。

The terminal opens on a static flow map: entries (route registrations), the trunk (calls expanded in source order from the handler serving the most entries), early exits with status and code, configuration reads, shared state and external systems. Everything comes from source; no model is called. Call targets are resolved only through declared types (parameters, fields, typed or constructed locals, return types, `use`/`import`); anything else is marked unknown, trait or interface dispatch lists its implementations, and order is source order rather than a recorded run.

```text
shop-api  4f1c2d9  Rust · 3 packages · 120 files · 18420 lines              Static analysis · no model
Entry POST /orders  /payments
Trunk OrderHandler::handle · 2 entries                                     Early exits
   1 Admission::admit  [cfg] limits                   handler.rs:41       → 429 too_busy
      ⟳ for attempt in 0..policy.max_attempts         service.rs:88
      │  4 BackendPool::select                        service.rs:93       → 503 no_backend
      │  6 Ledger::reserve                            service.rs:120      → 503 ledger_full
```

首屏按键：`↑↓` 选行，Enter 打开固定快照源码；`e` 全部错误出口，`c` 配置项及读取位置和读取方式（读取时取当前值 / 使用调用方传入的值 / 使用持有的值），`s` 共享状态及读写位置，`r` 全部入口与其他主干，`v` 查看改动，`d` 深入分析（入口与调用、八维视角、项目解释、构建包与文件、阅读记录和分析范围），`/` 搜索，`?` 帮助。

Keys: `↑↓` select a line and Enter opens pinned source; `e` every error exit, `c` configuration with read sites and modes, `s` shared state with access sites, `r` every entry and other trunks, `v` review changes, `d` explore further (calls, eight perspectives, project explanation, packages, reading notes, scope), `/` search, `?` help.

```sh
codexis --project /path/to/project flow
codexis --project /path/to/project flow --view errors|config|state|trunks
```

Rust 识别路由注册调用（含 `for (path, ..) in [..]` 循环注册和 `#[get("/x")]` 属性）、带状态码字面量或 `StatusCode::X` 的错误构造，以及在 `match` 中映射到状态码的错误枚举变体；配置根是源码中由 YAML/TOML/环境变量加载的 `Deserialize` 类型。Python 识别 FastAPI/Flask 风格装饰器（含 `APIRouter(prefix=…)`）、`HTTPException`/`abort` 等出口和 pydantic `BaseSettings` 配置。脉络图按快照缓存。

Rust recognizes route registration calls (including loop-registered paths and `#[get("/x")]` attributes), error constructors carrying a status literal or `StatusCode::X`, and error enum variants mapped to statuses in a `match`; configuration roots are `Deserialize` types loaded from YAML/TOML/environment in source. Python recognizes FastAPI/Flask-style decorators (including `APIRouter(prefix=…)`), `HTTPException`/`abort` exits and pydantic `BaseSettings`. The map is cached per snapshot.

认知基线以一个具体业务场景解释项目目的、协作步骤、关键状态、失败边界和首读理由，入口在“深入分析 → 项目解释”（`d`）。首次进入时，选择“用 Codex 生成项目解释”；只有结构索引时会明确提示基线尚未形成。每一步都可打开固定快照中的源码证据。

The baseline explains one concrete scenario: purpose, collaborating steps, state ownership, failure boundaries, and where to read first. Open it from **Explore further → Project explanation** (`d`) and choose **Generate a project explanation with Codex**, or run:

```sh
codexis --project /path/to/project baseline --generate --timeout-secs 300
codexis --project /path/to/project baseline
codexis --locale en --project /path/to/project baseline --generate
```

Explanation generation uses an installed, authenticated `codex` CLI and its model service, explicitly on request. `--model` selects a model; `CODEXIS_CODEX_BIN` selects the executable. Codex reads an isolated copy of admitted source, README, and manifests from the stored snapshot. Runtime data, credentials configuration, planning documents, and project instructions are excluded. Explanations retain source/declaration/interpretation labels and line citations, remain separate from human-confirmed knowledge, and are cached by snapshot, locale, provider and context. Generation can take a few minutes; cancel or timeout preserves the previous valid result.

深入页面保留 `b` 返回、`g` 回主页、`/` 搜索、`o` 看源码、`d` / `1–8` 切换视角、`c/v` 保存结论或疑问等操作，底部只提示当前页面常用按键。深入页面在宽终端左右布局，窄终端上下布局。

`--locale zh-CN|en` controls system text, with Chinese as the default. `CODEXIS_LOCALE` provides an environment default. Source, documentation excerpts, and user notes retain their original language. Pipes, redirects, `--plain`, and exports use noninteractive reports; the plain `analyze` report prints the flow map, and `--verbose` prints the detailed overview.

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
