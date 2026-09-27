//! `comping` —— 终端里的编程助手。
//!
//! 三个前端共用同一套运行时：
//! - `interactive`：全屏 TUI
//! - `headless`：管道友好，一次一问
//! - 未来：Web 前端只要消费 `AgentEvent` 即可

mod app;
mod commands;
mod markdown;
mod runtime;
mod theme;
mod tui;
mod ui;

use std::io::Write;

use agent_core::{AgentEvent, Decision};
use anyhow::Result;
use app::{App, Entry, Focus, NoticeLevel};
use runtime::{Args, Runtime};

fn main() -> Result<()> {
    // 日志走 stderr 的文件，不污染终端画面
    init_tracing();

    let args = match runtime::parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n");
            print!("{}", runtime::HELP);
            std::process::exit(2);
        }
    };

    if args.help {
        print!("{}", runtime::HELP);
        return Ok(());
    }
    if args.version {
        println!("comping {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run(args))
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let file = std::env::var("COMPING_LOG").ok().and_then(|p| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .ok()
    });
    match file {
        Some(f) => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(f)
                .try_init();
        }
        None => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .try_init();
        }
    }
}

async fn run(args: Args) -> Result<()> {
    if args.list_sessions {
        let list = runtime::list_sessions().await?;
        if list.is_empty() {
            println!("没有历史会话。");
        } else {
            for m in list {
                println!("{:<20} {}", m.id, m.title);
            }
        }
        return Ok(());
    }

    let mut rt = runtime::build(&args).await?;

    match args.prompt.clone() {
        Some(prompt) => headless(&mut rt, prompt).await,
        None => interactive(&mut rt).await,
    }
}

// ------------------------------------------------------------ headless

async fn headless(rt: &mut Runtime, prompt: String) -> Result<()> {
    // 管道里没法交互确认，headless 一律自动放行。
    for name in rt.registry.names() {
        rt.registry
            .set_risk(&name, agent_core::tools_exec::Risk::Safe);
    }

    let mut runner = runtime::start(rt, prompt);
    let events = runner.events();

    while let Some(ev) = events.recv().await {
        match ev {
            AgentEvent::ApprovalNeeded { respond, .. } => {
                let _ = respond.try_send(Decision::Allow);
            }
            AgentEvent::UserMessage { text } => {
                println!("\n\x1b[1;32m> {text}\x1b[0m");
            }
            AgentEvent::AssistantDelta { text } => {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            AgentEvent::ToolCallStarted { call } => {
                println!(
                    "\n\x1b[35m▸ {} {}\x1b[0m",
                    call.name,
                    compact(&call.arguments_json(), 70)
                );
            }
            AgentEvent::ToolCallFinished { outcome, .. } => {
                let color = if outcome.ok { "\x1b[32m✓" } else { "\x1b[31m✗" };
                println!("  {color} {}\x1b[0m", outcome.summary);
            }
            AgentEvent::Notice { text, .. } => {
                eprintln!("\x1b[33mℹ {text}\x1b[0m");
            }
            AgentEvent::Failed { error } => {
                eprintln!("\x1b[31m✗ {error}\x1b[0m");
            }
            AgentEvent::RunEnd { reason } => {
                println!("\n\x1b[2m{}\x1b[0m", agent_core::describe_stop(&reason));
                break;
            }
            _ => {}
        }
    }

    if let Some(conv) = wait_for_conversation(&mut runner).await {
        runtime::persist(rt, conv).await?;
    }
    println!(
        "\x1b[2m会话已保存到 {}\x1b[0m",
        rt.session.path().display()
    );
    Ok(())
}

async fn wait_for_conversation(
    runner: &mut agent_core::Runner,
) -> Option<agent_core::Conversation> {
    for _ in 0..100 {
        if let Some(c) = runner.try_take_conversation() {
            return Some(c);
        }
        if runner.is_finished() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            return runner.try_take_conversation();
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    None
}

// ----------------------------------------------------------- interactive

async fn interactive(rt: &mut Runtime) -> Result<()> {
    let mut app = App::new();
    app.status.provider = rt.provider_name.clone();
    app.status.model = rt.model.clone();
    app.status.model_display = rt.model_display_name(&rt.model);
    app.status.context_window = rt.budget.context_window;
    app.status.max_steps = rt.agent_config.max_steps;
    app.status.total_usage = rt.conversation.total_usage;
    app.status.used_tokens = rt.conversation.estimate_tokens();
    app.workspace = rt.workspace.root().display().to_string();
    app.headless = rt.headless;

    // 把恢复出来的历史渲染成条目
    for m in &rt.conversation.messages {
        match m.role {
            agent_core::Role::User => app.entries.push(Entry::User {
                text: m.content.clone(),
            }),
            agent_core::Role::Assistant if !m.content.trim().is_empty() => {
                app.entries.push(Entry::Assistant {
                    text: m.content.clone(),
                    usage: agent_core::Usage::default(),
                    stop: agent_core::StopReason::EndTurn,
                    elapsed_ms: 0,
                })
            }
            _ => {}
        }
    }

    let mut terminal = ratatui::init();
    let result = tui_loop(&mut terminal, &mut app, rt).await;
    ratatui::restore();
    result
}

async fn tui_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rt: &mut Runtime,
) -> Result<()> {
    let mut tui = tui::Tui::new(terminal);
    let mut runner: Option<agent_core::Runner> = None;
    let mut dirty = true;

    // 存下每次运行结束后的会话，等循环空闲时落盘
    let mut pending: Option<agent_core::Conversation> = None;

    while app.running {
        // 1. 收 agent 事件
        if let Some(r) = runner.as_mut() {
            let mut got = false;
            while let Ok(ev) = r.events().try_recv() {
                app.on_agent_event(ev);
                got = true;
            }
            if got {
                dirty = true;
            }
            // 运行结束：取回会话
            if r.is_finished() {
                if let Some(conv) = r.try_take_conversation() {
                    pending = Some(conv);
                }
                app.busy = false;
                dirty = true;
            }
        }

        // 2. 空闲时落盘
        if !app.busy {
            if let Some(conv) = pending.take() {
                if let Err(e) = runtime::persist(rt, conv).await {
                    app.last_error = Some(format!("写会话失败: {e}"));
                }
                dirty = true;
            }
        }

        // 3. 绘制
        if dirty {
            tui.draw(app)?;
            tui.autoresize()?;
            dirty = false;
        }

        // 4. 等事件
        let timeout = if runner.is_some() {
            std::time::Duration::from_millis(30)
        } else {
            std::time::Duration::from_millis(120)
        };

        if !crossterm::event::poll(timeout)? {
            continue;
        }

        match crossterm::event::read()? {
            crossterm::event::Event::Key(key)
                if key.kind != crossterm::event::KeyEventKind::Release =>
            {
                match handle_key(app, key) {
                    Action::None => {}
                    Action::Interrupt => {
                        if let Some(r) = &runner {
                            runtime::control(r).interrupt();
                        }
                    }
                    Action::Submit(text) => match commands::dispatch(app, rt, &text).await {
                        commands::Outcome::Pass => {
                            app.entries.push(Entry::User { text: text.clone() });
                            runner = Some(runtime::start(rt, text));
                            app.busy = true;
                            app.scroll_to_bottom();
                        }
                        commands::Outcome::Done => {
                            // `/clear` 等命令要刷新侧栏的计数
                            sync_status(app, rt);
                        }
                        commands::Outcome::Quit => app.running = false,
                    },
                    Action::Quit => app.running = false,
                }
                dirty = true;
            }
            crossterm::event::Event::Resize(_, _) => {
                tui.autoresize()?;
                dirty = true;
            }
            crossterm::event::Event::Paste(text) => {
                for c in text.chars() {
                    if c != '\n' && c != '\r' {
                        app.input.push_char(c);
                    }
                }
                dirty = true;
            }
            _ => {}
        }
    }

    // 退出前先中断：正在跑的工具会停，待批准的按拒绝处理，
    // 否则 runner 里的 future 悬着，task 永远 join 不完
    if let Some(r) = runner.as_ref() {
        runtime::control(r).interrupt();
    }
    app.drop_pending_approvals();

    // 退出前把最后的会话存下来
    if let Some(conv) = pending.take() {
        let _ = runtime::persist(rt, conv).await;
    }
    if let Some(r) = runner.as_ref() {
        if let Some(conv) = r.try_take_conversation() {
            let _ = runtime::persist(rt, conv).await;
        }
    }
    Ok(())
}

enum Action {
    None,
    /// 用户提交了一行文本，交给上层判断是命令还是提问
    Submit(String),
    Interrupt,
    Quit,
}

/// 把运行时的关键计数同步到侧栏。
fn sync_status(app: &mut App, rt: &runtime::Runtime) {
    app.status.provider = rt.provider_name.clone();
    app.status.model = rt.model.clone();
    app.status.model_display = rt.model_display_name(&rt.model);
    app.status.context_window = rt.budget.context_window;
    app.status.used_tokens = rt.conversation.estimate_tokens();
}

fn handle_key(app: &mut App, key: crossterm::event::KeyEvent) -> Action {
    use crossterm::event::{KeyCode, KeyModifiers};

    // 批准面板吃掉一切
    if app.has_approval() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.respond_approval(Decision::Allow)
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                app.respond_approval(Decision::AllowSession)
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                app.respond_approval(Decision::Deny)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(a) = app.approvals.front_mut() {
                    a.scroll = a.scroll.saturating_add(1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(a) = app.approvals.front_mut() {
                    a.scroll = a.scroll.saturating_sub(1);
                }
            }
            _ => {}
        }
        return Action::None;
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    if ctrl {
        match key.code {
            KeyCode::Char('c') => return Action::Quit,
            KeyCode::Char('j') | KeyCode::Down => {
                let (_, total, _) = app.last_scroll;
                app.scroll_by(2, total);
                return Action::None;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let (_, total, _) = app.last_scroll;
                app.scroll_by(-2, total);
                return Action::None;
            }
            KeyCode::Char('d') => {
                app.input.end();
                return Action::None;
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Esc => {
            if app.busy {
                return Action::Interrupt;
            } else if !app.input.is_empty() {
                app.input.set("");
            } else {
                return Action::Quit;
            }
        }
        // 对话区焦点下，Enter 用来展开工具输出
        KeyCode::Enter if app.focus == Focus::Conversation && app.input.is_empty() => {
            if let Some(i) = app.tool_cursor {
                app.toggle_expand(i);
            }
        }
        KeyCode::Enter if shift => app.input.push_char('\n'),
        KeyCode::Enter => {
            if app.busy {
                app.entries.push(Entry::Notice {
                    text: "当前请求还在跑，按 Esc 可中断。".into(),
                    level: NoticeLevel::Warn,
                });
            } else if let Some(text) = app.input.submit() {
                return Action::Submit(text);
            }
        }
        KeyCode::Backspace => app.input.backspace(),
        KeyCode::Delete => app.input.delete(),
        KeyCode::Left => app.input.left(),
        KeyCode::Right => app.input.right(),
        KeyCode::Home => app.input.home(),
        KeyCode::End => app.input.end(),
        KeyCode::Up => {
            match app.focus {
                Focus::Conversation => {
                    app.move_tool_cursor(-1);
                }
                _ => {
                    if app.input.is_empty() {
                        let (_, total, _) = app.last_scroll;
                        app.scroll_by(-1, total);
                    } else {
                        recall_history(app, true);
                    }
                }
            }
        }
        KeyCode::Down => {
            match app.focus {
                Focus::Conversation => {
                    app.move_tool_cursor(1);
                }
                _ => {
                    if app.input.is_empty() {
                        let (_, total, _) = app.last_scroll;
                        app.scroll_by(1, total);
                    } else {
                        recall_history(app, false);
                    }
                }
            }
        }
        KeyCode::Tab => {
            app.focus = match app.focus {
                Focus::Input => Focus::Conversation,
                Focus::Conversation => Focus::Input,
                Focus::Sidebar => Focus::Input,
            };
        }
        KeyCode::Char(' ') if app.focus == Focus::Conversation => {
            if let Some(i) = app.tool_cursor {
                app.toggle_expand(i);
            }
        }
        KeyCode::PageUp => {
            let (_, total, _) = app.last_scroll;
            app.scroll_by(-10, total);
        }
        KeyCode::PageDown => {
            let (_, total, _) = app.last_scroll;
            app.scroll_by(10, total);
        }
        KeyCode::Char(c) => app.input.push_char(c),
        _ => {}
    }

    Action::None
}

/// ↑↓ 翻历史输入。
fn recall_history(app: &mut App, up: bool) {
    let h = &app.input.history;
    if h.is_empty() {
        return;
    }
    let next = match (app.input.history_idx, up) {
        // 从没翻过：向上跳到最后一条
        (None, true) => Some(h.len() - 1),
        (None, false) => None,
        (Some(i), true) => i.checked_sub(1),
        (Some(i), false) => {
            if i + 1 >= h.len() {
                None
            } else {
                Some(i + 1)
            }
        }
    };
    app.input.history_idx = next;
    let text = next.map(|i| h[i].clone()).unwrap_or_default();
    app.input.set(&text);
}

fn compact(s: &str, max: usize) -> String {
    let one = s.replace('\n', " ");
    if one.chars().count() <= max {
        one
    } else {
        let t: String = one.chars().take(max).collect();
        format!("{t}…")
    }
}
