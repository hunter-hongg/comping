//! 纯渲染函数：只读 `&App`，产出 ratatui 组件。
//!
//! 布局：对话为主 + 右侧信息栏 + 底部输入框。
//! 宽度不足时自动降级为单栏，信息栏折叠成状态栏一行。

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, Wrap,
};

use agent_core::StopReason;

use crate::app::{App, Entry, Focus, NoticeLevel};
use crate::markdown;

/// 单栏与双栏的分界。
const WIDE_ENOUGH: u16 = 100;
/// 对话区与信息栏的比例。
const SIDEBAR_RATIO: u16 = 30;

/// 主入口。
pub fn render(frame: &mut ratatui::Frame, app: &mut App) {
    let area = frame.area();

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(1), // 状态栏
            Constraint::Length(3), // 输入框
        ])
        .split(area);

    if area.width >= WIDE_ENOUGH {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(100 - SIDEBAR_RATIO), Constraint::Length(SIDEBAR_RATIO)])
            .split(vertical[0]);
        render_conversation(frame, app, cols[0]);
        render_sidebar(frame, app, cols[1]);
    } else {
        render_conversation(frame, app, vertical[0]);
    }

    render_status_bar(frame, app, vertical[1]);
    render_input(frame, app, vertical[2]);

    // 批准面板浮在最上层
    if app.has_approval() {
        render_approval(frame, app, area);
    }
}

/// 带边框与标题的面板。
fn panel(title: &str, focused: bool, theme: &crate::theme::Theme) -> Block<'static> {
    let color = if focused { theme.border_focused } else { theme.border };
    Block::default()
        .borders(Borders::ALL)
        .border_type(if focused {
            BorderType::Rounded
        } else {
            BorderType::Plain
        })
        .border_style(Style::default().fg(color))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(color).add_modifier(if focused {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
        ))
}

/// 把 `App` 的条目展平成渲染行。
///
/// 流式文本作为一条临时助手行插在最后，这样边流边显示。
pub fn build_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();

    for entry in &app.entries {
        match entry {
            Entry::User { text } => {
                if !lines.is_empty() {
                    lines.push(Line::from(""));
                }
                lines.push(Line::from(vec![
                    Span::styled("你 ", app.theme.bold(app.theme.user)),
                    Span::styled(
                        format!("│ {text}"),
                        Style::default().fg(app.theme.user),
                    ),
                ]));
            }
            Entry::Assistant {
                text,
                usage,
                stop,
                elapsed_ms,
            } => {
                if !lines.is_empty() {
                    lines.push(Line::from(""));
                }
                lines.extend(markdown::render(text, width, &app.theme));
                // 回合末的小字统计：让人一眼看出这轮花了多少
                if *elapsed_ms > 0 || usage.total() > 0 {
                    let mut meta = vec![format!("{:.1}s", *elapsed_ms as f32 / 1000.0)];
                    if usage.input_tokens > 0 {
                        meta.push(format!("↑{}", usage.input_tokens));
                    }
                    if usage.output_tokens > 0 {
                        meta.push(format!("↓{}", usage.output_tokens));
                    }
                    if usage.cache_read_tokens > 0 {
                        meta.push(format!(
                            "缓存 {:.0}%",
                            usage.cache_hit_ratio() * 100.0
                        ));
                    }
                    let stop_txt = agent_core::describe_stop_reason(*stop);
                    if *stop != StopReason::EndTurn {
                        meta.push(stop_txt.to_string());
                    }
                    lines.push(Line::from(Span::styled(
                        meta.join(" · "),
                        app.theme.dimmed(),
                    )));
                }
            }
            Entry::Tool {
                name,
                args,
                summary,
                output,
                ok,
                elapsed_ms,
                expanded,
                ..
            } => {
                if !lines.is_empty() {
                    lines.push(Line::from(""));
                }
                let (icon, color) = match (ok, output.is_some()) {
                    (_, false) => ("◐", app.theme.pending),
                    (true, true) => ("✓", app.theme.tool_ok),
                    (false, true) => ("✗", app.theme.tool_error),
                };
                let caret = if *expanded { "▾" } else { "▸" };
                let mut spans = vec![
                    Span::styled(
                        format!("{caret} {icon} "),
                        Style::default().fg(color),
                    ),
                    Span::styled(
                        name.clone(),
                        app.theme.bold(app.theme.tool),
                    ),
                    Span::styled(
                        format!("  {}", compact_args(args)),
                        app.theme.dimmed(),
                    ),
                    Span::styled(
                        format!("  {summary}"),
                        Style::default().fg(color),
                    ),
                ];
                if *elapsed_ms > 0 {
                    spans.push(Span::styled(
                        format!("  {:.1}s", *elapsed_ms as f32 / 1000.0),
                        app.theme.dimmed(),
                    ));
                }
                lines.push(Line::from(spans));

                if *expanded {
                    if let Some(out) = output {
                        let inner = (width as usize).saturating_sub(6);
                        for l in out.lines() {
                            let text: String = if l.chars().count() > inner {
                                let t: String = l.chars().take(inner.saturating_sub(1)).collect();
                                format!("{t}…")
                            } else {
                                l.to_string()
                            };
                            lines.push(Line::from(vec![
                                Span::styled("    │ ", app.theme.dimmed()),
                                Span::styled(text, app.theme.dimmed()),
                            ]));
                        }
                    }
                }
            }
            Entry::Notice { text, level } => {
                let color = match level {
                    NoticeLevel::Info => app.theme.dim,
                    NoticeLevel::Warn => app.theme.warn,
                    NoticeLevel::Error => app.theme.error,
                };
                let icon = match level {
                    NoticeLevel::Info => "ℹ",
                    NoticeLevel::Warn => "⚠",
                    NoticeLevel::Error => "✗",
                };
                if !lines.is_empty() {
                    lines.push(Line::from(""));
                }
                lines.push(Line::from(vec![
                    Span::styled(format!("{icon} "), Style::default().fg(color)),
                    Span::styled(text.clone(), Style::default().fg(color)),
                ]));
            }
        }
    }

    // 正在流式输出的内容
    if !app.streaming.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.extend(markdown::render(&app.streaming, width, &app.theme));
        lines.push(Line::from(Span::styled(
            "▌",
            Style::default().fg(app.theme.accent),
        )));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "输入你的问题，Enter 发送。输入 /help 查看命令。",
            app.theme.dimmed(),
        )));
    }

    lines
}

/// 工具行里显示的参数摘要。
///
/// 直接摊开 JSON 的话每行都是 `{"path":"src/main.rs"}` 这种噪声，
/// 占了半行宽度却没提供信息。只挑"最能说明这次调用在干嘛"的那个字段
/// 显示，其余参数折叠成 `+N`。
fn compact_args(args: &str) -> String {
    const MAX: usize = 68;
    // 优先展示的字段，按"最常用"排序
    const PRIORITY: &[&str] = &["command", "path", "pattern", "query", "file", "name"];

    let v: serde_json::Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(_) => return truncate_tail(&args.replace('\n', " "), MAX),
    };
    let Some(obj) = v.as_object() else {
        return truncate_tail(&args.replace('\n', " "), MAX);
    };

    let shown = PRIORITY
        .iter()
        .find_map(|k| obj.get(*k).and_then(|x| x.as_str()).map(|x| (*k, x)))
        // 没有优先字段就退而求其次：第一个字符串值
        .or_else(|| {
            obj.iter()
                .find(|(_, x)| x.is_string())
                .and_then(|(k, x)| x.as_str().map(|s| (k.as_str(), s)))
        });

    let Some((key, val)) = shown else {
        return truncate_tail(&args.replace('\n', " "), MAX);
    };

    // 省略掉的字段数：让人知道"还有别的参数"
    let hidden = obj.len().saturating_sub(1);
    let mut out = format!("{key}={val}");
    if hidden > 0 {
        out.push_str(&format!(" +{hidden}"));
    }
    truncate_tail(&out, MAX)
}

fn truncate_tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "\u{2026}".to_string();
    }
    let keep = max - 1;
    format!("{}\u{2026}", s.chars().take(keep).collect::<String>())
}

fn render_conversation(frame: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Conversation;
    let block = panel("对话", focused, &app.theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width;
    let lines = build_lines(app, width);
    let total = lines.len() as u16;

    // 视口高度决定滚动步长
    let viewport = inner.height;
    let offset = match app.scroll {
        Some(s) => s.min(total.saturating_sub(1)),
        // 跟随底部
        None => total.saturating_sub(viewport),
    };
    app.last_scroll = (offset, total, viewport);

    let para = Paragraph::new(lines.clone())
        .wrap(Wrap { trim: false })
        .scroll((offset, 0));
    frame.render_widget(para, inner);

    if total > viewport {
        let sb = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_style(app.theme.dimmed())
            .thumb_style(Style::default().fg(app.theme.accent));
        // Scrollbar 需要独立状态，用一个临时值渲染
        let mut state = ratatui::widgets::ScrollbarState::new(
            total.saturating_sub(viewport) as usize,
        )
        .position(offset as usize);
        frame.render_stateful_widget(sb, area.inner(ratatui::layout::Margin {
            vertical: 1,
            horizontal: 0,
        }), &mut state);
    }
}

fn render_sidebar(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let block = panel("状态", false, &app.theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let s = &app.status;
    let t = &app.theme;
    let mut lines: Vec<Line> = Vec::new();

    // 闭包捕获 lines 会和后续 push 抢可变借用，改成普通函数
    fn section(lines: &mut Vec<Line>, title: &str, t: &crate::theme::Theme) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            title.to_uppercase(),
            t.bold(t.dim),
        )));
    }

    // 键占 10 列，减去边框留 2，余下才是值的宽度
    let vw = inner.width.saturating_sub(12).max(6) as usize;
    section(&mut lines, "模型", t);
    lines.push(kv("provider", &elide(&s.provider, vw), t));
    // 优先用 display_name：模型 id 常常又长又带命名空间前缀，
    // 中间省略会毁掉可读性（openco…ny-free 谁都认不出）
    lines.push(kv("model", &elide(&app.model_label(), vw), t));

    section(&mut lines, "上下文", t);
    let ratio = s.used_ratio();
    lines.push(bar(ratio, inner.width.saturating_sub(2), t));
    lines.push(Line::from(Span::styled(
        format!("  {} / {} ({:.0}%)", s.used_tokens, s.context_window, ratio * 100.0),
        t.dimmed(),
    )));

    section(&mut lines, "用量", t);
    lines.push(kv("输入", &s.total_usage.input_tokens.to_string(), t));
    lines.push(kv("输出", &s.total_usage.output_tokens.to_string(), t));
    let hit = s.total_usage.cache_hit_ratio();
    if s.total_usage.cache_read_tokens > 0 {
        lines.push(kv(
            "缓存命中",
            &format!("{} ({:.0}%)", s.total_usage.cache_read_tokens, hit * 100.0),
            t,
        ));
    }

    section(&mut lines, "本次运行", t);
    if s.step > 0 {
        lines.push(kv("轮次", &format!("{} / {}", s.step, s.max_steps), t));
        lines.push(kv(
            "上轮耗时",
            &format!("{:.1}s", s.last_elapsed_ms as f32 / 1000.0),
            t,
        ));
    }
    if let Some(reason) = &s.last_run {
        lines.push(kv("状态", &agent_core::describe_stop(reason), t));
    }

    let para = Paragraph::new(lines).wrap(Wrap { trim: true });
    frame.render_widget(para, inner);
}

/// 键值行。值超长时**在中间省略**——
/// 直接让 Paragraph 换行会把 `opencode-zen/space-bunny-free`
/// 断成 `space-bunny-fre` + `e`，很难看。
fn kv<'a>(k: &str, v: &str, t: &crate::theme::Theme) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{k:<10}"), t.dimmed()),
        Span::styled(v.to_string(), Style::default().fg(t.assistant)),
    ])
}

/// 按可用宽度截断。**尾部省略**：模型 id 带命名空间前缀
/// （`opencode-zen/space-bunny-free`），中间省略会把前缀和词根都切掉，
/// 变成认不出的碎片。
fn elide(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "\u{2026}".to_string();
    }
    let keep = max - 1;
    format!("{}\u{2026}", s.chars().take(keep).collect::<String>())
}

/// 上下文占用条。
///
/// 轨道用 `─` 而不是 `░`：后者密集排列时像一团噪点，
/// 在 0% 时尤其明显，看着像渲染出错而不是"还没用"。
fn bar(ratio: f32, width: u16, t: &crate::theme::Theme) -> Line<'static> {
    let width = width.max(4) as usize;
    let filled = ((ratio.clamp(0.0, 1.0) * width as f32) as usize).min(width);
    let color = t.bar_color(ratio);
    let bar: String = "█".repeat(filled) + &"─".repeat(width - filled);
    Line::from(Span::styled(bar, Style::default().fg(color)))
}

fn render_status_bar(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let mut spans = Vec::new();

    let state: String = if app.has_approval() {
        let n = app.approval_queue_len();
        if n > 1 {
            format!("◐ 等待确认 {n}")
        } else {
            "◐ 等待确认".to_string()
        }
    } else if app.busy {
        "● 运行中".to_string()
    } else {
        "○ 就绪".to_string()
    };
    let color = if app.has_approval() {
        t.pending
    } else if app.busy {
        t.accent
    } else {
        t.dim
    };
    spans.push(Span::styled(format!(" {state} "), Style::default().fg(color)));

    spans.push(Span::styled(
        format!(" {} · {}", app.status.provider, app.status.model),
        Style::default().fg(t.dim),
    ));

    if app.busy && app.status.step > 0 {
        spans.push(Span::styled(
            format!("  第 {} 轮", app.status.step),
            t.dimmed(),
        ));
    }

    if let Some(err) = &app.last_error {
        spans.push(Span::styled(
            format!("  ⚠ {}", truncate(err, 40)),
            Style::default().fg(t.error),
        ));
    }

    // 右侧提示
    let hint = if app.has_approval() {
        "y 允许 · n 拒绝 · a 本轮全允许"
    } else if app.busy {
        "Esc 中断"
    } else {
        "/help 命令 · Ctrl-C 退出"
    };
    let hint_w = hint.chars().count() as u16 + 2;
    let used: u16 = spans.iter().map(|s| s.content.chars().count() as u16).sum();
    if used + hint_w < area.width {
        let pad = area.width.saturating_sub(used + hint_w) as usize;
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(format!(" {hint} "), t.dimmed()));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_input(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let focused = app.focus == Focus::Input;
    let border = if focused { t.accent } else { t.border };

    let hint = match (app.busy, app.input.is_empty()) {
        (true, true) => "运行中，可继续输入以排队 · Esc 中断",
        (true, false) => "Enter 排队 · Esc 中断",
        (false, true) => "Enter 发送 · Shift-Enter 换行 · /help 看命令",
        (false, false) => "Enter 发送 · Esc 清空",
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border));

    // 把光标位置转成显示宽度偏移
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 光标之前的正文 + 光标符 + 之后的正文，一次拼成一行，
    // 这样光标位置天然就是字符偏移，不需要额外算列。
    let visible: String = app
        .input
        .buffer
        .chars()
        .take(inner.width.saturating_sub(2) as usize)
        .collect();
    let cursor = app.input.cursor.min(visible.chars().count());
    let before: String = visible.chars().take(cursor).collect();
    let after: String = visible.chars().skip(cursor).collect();

    let text_style = Style::default().fg(if app.busy { t.dim } else { t.input });
    let text_line = Line::from(vec![
        Span::styled("❯ ", Style::default().fg(t.accent)),
        Span::styled(before, text_style),
        Span::styled(
            "▏",
            Style::default().fg(t.accent).add_modifier(Modifier::SLOW_BLINK),
        ),
        Span::styled(after, text_style),
    ]);

    let lines = vec![text_line, Line::from(Span::styled(hint, t.dimmed()))];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// 批准面板：浮层，覆盖在中央。
fn render_approval(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    // 只显示队首；其余的等用户处理完这个再问
    let Some(approval) = app.approvals.front() else { return };
    let t = &app.theme;

    let w = (area.width * 3 / 4).clamp(30, 100);
    let h = (area.height / 2).clamp(8, 30);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect::new(x, y, w, h);

    frame.render_widget(Clear, rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.pending))
        .title(Span::styled(approval_title(app), t.bold(t.pending)))
        .title_bottom(Span::styled(
            " y 允许一次 · n 拒绝 · a 本轮全部允许 ",
            t.dimmed(),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let mut lines = vec![Line::from(Span::styled(
        approval.title.clone(),
        t.bold(t.assistant),
    ))];
    lines.push(Line::from(""));
    for l in approval.body.lines() {
        let text = if l.chars().count() > inner.width as usize {
            let s: String = l.chars().take(inner.width as usize).collect();
            format!("{s}…")
        } else {
            l.to_string()
        };
        lines.push(Line::from(Span::styled(text, Style::default().fg(t.dim))));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((approval.scroll, 0)),
        inner,
    );
}

/// 批准面板标题。队列里还有别的请求时标出进度，
/// 否则用户会以为"批完这一个就没事了"，然后被下一个弹窗再吓一跳。
fn approval_title(app: &App) -> String {
    let n = app.approval_queue_len();
    if n > 1 {
        format!(" 需要确认 (1/{n}) ")
    } else {
        " 需要确认 ".to_string()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

#[cfg(test)]
mod arg_tests {
    use super::*;

    #[test]
    fn 显示最有用的那个参数() {
        assert_eq!(compact_args(r#"{"path":"src/main.rs"}"#), "path=src/main.rs");
        assert_eq!(
            compact_args(r#"{"command":"cargo test --all"}"#),
            "command=cargo test --all"
        );
    }

    #[test]
    fn 其余参数折叠成计数() {
        assert_eq!(
            compact_args(r#"{"path":"a.rs","offset":1,"limit":20}"#),
            "path=a.rs +2"
        );
    }

    /// 回归：以前每行都摊开 JSON，半行宽度全是噪声
    #[test]
    fn 不再摊开原始_json() {
        let out = compact_args(r#"{"path":"src/main.rs"}"#);
        assert!(!out.contains('{'), "不该出现 JSON 花括号: {out}");
        assert!(!out.contains('"'), "不该出现引号: {out}");
    }

    #[test]
    fn 没有优先字段时取第一个字符串值() {
        assert_eq!(compact_args(r#"{"weird":"value"}"#), "weird=value");
    }

    #[test]
    fn 非_json也能显示() {
        assert!(!compact_args("not json at all").is_empty());
    }

    #[test]
    fn 超长会截断() {
        let long = format!(r#"{{"path":"{}"}}"#, "x".repeat(200));
        let out = compact_args(&long);
        assert!(out.chars().count() <= 68, "长度 {} 超出上限", out.chars().count());
    }

    #[test]
    fn elide_是尾部省略() {
        // 回归：中间省略会把 opencode-zen/space-bunny-free
        // 变成认不出的碎片
        assert_eq!(elide("abcdefghij", 5), "abcd…");
        assert_eq!(elide("abc", 5), "abc");
    }
}
