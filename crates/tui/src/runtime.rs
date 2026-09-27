//! 运行时组装：把配置、provider、工具、会话拼成一个可运行的 agent。

use std::path::PathBuf;
use std::sync::Arc;

use agent_core::{
    AgentConfig, ContextBudget, Conversation, ControlHandle, Provider, Registry, Risk,
    Runner, spawn_run,
};
use anyhow::{Context, Result, bail};
use session::{Config, Session};
use tools::Workspace;

use llm::ProviderConfig;

/// 一套组装好的运行环境。
pub struct Runtime {
    pub provider: Arc<dyn Provider>,
    pub provider_name: String,
    pub model: String,
    pub registry: Registry,
    pub workspace: Workspace,
    pub budget: ContextBudget,
    pub agent_config: AgentConfig,
    pub session: Session,
    pub conversation: Conversation,
    pub system: String,
    pub headless: bool,
    /// 留着 `/provider` 切换时重新读配置
    pub config: Config,
}

/// 命令行参数。
#[derive(Debug, Default)]
pub struct Args {
    /// 一次性提问后退出
    pub prompt: Option<String>,
    pub resume: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub list_sessions: bool,
    pub yes: bool,
    pub cwd: PathBuf,
    pub help: bool,
    pub version: bool,
}

pub const HELP: &str = r#"comping —— 终端里的编程助手

用法：
  comping [选项] [问题]

选项：
  -p, --prompt <文本>    提问后退出，不进入交互界面
  -r, --resume <id>      恢复指定会话（用 -l 查看列表）
  -l, --list-sessions    列出历史会话
      --provider <名字>   覆盖配置里的 provider
      --model <id>       覆盖配置里的模型
      --cwd <路径>        在指定目录工作，默认当前目录
  -y, --yes              所有工具免确认（谨慎使用）
  -V, --version          显示版本
  -h, --help             显示帮助

环境变量：
  ANTHROPIC_API_KEY / OPENAI_API_KEY / GEMINI_API_KEY
  都可以省掉配置文件里的 api_key。
"#;

/// 手写解析：两个 flag 不值得引入 clap。
pub fn parse_args() -> Result<Args> {
    let mut a = Args {
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        ..Default::default()
    };

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut need = |flag: &str| -> Result<String> {
            it.next().with_context(|| format!("{flag} 需要一个参数"))
        };
        match arg.as_str() {
            "-p" | "--prompt" => a.prompt = Some(need("-p")?),
            "-r" | "--resume" => a.resume = Some(need("-r")?),
            "--provider" => a.provider = Some(need("--provider")?),
            "--model" => a.model = Some(need("--model")?),
            "--cwd" => a.cwd = PathBuf::from(need("--cwd")?),
            "-l" | "--list-sessions" => a.list_sessions = true,
            "-y" | "--yes" => a.yes = true,
            "-h" | "--help" => a.help = true,
            "-V" | "--version" => a.version = true,
            other if other.starts_with('-') => {
                bail!("未知参数 `{other}`，用 --help 查看用法")
            }
            other => a.prompt = Some(other.to_string()),
        }
    }
    Ok(a)
}

/// 组装运行环境。
pub async fn build(args: &Args) -> Result<Runtime> {
    let cfg = Config::load(&args.cwd)?;

    let provider_name = args
        .provider
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| cfg.provider.clone());

    let mut pcfg: ProviderConfig = cfg.provider_config(&provider_name).cloned().with_context(|| {
        format!(
            "配置里没有 provider `{provider_name}`。\
             当前可用: {}。\
             在 ~/.config/comping/config.toml 里加一段 [providers.{provider_name}]。",
            if cfg.provider_names().is_empty() {
                "（无）".to_string()
            } else {
                cfg.provider_names().join(", ")
            }
        )
    })?;

    // 模型优先级：命令行 > 顶层 `model` > provider 段里的 `model`
    pcfg.model = args
        .model
        .clone()
        .or_else(|| cfg.model.clone())
        .or(pcfg.model);

    let provider: Arc<dyn Provider> = llm::build(&provider_name, &pcfg)?;
    let model = pcfg
        .model
        .clone()
        .unwrap_or_else(|| provider.default_model().to_string());

    // 工具与权限
    let workspace = Workspace::new(&args.cwd);
    let mut registry =
        tools::default_registry(workspace.clone(), cfg.agent.bash_allowlist.clone());
    if args.yes {
        for name in registry.names() {
            registry.set_risk(&name, Risk::Safe);
        }
    } else {
        tools::apply_allowlist(&mut registry, &cfg.agent.auto_approve_tools);
    }
    tools::apply_blocklist(&mut registry, &cfg.agent.disabled_tools);

    // 会话
    let dir = session::sessions_dir();
    let session = match &args.resume {
        Some(id) => Session::load(&dir.join(format!("{id}.jsonl"))).await?,
        None => Session::create(&dir, &provider_name, &model).await?,
    };

    let budget = ContextBudget {
        context_window: context_window_of(&provider, &model),
        compact_at: cfg.agent.compact_at,
        keep_recent: cfg.agent.keep_recent,
        ..Default::default()
    };
    let conversation = session.to_conversation(budget.clone());

    let system = prompt::system_prompt(&workspace.root().display().to_string(), &today());
    let agent_config = AgentConfig {
        max_steps: cfg.agent.max_steps,
        tool_concurrency: cfg.agent.tool_concurrency,
        auto_compact: true,
        tool_timeout: (cfg.agent.tool_timeout_secs > 0)
            .then(|| std::time::Duration::from_secs(cfg.agent.tool_timeout_secs)),
    };

    Ok(Runtime {
        provider,
        provider_name,
        model,
        registry,
        workspace,
        budget,
        agent_config,
        session,
        conversation,
        system,
        headless: args.prompt.is_some(),
        config: cfg,
    })
}

fn context_window_of(provider: &Arc<dyn Provider>, model: &str) -> u32 {
    provider
        .models()
        .iter()
        .find(|m| m.id == model)
        .map(|m| m.capabilities.context_window)
        .unwrap_or(128_000)
}

/// 启动一次运行。
pub fn start(rt: &Runtime, input: String) -> Runner {
    spawn_run(
        rt.provider.clone(),
        rt.registry.clone(),
        rt.conversation.clone(),
        rt.system.clone(),
        rt.agent_config.clone(),
        input,
        rt.model.clone(),
    )
}

/// 中断句柄。
pub fn control(runner: &Runner) -> ControlHandle {
    runner.control()
}

/// 列出历史会话。
pub async fn list_sessions() -> Result<Vec<session::SessionMeta>> {
    Ok(session::Session::list(&session::sessions_dir()).await?)
}

/// 把更新后的会话写回磁盘。
///
/// 靠"已写条数"做水位线，只追加新增部分。
/// 因此**重写历史的操作（`/clear`、`/compact`）必须另起一个会话文件**，
/// 否则水位线会错位、要么重复写要么漏写。
pub async fn persist(rt: &mut Runtime, conversation: Conversation) -> Result<()> {
    // 只追加新增的消息，避免重复写
    let old_len = rt.conversation.messages.len();
    let new_messages: Vec<_> = conversation
        .messages
        .iter()
        .skip(old_len)
        .cloned()
        .collect();

    for m in new_messages {
        rt.session.append(m).await?;
    }

    // 用量累加后落盘，跨会话也能看到花了多少
    let delta = conversation.total_usage.since(&rt.conversation.total_usage);
    if delta.total() > 0 {
        rt.session.record_usage(&delta).await?;
    }

    if let Some(last) = conversation
        .messages
        .iter()
        .rev()
        .find(|m| m.role == agent_core::Role::User)
    {
        let title: String = last.content.chars().take(40).collect();
        // 标题落盘失败不该让整轮对话失败
        if let Err(e) = rt.session.retitle(title).await {
            tracing::warn!(error = %e, "写会话标题失败");
        }
    }

    rt.conversation = conversation;
    Ok(())
}

// ==================================================== `/` 命令需要的操作

impl Runtime {
    /// 模型的友好名。侧栏优先显示它，原始 id 又长又带命名空间前缀。
    pub fn model_display_name(&self, model: &str) -> Option<String> {
        self.provider
            .models()
            .iter()
            .find(|m| m.id == model)
            .map(|m| m.display_name.clone())
            .filter(|d| !d.trim().is_empty() && d != model)
    }

    /// 切模型。只改字段，不重建 provider（重建会丢连接池）。
    pub fn switch_model(&mut self, model: &str) -> Result<&str> {
        let model = model.trim();
        if model.is_empty() {
            anyhow::bail!("模型名不能为空");
        }
        // 已知模型清单里没有就放行：本地端点常有配置外的模型
        self.model = model.to_string();
        self.budget.context_window = context_window_of(&self.provider, model);
        Ok(self.model.as_str())
    }

    /// 当前 provider 支持的模型。
    pub fn models(&self) -> &[agent_core::ModelInfo] {
        self.provider.models()
    }

    /// 配置里可用的 provider 名字。
    pub fn provider_names(&self) -> Vec<String> {
        self.config.provider_names()
    }

    /// 切 provider。需要重建实例，因此可能失败（比如缺 API key）。
    pub fn switch_provider(&mut self, name: &str) -> Result<()> {
        let name = name.trim();
        let pcfg = self.config.provider_config(name).cloned().with_context(|| {
            format!(
                "配置里没有 provider `{name}`。当前可用: {}。",
                if self.provider_names().is_empty() {
                    "（无）".to_string()
                } else {
                    self.provider_names().join(", ")
                }
            )
        })?;

        // 先建，成功了再替换——失败时运行时状态保持不变
        let provider: Arc<dyn Provider> = llm::build(name, &pcfg)?;

        let model = pcfg
            .model
            .clone()
            .or_else(|| self.config.model.clone())
            .unwrap_or_else(|| provider.default_model().to_string());

        self.provider = provider;
        self.provider_name = name.to_string();
        self.model = model.clone();
        self.budget.context_window = context_window_of(&self.provider, &model);
        self.config.provider = name.to_string();
        Ok(())
    }

    /// 另起一个会话文件，并把 `seed` 立刻写进去。
    ///
    /// `/clear` 与 `/compact` 都走这里：它们的共同点是**重写历史**，
    /// 而 `persist` 靠水位线增量追加，历史被重写后水位线就不再可信。
    /// 旧文件保留完整记录，想回看可以 `-r <id>`。
    pub async fn reset_history(&mut self, seed: Option<Conversation>) -> Result<()> {
        let dir = session::sessions_dir();
        let mut s = Session::create(&dir, &self.provider_name, &self.model).await?;

        let conv = seed.unwrap_or_else(|| Conversation::new(self.budget.clone()));
        for m in &conv.messages {
            s.append(m.clone()).await?;
        }

        self.session = s;
        self.conversation = conv;
        Ok(())
    }

    /// 压缩上下文。
    ///
    /// 先用一次 LLM 调用把早期对话总结掉——确定性截断虽然零成本，
    /// 但会把模型对"自己在干什么"的判断一起丢掉，压缩后往往接不上。
    /// LLM 调用失败时退回确定性截断，不让命令失败。
    pub async fn compact(&mut self) -> Result<CompactReport> {
        let Some(plan) = self.conversation.plan_compact() else {
            anyhow::bail!(
                "上下文还不需要压缩（{}/{} 条消息，阈值 {} 条）",
                self.conversation.messages.len(),
                self.conversation.messages.len(),
                self.budget.keep_recent
            );
        };

        let before = self.conversation.estimate_tokens();
        let summary = self.summarize(&plan.older).await;
        let used_llm = summary.is_some();

        let mut next = self.conversation.clone();
        next.apply_compact(&plan, summary.as_deref());
        let after = next.estimate_tokens();

        self.reset_history(Some(next)).await?;

        Ok(CompactReport {
            before,
            after,
            folded: plan.older.len(),
            kept: plan.keep.len(),
            used_llm,
        })
    }

    /// 让模型总结一段历史。返回 None 表示调用失败。
    async fn summarize(&self, older: &[agent_core::Message]) -> Option<String> {
        let transcript = older
            .iter()
            .map(|m| {
                let role = match m.role {
                    agent_core::Role::User => "用户",
                    agent_core::Role::Assistant => "助手",
                    agent_core::Role::Tool => "工具",
                    agent_core::Role::System => "系统",
                };
                let mut s = format!("{role}：{}", m.content.trim());
                for c in &m.tool_calls {
                    s.push_str(&format!("\n  [调用 {} {}]", c.name, c.arguments_json()));
                }
                s
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let req = agent_core::ChatRequest {
            system: Some(
                "你在压缩一段编程助手的对话历史。请写一份紧凑的摘要，\
                 覆盖：已完成的事、当前文件/代码状态、待办与约束、用户的明确要求。\
                 不要客套，不要复述原文，直接给结论。限 400 字以内。"
                    .into(),
            ),
            messages: vec![agent_core::Message::user(transcript)],
            tools: Vec::new(),
            max_tokens: Some(1024),
            temperature: Some(0.0),
            cache_hint: None,
        };

        // 增量直接丢掉：这里只要最终文本
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let resp = self.provider.stream(req, &self.model, tx).await.ok()?;
        while rx.try_recv().is_ok() {}

        let s = resp.content.trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    }
}

/// `/compact` 的结果，给 UI 显示。
#[derive(Debug, Clone)]
pub struct CompactReport {
    pub before: u32,
    pub after: u32,
    pub folded: usize,
    pub kept: usize,
    pub used_llm: bool,
}

/// 今天的日期。用 civil_from_days，不引入 chrono。
pub fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant 的 civil_from_days。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 日期换算正确() {
        // 2020-01-01 是第 18262 天
        assert_eq!(civil_from_days(18_262), (2020, 1, 1));
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn 解析参数() {
        let a = Args {
            prompt: Some("hi".into()),
            yes: true,
            ..Default::default()
        };
        assert_eq!(a.prompt.as_deref(), Some("hi"));
        assert!(a.yes);
    }
}
