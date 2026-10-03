//! A terminal workbench over immutable evidence and local reading/knowledge records.
mod browser;
mod jobs;
#[cfg(test)]
mod tests;

use crate::{index::Index, model::Snapshot, source::CANCELLED};
use anyhow::Result;
use browser::{Action, Browser};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{self, Color},
    terminal::{self, ClearType},
};
use jobs::Job;
use serde_json::Value;
use std::{
    io::{self, Write},
    path::PathBuf,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};
use unicode_width::UnicodeWidthChar;

struct Terminal;
impl Terminal {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = Self;
        execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
        Ok(guard)
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            style::ResetColor,
            cursor::Show,
            terminal::LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

enum Input {
    Search(String),
    Question(String),
    Conclusion {
        state: String,
        text: String,
    },
    Note {
        snapshot: String,
        id: String,
        state: String,
        text: String,
    },
}
impl Input {
    fn text_mut(&mut self) -> &mut String {
        match self {
            Self::Search(text)
            | Self::Question(text)
            | Self::Note { text, .. }
            | Self::Conclusion { text, .. } => text,
        }
    }
    fn label(&self) -> String {
        match self {
            Self::Search(text) => crate::localize!(
                "搜索名称/路径：{text}▏  [Enter 搜索 · Esc 取消]",
                "Search name/path: {text}▏  [Enter search · Esc cancel]",
                text = text
            ),
            Self::Note { text, .. } => crate::localize!(
                "笔记：{text}▏  [Enter 保存 · Esc 取消]",
                "Note: {text}▏  [Enter save · Esc cancel]",
                text = text
            ),
            Self::Question(text) => crate::localize!(
                "当前问题：{text}▏  [Enter 保存上下文 · Esc 取消]",
                "Current question: {text}▏  [Enter keep context · Esc cancel]",
                text = text
            ),
            Self::Conclusion { state, text } => crate::localize!(
                "{}：{text}▏  [Enter 保存认知 · Esc 取消]",
                "{}: {text}▏  [Enter save knowledge · Esc cancel]",
                if state == "question" {
                    crate::localize!("疑问", "Question")
                } else {
                    crate::localize!("结论", "Conclusion")
                }
            ),
        }
    }
}

pub fn run(index: &Index, snapshot: Snapshot, guide: Value, cache: Option<PathBuf>) -> Result<()> {
    let mut browser = Browser::new(index, snapshot, guide)?;
    let terminal = Terminal::enter()?;
    let mut job: Option<Job> = None;
    let mut started = Instant::now();
    let mut job_label = String::new();
    let mut input: Option<Input> = None;
    let result = (|| -> Result<()> {
        let mut dirty = true;
        let mut elapsed_second = 0;
        loop {
            if CANCELLED.load(Ordering::Relaxed) {
                break;
            }
            if let Some(result) = job.as_mut().and_then(Job::poll) {
                job.take();
                match result {
                    Ok(result) => {
                        if let Err(error) = browser.completed(result) {
                            browser.status = crate::localize!(
                                "无法展示结果：{error:#}",
                                "Cannot display result: {error:#}",
                                error = error
                            );
                        }
                    }
                    Err(error) => {
                        browser.status = crate::localize!(
                            "分析未完成：{error}。原有结果仍可浏览。",
                            "Analysis incomplete: {error}. Existing results remain available.",
                            error = error
                        )
                    }
                }
                dirty = true;
            }
            if job.is_some() && elapsed_second != started.elapsed().as_secs() {
                elapsed_second = started.elapsed().as_secs();
                dirty = true;
            }
            if dirty {
                let progress = job.as_ref().map(|_| {
                    crate::localize!(
                        "{job_label} · {}s · 可继续浏览，q 退出并取消",
                        "{job_label} · {}s · Browsing remains available; q exits and cancels",
                        started.elapsed().as_secs()
                    )
                });
                draw(
                    &browser,
                    input.as_ref().map(Input::label).as_deref(),
                    progress.as_deref(),
                )?;
                dirty = false;
            }
            if !event::poll(Duration::from_millis(150))? {
                continue;
            }
            match event::read()? {
                Event::Resize(_, _) => {
                    dirty = true;
                    continue;
                }
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    dirty = true;
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        break;
                    }
                    if let Some(prompt) = &mut input {
                        match key.code {
                            KeyCode::Esc => input = None,
                            KeyCode::Enter => {
                                match input.take().unwrap() {
                                    Input::Search(text) if !text.trim().is_empty() => {
                                        if let Err(e) = browser.open(Action::Search {
                                            query: text.trim().into(),
                                            offset: 0,
                                        }) {
                                            browser.status = crate::localize!(
                                                "搜索失败：{e:#}",
                                                "Search failed: {e:#}",
                                                e = e
                                            );
                                        }
                                    }
                                    Input::Note {
                                        snapshot,
                                        id,
                                        state,
                                        text,
                                    } => {
                                        if let Err(e) =
                                            browser.mark(&snapshot, &id, &state, Some(&text))
                                        {
                                            browser.status = crate::localize!(
                                                "保存失败：{e:#}",
                                                "Save failed: {e:#}",
                                                e = e
                                            );
                                        }
                                        browser.refresh()?;
                                    }
                                    Input::Question(text) => {
                                        browser.question = text.trim().into();
                                        browser.status = crate::localize!("当前问题已保留；跨视角继续核查。", "Question retained; continue checking across perspectives.").into();
                                    }
                                    Input::Conclusion { state, text } => {
                                        if let Err(error) = browser.remember(text.trim(), &state) {
                                            browser.status = crate::localize!(
                                                "认知未保存：{error:#}",
                                                "Knowledge not saved: {error:#}",
                                                error = error
                                            );
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            KeyCode::Backspace => {
                                prompt.text_mut().pop();
                            }
                            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                prompt.text_mut().clear()
                            }
                            KeyCode::Char(c)
                                if !key
                                    .modifiers
                                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                                    && !c.is_control() =>
                            {
                                if prompt.text_mut().chars().count() < 4096 {
                                    prompt.text_mut().push(c);
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }
                    if key.code == KeyCode::Char('q') {
                        break;
                    }
                    if key.code == KeyCode::Char('/') {
                        input = Some(Input::Search(String::new()));
                        continue;
                    }
                    if key.code == KeyCode::Char('p') {
                        input = Some(Input::Question(browser.question.clone()));
                        continue;
                    }
                    if matches!(key.code, KeyCode::Char('c' | 'v')) {
                        let text = browser
                            .selected_record()
                            .and_then(|v| v["claim"].as_str().map(str::to_owned))
                            .unwrap_or_default();
                        input = Some(Input::Conclusion {
                            state: if key.code == KeyCode::Char('v') {
                                "question"
                            } else {
                                "confirmed"
                            }
                            .into(),
                            text,
                        });
                        continue;
                    }
                    if key.code == KeyCode::Char('n') {
                        if let Some((snapshot, id)) = browser.selected_node() {
                            let node = index.find_nodes(&snapshot, &id, 1)?.pop().unwrap();
                            let mark =
                                crate::marks::get(index, &index.snapshot(Some(&snapshot))?, &node)?;
                            let state = match mark["state"].as_str().unwrap_or("unread") {
                                "needs_review" => "question",
                                other => other,
                            }
                            .into();
                            input = Some(Input::Note {
                                snapshot,
                                id,
                                state,
                                text: mark["note"].as_str().unwrap_or("").into(),
                            });
                        } else {
                            browser.status = crate::localize!(
                                "先选中或进入一个定义，再按 n 编辑笔记。",
                                "Select or open a definition, then n to edit its note."
                            )
                            .into();
                        }
                        continue;
                    }
                    match handle_key(&mut browser, key) {
                        Ok(Some(request)) if job.is_none() => {
                            job_label = match &request {
                                jobs::Request::Semantic(p) => crate::localize!("正在解析 {p} 的调用", "Resolving calls in {p}", p = p),
                                _ => crate::localize!("正在比较变更", "Comparing changes").into(),
                            };
                            started = Instant::now();
                            job = Some(Job::start(
                                request,
                                browser.snapshot.clone(),
                                cache.clone(),
                                index.file_hashes(&browser.snapshot.id)?,
                            ));
                            browser.status.clear();
                        }
                        Ok(Some(_)) => {
                            browser.status = crate::localize!("已有分析在运行；完成后可启动下一项。", "An analysis is already running; wait for it to finish before starting another.").into()
                        }
                        Ok(None) => {}
                        Err(error) => browser.status = crate::localize!("无法打开：{error:#}", "Cannot open: {error:#}", error = error),
                    }
                }
                _ => {}
            }
        }
        Ok(())
    })();
    if job.is_some() {
        CANCELLED.store(true, Ordering::Relaxed);
    }
    drop(terminal);
    if job.is_some() {
        eprintln!(
            "{}",
            crate::localize!(
                "正在停止后台分析并清理语言服务…",
                "Stopping background analysis and cleaning up language services…"
            )
        );
    }
    drop(job);
    result
}

fn handle_key(browser: &mut Browser<'_>, key: KeyEvent) -> Result<Option<jobs::Request>> {
    let page = terminal::size()
        .map(|(_, h)| usize::from(h).saturating_sub(9).max(1))
        .unwrap_or(15);
    match key.code {
        KeyCode::Up | KeyCode::Char('k') if browser.page.items.is_empty() => {
            browser.page.scroll = browser.page.scroll.saturating_sub(1)
        }
        KeyCode::Down | KeyCode::Char('j') if browser.page.items.is_empty() => {
            browser.page.scroll =
                (browser.page.scroll + 1).min(browser.detail.len().saturating_sub(1))
        }
        KeyCode::Up | KeyCode::Char('k') => browser.select(-1)?,
        KeyCode::Down | KeyCode::Char('j') => browser.select(1)?,
        KeyCode::Enter => return browser.enter(),
        KeyCode::Char(c @ '1'..='8') => {
            let dimension = crate::understanding::DIMENSION_IDS[(c as u8 - b'1') as usize];
            browser.open(Action::Dimension(dimension.into()))?;
        }
        KeyCode::Char('b') | KeyCode::Esc => browser.back()?,
        KeyCode::Char('g') => browser.home()?,
        KeyCode::Char('d') => {
            browser.open(Action::Dimensions)?;
        }
        KeyCode::Char('x') => browser.clear_focus()?,
        KeyCode::Char('h') => {
            if let Some(record) = browser.selected_record() {
                browser.open(Action::Knowledge {
                    history: record["id"].as_str().map(str::to_owned),
                })?;
            } else {
                browser.status = crate::localize!(
                    "选中认知记录后按 h 查看历史。",
                    "Select a knowledge record, then h to view its history."
                )
                .into();
            }
        }
        KeyCode::PageDown | KeyCode::Char(' ') => {
            browser.page.scroll =
                (browser.page.scroll + page).min(browser.detail.len().saturating_sub(1))
        }
        KeyCode::PageUp => browser.page.scroll = browser.page.scroll.saturating_sub(page),
        KeyCode::Home => browser.page.scroll = 0,
        KeyCode::End => browser.page.scroll = browser.detail.len().saturating_sub(page),
        KeyCode::Left => browser.page.horizontal = browser.page.horizontal.saturating_sub(8),
        KeyCode::Right => {
            browser.page.horizontal = (browser.page.horizontal + 8).min(
                browser
                    .detail
                    .iter()
                    .map(|s| s.chars().count() * 2)
                    .max()
                    .unwrap_or(0),
            )
        }
        KeyCode::Char('o') => {
            if let Some(evidence) = browser.selected_evidence() {
                browser.open(Action::Source(evidence))?;
            } else {
                browser.status = crate::localize!(
                    "此项没有源码位置；选择具体定义或关系后按 o。",
                    "This item has no source position; select a definition or relation, then o."
                )
                .into();
            }
        }
        KeyCode::Char('m') => {
            if let Some((snapshot, id)) = browser.selected_node() {
                browser.open(Action::Marks { snapshot, id })?;
            } else {
                browser.status = crate::localize!(
                    "先选中或进入一个定义，再按 m 标记。",
                    "Select or open a definition, then m to mark it."
                )
                .into();
            }
        }
        KeyCode::Char('?') => {
            browser.open(Action::Text {
                title: crate::localize!("操作说明", "Keyboard help").into(),
                lines: vec![
                    crate::localize!("↑↓ / j k  选择条目；源码页逐行滚动", "↑↓ / j k  Select items; scroll source pages line by line").into(),
                    crate::localize!("Enter      展开所选条目", "Enter      Open the selected item").into(),
                    crate::localize!("b / Esc    返回上一层；g 回到项目概览", "b / Esc    Go back; g returns to the workbench").into(),
                    crate::localize!("d / 1–8    切换八维视角，保留当前对象与问题", "d / 1–8    Switch perspectives, keeping the object and question").into(),
                    crate::localize!("x / p      清除对象筛选 / 输入当前问题", "x / p      Clear object filter / enter the current question").into(),
                    crate::localize!("c / v      保存认知结论 / 保存疑问；证据与历史保留", "c / v      Save a conclusion / question; preserve evidence and history").into(),
                    crate::localize!("h          查看当前认知记录历史", "h          View current knowledge record history").into(),
                    crate::localize!("/          搜索名称或路径；Enter 提交，Esc 取消", "/          Search names or paths; Enter submits, Esc cancels").into(),
                    crate::localize!("o          查看所选条目的固定快照源码", "o          View pinned source for the selected item").into(),
                    crate::localize!("PgUp/PgDn  滚动右侧详情；←→ 横向滚动长行", "PgUp/PgDn  Scroll details; ←→ scroll long lines horizontally").into(),
                    crate::localize!("Home/End   跳到详情开头/结尾", "Home/End   Jump to the beginning/end of details").into(),
                    crate::localize!("m          标记已读 / 有疑问 / 未读；n 编辑笔记", "m          Mark Read / Question / Unread; n edits notes").into(),
                    crate::localize!("q / Ctrl-C 退出；取消后台分析并清理语言服务", "q / Ctrl-C Exit; cancel background analysis and clean up language services").into(),
                    String::new(),
                    crate::localize!("源码与分析结果来自固定快照；重新打开项目会更新结构。", "Source and analysis use a pinned snapshot; reopening updates the structure.").into(),
                    crate::localize!("未解析 / 接口关系都不是确定的完整运行时路径。", "Unresolved and interface relations do not establish complete runtime paths.").into(),
                    crate::localize!("不执行待分析项目的构建脚本；不上传代码。", "Project build scripts are not executed; source code is not uploaded.").into(),
                ],
            })?;
        }
        _ => {}
    }
    Ok(None)
}

fn draw(browser: &Browser<'_>, input: Option<&str>, progress: Option<&str>) -> Result<()> {
    let (width, height) = terminal::size()?;
    let mut out = io::stdout().lock();
    queue!(out, cursor::MoveTo(0, 0), terminal::Clear(ClearType::All))?;
    if width < 60 || height < 16 {
        line(
            &mut out,
            0,
            0,
            width,
            crate::localize!(
                "请将终端放大到至少 60 列 × 16 行；q 退出",
                "Resize the terminal to at least 60 columns × 16 rows; q exits"
            ),
            Color::Yellow,
            false,
            0,
        )?;
        out.flush()?;
        return Ok(());
    }
    let name = std::path::Path::new(&browser.snapshot.project_root)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    line(
        &mut out,
        1,
        0,
        width - 2,
        &crate::localize!(
            "CODEXIS  /  {name}                                      项目认知工作台",
            "CODEXIS  /  {name}                                      Knowledge workbench",
            name = name
        ),
        Color::Cyan,
        false,
        0,
    )?;
    let warning = if browser.snapshot.completeness.stale {
        crate::localize!("源码已过期", "Source is outdated")
    } else if browser.snapshot.completeness.status != "complete" {
        crate::localize!(
            "分析不完整 · 详情见概览中的状态页",
            "Partial analysis · inspect the status page for details"
        )
    } else {
        crate::localize!(
            "结构已索引 · 调用按模块解析",
            "Structure indexed · resolve calls per module"
        )
    };
    line(
        &mut out,
        1,
        1,
        width - 2,
        &crate::localize!(
            "{} 个构建包  ·  {} 个证据文件  ·  {} 行  ·  {warning}",
            "{} build packages  ·  {} evidence files  ·  {} lines  ·  {warning}",
            browser.snapshot.project.packages.len(),
            browser.snapshot.stats.source_files,
            browser.snapshot.stats.source_lines
        ),
        Color::Grey,
        false,
        0,
    )?;
    line(
        &mut out,
        1,
        2,
        width - 2,
        &crate::localize!(
            "对象：{}  ·  问题：{}",
            "Object: {}  ·  Question: {}",
            browser
                .focus
                .as_ref()
                .map(|f| f.title.as_str())
                .unwrap_or(crate::localize!("项目全景", "Full project")),
            if browser.question.is_empty() {
                crate::localize!("p 输入当前问题", "p to enter a question")
            } else {
                &browser.question
            }
        ),
        if browser.snapshot.completeness.status == "complete" {
            Color::DarkGrey
        } else {
            Color::Yellow
        },
        false,
        0,
    )?;
    line(
        &mut out,
        1,
        3,
        width - 2,
        &visible_breadcrumb(&browser.breadcrumb(), usize::from(width - 2)),
        Color::White,
        false,
        0,
    )?;
    let body_height = usize::from(height - 9);
    let full = browser.page.items.is_empty();
    let stacked = width < 100 && !full;
    let left = if full || stacked {
        0
    } else {
        (width * 2 / 5).min(52)
    };
    let list_height = if stacked {
        (body_height / 3).max(5).min(body_height.saturating_sub(3))
    } else {
        body_height
    };
    if !full {
        let row_height = if stacked { 1 } else { 2 };
        let visible = (list_height / row_height).max(1);
        let offset = browser.page.selected / visible * visible;
        for (row, item) in browser
            .page
            .items
            .iter()
            .skip(offset)
            .take(visible)
            .enumerate()
        {
            let selected = offset + row == browser.page.selected;
            let available = if stacked { width - 2 } else { left - 2 };
            line(
                &mut out,
                1,
                5 + (row * row_height) as u16,
                available,
                &format!("{} {}", if selected { "›" } else { " " }, item.label),
                if selected { Color::Cyan } else { Color::White },
                selected,
                0,
            )?;
            if !stacked {
                line(
                    &mut out,
                    1,
                    6 + row as u16 * 2,
                    available,
                    &format!("  {}", item.hint),
                    Color::DarkGrey,
                    false,
                    0,
                )?;
            }
        }
    }
    let detail_top = if stacked { 5 + list_height as u16 } else { 5 };
    let detail_height = if stacked {
        body_height - list_height
    } else {
        body_height
    };
    let x = if left == 0 { 1 } else { left + 1 };
    for row in 0..detail_height {
        if left > 0 {
            line(
                &mut out,
                left - 1,
                5 + row as u16,
                1,
                "│",
                Color::DarkGrey,
                false,
                0,
            )?;
        }
        if let Some(text) = browser.detail.get(browser.page.scroll + row) {
            let color = if text.starts_with('+') {
                Color::Green
            } else if text.starts_with('-') {
                Color::Red
            } else if text.starts_with('▶') {
                Color::Cyan
            } else {
                Color::White
            };
            line(
                &mut out,
                x,
                detail_top + row as u16,
                width - x - 1,
                text,
                color,
                false,
                browser.page.horizontal,
            )?;
        }
    }
    let position = if full {
        String::new()
    } else {
        crate::localize!(
            "条目 {}/{}  ·  ",
            "Item {}/{}  ·  ",
            browser.page.selected + 1,
            browser.page.items.len()
        )
    };
    line(
        &mut out,
        1,
        height - 4,
        width - 2,
        &crate::localize!(
            "{position}详情 {}/{} 行 · PgDn 翻页 / ←→ 长行",
            "{position}Details {}/{} lines · PgDn scrolls / ←→ long lines",
            browser.page.scroll + 1,
            browser.detail.len().max(1)
        ),
        Color::DarkGrey,
        false,
        0,
    )?;
    let status = input.or(progress).unwrap_or(&browser.status);
    line(
        &mut out,
        1,
        height - 3,
        width - 2,
        status,
        if input.is_some() {
            Color::Cyan
        } else {
            Color::Yellow
        },
        false,
        0,
    )?;
    line(
        &mut out,
        1,
        height - 2,
        width - 2,
        if width < 100 {
            crate::localize!(
                "q 退出  ↑↓ 选择  Enter 展开  d 维度  c 结论  ? 更多",
                "q Exit  ↑↓ Select  Enter Open  d Views  c Save  ? More"
            )
        } else {
            crate::localize!("↑↓ 选择  Enter 展开  b 返回  d/1–8 维度  / 搜索  o 源码  c 结论  v 疑问  h 历史  g 工作台  q 退出  ? 帮助", "↑↓ Select  Enter Open  b Back  d/1–8 Views  / Search  o Source  c Conclusion  v Question  h History  g Workbench  q Exit  ? Help")
        },
        Color::Cyan,
        false,
        0,
    )?;
    queue!(out, style::ResetColor)?;
    out.flush()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn line(
    out: &mut impl Write,
    x: u16,
    y: u16,
    width: u16,
    text: &str,
    color: Color,
    selected: bool,
    offset: usize,
) -> Result<()> {
    queue!(
        out,
        cursor::MoveTo(x, y),
        style::SetForegroundColor(color),
        style::SetBackgroundColor(if selected {
            Color::DarkBlue
        } else {
            Color::Reset
        }),
        style::Print(clip(text, usize::from(width), offset)),
        style::ResetColor
    )?;
    Ok(())
}

// User-controlled source and manifest text must never become terminal commands.
// Widths are display cells (Chinese characters consume two), not bytes/chars.
fn visible_breadcrumb(text: &str, width: usize) -> String {
    let text = clip(text, usize::MAX, 0);
    let cells = |value: &str| value.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>();
    if cells(&text) <= width {
        return text;
    }
    let prefix = "… › ";
    let available = width.saturating_sub(cells(prefix));
    let mut parts = text.rsplit(" › ");
    // Keep the active page, then add as many recent ancestors as fit. Clipping
    // the complete path from its start hid the active page after deep navigation.
    let mut suffix = clip(parts.next().unwrap_or(""), available, 0);
    for parent in parts {
        let candidate = format!("{parent} › {suffix}");
        if cells(&candidate) > available {
            break;
        }
        suffix = candidate;
    }
    format!("{prefix}{suffix}")
}

fn clip(text: &str, width: usize, offset: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    let mut skipped = 0;
    for c in text.chars().filter(|c| {
        !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    }) {
        let cells = c.width().unwrap_or(0);
        if skipped < offset {
            skipped += cells;
            continue;
        }
        if used + cells > width {
            break;
        }
        result.push(c);
        used += cells;
    }
    result
}
