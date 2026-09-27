//! 主题：把所有配色集中一处，界面代码只引用语义名。
//!
//! 风格取向：低饱和背景 + 单一强调色，让对话内容本身成为视觉重心。

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone)]
pub struct Theme {
    /// 边框与分隔线
    pub border: Color,
    pub border_focused: Color,
    /// 强调色，用于选中、焦点
    pub accent: Color,
    /// 用户消息
    pub user: Color,
    /// 助手消息
    pub assistant: Color,
    /// 工具调用的折叠标题
    pub tool: Color,
    pub tool_ok: Color,
    pub tool_error: Color,
    /// 工具执行中
    pub pending: Color,
    /// 二级文字、摘要
    pub dim: Color,
    /// 输入框文字
    pub input: Color,
    /// 代码块
    pub code: Color,
    /// 警告 / 错误
    pub warn: Color,
    pub error: Color,
    /// 上下文占用条
    pub bar_low: Color,
    pub bar_mid: Color,
    pub bar_high: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            border: Color::Rgb(58, 62, 74),
            border_focused: Color::Rgb(120, 165, 255),
            accent: Color::Rgb(120, 165, 255),
            user: Color::Rgb(126, 211, 168),
            assistant: Color::Rgb(214, 220, 233),
            tool: Color::Rgb(178, 148, 240),
            tool_ok: Color::Rgb(126, 211, 168),
            tool_error: Color::Rgb(240, 130, 130),
            pending: Color::Rgb(240, 190, 110),
            dim: Color::Rgb(128, 134, 150),
            input: Color::Rgb(236, 240, 248),
            code: Color::Rgb(150, 200, 240),
            warn: Color::Rgb(240, 190, 110),
            error: Color::Rgb(240, 130, 130),
            bar_low: Color::Rgb(126, 211, 168),
            bar_mid: Color::Rgb(240, 190, 110),
            bar_high: Color::Rgb(240, 130, 130),
        }
    }
}

impl Theme {
    /// 上下文占用 < 60% 用绿色，< 85% 黄色，否则红色。
    pub fn bar_color(&self, ratio: f32) -> Color {
        match ratio {
            r if r < 0.6 => self.bar_low,
            r if r < 0.85 => self.bar_mid,
            _ => self.bar_high,
        }
    }

    pub fn dimmed(&self) -> Style {
        Style::default().fg(self.dim)
    }

    pub fn bold(&self, c: Color) -> Style {
        Style::default().fg(c).add_modifier(Modifier::BOLD)
    }
}
