//! 应用状态：把 `AgentEvent` 流折叠成可渲染的对话视图。
//!
//! 这一层不含任何 ratatui 代码，纯数据。好处是状态逻辑可以单独测试，
//! 渲染函数也能保持成纯函数。

use std::collections::VecDeque;

use agent_core::{RunEndReason, StopReason, Usage};

use crate::theme::Theme;

/// 一条对话项。
#[derive(Debug, Clone)]
pub enum Entry {
    User {
        text: String,
    },
    Assistant {
        text: String,
        usage: Usage,
        stop: StopReason,
        elapsed_ms: u64,
    },
    /// 工具调用。`output` 为 None 表示还在跑。
    Tool {
        /// 调用 id。**必须靠它回填结果**——同一轮里可能有多个同名调用
        /// （比如两个 list_dir），按名字匹配会把结果贴到错误的行上。
        id: String,
        name: String,
        args: String,
        summary: String,
        output: Option<String>,
        ok: bool,
        elapsed_ms: u64,
        /// 是否被用户展开
        expanded: bool,
    },
    /// 系统提示：压缩、警告等。
    Notice {
        text: String,
        level: NoticeLevel,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warn,
    Error,
}

/// 输入框状态。
#[derive(Debug, Default)]
pub struct Input {
    pub buffer: String,
    /// 光标在 buffer 中的字符位置
    pub cursor: usize,
    /// 历史输入，↑↓ 翻
    pub history: Vec<String>,
    pub history_idx: Option<usize>,
}

impl Input {
    /// `cursor` 始终是**字符下标**（不是字节偏移）。
    ///
    /// Rust 的 `String::insert`/`remove` 用的是字节偏移，中文场景下两者
    /// 不一致；统一在字符下标上运算，跨越函数边界时再换算。
    pub fn push_char(&mut self, c: char) {
        self.buffer.insert(self.byte_of(self.cursor), c);
        self.cursor += 1;
    }

    pub fn pop_char(&mut self) {
        if self.cursor > 0 {
            let at = self.byte_of(self.cursor - 1);
            self.buffer.remove(at);
            self.cursor -= 1;
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.pop_char();
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.len() {
            self.buffer.remove(self.byte_of(self.cursor));
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        if self.cursor < self.len() {
            self.cursor += 1;
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.len();
    }

    /// 字符下标 → 字节偏移。
    fn byte_of(&self, char_idx: usize) -> usize {
        self.buffer
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.buffer.len())
    }

    /// 提交内容。空输入返回 None。
    pub fn submit(&mut self) -> Option<String> {
        let text = self.buffer.trim().to_string();
        if text.is_empty() {
            return None;
        }
        if self.history.last().map(|h| h != &text).unwrap_or(true) {
            self.history.push(text.clone());
        }
        self.buffer.clear();
        self.cursor = 0;
        self.history_idx = None;
        Some(text)
    }

    /// 插入一段预填文本（翻历史时用）。
    pub fn set(&mut self, text: &str) {
        self.buffer = text.to_string();
        self.cursor = self.len();
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn len(&self) -> usize {
        self.buffer.chars().count()
    }
}

/// 一个待裁决的请求。
///
/// `respond` 是一次性的，类型上就是 `FnOnce`——这让"忘记回应"
/// 变成编译期或所有权层面的问题，而不是运行时静默失败。
pub struct Approval {
    pub title: String,
    pub body: String,
    /// 回传决定的回调
    pub respond: Box<dyn FnOnce(agent_core::Decision) + Send>,
    pub scroll: u16,
}

impl std::fmt::Debug for Approval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Approval")
            .field("title", &self.title)
            .field("scroll", &self.scroll)
            .finish_non_exhaustive()
    }
}

/// 应用总状态。
pub struct App {
    pub running: bool,
    pub entries: Vec<Entry>,
    /// 正在流式输出的助手文本
    pub streaming: String,
    /// 是否有正在跑的请求
    pub busy: bool,
    pub input: Input,
    pub theme: Theme,
    /// 对话区滚动位置（None = 跟随底部）
    pub scroll: Option<u16>,
    /// 焦点面板
    pub focus: Focus,
    pub status: Status,
    /// 待处理的批准请求。**必须排队**：并行工具会同时发多个，
    /// 以前只留一个槽位，后来的会把前面的 `respond` drop 掉，
    /// 那个工具于是被误判成"用户拒绝了"。
    pub approvals: VecDeque<Approval>,
    /// 本次运行的最近一个错误
    pub last_error: Option<String>,
    /// 上次渲染算出的 (滚动偏移, 总行数, 视口高度)，供按键处理用
    pub last_scroll: (u16, u16, u16),
    /// 对话区里当前选中的工具条目
    pub tool_cursor: Option<usize>,
    /// 工作区根目录，`/status` 里显示
    pub workspace: String,
    /// 是否一次性模式（headless），影响状态栏提示
    pub headless: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Input,
    Conversation,
    Sidebar,
}

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub provider: String,
    pub model: String,
    /// 模型的友好名，来自配置里的 `display_name`。
    /// 侧栏优先显示它——原始 id 常常又长又带命名空间前缀。
    pub model_display: Option<String>,
    pub context_window: u32,
    pub used_tokens: u32,
    pub total_usage: Usage,
    pub step: usize,
    pub max_steps: usize,
    pub last_elapsed_ms: u64,
    /// 本次运行是否刚结束及其原因
    pub last_run: Option<RunEndReason>,
}

impl Status {
    pub fn used_ratio(&self) -> f32 {
        if self.context_window == 0 {
            0.0
        } else {
            self.used_tokens as f32 / self.context_window as f32
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            running: true,
            entries: Vec::new(),
            streaming: String::new(),
            busy: false,
            input: Input::default(),
            theme: Theme::default(),
            scroll: None,
            focus: Focus::Input,
            status: Status::default(),
            approvals: VecDeque::new(),
            last_error: None,
            last_scroll: (0, 0, 0),
            tool_cursor: None,
            workspace: String::new(),
            headless: false,
        }
    }

    /// 处理一个 agent 事件，就地更新状态。
    pub fn on_agent_event(&mut self, ev: agent_core::AgentEvent) {
        match ev {
            agent_core::AgentEvent::UserMessage { text } => {
                self.entries.push(Entry::User { text });
                self.scroll = None;
            }
            agent_core::AgentEvent::AssistantDelta { text } => {
                self.streaming.push_str(&text);
            }
            agent_core::AgentEvent::ToolCallStarted { call } => {
                // 流式文本先落成一条助手消息，工具行紧随其后
                self.flush_streaming(Usage::default(), StopReason::ToolUse, 0);
                self.entries.push(Entry::Tool {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    args: call.arguments_json(),
                    summary: "执行中…".into(),
                    output: None,
                    ok: true,
                    elapsed_ms: 0,
                    expanded: false,
                });
            }
            agent_core::AgentEvent::ToolCallFinished {
                call,
                outcome,
                elapsed_ms,
            } => {
                // 按调用 id 精确回填。同名并行调用（两个 list_dir）
                // 才不会互相串台——按名字找会贴错行。
                let target = self.entries.iter().position(|e| {
                    matches!(e, Entry::Tool { id, output: None, .. } if *id == call.id)
                });

                if let Some(i) = target {
                    if let Entry::Tool {
                        summary,
                        output,
                        ok,
                        elapsed_ms: el,
                        ..
                    } = &mut self.entries[i]
                    {
                        *summary = outcome.summary;
                        *output = Some(outcome.output);
                        *ok = outcome.ok;
                        *el = elapsed_ms;
                    }
                }
            }
            agent_core::AgentEvent::TurnEnd {
                text,
                usage,
                stop_reason,
                had_tool_calls,
                elapsed_ms,
                step,
            } => {
                self.status.step = step;
                self.status.max_steps = self.status.max_steps.max(step);
                self.status.last_elapsed_ms = elapsed_ms;
                self.status.total_usage.merge(&usage);
                // 上下文占用：最近一次请求的提示词规模就是当前占用量。
                // 之前只在启动时算一次，所以整个会话里进度条都钉在 0%。
                let prompt = usage.input_tokens
                    + usage.cache_read_tokens
                    + usage.cache_write_tokens;
                self.status.used_tokens = prompt.max(self.status.used_tokens);
                if !had_tool_calls {
                    self.status.last_run = Some(RunEndReason::Completed);
                }
                // 有工具调用时 ToolCallStarted 已经 flush 过一次，这里补上用量
                if self.streaming.is_empty() && !text.is_empty() {
                    self.entries.push(Entry::Assistant {
                        text,
                        usage,
                        stop: stop_reason,
                        elapsed_ms,
                    });
                } else if !text.is_empty() {
                    // 流式已经攒了内容，替换成最终版
                    self.streaming = text;
                }
            }
            agent_core::AgentEvent::ApprovalNeeded {
                call,
                title,
                body,
                respond,
            } => {
                self.flush_streaming(Usage::default(), StopReason::ToolUse, 0);
                // 入队，不覆盖：并行工具会同时要批准
                self.approvals.push_back(Approval {
                    title: format!("{title}  [{}]", call.name),
                    body,
                    respond: Box::new(move |d| {
                        // 通道容量 1 且只有这一个发送方，try_send 不会阻塞。
                        // 失败只可能是执行器那边已经放弃（被取消），忽略即可
                        let _ = respond.try_send(d);
                    }),
                    scroll: 0,
                });
                self.focus = Focus::Sidebar;
            }
            agent_core::AgentEvent::RunEnd { reason } => {
                self.busy = false;
                self.flush_streaming(Usage::default(), StopReason::EndTurn, 0);
                self.status.last_run = Some(reason);
                self.scroll = None;
            }
            agent_core::AgentEvent::Notice { level, text } => {
                let level = match level {
                    agent_core::NoticeLevel::Info => NoticeLevel::Info,
                    agent_core::NoticeLevel::Warn => NoticeLevel::Warn,
                };
                self.entries.push(Entry::Notice { text, level });
            }
            agent_core::AgentEvent::Failed { error } => {
                self.last_error = Some(error.clone());
                self.entries.push(Entry::Notice {
                    text: error,
                    level: NoticeLevel::Error,
                });
            }
        }
    }

    /// 把攒着的流式文本转成正式条目。
    fn flush_streaming(&mut self, usage: Usage, stop: StopReason, elapsed_ms: u64) {
        if self.streaming.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.streaming);
        self.entries.push(Entry::Assistant {
            text,
            usage,
            stop,
            elapsed_ms,
        });
    }

    /// 回答当前待裁决的请求（队首），然后露出下一个。
    pub fn respond_approval(&mut self, d: agent_core::Decision) {
        if let Some(a) = self.approvals.pop_front() {
            (a.respond)(d);
        }
        // 队列还有剩就继续保持焦点，用户可以接着按下一个
        if self.approvals.is_empty() {
            self.focus = Focus::Input;
        }
    }

    /// 退出前丢弃所有待裁决的。
    ///
    /// 这里**不**主动回 Deny：执行器那边有取消信号兜底，
    /// `respond` 被 drop 时它会走 `recv() == None` 分支按拒绝处理。
    pub fn drop_pending_approvals(&mut self) {
        self.approvals.clear();
    }

    pub fn has_approval(&self) -> bool {
        !self.approvals.is_empty()
    }

    /// 当前待裁决的数量，UI 用来显示 "1/3"。
    pub fn approval_queue_len(&self) -> usize {
        self.approvals.len()
    }

    /// 滚动。`delta` 为正向下，负向上。
    pub fn scroll_by(&mut self, delta: i32, total_lines: u16) {
        let max = total_lines.saturating_sub(1);
        let cur = self.scroll.unwrap_or(max);
        let next = if delta < 0 {
            cur.saturating_sub((-delta) as u16)
        } else {
            (cur + delta as u16).min(max)
        };
        // 滚回底部就恢复跟随
        self.scroll = if next >= max { None } else { Some(next) };
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll = None;
    }

    /// 侧栏该显示的模型名：有 display_name 就用它，否则退回 id。
    pub fn model_label(&self) -> &str {
        match &self.status.model_display {
            Some(d) if !d.trim().is_empty() => d,
            _ => &self.status.model,
        }
    }

    /// 某条工具条目是否还在执行中。
    #[cfg(test)]
    pub fn is_tool_pending(&self, i: usize) -> bool {
        matches!(self.entries.get(i), Some(Entry::Tool { output: None, .. }))
    }

    /// 折叠/展开某个工具条目。
    pub fn toggle_expand(&mut self, idx: usize) {
        if let Some(Entry::Tool { expanded, .. }) = self.entries.get_mut(idx) {
            *expanded = !*expanded;
        }
    }

    /// 向上/向下在对话条目间移动光标，返回新位置。
    ///
    /// 只在对话区有焦点时使用：上下键既翻输入历史也移动条目焦点，
    /// 这里给出可被 Enter 展开的那一条。
    pub fn move_tool_cursor(&mut self, delta: i32) -> Option<usize> {
        let n = self.entries.len();
        if n == 0 {
            return None;
        }
        let cur = self.tool_cursor.unwrap_or(n.saturating_sub(1));
        let next = if delta < 0 {
            cur.saturating_sub(1)
        } else {
            (cur + 1).min(n - 1)
        };
        // 只停在工具条目上，否则继续朝同方向找
        let mut i = next as i64;
        while i >= 0 && i < n as i64 {
            if matches!(self.entries[i as usize], Entry::Tool { .. }) {
                self.tool_cursor = Some(i as usize);
                return self.tool_cursor;
            }
            i += delta as i64;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::ToolCall;

    #[test]
    fn 提交后清空并入历史() {
        let mut i = Input::default();
        i.set("hello");
        assert_eq!(i.submit(), Some("hello".into()));
        assert!(i.is_empty());
        assert_eq!(i.history, vec!["hello"]);
    }

    #[test]
    fn 空白输入不提交() {
        let mut i = Input::default();
        i.set("   ");
        assert_eq!(i.submit(), None);
    }

    #[test]
    fn 中文按字符移动光标() {
        let mut i = Input::default();
        i.set("中文");
        assert_eq!(i.cursor, 2);
        assert_eq!(i.len(), 2);

        i.left();
        assert_eq!(i.cursor, 1);

        // 光标在 "文" 前，backspace 删掉的是 "中"
        i.backspace();
        assert_eq!(i.buffer, "文");
        assert_eq!(i.cursor, 0);
    }

    #[test]
    fn 中间插入不破坏字节边界() {
        let mut i = Input::default();
        i.set("中文");
        i.left();
        i.push_char('X');
        assert_eq!(i.buffer, "中X文");
        assert_eq!(i.cursor, 2);
        i.right();
        i.push_char('Y');
        assert_eq!(i.buffer, "中X文Y");
    }

    #[test]
    fn 流式文本转正条目() {
        let mut a = App::new();
        a.on_agent_event(agent_core::AgentEvent::AssistantDelta {
            text: "你".into(),
        });
        a.on_agent_event(agent_core::AgentEvent::AssistantDelta {
            text: "好".into(),
        });
        assert!(a.streaming.is_empty() == false);
        a.on_agent_event(agent_core::AgentEvent::RunEnd {
            reason: RunEndReason::Completed,
        });
        assert!(a.streaming.is_empty());
        assert!(matches!(a.entries[0], Entry::Assistant { .. }));
    }

    /// 回归测试：以前 `approval` 是单个槽位，并行工具同时要批准时
    /// 后来的会把前面的 respond drop 掉，那个工具于是被误判成"用户拒绝"。
    #[test]
    fn 并行批准会排队而不是互相顶掉() {
        let mut a = App::new();
        let mut sent = Vec::new();

        for i in 0..3 {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            a.on_agent_event(agent_core::AgentEvent::ApprovalNeeded {
                call: ToolCall::new(format!("c{i}"), "bash", serde_json::json!({})),
                title: format!("第 {i} 个"),
                body: String::new(),
                respond: tx,
            });
            sent.push(rx);
        }

        assert_eq!(a.approval_queue_len(), 3, "三个批准请求都该在队列里");
        assert!(a.has_approval());

        // 逐个回答，每次都应该回给最早的那个
        for (i, mut rx) in sent.into_iter().enumerate() {
            a.respond_approval(agent_core::Decision::Allow);
            // respond 是 try_send，队列满时才失败；这里应当都成功
            let got = rx.try_recv();
            assert!(
                got.is_ok(),
                "第 {i} 个批准请求的通道应有回应，实际 {got:?}"
            );
        }

        assert!(!a.has_approval());
        assert_eq!(a.approval_queue_len(), 0);
    }

    #[test]
    fn 队列未空时保持批准焦点() {
        let mut a = App::new();
        for i in 0..2 {
            let (tx, _rx) = tokio::sync::mpsc::channel(1);
            a.on_agent_event(agent_core::AgentEvent::ApprovalNeeded {
                call: ToolCall::new(format!("c{i}"), "bash", serde_json::json!({})),
                title: "t".into(),
                body: String::new(),
                respond: tx,
            });
        }
        a.respond_approval(agent_core::Decision::Deny);
        assert!(a.has_approval(), "还有一个待处理，焦点应留在批准面板");
        assert_eq!(a.focus, Focus::Sidebar);

        a.respond_approval(agent_core::Decision::Deny);
        assert!(!a.has_approval());
        assert_eq!(a.focus, Focus::Input, "处理完才把焦点还给输入框");
    }

    #[test]
    fn 退出时丢弃待批准不会卡住() {
        let mut a = App::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        a.on_agent_event(agent_core::AgentEvent::ApprovalNeeded {
            call: ToolCall::new("c1", "bash", serde_json::json!({})),
            title: "t".into(),
            body: String::new(),
            respond: tx,
        });
        a.drop_pending_approvals();
        assert!(!a.has_approval());
        // 接收端被 drop → 执行器走 recv()==None 分支，按拒绝处理
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn 滚到底部恢复跟随() {
        let mut a = App::new();
        // total=10 时最大合法偏移是 9
        a.scroll_by(-5, 10);
        assert_eq!(a.scroll, Some(4));
        a.scroll_by(100, 10);
        assert_eq!(a.scroll, None);
    }

    #[test]
    fn 光标在中间插入() {
        let mut i = Input::default();
        i.set("ac");
        i.left();
        i.push_char('b');
        assert_eq!(i.buffer, "abc");
        assert_eq!(i.cursor, 2);
    }

    #[test]
    fn delete_删掉光标后的字符() {
        let mut i = Input::default();
        i.set("abc");
        i.home();
        // 光标在 a 前面，delete 删 a
        i.delete();
        assert_eq!(i.buffer, "bc");
        assert_eq!(i.cursor, 0);

        // 光标在末尾，delete 无事发生
        i.end();
        i.delete();
        assert_eq!(i.buffer, "bc");
    }
}

#[cfg(test)]
mod regression {
    use super::*;
    use agent_core::ToolOutcome;

    fn call(id: &str, name: &str, args: &str) -> agent_core::ToolCall {
        agent_core::ToolCall::new(id, name, serde_json::from_str(args).unwrap())
    }

    /// 回归：工具**失败**时以前不发 ToolCallFinished，界面上那行
    /// "执行中…"永远转不完。工具报错在实践中非常常见（路径写错、
    /// 参数缺失、被沙箱拒绝），所以每错一次就留一条幽灵条目。
    #[test]
    fn 工具失败后不留幽灵条目() {
        let mut a = App::new();
        a.on_agent_event(agent_core::AgentEvent::ToolCallStarted {
            call: call("c1", "read_file", "{}"),
        });
        assert!(a.is_tool_pending(0), "刚启动时应在执行中");

        a.on_agent_event(agent_core::AgentEvent::ToolCallFinished {
            call: call("c1", "read_file", "{}"),
            outcome: ToolOutcome::err("路径为空"),
            elapsed_ms: 3,
        });

        assert!(!a.is_tool_pending(0), "失败后必须脱离执行中状态");
        match &a.entries[0] {
            Entry::Tool { ok, summary, output, .. } => {
                assert!(!ok, "失败要标记成 !ok");
                assert_eq!(summary, "路径为空");
                assert!(output.is_some(), "失败也要有输出，好让用户看到原因");
            }
            other => panic!("应当是工具条目，实际 {other:?}"),
        }
    }

    /// 回归：以前回填是"找最后一个同名的待执行条目"。
    /// 同一轮两个 list_dir 时，结果会贴到错误的行上，
    /// 而且有一条永远停在"执行中…"。
    #[test]
    fn 同名并行调用按id各自回填() {
        let mut a = App::new();
        a.on_agent_event(agent_core::AgentEvent::ToolCallStarted {
            call: call("c1", "list_dir", r#"{"path":""}"#),
        });
        a.on_agent_event(agent_core::AgentEvent::ToolCallStarted {
            call: call("c2", "list_dir", r#"{"path":"src"}"#),
        });
        assert!(a.is_tool_pending(0) && a.is_tool_pending(1));

        // 故意让 c2 先完成
        a.on_agent_event(agent_core::AgentEvent::ToolCallFinished {
            call: call("c2", "list_dir", r#"{"path":"src"}"#),
            outcome: ToolOutcome::ok("src (3 项)", "a b c"),
            elapsed_ms: 1,
        });
        a.on_agent_event(agent_core::AgentEvent::ToolCallFinished {
            call: call("c1", "list_dir", r#"{"path":""}"#),
            outcome: ToolOutcome::ok("根 (4 项)", "x y z w"),
            elapsed_ms: 2,
        });

        // 各自落在正确的行上
        let s0 = match &a.entries[0] { Entry::Tool { summary, .. } => summary.clone(), _ => panic!() };
        let s1 = match &a.entries[1] { Entry::Tool { summary, .. } => summary.clone(), _ => panic!() };
        assert_eq!(s0, "根 (4 项)", "第 0 行应是 c1 的结果");
        assert_eq!(s1, "src (3 项)", "第 1 行应是 c2 的结果");
        assert!(!a.is_tool_pending(0) && !a.is_tool_pending(1));
    }

    /// 回归：上下文占用以前只在启动时算一次，整个会话里钉在 0%，
    /// 而旁边却显示着真实的 token 用量——自相矛盾。
    #[test]
    fn 上下文占用随轮次更新() {
        let mut a = App::new();
        a.status.context_window = 200_000;
        assert_eq!(a.status.used_tokens, 0);

        a.on_agent_event(agent_core::AgentEvent::TurnEnd {
            text: "好".into(),
            usage: Usage {
                input_tokens: 1_200,
                output_tokens: 300,
                cache_read_tokens: 4_000,
                cache_write_tokens: 0,
            },
            stop_reason: StopReason::EndTurn,
            had_tool_calls: false,
            elapsed_ms: 10,
            step: 1,
        });

        // 提示词规模 = 输入 + 缓存读，才是真正占掉的窗口
        assert_eq!(a.status.used_tokens, 5_200);
        assert!((a.status.used_ratio() - 0.026).abs() < 0.001);
    }
}
