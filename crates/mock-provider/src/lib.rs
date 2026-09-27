//! 可脚本化的假模型服务。
//!
//! 目标：让协议适配器能在**没有 API key、没有网络**的前提下被完整测试。
//! 三件事：
//!
//! 1. [`script`] —— 声明"第 N 次请求该回什么"，完全确定性
//! 2. [`protocol`] —— 把回复编码成 OpenAI / Anthropic / Gemini 三种真实 SSE 流
//! 3. [`server`] —— 一个只依赖 std 的 HTTP 服务，能逐事件 flush，
//!    这样适配器的"边解析边拼接"路径才真的被跑到

pub mod protocol;
pub mod scenario;
pub mod server;

pub use scenario::{Reply, Script, Stop, ToolCallSpec, Usage};
pub use server::{MockServer, Protocol};

use std::sync::{Arc, Mutex};

/// 服务抓到的请求，用于断言适配器**发出去的**内容。
#[derive(Debug, Clone)]
pub struct Captured {
    pub path: String,
    pub auth: Option<String>,
    pub api_key_query: Option<String>,
    pub anthropic_version: Option<String>,
    pub body: serde_json::Value,
    /// 原始字节，某些测试要检查编码细节。
    pub raw: String,
}

impl Captured {
    /// 取出请求里的某条消息。
    pub fn message(&self, idx: usize) -> Option<&serde_json::Value> {
        self.body.get("messages").and_then(|m| m.get(idx))
    }

    pub fn system(&self) -> Option<&str> {
        match self.body.get("system") {
            // Anthropic 走顶层字段，但包装成 block 数组
            Some(serde_json::Value::String(s)) => Some(s.as_str()),
            Some(serde_json::Value::Array(a)) => a
                .first()
                .and_then(|b| b.get("text"))
                .and_then(|t| t.as_str()),
            _ => None,
        }
    }

    pub fn system_instruction(&self) -> Option<&str> {
        self.body
            .get("systemInstruction")
            .and_then(|s| s.pointer("/parts/0/text"))
            .and_then(|t| t.as_str())
    }

    /// 所有 system 文本（不同协议位置不同，统一取出）。
    pub fn any_system(&self) -> Option<String> {
        self.system()
            .map(str::to_string)
            .or_else(|| self.system_instruction().map(str::to_string))
            .or_else(|| {
                self.body
                    .get("messages")
                    .and_then(|m| m.as_array())
                    .and_then(|a| a.first())
                    .filter(|m| m["role"] == serde_json::json!("system"))
                    .and_then(|m| m["content"].as_str())
                    .map(str::to_string)
            })
    }

    pub fn tool_names(&self) -> Vec<String> {
        let mut v = Vec::new();
        // OpenAI / Anthropic
        if let Some(tools) = self.body.get("tools").and_then(|t| t.as_array()) {
            for t in tools {
                let name = t
                    .pointer("/function/name")
                    .or_else(|| t.get("name"))
                    .and_then(|n| n.as_str());
                if let Some(n) = name {
                    v.push(n.to_string());
                }
            }
        }
        // Gemini
        if let Some(decls) = self
            .body
            .pointer("/tools/0/functionDeclarations")
            .and_then(|d| d.as_array())
        {
            for d in decls {
                if let Some(n) = d["name"].as_str() {
                    v.push(n.to_string());
                }
            }
        }
        v
    }

    /// 请求里是否带了工具执行结果。
    ///
    /// 三家表达方式不同：
    /// - OpenAI：`{"role": "tool", "tool_call_id": ...}`
    /// - Anthropic：`{"type": "tool_result", ...}` —— 是 `type` 的**值**，不是键
    /// - Gemini：`{"functionResponse": {...}}`
    pub fn has_tool_result(&self) -> bool {
        fn scan(v: &serde_json::Value) -> bool {
            match v {
                serde_json::Value::Array(a) => a.iter().any(scan),
                serde_json::Value::Object(m) => {
                    m.get("role").and_then(|r| r.as_str()) == Some("tool")
                        || m.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                        || m.contains_key("functionResponse")
                        || m.iter().any(|(_, v)| scan(v))
                }
                _ => false,
            }
        }
        scan(&self.body)
    }
}

/// 共享状态：脚本 + 抓到的请求。
#[derive(Debug, Default)]
pub struct State {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    turn: usize,
    captured: Vec<Captured>,
}

impl State {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 记录一次请求并返回它的序号（从 0 起）。
    pub fn record(&self, c: Captured) -> usize {
        let mut g = self.inner.lock().expect("state poisoned");
        let turn = g.turn;
        g.turn += 1;
        g.captured.push(c);
        turn
    }

    pub fn requests(&self) -> Vec<Captured> {
        self.inner.lock().expect("state poisoned").captured.clone()
    }

    pub fn turn(&self) -> usize {
        self.inner.lock().expect("state poisoned").turn
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cap(body: serde_json::Value) -> Captured {
        Captured {
            path: "/v1/chat/completions".into(),
            auth: None,
            api_key_query: None,
            anthropic_version: None,
            raw: String::new(),
            body,
        }
    }

    #[test]
    fn 抓取系统提示词_覆盖三种协议() {
        assert_eq!(
            cap(json!({"system": "S"})).any_system().as_deref(),
            Some("S")
        );
        assert_eq!(
            cap(json!({"systemInstruction": {"parts": [{"text": "S"}]}}))
                .any_system()
                .as_deref(),
            Some("S")
        );
        assert_eq!(
            cap(json!({"messages": [{"role": "system", "content": "S"}]}))
                .any_system()
                .as_deref(),
            Some("S")
        );
    }

    #[test]
    fn 抓取工具名_覆盖三种协议() {
        let o = cap(json!({"tools": [{"function": {"name": "read_file"}}]}));
        assert_eq!(o.tool_names(), vec!["read_file"]);

        let a = cap(json!({"tools": [{"name": "bash"}]}));
        assert_eq!(a.tool_names(), vec!["bash"]);

        let g = cap(json!({"tools": [{"functionDeclarations": [{"name": "grep"}]}]}));
        assert_eq!(g.tool_names(), vec!["grep"]);
    }

    #[test]
    fn 识别工具结果() {
        // OpenAI
        assert!(cap(json!({"messages": [{"role": "tool"}]})).has_tool_result());
        // Anthropic：tool_result 是 type 的值，不是键
        assert!(cap(json!({"messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1"}
        ]}]})).has_tool_result());
        // Gemini
        assert!(
            cap(json!({"contents": [{"parts": [{"functionResponse": {}}]}]})).has_tool_result()
        );
        // 不该误判
        assert!(cap(json!({"messages": [{"role": "user", "content": "hi"}]})).has_tool_result() == false);
    }

    #[test]
    fn anthropic_的_system_是数组() {
        let c = cap(json!({"system": [{"type": "text", "text": "S"}]}));
        assert_eq!(c.system(), Some("S"));
        assert_eq!(c.any_system().as_deref(), Some("S"));
    }
}
