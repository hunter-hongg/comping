//! 终端抽象：负责绘制，以及在尺寸变化时重建后端。

use std::io;

use ratatui::DefaultTerminal;

use crate::app::App;

pub struct Tui {
    terminal: DefaultTerminal,
}

impl Tui {
    /// 接管一个已初始化的终端。
    pub fn new(terminal: &mut DefaultTerminal) -> Self {
        Self {
            terminal: std::mem::replace(terminal, ratatui::init()),
        }
    }

    pub fn draw(&mut self, app: &mut App) -> io::Result<()> {
        self.terminal.draw(|frame| crate::ui::render(frame, app))?;
        Ok(())
    }

    /// 终端尺寸变化后同步 viewport。
    pub fn autoresize(&mut self) -> io::Result<()> {
        self.terminal.autoresize()?;
        Ok(())
    }
}
