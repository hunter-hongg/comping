//! 会话状态：消息历史 + token 预算 + 压缩。

use serde::{Deserialize, Serialize};

use crate::message::{ChatRequest, Message, Role, ToolCall, Usage};

/// 上下文预算配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextBudget {
    /// 模型上下文窗口。
    pub context_window: u32,
    /// 触发压缩的阈值占比。
    pub compact_at: f32,
    /// 压缩后保留的最近消息条数。
    pub keep_recent: usize,
    /// 单条消息的粗略 token 估算除数（字符数 / 该值）。
    pub chars_per_token: usize,
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            context_window: 200_000,
            compact_at: 0.85,
            keep_recent: 12,
            chars_per_token: 4,
        }
    }
}

impl ContextBudget {
    /// 触发压缩的绝对 token 数。
    pub fn soft_limit(&self) -> u32 {
        (self.context_window as f32 * self.compact_at.clamp(0.1, 0.99)) as u32
    }
}

/// 一次压缩的方案。
#[derive(Debug, Clone)]
pub struct CompactPlan {
    /// 会被折叠成摘要的早期消息
    pub older: Vec<Message>,
    /// 保留原文的近期消息
    pub keep: Vec<Message>,
}

/// 会话状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub messages: Vec<Message>,
    pub budget: ContextBudget,
    /// 累计用量（含历史轮次）。
    pub total_usage: Usage,
    /// 已完成轮次。
    pub steps: usize,
}

impl Default for Conversation {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            budget: ContextBudget::default(),
            total_usage: Usage::default(),
            steps: 0,
        }
    }
}

impl Conversation {
    pub fn new(budget: ContextBudget) -> Self {
        Self {
            budget,
            ..Default::default()
        }
    }

    pub fn push(&mut self, msg: Message) {
        self.messages.push(msg);
    }

    /// 粗略 token 估算。
    ///
    /// MVP 阶段用字符数除以经验值，不引入分词器依赖；
    /// 预留 `estimate_tokens` 钩子，将来可换成真 tokenizer。
    pub fn estimate_tokens(&self) -> u32 {
        self.messages
            .iter()
            .map(|m| estimate_tokens_of(m, self.budget.chars_per_token))
            .sum()
    }

    pub fn used_ratio(&self) -> f32 {
        if self.budget.context_window == 0 {
            return 1.0;
        }
        self.estimate_tokens() as f32 / self.budget.context_window as f32
    }

    pub fn should_compact(&self) -> bool {
        self.estimate_tokens() >= self.budget.soft_limit()
    }

    /// 算出压缩方案：哪些旧消息会被折叠，哪些近期消息保留。
    ///
    /// 拆成两步是为了让上层能在中间插入一次 LLM 调用来生成摘要——
    /// 确定性截断虽然零成本，但会丢掉模型对"自己在干什么"的判断。
    pub fn plan_compact(&self) -> Option<CompactPlan> {
        let keep = self.budget.keep_recent.max(4);
        if self.messages.len() <= keep {
            return None;
        }

        let split = self.messages.len() - keep;
        // 切分点向后对齐，避免把一组 tool_call 与其 tool_result 拆散
        let split = adjust_split_for_tool_pairs(&self.messages, split);
        if split == 0 {
            return None;
        }

        Some(CompactPlan {
            older: self.messages[..split].to_vec(),
            keep: self.messages[split..].to_vec(),
        })
    }

    /// 按方案执行压缩。`summary` 为 None 时退化为确定性截断。
    pub fn apply_compact(&mut self, plan: &CompactPlan, summary: Option<&str>) -> bool {
        if plan.older.is_empty() || plan.keep.is_empty() {
            return false;
        }

        let rendered: Vec<String> = plan
            .older
            .iter()
            .map(|m| format!("{}: {}", m.role.as_str(), m.content))
            .collect();

        let summary = match summary {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => deterministic_summary(&rendered),
        };

        // 折叠区末尾若有孤儿 tool_result（其 tool_call 被切走了），丢掉，
        // 否则部分厂商会因为配对不上而报错。
        let tail_len = plan
            .keep
            .iter()
            .take_while(|m| {
                !(m.role == Role::Tool
                    && !plan.older.iter().any(|o| {
                        o.tool_calls
                            .iter()
                            .any(|c| Some(c.id.as_str()) == m.tool_call_id.as_deref())
                    }))
            })
            .count();

        self.messages = std::iter::once(Message::system(format!(
            "<conversation_summary>\n以下是被压缩的早期对话记录：\n{summary}\n</conversation_summary>"
        )))
        .chain(plan.keep.iter().take(tail_len).cloned())
        .chain(plan.keep.iter().skip(tail_len).cloned())
        .collect();

        true
    }

    /// 一步到位的压缩。拿不到摘要时退化为确定性截断，不阻塞 agent 继续跑。
    pub fn compact(&mut self, summarizer: Option<&str>) -> bool {
        match self.plan_compact() {
            Some(plan) => self.apply_compact(&plan, summarizer),
            None => false,
        }
    }

    /// 组装给 provider 的请求。
    pub fn to_request(&self, system: &str, tools: Vec<crate::message::ToolSpec>) -> ChatRequest {
        ChatRequest {
            system: Some(system.to_string()),
            messages: self.messages.clone(),
            tools,
            max_tokens: None,
            temperature: Some(0.0),
            cache_hint: Some(crate::message::CacheHint::default()),
        }
    }

    /// 记录一轮助手回复。若有工具调用，紧随其后压入对应的 assistant 消息。
    pub fn record_assistant(&mut self, text: &str, calls: Vec<ToolCall>, usage: &Usage) {
        self.total_usage.merge(usage);
        self.messages
            .push(Message::assistant(text).with_tool_calls(calls));
        self.steps += 1;
    }
}

fn estimate_tokens_of(m: &Message, chars_per_token: usize) -> u32 {
    let cpt = chars_per_token.max(1);
    let mut chars = m.content.len() + m.role.as_str().len() + 8;
    for c in &m.tool_calls {
        chars += c.name.len() + c.arguments_json().len() + 12;
    }
    (chars / cpt) as u32
}

/// 让 `split` 落在消息边界上，且不把 tool_call 及其结果拆到两边。
fn adjust_split_for_tool_pairs(messages: &[Message], split: usize) -> usize {
    let mut s = split.clamp(0, messages.len());
    // 若切分点落在某个 tool 结果之后、对应 tool_call 之前，则前移。
    for _ in 0..8 {
        if s == 0 || s >= messages.len() {
            break;
        }
        let prev_is_call = messages[s - 1]
            .tool_calls
            .iter()
            .any(|c| messages.get(s).map(|m| m.tool_call_id.as_deref() == Some(c.id.as_str())).unwrap_or(false));
        if prev_is_call {
            s -= 1;
        } else {
            break;
        }
    }
    s
}

fn deterministic_summary(lines: &[String]) -> String {
    const MAX: usize = 2400;
    let mut out = String::new();
    for l in lines {
        if out.len() + l.len() > MAX {
            out.push_str("\n…(已省略)");
            break;
        }
        out.push_str(l);
        out.push('\n');
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(msgs: usize, keep: usize) -> Conversation {
        let mut c = Conversation::new(ContextBudget {
            keep_recent: keep,
            ..Default::default()
        });
        for i in 0..msgs {
            c.push(Message::user(format!("第 {i} 条")));
        }
        c
    }

    #[test]
    fn 消息不够多时不该压缩() {
        let c = conv(4, 4);
        assert!(c.plan_compact().is_none());
    }

    #[test]
    fn 方案把旧消息和近期消息分开() {
        let c = conv(10, 4);
        let plan = c.plan_compact().expect("10 > 4 应该可压缩");
        assert_eq!(plan.older.len(), 6);
        assert_eq!(plan.keep.len(), 4);
        // 保留的应是最新 4 条
        assert_eq!(plan.keep.last().unwrap().content, "第 9 条");
        assert_eq!(plan.older.first().unwrap().content, "第 0 条");
    }

    #[test]
    fn 应用压缩后只剩摘要加近期消息() {
        let mut c = conv(10, 4);
        let plan = c.plan_compact().unwrap();
        assert!(c.apply_compact(&plan, Some("摘要内容")));

        // 1 条摘要 + 4 条近期
        assert_eq!(c.messages.len(), 5);
        assert_eq!(c.messages[0].role, Role::System);
        assert!(c.messages[0].content.contains("摘要内容"));
        assert_eq!(c.messages[4].content, "第 9 条");
    }

    #[test]
    fn 压缩后token数下降() {
        let mut c = conv(40, 6);
        let before = c.estimate_tokens();
        assert!(c.compact(Some("短摘要")));
        let after = c.estimate_tokens();
        assert!(after < before, "{after} 应该小于 {before}");
    }

    #[test]
    fn 确定性截断在没有摘要时也能工作() {
        let mut c = conv(20, 4);
        assert!(c.compact(None));
        // 没有 <conversation_summary> 里的自定义摘要，但结构还在
        assert_eq!(c.messages.len(), 5);
        assert!(c.messages[0].content.contains("conversation_summary"));
    }

    #[test]
    fn 切分点不拆散工具调用与其结果() {
        // 构造：前 6 条里有一个 tool_use，紧接着是它的 tool_result
        let mut c = conv(0, 4);
        for i in 0..5 {
            c.push(Message::user(format!("杂项 {i}")));
        }
        c.push(Message::assistant("").with_tool_calls(vec![ToolCall::new(
            "call_1",
            "read_file",
            serde_json::json!({}),
        )]));
        c.push(Message::tool_result("call_1", "文件内容"));
        for i in 0..3 {
            c.push(Message::user(format!("后续 {i}")));
        }

        let plan = c.plan_compact().expect("应该可压缩");
        // 切分点不能正好落在 tool_use 和 tool_result 之间
        let cut = plan.older.len();
        let is_orphan = plan.older.last().map(|m| m.role == Role::Tool).unwrap_or(false);
        assert!(
            !is_orphan,
            "切分点把 tool_result 留在了折叠区，它的 tool_call 却在保留区"
        );
        assert!(cut > 0);
    }

    #[test]
    fn 空方案不会误伤会话() {
        let mut c = Conversation::new(ContextBudget::default());
        let plan = CompactPlan {
            older: vec![],
            keep: c.messages.clone(),
        };
        assert!(!c.apply_compact(&plan, Some("x")));
        assert!(c.messages.is_empty());
    }

    #[test]
    fn keep_recent_下限被兜住() {
        // 配置成 0 或 1 也不该把会话切没
        let mut c = Conversation::new(ContextBudget {
            keep_recent: 0,
            ..Default::default()
        });
        for i in 0..10 {
            c.push(Message::user(format!("{i}")));
        }
        let plan = c.plan_compact().expect("应该可压缩");
        assert!(plan.older.len() >= 1);
        assert!(plan.keep.len() >= 4);
    }

    #[test]
    fn 统计与命中率() {
        let u = Usage {
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 300,
            cache_write_tokens: 5,
        };
        assert_eq!(u.total(), 425);
        assert!((u.cache_hit_ratio() - 0.75).abs() < 0.001);
        let mut acc = Usage::default();
        acc.merge(&u);
        assert_eq!(acc, u);
    }
}
