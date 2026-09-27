//! `prompt` —— 系统提示词与内置 slash 命令。
//!
//! 提示词集中在这里而不是散落在代码里，方便迭代措辞而不动逻辑。

use std::fmt::Write;

/// 系统提示词。会拼上工作区信息与当前日期。
pub fn system_prompt(workspace_root: &str, today: &str) -> String {
    let mut s = String::new();

    s.push_str(
        "你是一个运行在终端里的编程助手。你可以直接读写工作区文件、执行命令，\
         通过工具来完成任务，而不是只给建议。\n\n",
    );

    let _ = write!(
        s,
        "## 环境\n- 工作区根目录: {workspace_root}\n- 今天: {today}\n\n"
    );

    s.push_str(
        "## 工作方式\n\
         1. 动手前先看清现状：用 list_dir 了解结构，用 grep 定位，用 read_file 读具体内容。\
            不要凭猜测修改文件。\n\
         2. 修改尽量用 edit_file 做局部替换，它会校验匹配唯一性；只有新建文件或整体重写才用 write_file。\n\
         3. 改完代码用 bash 跑构建或测试验证，把失败信息读懂再改，不要盲目重试。\n\
         4. 一轮里可以并行发起多个互不依赖的工具调用来提速。\n\n",
    );

    s.push_str(
        "## 边界\n\
         - 只能操作工作区内的路径，工具会拒绝越界访问。\n\
         - 破坏性操作（删除大量文件、force push、reset --hard）执行前必须先向用户说明并等待确认。\n\
         - 不要修改与任务无关的文件。\n\
         - 工具报错时先读懂错误信息，多半是参数或文件状态问题，不是让你原样重试。\n\n",
    );

    s.push_str(
        "## 表达\n\
         - 用中文回答，除非用户用其他语言提问。\n\
         - 简洁直接：做了什么、结果如何、下一步是什么。\n\
         - 代码和命令放代码块，路径用行内代码标注。\n\
         - 不确定就说不确定，不要编造文件内容或运行结果。\n",
    );

    s
}

/// 一个 slash 命令的元信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Help,
    Model,
    Provider,
    Clear,
    Compact,
    Init,
    Status,
    Exit,
}

impl Command {
    /// 解析用户输入的命令名。
    pub fn parse(input: &str) -> Option<Self> {
        let name = input.trim().strip_prefix('/')?.split_whitespace().next()?;
        let cmd = match name {
            "help" | "h" | "?" => Self::Help,
            "model" | "m" => Self::Model,
            "provider" | "p" => Self::Provider,
            "clear" | "c" => Self::Clear,
            "compact" => Self::Compact,
            "init" => Self::Init,
            "status" | "s" => Self::Status,
            "exit" | "quit" | "q" => Self::Exit,
            _ => return None,
        };
        Some(cmd)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Help => "/help",
            Self::Model => "/model",
            Self::Provider => "/provider",
            Self::Clear => "/clear",
            Self::Compact => "/compact",
            Self::Init => "/init",
            Self::Status => "/status",
            Self::Exit => "/exit",
        }
    }

    /// 一行说明。
    pub fn usage(self) -> &'static str {
        match self {
            Self::Help => "显示可用命令",
            Self::Model => "查看或切换模型，如 /model claude-sonnet-4-5",
            Self::Provider => "查看或切换 provider，如 /provider anthropic",
            Self::Clear => "清空当前会话历史，重新开始（旧记录仍可用 -r 找回）",
            Self::Compact => "把早期对话压缩成摘要，腾出上下文",
            Self::Init => "在项目里生成 .agent/config.toml",
            Self::Status => "显示会话、用量与工具状态",
            Self::Exit => "退出会话",
        }
    }

    pub const ALL: &'static [Command] = &[
        Self::Help,
        Self::Model,
        Self::Provider,
        Self::Clear,
        Self::Compact,
        Self::Init,
        Self::Status,
        Self::Exit,
    ];
}

/// `/help` 的输出。
pub fn help_text() -> String {
    let mut s = String::from("可用命令\n\n");
    for c in Command::ALL {
        let _ = writeln!(s, "  {:<12} {}", c.name(), c.usage());
    }
    s.push_str("\n快捷键\n\n");
    s.push_str("  Ctrl-C / Esc   中断当前请求，再按一次退出\n");
    s.push_str("  Enter          发送　Shift-Enter 换行\n");
    s.push_str("  Tab            在输入框与对话区之间切换焦点\n");
    s.push_str("  ↑ / ↓          滚动对话（焦点在对话区时选中工具条目）\n");
    s.push_str("  空格 / Enter   展开选中的工具输出\n");
    s.push_str("  y / n / a      批准面板：允许一次 / 拒绝 / 本轮全部允许\n");
    s.push_str("\n以 / 开头的一律是命令，不会发给模型。\n");
    s
}

/// `/init` 生成的配置模板。
pub fn config_template() -> String {
    r#"# comping 项目级配置
# 放在 .agent/config.toml，会覆盖 ~/.config/comping/config.toml

provider = "anthropic"
# model = "claude-sonnet-4-5"

# 自定义 OpenAI 兼容端点（DeepSeek / vLLM / Ollama / OpenRouter 通用）
# [providers.deepseek]
# kind = "openai"
# base_url = "https://api.deepseek.com/v1"
# api_key = "sk-..."
# model = "deepseek-chat"

# [providers.local]
# kind = "openai"
# base_url = "http://localhost:11434/v1"
# supports_usage = false
# model = "qwen2.5-coder"

[agent]
max_steps = 25
tool_concurrency = 4
tool_timeout_secs = 300     # 单个工具超时兜底，0 = 不限
compact_at = 0.85
keep_recent = 12

# 永久免确认的工具
auto_approve_tools = []

# 永久禁用的工具
disabled_tools = []

# bash 免确认的命令首个词
bash_allowlist = ["cargo", "git", "ls", "cat"]

[ui]
theme = "default"
show_context_bar = true
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析斜杠命令() {
        assert_eq!(Command::parse("/help"), Some(Command::Help));
        assert_eq!(Command::parse("/model gpt-4.1"), Some(Command::Model));
        assert_eq!(Command::parse("/quit"), Some(Command::Exit));
        assert_eq!(Command::parse("不是命令"), None);
        assert_eq!(Command::parse("/unknown"), None);
    }

    #[test]
    fn 帮助里覆盖所有命令() {
        let text = help_text();
        for c in Command::ALL {
            assert!(text.contains(c.name()), "帮助缺少 {}", c.name());
        }
    }
}
