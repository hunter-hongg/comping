//! 配置加载：`~/.config/comping/config.toml` 与项目级 `.agent/config.toml`。
//!
//! 合并规则：项目级覆盖用户级，深层键逐一覆盖（不做整段替换）。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 顶层配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 默认 provider 名，对应 `[providers.<name>]`。
    pub provider: String,
    /// 默认模型 id。
    pub model: Option<String>,
    pub providers: Providers,
    pub agent: AgentSection,
    pub ui: UiSection,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // 默认值本身就要可用：不 normalize 也能直接跑
            provider: "anthropic".into(),
            model: None,
            providers: Providers::default(),
            agent: AgentSection::default(),
            ui: UiSection::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Providers {
    /// 协议名 → 配置。`anthropic` 永远指向自己这个 key。
    pub anthropic: Option<llm::ProviderConfig>,
    pub openai: Option<llm::ProviderConfig>,
    pub gemini: Option<llm::ProviderConfig>,
    /// 自定义 OpenAI 兼容端点，如 `[providers.deepseek]`。
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, llm::ProviderConfig>,
}

/// 项目级配置：所有可覆盖字段都是 `Option`，`None` 表示"文件里没写"。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Overrides {
    provider: Option<String>,
    model: Option<String>,
    providers: OverridesProviders,
    agent: OverridesAgent,
    ui: OverridesUi,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct OverridesProviders {
    anthropic: Option<llm::ProviderConfig>,
    openai: Option<llm::ProviderConfig>,
    gemini: Option<llm::ProviderConfig>,
    #[serde(flatten)]
    extra: std::collections::BTreeMap<String, llm::ProviderConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct OverridesAgent {
    max_steps: Option<usize>,
    tool_concurrency: Option<usize>,
    compact_at: Option<f32>,
    keep_recent: Option<usize>,
    auto_approve_tools: Option<Vec<String>>,
    disabled_tools: Option<Vec<String>>,
    bash_allowlist: Option<Vec<String>>,
    tool_timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct OverridesUi {
    theme: Option<String>,
    show_context_bar: Option<bool>,
    inline_streaming: Option<bool>,
}

/// 读一个 TOML 文件，不存在返回 None。
fn read_toml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("读取 {}", path.display()))?;
    let v = toml::from_str(&text).with_context(|| format!("解析 {}", path.display()))?;
    Ok(Some(v))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSection {
    /// 单次请求的最大 step 数。
    pub max_steps: usize,
    /// 工具并发上限。
    pub tool_concurrency: usize,
    /// 上下文压缩阈值占比。
    pub compact_at: f32,
    /// 触发压缩后保留的最近消息条数。
    pub keep_recent: usize,
    /// 永久免确认的工具名。
    pub auto_approve_tools: Vec<String>,
    /// 永久禁用的工具名。
    pub disabled_tools: Vec<String>,
    /// bash 免确认的命令首个词。
    pub bash_allowlist: Vec<String>,
    /// 单个工具的执行上限（秒）。0 表示不限时。
    ///
    /// 这是兜底：某个工具的实现挂死时，agent 循环至少不会永远卡住。
    pub tool_timeout_secs: u64,
}

impl Default for AgentSection {
    fn default() -> Self {
        Self {
            max_steps: 25,
            tool_concurrency: 4,
            compact_at: 0.85,
            keep_recent: 12,
            auto_approve_tools: Vec::new(),
            disabled_tools: Vec::new(),
            bash_allowlist: vec!["cargo".into(), "git".into(), "ls".into(), "cat".into()],
            tool_timeout_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSection {
    /// 主题名。
    pub theme: String,
    /// 是否显示上下文占用条。
    pub show_context_bar: bool,
    /// 流式输出是否原地更新。
    pub inline_streaming: bool,
}

impl Default for UiSection {
    fn default() -> Self {
        Self {
            theme: "default".into(),
            show_context_bar: true,
            inline_streaming: true,
        }
    }
}

impl Config {
    /// 用户级配置路径。
    pub fn user_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("comping")
            .join("config.toml")
    }

    /// 项目级配置路径。
    pub fn project_path(root: &Path) -> PathBuf {
        root.join(".agent").join("config.toml")
    }

    /// 按顺序加载并合并：内置默认 → 用户级 → 项目级。
    pub fn load(root: &Path) -> Result<Self> {
        let user_path = Self::user_path();
        let project_path = Self::project_path(root);

        // 用户级用完整 Config（拿到默认值），项目级用 Overrides（区分"没写"）
        let user: Config = read_toml(&user_path)?.unwrap_or_default();
        let project: Overrides = read_toml(&project_path)?.unwrap_or_default();

        let mut cfg = user.merge(project);
        cfg.normalize();
        Ok(cfg)
    }

    /// 用项目级覆盖用户级。
    ///
    /// 关键：只有"项目级文件里真正出现过这个键"才算覆盖。
    /// 用 `Option` 承载所有可覆盖字段来表达这一点，
    /// 而不是拿默认值当哨兵值——那样用户显式写 `provider = "anthropic"`
    /// 就压不掉用户级的 `openai` 了。
    fn merge(self, ov: Overrides) -> Self {
        Self {
            provider: ov.provider.unwrap_or(self.provider),
            model: ov.model.or(self.model),
            providers: Providers {
                anthropic: ov.providers.anthropic.or(self.providers.anthropic),
                openai: ov.providers.openai.or(self.providers.openai),
                gemini: ov.providers.gemini.or(self.providers.gemini),
                extra: {
                    let mut e = self.providers.extra;
                    e.extend(ov.providers.extra);
                    e
                },
            },
            agent: AgentSection {
                max_steps: ov.agent.max_steps.unwrap_or(self.agent.max_steps),
                tool_concurrency: ov
                    .agent
                    .tool_concurrency
                    .unwrap_or(self.agent.tool_concurrency),
                compact_at: ov.agent.compact_at.unwrap_or(self.agent.compact_at),
                keep_recent: ov.agent.keep_recent.unwrap_or(self.agent.keep_recent),
                auto_approve_tools: ov
                    .agent
                    .auto_approve_tools
                    .unwrap_or(self.agent.auto_approve_tools),
                disabled_tools: ov.agent.disabled_tools.unwrap_or(self.agent.disabled_tools),
                bash_allowlist: ov.agent.bash_allowlist.unwrap_or(self.agent.bash_allowlist),
                tool_timeout_secs: ov
                    .agent
                    .tool_timeout_secs
                    .unwrap_or(self.agent.tool_timeout_secs),
            },
            ui: UiSection {
                theme: ov.ui.theme.unwrap_or(self.ui.theme),
                show_context_bar: ov.ui.show_context_bar.unwrap_or(self.ui.show_context_bar),
                inline_streaming: ov.ui.inline_streaming.unwrap_or(self.ui.inline_streaming),
            },
        }
    }

    /// 修掉明显不合理的取值。
    pub fn normalize(&mut self) {
        self.agent.max_steps = self.agent.max_steps.clamp(1, 200);
        self.agent.tool_concurrency = self.agent.tool_concurrency.clamp(1, 32);
        self.agent.compact_at = self.agent.compact_at.clamp(0.1, 0.99);
        self.agent.keep_recent = self.agent.keep_recent.clamp(2, 200);
        // 上限给个天花板，免得有人写成 999999 小时
        self.agent.tool_timeout_secs = self.agent.tool_timeout_secs.min(86_400);
        if self.provider.trim().is_empty() {
            self.provider = "anthropic".into();
        }
    }

    /// 列出配置里所有 provider 的名字。
    pub fn provider_names(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.providers.anthropic.is_some() {
            v.push("anthropic".into());
        }
        if self.providers.openai.is_some() {
            v.push("openai".into());
        }
        if self.providers.gemini.is_some() {
            v.push("gemini".into());
        }
        v.extend(self.providers.extra.keys().cloned());
        v.sort();
        v
    }

    /// 取某个 provider 的配置。
    pub fn provider_config(&self, name: &str) -> Option<&llm::ProviderConfig> {
        match name {
            "anthropic" => self.providers.anthropic.as_ref(),
            "openai" => self.providers.openai.as_ref(),
            "gemini" => self.providers.gemini.as_ref(),
            other => self.providers.extra.get(other),
        }
    }
}

/// 项目级配置出现过的键才算"显式设置"——见 `Overrides`。
/// 这个类型本身不导出，它是 merge 的内部表示。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 项目级覆盖用户级() {
        let mut a = Config::default();
        a.provider = "openai".into();
        a.agent.max_steps = 10;

        // 项目级只写了 max_steps
        let ov: Overrides = toml::from_str("[agent]\nmax_steps = 30").unwrap();
        let m = a.merge(ov);

        assert_eq!(m.provider, "openai"); // 项目级没写 provider，保留用户级
        assert_eq!(m.agent.max_steps, 30);
    }

    #[test]
    fn 项目级可以显式改回默认_provider() {
        // 这是旧实现（拿默认值当哨兵）压不掉的场景：
        // 用户级是 openai，项目级显式写 anthropic，必须生效。
        let mut a = Config::default();
        a.provider = "openai".into();

        let ov: Overrides = toml::from_str(r#"provider = "anthropic""#).unwrap();
        let m = a.merge(ov);

        assert_eq!(m.provider, "anthropic");
    }

    #[test]
    fn 项目级可加自定义_provider() {
        let a = Config::default();
        let ov: Overrides = toml::from_str(
            r#"
provider = "deepseek"

[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
model = "deepseek-chat"
"#,
        )
        .unwrap();
        let m = a.merge(ov);

        assert_eq!(m.provider, "deepseek");
        let p = m
            .provider_config("deepseek")
            .expect("自定义 provider 应存在");
        assert_eq!(p.model.as_deref(), Some("deepseek-chat"));
        assert!(m.provider_names().contains(&"deepseek".to_string()));
    }

    #[test]
    fn 项目级空数组可以清空用户级列表() {
        let mut a = Config::default();
        a.agent.bash_allowlist = vec!["cargo".into()];

        let ov: Overrides = toml::from_str("[agent]\nbash_allowlist = []").unwrap();
        let m = a.merge(ov);

        assert!(m.agent.bash_allowlist.is_empty());
    }

    #[test]
    fn 未设置的键回落到默认() {
        let c = Config::default();
        assert_eq!(c.agent.max_steps, 25);
        assert_eq!(c.provider, "anthropic");
    }

    #[test]
    fn normalize_夹住越界值() {
        let mut c = Config::default();
        c.agent.max_steps = 9999;
        c.agent.compact_at = 5.0;
        c.normalize();
        assert_eq!(c.agent.max_steps, 200);
        assert_eq!(c.agent.compact_at, 0.99);
    }
}
