//! `/` 命令的实现。
//!
//! 独立成模块有两个原因：
//! - 命令要同时改 `App`（界面）和 `Runtime`（模型/会话），放在 `main.rs` 里
//!   会让那个文件继续膨胀
//! - 关键约束——**没实现的命令绝不能漏给模型**——集中在这里，好审计
//!
//! 契约：`dispatch` 返回 [`Outcome::Pass`] 的唯一情况是"这不是命令"。
//! 只要以 `/` 开头，就一定会被消费掉：要么执行，要么报错。

use prompt::Command;

use crate::app::{App, Entry, NoticeLevel};
use crate::runtime::Runtime;

/// 命令执行结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 不是命令，交给模型
    Pass,
    /// 已处理
    Done,
    /// 退出
    Quit,
}

/// 处理一条输入。
///
/// 契约：
/// - 不以 `/` 开头 → [`Outcome::Pass`]，交给模型
/// - 以 `/` 开头 → **一定会被消费掉**。要么执行，要么报错。
///   绝不会漏给模型——否则用户会看到模型一本正经地回答
///   「关于 `/model claude-4` 这个指令……」
pub async fn dispatch(app: &mut App, rt: &mut Runtime, text: &str) -> Outcome {
    let trimmed = text.trim();

    if !looks_like_command(trimmed) {
        return Outcome::Pass;
    }

    // 未知命令：明确报错，而不是把它当提示词发给模型
    let Some(cmd) = Command::parse(trimmed) else {
        let name = trimmed.split_whitespace().next().unwrap_or(trimmed);
        notice(
            app,
            format!("未知命令 `{name}`。\n\n{}", prompt::help_text()),
            NoticeLevel::Warn,
        );
        return Outcome::Done;
    };

    // 参数（命令名之后的部分）
    let arg = trimmed
        .strip_prefix('/')
        .and_then(|rest| rest.split_once(char::is_whitespace))
        .map(|(_, a)| a.trim().to_string())
        .unwrap_or_default();

    match cmd {
        Command::Help => {
            notice(app, prompt::help_text(), NoticeLevel::Info);
            Outcome::Done
        }
        Command::Exit => Outcome::Quit,
        Command::Status => {
            notice(app, status_text(app), NoticeLevel::Info);
            Outcome::Done
        }
        Command::Model => {
            model(app, rt, &arg).await;
            Outcome::Done
        }
        Command::Provider => {
            provider(app, rt, &arg);
            Outcome::Done
        }
        Command::Clear => {
            clear(app, rt).await;
            Outcome::Done
        }
        Command::Compact => {
            compact(app, rt).await;
            Outcome::Done
        }
        Command::Init => {
            init(app, rt);
            Outcome::Done
        }
    }
}

fn notice(app: &mut App, text: impl Into<String>, level: NoticeLevel) {
    app.entries.push(Entry::Notice {
        text: text.into(),
        level,
    });
    app.scroll_to_bottom();
}

// ------------------------------------------------------------------ /model

async fn model(app: &mut App, rt: &mut Runtime, arg: &str) {
    if arg.is_empty() {
        let mut out = format!("当前模型: {}  (provider {})\n\n可用模型:\n", rt.model, rt.provider_name);
        for m in rt.models() {
            let mark = if m.id == rt.model { " ●" } else { "  " };
            let ctx = m.capabilities.context_window;
            out.push_str(&format!(
                "{mark} {:<28} {:>9} tokens{}\n",
                m.id,
                ctx,
                if m.capabilities.tools { " · 工具" } else { "" }
            ));
        }
        out.push_str(&format!("\n切换: /model <id>"));
        notice(app, out, NoticeLevel::Info);
        return;
    }

    let result = rt.switch_model(arg).map(str::to_string);
    match result {
        Ok(m) => {
            let window = rt.budget.context_window;
            app.status.model = m.clone();
            app.status.context_window = window;
            notice(
                app,
                format!("已切到模型 {m}，上下文窗口 {window}"),
                NoticeLevel::Info,
            );
        }
        Err(e) => notice(app, format!("切换失败: {e}"), NoticeLevel::Error),
    }
}

// -------------------------------------------------------------- /provider

fn provider(app: &mut App, rt: &mut Runtime, arg: &str) {
    if arg.is_empty() {
        let names = rt.provider_names();
        let mut out = format!("当前 provider: {}\n\n配置里可用的:\n", rt.provider_name);
        if names.is_empty() {
            out.push_str("  （没有。在 ~/.config/comping/config.toml 加一段 [providers.<名字>]）");
        } else {
            for n in &names {
                let mark = if *n == rt.provider_name { " ●" } else { "  " };
                out.push_str(&format!("{mark} {n}\n"));
            }
            out.push_str("\n切换: /provider <名字>");
        }
        notice(app, out, NoticeLevel::Info);
        return;
    }

    if arg == rt.provider_name {
        notice(app, format!("已经在用 {arg} 了"), NoticeLevel::Info);
        return;
    }

    let old = (rt.provider_name.clone(), rt.model.clone());
    match rt.switch_provider(arg) {
        Ok(()) => {
            app.status.provider = rt.provider_name.clone();
            app.status.model = rt.model.clone();
            app.status.context_window = rt.budget.context_window;
            notice(
                app,
                format!(
                    "已切到 {} / {}，上下文窗口 {}",
                    rt.provider_name, rt.model, rt.budget.context_window
                ),
                NoticeLevel::Info,
            );
        }
        Err(e) => {
            // switch_provider 失败时不改状态，这里只是保险
            rt.provider_name = old.0;
            rt.model = old.1;
            notice(app, format!("切换失败: {e:#}"), NoticeLevel::Error);
        }
    }
}

// ----------------------------------------------------------------- /clear

async fn clear(app: &mut App, rt: &mut Runtime) {
    if let Err(e) = rt.reset_history(None).await {
        notice(app, format!("清空失败: {e}"), NoticeLevel::Error);
        return;
    }
    app.entries.clear();
    app.streaming.clear();
    app.status.used_tokens = 0;
    app.status.total_usage = agent_core::Usage::default();
    app.status.step = 0;
    app.status.last_run = None;
    app.last_error = None;
    notice(
        app,
        format!("已清空，开始新会话（{}）", rt.session.path().display()),
        NoticeLevel::Info,
    );
}

// --------------------------------------------------------------- /compact

async fn compact(app: &mut App, rt: &mut Runtime) {
    // 先记住旧会话 id：compact 会另起新文件，之后就拿不到了
    let old_id = rt.session.meta().id.clone();
    let before_entries = app.entries.len();

    match rt.compact().await {
        Ok(r) => {
            app.status.used_tokens = r.after;
            let how = if r.used_llm {
                "模型摘要"
            } else {
                "确定性截断（摘要调用失败）"
            };
            // 历史被重写了，界面上的旧条目已与运行时脱节，收敛成一条说明
            app.entries.truncate(before_entries);
            notice(
                app,
                format!(
                    "上下文已压缩\n{} → {} tokens（省 {}）\n折叠 {} 条，保留 {} 条 · 方式: {}\n\
                     完整记录仍在旧会话里：comping -r {old_id}",
                    r.before,
                    r.after,
                    r.before.saturating_sub(r.after),
                    r.folded,
                    r.kept,
                    how,
                ),
                NoticeLevel::Info,
            );
        }
        Err(e) => notice(app, format!("无需压缩: {e}"), NoticeLevel::Info),
    }
}

// ------------------------------------------------------------------ /init

fn init(app: &mut App, rt: &mut Runtime) {
    let path = rt.workspace.root().join(".agent").join("config.toml");
    if path.exists() {
        notice(
            app,
            format!("{} 已存在，没有覆盖。", path.display()),
            NoticeLevel::Warn,
        );
        return;
    }

    match std::fs::create_dir_all(path.parent().expect("至少有一层父目录"))
        .and_then(|_| std::fs::write(&path, prompt::config_template()))
    {
        Ok(()) => notice(
            app,
            format!("已写入 {}\n\n接下来编辑它，填上你的 provider 与模型。", path.display()),
            NoticeLevel::Info,
        ),
        Err(e) => notice(app, format!("写入 {} 失败: {e}", path.display()), NoticeLevel::Error),
    }
}

// ---------------------------------------------------------------- /status

fn status_text(app: &App) -> String {
    let s = &app.status;
    let mut out = format!(
        "工作区   {}\n会话     {} 轮 / 上限 {}\n上下文   {}/{} ({:.0}%)\n模型     {} · {}\n用量     输入 {} · 输出 {} · 缓存读 {}",
        app.workspace,
        s.step,
        s.max_steps,
        s.used_tokens,
        s.context_window,
        s.used_ratio() * 100.0,
        s.provider,
        s.model,
        s.total_usage.input_tokens,
        s.total_usage.output_tokens,
        s.total_usage.cache_read_tokens,
    );
    if let Some(r) = &s.last_run {
        out.push_str(&format!("\n上次运行 {}", agent_core::describe_stop(r)));
    }
    if let Some(e) = &app.last_error {
        out.push_str(&format!("\n最后错误 {e}"));
    }
    out
}

/// 命令是否以 `/` 开头。用于快速路径判定。
pub fn looks_like_command(text: &str) -> bool {
    text.trim_start().starts_with('/')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Entry;

    /// 一个不连任何 provider 的 Runtime，专门测命令分支。
    ///
    /// 会话文件落在临时目录里——`Session` 需要真实路径，
    /// 造个假的反而测不到真东西。
    async fn fake_runtime() -> Runtime {
        use std::sync::Arc;
        let dir = std::env::temp_dir().join(format!("comping-cmd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let session = session::Session::create(&dir, "fake", "m1").await.unwrap();

        Runtime {
            provider: Arc::new(NoopProvider),
            provider_name: "fake".into(),
            model: "m1".into(),
            registry: agent_core::Registry::new(),
            workspace: tools::Workspace::new(&dir),
            budget: agent_core::ContextBudget {
                keep_recent: 4,
                ..Default::default()
            },
            agent_config: agent_core::AgentConfig::default(),
            session,
            conversation: agent_core::Conversation::default(),
            system: String::new(),
            headless: false,
            config: session::Config::default(),
        }
    }

    struct NoopProvider;
    #[async_trait::async_trait]
    impl agent_core::Provider for NoopProvider {
        fn name(&self) -> &str {
            "noop"
        }
        fn default_model(&self) -> &str {
            "m1"
        }
        fn models(&self) -> &[agent_core::ModelInfo] {
            &[]
        }
        async fn stream(
            &self,
            _r: agent_core::ChatRequest,
            _m: &str,
            _tx: tokio::sync::mpsc::UnboundedSender<String>,
        ) -> anyhow::Result<agent_core::ChatResponse> {
            anyhow::bail!("NoopProvider 不该被调用")
        }
    }

    fn notices(app: &App) -> Vec<String> {
        app.entries
            .iter()
            .filter_map(|e| match e {
                Entry::Notice { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn 识别命令前缀() {
        assert!(looks_like_command("/help"));
        assert!(looks_like_command("  /help"));
        assert!(!looks_like_command("帮我看看 /etc/passwd"));
        assert!(!looks_like_command("斜杠 / 不是命令"));
        assert!(!looks_like_command(""));
    }

    #[test]
    fn 参数切分() {
        let arg_of = |s: &str| {
            s.trim()
                .strip_prefix('/')
                .and_then(|r| r.split_once(char::is_whitespace))
                .map(|(_, a)| a.trim().to_string())
                .unwrap_or_default()
        };
        assert_eq!(arg_of("/model"), "");
        assert_eq!(arg_of("/model gpt-4.1"), "gpt-4.1");
        assert_eq!(arg_of("/model  claude-sonnet-4-5  "), "claude-sonnet-4-5");
    }

    /// 核心契约：任何 `/` 开头的输入都不能变成 Pass。
    #[tokio::test]
    async fn 斜杠开头绝不会被漏给模型() {
        let mut rt = fake_runtime().await;
        for name in Command::ALL {
            let text = name.name().to_string();
            let mut app = App::new();
            let out = dispatch(&mut app, &mut rt, &text).await;
            assert_ne!(
                out,
                Outcome::Pass,
                "`{text}` 被漏给了模型，这是要修的 bug"
            );
        }
    }

    #[tokio::test]
    async fn 未知命令会报错而不是发给模型() {
        let mut rt = fake_runtime().await;
        let mut app = App::new();
        let out = dispatch(&mut app, &mut rt, "/definitely-not-a-command").await;
        assert_eq!(out, Outcome::Done);
        let n = notices(&app);
        assert!(
            n.iter().any(|t| t.contains("未知命令")),
            "应当提示未知命令，实际: {n:?}"
        );
    }

    #[tokio::test]
    async fn 普通提问原样放行() {
        let mut rt = fake_runtime().await;
        let mut app = App::new();
        for text in ["帮我修个 bug", "读一下 /etc/hosts", "a/b/c", ""] {
            let out = dispatch(&mut app, &mut rt, text).await;
            assert_eq!(out, Outcome::Pass, "`{text}` 不该被当成命令");
        }
        assert!(notices(&app).is_empty());
    }

    #[tokio::test]
    async fn 切模型会更新状态() {
        let mut rt = fake_runtime().await;
        let mut app = App::new();
        dispatch(&mut app, &mut rt, "/model brand-new").await;
        assert_eq!(rt.model, "brand-new");
        assert!(notices(&app).iter().any(|t| t.contains("brand-new")));
    }

    #[tokio::test]
    async fn 空模型名被拒绝且不改状态() {
        let mut rt = fake_runtime().await;
        let before = rt.model.clone();
        let mut app = App::new();
        dispatch(&mut app, &mut rt, "/model").await;
        // 无参数时是"列出模型"，不是切换
        assert_eq!(rt.model, before);
        assert!(notices(&app).iter().any(|t| t.contains("可用模型")));
    }

    #[tokio::test]
    async fn 切到不存在的_provider_会报错且保留原状态() {
        let mut rt = fake_runtime().await;
        let (p, m) = (rt.provider_name.clone(), rt.model.clone());
        let mut app = App::new();
        dispatch(&mut app, &mut rt, "/provider nope").await;
        assert_eq!(rt.provider_name, p, "失败的切换不应改变状态");
        assert_eq!(rt.model, m);
        assert!(notices(&app).iter().any(|t| t.contains("切换失败")));
    }

    #[tokio::test]
    async fn 帮助与状态都能渲染出来() {
        let mut rt = fake_runtime().await;
        let mut app = App::new();
        app.workspace = "/some/dir".into();
        dispatch(&mut app, &mut rt, "/help").await;
        dispatch(&mut app, &mut rt, "/status").await;
        let n = notices(&app);
        assert!(n.iter().any(|t| t.contains("/compact")));
        assert!(n.iter().any(|t| t.contains("/some/dir")));
    }

    #[tokio::test]
    async fn 退出会话() {
        let mut rt = fake_runtime().await;
        let mut app = App::new();
        assert_eq!(dispatch(&mut app, &mut rt, "/exit").await, Outcome::Quit);
        assert_eq!(dispatch(&mut app, &mut rt, "/q").await, Outcome::Quit);
    }
}
