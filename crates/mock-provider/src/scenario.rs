//! 脚本：声明第 N 次请求该回什么。
//!
//! agent 循环每次 step 发一次请求，所以"请求序号"就等于"第几步"，
//! 断言可以直接对着轮次写。

use serde_json::Value;

/// 停止原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stop {
    #[default]
    EndTurn,
    ToolUse,
    MaxTokens,
}

/// 用量。默认给一组好认的数字，测试里能直接写死断言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub input: u32,
    pub output: u32,
    pub cache_read: u32,
    pub cache_write: u32,
}

impl Default for Usage {
    fn default() -> Self {
        Self {
            input: 1_000,
            output: 50,
            cache_read: 800,
            cache_write: 0,
        }
    }
}

impl Usage {
}

/// 一个工具调用。
#[derive(Debug, Clone)]
pub struct ToolCallSpec {
    pub id: String,
    pub name: String,
    /// 字符串化的 JSON 参数。
    pub args: String,
    /// 把 args 切成多片下发，用来验证适配器能把分片拼回去。
    /// `None` 表示一次性给完。
    pub arg_chunk: Option<usize>,
}

impl ToolCallSpec {
    pub fn new(id: &str, name: &str, args: &str) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            args: args.into(),
            arg_chunk: None,
        }
    }

    /// 强制分片下发，步长 `n` 字符。
    pub fn chunked(mut self, n: usize) -> Self {
        self.arg_chunk = Some(n.max(1));
        self
    }

    /// 把 args 切块。
    pub fn chunks(&self) -> Vec<&str> {
        match self.arg_chunk {
            None => vec![self.args.as_str()],
            Some(n) => self
                .args
                .as_bytes()
                .chunks(n)
                .map(|c| std::str::from_utf8(c).unwrap_or(""))
                .collect(),
        }
    }

    /// 参数解析成 JSON 后的值。
    pub fn args_value(&self) -> Value {
        serde_json::from_str(&self.args).unwrap_or(Value::Null)
    }
}

/// 一次回复。
#[derive(Debug, Clone, Default)]
pub struct Reply {
    pub text: String,
    pub tool_calls: Vec<ToolCallSpec>,
    pub stop: Option<Stop>,
    pub usage: Option<Usage>,
}

impl Reply {
    /// 纯文本回复。
    pub fn text(t: &str) -> Self {
        Self {
            text: t.into(),
            ..Default::default()
        }
    }

    /// 文本 + 一个工具调用。
    pub fn text_then_tool(text: &str, call: ToolCallSpec) -> Self {
        Self {
            text: text.into(),
            tool_calls: vec![call],
            stop: Some(Stop::ToolUse),
            usage: None,
        }
    }

    /// 并行发多个工具调用。
    pub fn parallel(calls: Vec<ToolCallSpec>) -> Self {
        Self {
            text: String::new(),
            tool_calls: calls,
            stop: Some(Stop::ToolUse),
            usage: None,
        }
    }

    /// 指定用量。
    pub fn with_usage(mut self, u: Usage) -> Self {
        self.usage = Some(u);
        self
    }


    pub fn stop(&self) -> Stop {
        self.stop.unwrap_or(if self.tool_calls.is_empty() {
            Stop::EndTurn
        } else {
            Stop::ToolUse
        })
    }

    pub fn usage(&self) -> Usage {
        self.usage.unwrap_or_default()
    }
}

/// 按请求序号返回回复的脚本。
#[derive(Debug, Clone)]
pub struct Script {
    replies: Vec<Reply>,
    /// 脚本用完后的行为
    tail: Tail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tail {
    /// 重复最后一条
    Repeat,
    /// 返回空回复
    Empty,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            replies: Vec::new(),
            tail: Tail::Empty,
        }
    }
}

impl Script {
    pub fn new(replies: Vec<Reply>) -> Self {
        Self {
            replies,
            tail: Tail::Empty,
        }
    }

    /// 单轮脚本：脚本用完后重复最后一条。
    pub fn once(reply: Reply) -> Self {
        Self {
            replies: vec![reply],
            tail: Tail::Repeat,
        }
    }

    /// 常见的"先调工具，拿到结果再收尾"两轮脚本。
    pub fn tool_then_text(text: &str, call: ToolCallSpec) -> Self {
        Self {
            replies: vec![Reply::text_then_tool("", call), Reply::text(text)],
            tail: Tail::Repeat,
        }
    }

    pub fn repeat(mut self) -> Self {
        self.tail = Tail::Repeat;
        self
    }

    pub fn reply_at(&self, turn: usize) -> Reply {
        match self.replies.get(turn) {
            Some(r) => r.clone(),
            None => match self.tail {
                Tail::Repeat => self.replies.last().cloned().unwrap_or_default(),
                Tail::Empty => Reply::default(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 超出脚本后重复最后一条() {
        let s = Script::new(vec![Reply::text("a"), Reply::text("b")]).repeat();
        assert_eq!(s.reply_at(0).text, "a");
        assert_eq!(s.reply_at(1).text, "b");
        assert_eq!(s.reply_at(9).text, "b");
    }

    #[test]
    fn 超出脚本后返回空() {
        let s = Script::new(vec![Reply::text("a")]);
        assert!(s.reply_at(1).text.is_empty());
    }

    #[test]
    fn 分片切分参数() {
        let c = ToolCallSpec::new("i", "read_file", "{\"path\":\"a.txt\"}").chunked(4);
        let parts = c.chunks();
        assert!(parts.len() > 1);
        // 拼回去必须等于原文
        assert_eq!(parts.concat(), c.args);
    }

    #[test]
    fn 不分片时只有一块() {
        let c = ToolCallSpec::new("i", "bash", "{\"command\":\"ls\"}");
        assert_eq!(c.chunks().len(), 1);
    }

    #[test]
    fn 停止原因按有无工具推导() {
        assert_eq!(Reply::text("x").stop(), Stop::EndTurn);
        assert_eq!(
            Reply::text_then_tool("", ToolCallSpec::new("i", "n", "{}")).stop(),
            Stop::ToolUse
        );
    }
}
