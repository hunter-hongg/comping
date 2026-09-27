//! Markdown 轻量渲染。
//!
//! 只支持真正影响可读性的子集：标题、列表、代码块、引用、行内 code、粗体。
//! 不做完整 CommonMark —— 界面宽度有限，复杂嵌套反而更难读。

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme::Theme;

/// 把一段 markdown 变成带样式的行。
pub fn render(text: &str, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let mut out: Vec<Line> = Vec::new();
    let mut in_code = false;
    let mut code_lang: Option<String> = None;
    let mut code_buf: Vec<String> = Vec::new();

    for raw in text.lines() {
        // 代码块围栏
        if let Some(rest) = raw.trim_start().strip_prefix("```") {
            if in_code {
                out.extend(close_code(&code_buf, &code_lang, width, theme));
                code_buf.clear();
                code_lang = None;
                in_code = false;
            } else {
                in_code = true;
                code_lang = Some(rest.trim().to_string());
            }
            continue;
        }

        if in_code {
            code_buf.push(raw.to_string());
            continue;
        }

        // 标题
        let trimmed = raw.trim_start();
        if let Some(rest) = trimmed.strip_prefix('#') {
            let level = 1 + trimmed.chars().take_while(|c| *c == '#').count() - 1;
            let content = rest.trim_start_matches('#').trim();
            let style = match level {
                1 => theme.bold(theme.accent).add_modifier(Modifier::UNDERLINED),
                2 => theme.bold(theme.accent),
                _ => Style::default().fg(theme.assistant).add_modifier(Modifier::BOLD),
            };
            out.push(Line::from(vec![
                Span::styled(if level == 1 { "\n" } else { "" }, style),
                Span::styled(content.to_string(), style),
            ]));
            continue;
        }

        // 引用
        if let Some(rest) = trimmed.strip_prefix("> ") {
            let mut spans = vec![Span::styled("│ ", theme.dimmed())];
            for sp in inline(rest, theme) {
                spans.push(Span::styled(sp.content, theme.dim));
            }
            out.push(Line::from(spans));
            continue;
        }

        // 列表
        if let Some((indent, marker, content)) = parse_list_item(raw) {
            let mut spans = vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(format!("{marker} "), Style::default().fg(theme.accent)),
            ];
            spans.extend(inline(content, theme));
            out.push(Line::from(spans));
            continue;
        }

        // 空行：压缩连续空行，界面更紧凑
        if raw.trim().is_empty() {
            if out.last().map(|l| l.spans.is_empty()).unwrap_or(true) {
                continue;
            }
            out.push(Line::from(""));
            continue;
        }

        out.push(Line::from(inline(raw, theme)));
    }

    // 结尾没有闭合围栏也要收尾
    if in_code {
        out.extend(close_code(&code_buf, &code_lang, width, theme));
    }

    out
}

/// 渲染代码块：左侧竖线 + 语言标签，缩进对齐。
fn close_code(buf: &[String], lang: &Option<String>, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    if buf.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(buf.len() + 2);
    let inner_w = (width as usize).saturating_sub(4).max(8);

    out.push(Line::from(Span::styled(
        match lang {
            Some(l) if !l.is_empty() => format!("┌─ {l}"),
            _ => "┌─".to_string(),
        },
        theme.dimmed(),
    )));
    for line in buf {
        // 超宽行截断，避免破坏布局
        let text: String = if line.chars().count() > inner_w {
            let t: String = line.chars().take(inner_w.saturating_sub(1)).collect();
            format!("{t}…")
        } else {
            line.clone()
        };
        out.push(Line::from(vec![
            Span::styled("│ ", theme.dimmed()),
            Span::styled(text, Style::default().fg(theme.code)),
        ]));
    }
    out.push(Line::from(Span::styled("└", theme.dimmed())));
    out
}

/// 解析 `- item` / `* item` / `1. item`。
fn parse_list_item(raw: &str) -> Option<(usize, String, &str)> {
    let indent = raw.len() - raw.trim_start().len();
    let t = raw.trim_start();
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(marker) {
            return Some((indent, "•".into(), rest));
        }
    }
    // 有序列表：数字 + .
    if let Some(dot) = t.find(". ") {
        if dot > 0 && t[..dot].chars().all(|c| c.is_ascii_digit()) {
            return Some((indent, format!("{}.", &t[..dot]), &t[dot + 2..]));
        }
    }
    None
}

/// 行内标记：`` `code` `` 与 `**粗体**`。
fn inline(text: &str, theme: &Theme) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let base = Style::default().fg(theme.assistant);
    let code_style = Style::default().fg(theme.code);
    let bold_style = Style::default()
        .fg(theme.assistant)
        .add_modifier(Modifier::BOLD);

    let mut rest = text;
    while !rest.is_empty() {
        // 行内代码优先，优先级高于粗体
        if let Some(start) = rest.find('`') {
            if let Some(rel_end) = rest[start + 1..].find('`') {
                if start > 0 {
                    push_with_emphasis(&mut out, &rest[..start], base, bold_style);
                }
                out.push(Span::styled(rest[start + 1..start + 1 + rel_end].to_string(), code_style));
                rest = &rest[start + rel_end + 2..];
                continue;
            }
        }

        if let Some(start) = rest.find("**") {
            if let Some(rel_end) = rest[start + 2..].find("**") {
                if start > 0 {
                    out.push(Span::styled(rest[..start].to_string(), base));
                }
                out.push(Span::styled(
                    rest[start + 2..start + 2 + rel_end].to_string(),
                    bold_style,
                ));
                rest = &rest[start + rel_end + 4..];
                continue;
            }
        }

        out.push(Span::styled(rest.to_string(), base));
        break;
    }

    out
}

fn push_with_emphasis(
    out: &mut Vec<Span<'static>>,
    text: &str,
    base: Style,
    bold: Style,
) {
    let mut rest = text;
    while let Some(i) = rest.find("**") {
        if let Some(e) = rest[i + 2..].find("**") {
            out.push(Span::styled(rest[..i].to_string(), base));
            out.push(Span::styled(rest[i + 2..i + 2 + e].to_string(), bold));
            rest = &rest[i + e + 4..];
        } else {
            break;
        }
    }
    if !rest.is_empty() {
        out.push(Span::styled(rest.to_string(), base));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn 渲染标题() {
        let t = Theme::default();
        let lines = render("## 环境\n正文", 80, &t);
        assert!(plain(&lines[0]).contains("环境"));
    }

    #[test]
    fn 渲染列表() {
        let t = Theme::default();
        let lines = render("- 第一项\n- 第二项", 80, &t);
        assert!(plain(&lines[0]).contains("第一项"));
        assert!(plain(&lines[1]).contains("第二项"));
    }

    #[test]
    fn 渲染代码块含语言标签() {
        let t = Theme::default();
        let lines = render("```rust\nfn main() {}\n```", 80, &t);
        assert!(plain(&lines[0]).contains("rust"));
        assert!(plain(&lines[1]).contains("fn main"));
    }

    #[test]
    fn 行内代码被单独标出() {
        let t = Theme::default();
        let lines = render("调用 `read_file` 工具", 80, &t);
        let has_code = lines[0]
            .spans
            .iter()
            .any(|s| s.content.as_ref() == "read_file");
        assert!(has_code);
    }

    #[test]
    fn 未闭合围栏也能收尾() {
        let t = Theme::default();
        let lines = render("```\n未写完", 80, &t);
        assert!(lines.iter().any(|l| plain(l).contains("└")));
    }
}
