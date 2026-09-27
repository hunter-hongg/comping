//! 把 [`Reply`] 编码成三种真实协议的事件流。
//!
//! 这里的保真度直接决定测试价值：
//! - 工具参数**分片下发**，逼适配器真的做增量拼接
//! - Anthropic 用 `index` 区分并行调用，OpenAI 也用 `index`，
//!   两者都能暴露"分片串台"的 bug
//! - `finish_reason` / `stop_reason` 单独一包，不和内容混在一起
//!
//! 纯函数，不碰网络，方便直接单测。

use serde_json::{Value, json};

use crate::scenario::{Reply, Stop};

/// 某个协议的一次流式响应，元素是完整的 SSE 事件（含结尾空行）。
pub type Events = Vec<String>;

fn sse_data(v: &Value) -> String {
    format!("data: {v}\n\n")
}

fn sse_event(name: &str, v: &Value) -> String {
    format!("event: {name}\ndata: {v}\n\n")
}

fn stop_str(s: Stop) -> &'static str {
    match s {
        Stop::EndTurn => "end_turn",
        Stop::ToolUse => "tool_use",
        Stop::MaxTokens => "max_tokens",
    }
}

// ------------------------------------------------------------------ OpenAI

pub fn openai(reply: &Reply) -> Events {
    let mut out = Events::new();

    // 首包只带 role
    out.push(sse_data(&json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "model": "mock",
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""},
                     "finish_reason": Value::Null}],
    })));

    // 文本切片下发，模拟真实的小块流式
    for piece in split_text(&reply.text, 8) {
        out.push(sse_data(&json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion.chunk",
            "model": "mock",
            "choices": [{"index": 0, "delta": {"content": piece}, "finish_reason": Value::Null}],
        })));
    }

    // 工具调用：逐个 index 交错下发，参数切片
    for (i, call) in reply.tool_calls.iter().enumerate() {
        let idx = i as u64;
        // 头：id + name（不带参数）
        out.push(sse_data(&json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion.chunk",
            "choices": [{"index": 0, "delta": {"tool_calls": [{
                "index": idx,
                "id": call.id,
                "type": "function",
                "function": {"name": call.name, "arguments": ""},
            }]}, "finish_reason": Value::Null}],
        })));

        for piece in call.chunks() {
            out.push(sse_data(&json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {"tool_calls": [{
                    "index": idx,
                    "function": {"arguments": piece},
                }]}, "finish_reason": Value::Null}],
            })));
        }
    }

    let finish = match reply.stop() {
        Stop::ToolUse => "tool_calls",
        Stop::MaxTokens => "length",
        Stop::EndTurn => "stop",
    };
    out.push(sse_data(&json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "choices": [{"index": 0, "delta": {}, "finish_reason": finish}],
        "usage": {
            "prompt_tokens": reply.usage().input + reply.usage().cache_read,
            "completion_tokens": reply.usage().output,
            "prompt_tokens_details": {"cached_tokens": reply.usage().cache_read},
        },
    })));

    out.push("data: [DONE]\n\n".to_string());
    out
}

// ---------------------------------------------------------------- Anthropic

pub fn anthropic(reply: &Reply) -> Events {
    let u = reply.usage();
    let mut out = Events::new();

    out.push(sse_event(
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": "msg_mock", "type": "message", "role": "assistant",
                "model": "mock", "content": [],
                "stop_reason": Value::Null, "stop_sequence": Value::Null,
                "usage": {
                    "input_tokens": u.input,
                    "cache_read_input_tokens": u.cache_read,
                    "cache_creation_input_tokens": u.cache_write,
                    "output_tokens": 1,
                },
            },
        }),
    ));

    let mut next_index: i64 = 0;

    if !reply.text.is_empty() {
        out.push(sse_event(
            "content_block_start",
            &json!({
                "type": "content_block_start", "index": next_index,
                "content_block": {"type": "text", "text": ""},
            }),
        ));
        // 文本也切片，逼适配器处理跨包 UTF-8 边界
        for piece in split_text(&reply.text, 8) {
            out.push(sse_event(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta", "index": next_index,
                    "delta": {"type": "text_delta", "text": piece},
                }),
            ));
        }
        out.push(sse_event(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": next_index}),
        ));
        next_index += 1;
    }

    for call in &reply.tool_calls {
        out.push(sse_event(
            "content_block_start",
            &json!({
                "type": "content_block_start", "index": next_index,
                "content_block": {
                    "type": "tool_use", "id": call.id, "name": call.name, "input": {},
                },
            }),
        ));
        for piece in call.chunks() {
            out.push(sse_event(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta", "index": next_index,
                    "delta": {"type": "input_json_delta", "partial_json": piece},
                }),
            ));
        }
        out.push(sse_event(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": next_index}),
        ));
        next_index += 1;
    }

    out.push(sse_event(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_str(reply.stop()), "stop_sequence": Value::Null},
            "usage": {"output_tokens": u.output},
        }),
    ));
    out.push(sse_event("message_stop", &json!({"type": "message_stop"})));
    out
}

// ------------------------------------------------------------------- Gemini

pub fn gemini(reply: &Reply) -> Events {
    let u = reply.usage();
    let mut out = Events::new();

    if !reply.text.is_empty() {
        for piece in split_text(&reply.text, 8) {
            out.push(sse_data(&json!({
                "candidates": [{"content": {"role": "model", "parts": [{"text": piece}]},
                                "index": 0}],
                "usageMetadata": {"promptTokenCount": u.input + u.cache_read,
                                 "candidatesTokenCount": u.output,
                                 "cachedContentTokenCount": u.cache_read},
            })));
        }
    }

    // functionCall 没有 id，一个 part 一个调用，参数整块给出
    if !reply.tool_calls.is_empty() {
        let parts: Vec<Value> = reply
            .tool_calls
            .iter()
            .map(|c| json!({"functionCall": {"name": c.name, "args": c.args_value()}}))
            .collect();
        out.push(sse_data(&json!({
            "candidates": [{
                "content": {"role": "model", "parts": parts},
                "index": 0,
                "finishReason": match reply.stop() {
                    Stop::ToolUse | Stop::EndTurn => "STOP",
                    Stop::MaxTokens => "MAX_TOKENS",
                },
            }],
            "usageMetadata": {"promptTokenCount": u.input + u.cache_read,
                             "candidatesTokenCount": u.output,
                             "cachedContentTokenCount": u.cache_read},
        })));
    } else {
        out.push(sse_data(&json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": ""}]},
                            "index": 0,
                            "finishReason": match reply.stop() {
                                Stop::MaxTokens => "MAX_TOKENS",
                                _ => "STOP",
                            }}],
            "usageMetadata": {"promptTokenCount": u.input + u.cache_read,
                             "candidatesTokenCount": u.output,
                             "cachedContentTokenCount": u.cache_read},
        })));
    }

    out
}

/// 按字符数切文本。刻意切在可能跨 UTF-8 边界的位置上，
/// 逼适配器对分片做正确的字符串处理。
fn split_text(s: &str, n: usize) -> Vec<String> {
    s.chars()
        .collect::<Vec<_>>()
        .chunks(n.max(1))
        .map(|c| c.iter().collect())
        .collect()
}

/// 非流式响应体（`stream: false` 时用）。
pub fn openai_completion(reply: &Reply) -> Value {
    let u = reply.usage();
    let tool_calls: Vec<Value> = reply
        .tool_calls
        .iter()
        .map(|c| {
            json!({
                "id": c.id, "type": "function",
                "function": {"name": c.name, "arguments": c.args},
            })
        })
        .collect();

    json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": reply.text, "tool_calls": tool_calls},
            "finish_reason": match reply.stop() {
                Stop::ToolUse => "tool_calls",
                Stop::MaxTokens => "length",
                Stop::EndTurn => "stop",
            },
        }],
        "usage": {
            "prompt_tokens": u.input + u.cache_read,
            "completion_tokens": u.output,
            "prompt_tokens_details": {"cached_tokens": u.cache_read},
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::ToolCallSpec;

    #[test]
    fn openai_以_done_收尾() {
        let e = openai(&Reply::text("hi"));
        assert_eq!(e.last().unwrap().trim(), "data: [DONE]");
    }

    #[test]
    fn openai_参数分片能拼回原文() {
        let call = ToolCallSpec::new("call_1", "read_file", "{\"path\":\"a.txt\"}").chunked(5);
        let e = openai(&Reply::text_then_tool("", call));
        let joined: String = e
            .iter()
            .filter_map(|s| serde_json::from_str::<Value>(s.trim_start_matches("data: ")).ok())
            .filter_map(|v| v.pointer("/choices/0/delta/tool_calls/0/function/arguments").and_then(|x| x.as_str()).map(str::to_string))
            .collect();
        assert_eq!(joined, "{\"path\":\"a.txt\"}");
    }

    #[test]
    fn openai_多个调用_index_不串台() {
        let e = openai(&Reply::parallel(vec![
            ToolCallSpec::new("c1", "read_file", "{\"path\":\"a\"}").chunked(3),
            ToolCallSpec::new("c2", "read_file", "{\"path\":\"bb\"}").chunked(3),
        ]));
        let mut by_idx: std::collections::BTreeMap<u64, String> = Default::default();
        for s in &e {
            let Ok(v) = serde_json::from_str::<Value>(s.trim_start_matches("data: ")) else {
                continue;
            };
            if let Some(tc) = v.pointer("/choices/0/delta/tool_calls/0") {
                let i = tc["index"].as_u64().unwrap();
                if let Some(a) = tc.pointer("/function/arguments").and_then(|x| x.as_str()) {
                    by_idx.entry(i).or_default().push_str(a);
                }
            }
        }
        assert_eq!(by_idx[&0], "{\"path\":\"a\"}");
        assert_eq!(by_idx[&1], "{\"path\":\"bb\"}");
    }

    #[test]
    fn anthropic_事件名齐全() {
        let e = anthropic(&Reply::text_then_tool(
            "hi",
            ToolCallSpec::new("toolu_1", "bash", "{\"command\":\"ls\"}"),
        ));
        let names: Vec<&str> = e
            .iter()
            .filter_map(|s| s.strip_prefix("event: "))
            .filter_map(|s| s.lines().next())
            .collect();

        assert_eq!(names.first(), Some(&"message_start"));
        assert_eq!(names.last(), Some(&"message_stop"));
        for expected in [
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
        ] {
            assert!(names.contains(&expected), "缺少事件 {expected}");
        }
    }

    #[test]
    fn anthropic_并行调用_index_递增() {
        let e = anthropic(&Reply::parallel(vec![
            ToolCallSpec::new("t1", "grep", "{\"pattern\":\"a\"}"),
            ToolCallSpec::new("t2", "grep", "{\"pattern\":\"b\"}"),
        ]));
        let idx: Vec<u64> = e
            .iter()
            .filter(|s| s.starts_with("event: content_block_start"))
            .filter_map(|s| serde_json::from_str::<Value>(s.split_once("data: ")?.1).ok())
            .filter_map(|v| v["index"].as_u64())
            .collect();
        assert_eq!(idx, vec![0, 1]);
    }

    #[test]
    fn anthropic_参数分片能拼回原文() {
        let call = ToolCallSpec::new("t", "read_file", "{\"path\":\"hello world\"}").chunked(4);
        let e = anthropic(&Reply::text_then_tool("", call));
        let joined: String = e
            .iter()
            .filter_map(|s| s.split_once("data: "))
            .filter_map(|(h, d)| h.starts_with("event: content_block_delta").then_some(d))
            .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
            .filter_map(|v| v.pointer("/delta/partial_json").and_then(|x| x.as_str()).map(str::to_string))
            .collect();
        assert_eq!(joined, "{\"path\":\"hello world\"}");
    }

    #[test]
    fn gemini_函数调用不带_id() {
        // 不带文本时，第一个事件就是带 functionCall 的那个
        let e = gemini(&Reply::parallel(vec![ToolCallSpec::new(
            "ignored",
            "bash",
            "{\"command\":\"ls\"}",
        )]));
        let v: Value = serde_json::from_str(e[0].trim_start_matches("data: ")).unwrap();
        let fc = v.pointer("/candidates/0/content/parts/0/functionCall").unwrap();
        assert!(fc.get("id").is_none(), "Gemini 的 functionCall 不带 id");
        assert_eq!(fc["name"], "bash");
        assert_eq!(fc.pointer("/args/command").and_then(|v| v.as_str()), Some("ls"));
    }

    #[test]
    fn 分片不破坏中文() {
        let e = openai(&Reply::text("中文内容测试"));
        let joined: String = e
            .iter()
            .filter_map(|s| serde_json::from_str::<Value>(s.trim_start_matches("data: ")).ok())
            .filter_map(|v| v.pointer("/choices/0/delta/content").and_then(|x| x.as_str()).map(str::to_string))
            .collect();
        assert_eq!(joined, "中文内容测试");
    }
}
