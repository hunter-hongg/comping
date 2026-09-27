//! 三个协议适配器的集成测试。
//!
//! 全部打在 `mock-provider` 上：不需要 API key，不烧额度，
//! 而且工具参数是**分片下发**的，所以"边解析边拼接"的路径真的被执行到了。
//!
//! 每个协议测同样的语义：
//! - 纯文本流式拼接（含中文，验证跨包 UTF-8 边界）
//! - 单个工具调用：名字 + 参数要从分片里拼回来
//! - 并行工具调用：不能串台，顺序要对
//! - 用量归一：缓存读要单独算
//! - 编码方向：适配器**发出去**的请求长什么样

use agent_core::{
    ChatRequest, Message, Provider, Role, StopReason, ToolCall, Usage,
};
use mock_provider::{MockServer, Protocol, Reply, Script, ToolCallSpec, Usage as MUsage};
use tokio::sync::mpsc::UnboundedSender;

use llm::registry::ProviderConfig;

/// 收集流式增量，顺便验证它们真的被逐个推送出来了。
fn collector() -> (UnboundedSender<String>, tokio::sync::mpsc::UnboundedReceiver<String>) {
    tokio::sync::mpsc::unbounded_channel()
}

fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> String {
    let mut s = String::new();
    while let Ok(v) = rx.try_recv() {
        s.push_str(&v);
    }
    s
}

fn base_request() -> ChatRequest {
    ChatRequest {
        system: Some("你是测试用系统提示".into()),
        messages: vec![Message::user("读一下 a.txt")],
        tools: vec![agent_core::ToolSpec::new(
            "read_file",
            "读文件",
            serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        )],
        max_tokens: Some(1024),
        temperature: Some(0.0),
        cache_hint: Some(agent_core::CacheHint::default()),
    }
}

// ==================================================================== OpenAI

fn openai_cfg(server: &MockServer) -> ProviderConfig {
    ProviderConfig {
        kind: Some("openai".into()),
        base_url: Some(server.openai_base()),
        api_key: Some("test-key".into()),
        model: Some("mock-model".into()),
        supports_usage: Some(true),
        ..Default::default()
    }
}

async fn run_openai(
    server: &MockServer,
    req: ChatRequest,
) -> anyhow::Result<agent_core::ChatResponse> {
    let p = llm::openai::OpenAiProvider::new(&openai_cfg(server))?;
    let (tx, mut rx) = collector();
    let resp = p.stream(req, "mock-model", tx).await?;
    assert_eq!(
        drain(&mut rx),
        resp.content,
        "流式推送的片段拼起来必须等于最终内容"
    );
    Ok(resp)
}

#[tokio::test]
async fn openai_纯文本流式拼接() {
    let s = MockServer::start(Protocol::OpenAi, Script::once(Reply::text("你好，这是回复。"))).unwrap();
    let r = run_openai(&s, base_request()).await.unwrap();

    assert_eq!(r.content, "你好，这是回复。");
    assert_eq!(r.stop_reason, StopReason::EndTurn);
    assert!(r.tool_calls.is_empty());
}

#[tokio::test]
async fn openai_中文跨包不乱码() {
    let text = "第一段中文。第二段带 emoji 🚀。第三段结束。";
    let s = MockServer::start(Protocol::OpenAi, Script::once(Reply::text(text))).unwrap();
    let r = run_openai(&s, base_request()).await.unwrap();
    assert_eq!(r.content, text);
}

#[tokio::test]
async fn openai_工具参数从分片拼回() {
    let call = ToolCallSpec::new("call_abc", "read_file", r#"{"path":"src/lib.rs"}"#).chunked(4);
    let s = MockServer::start(
        Protocol::OpenAi,
        Script::new(vec![Reply::text_then_tool("我读一下。", call)]),
    )
    .unwrap();

    let r = run_openai(&s, base_request()).await.unwrap();
    assert_eq!(r.content, "我读一下。");
    assert_eq!(r.tool_calls.len(), 1);
    assert_eq!(r.tool_calls[0].id, "call_abc");
    assert_eq!(r.tool_calls[0].name, "read_file");
    assert_eq!(r.tool_calls[0].arguments["path"], "src/lib.rs");
    assert_eq!(r.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn openai_并行调用不串台且保序() {
    let s = MockServer::start(
        Protocol::OpenAi,
        Script::new(vec![Reply::parallel(vec![
            ToolCallSpec::new("c1", "read_file", r#"{"path":"a.txt"}"#).chunked(3),
            ToolCallSpec::new("c2", "grep", r#"{"pattern":"fn main"}"#).chunked(3),
            ToolCallSpec::new("c3", "list_dir", r#"{"path":"src"}"#).chunked(3),
        ])]),
    )
    .unwrap();

    let r = run_openai(&s, base_request()).await.unwrap();
    assert_eq!(r.tool_calls.len(), 3);
    assert_eq!(r.tool_calls[0].id, "c1");
    assert_eq!(r.tool_calls[0].name, "read_file");
    assert_eq!(r.tool_calls[0].arguments["path"], "a.txt");
    assert_eq!(r.tool_calls[1].name, "grep");
    assert_eq!(r.tool_calls[1].arguments["pattern"], "fn main");
    assert_eq!(r.tool_calls[2].name, "list_dir");
    assert_eq!(r.tool_calls[2].arguments["path"], "src");
}

#[tokio::test]
async fn openai_用量与缓存归一() {
    let usage = MUsage {
        input: 200,
        output: 33,
        cache_read: 1_024,
        cache_write: 0,
    };
    let s = MockServer::start(
        Protocol::OpenAi,
        Script::once(Reply::text("x").with_usage(usage)),
    )
    .unwrap();

    let r = run_openai(&s, base_request()).await.unwrap();
    // prompt_tokens 1124 里 1024 是缓存命中，输入应当扣掉
    assert_eq!(r.usage.input_tokens, 200);
    assert_eq!(r.usage.cache_read_tokens, 1_024);
    assert_eq!(r.usage.output_tokens, 33);
    assert!((r.usage.cache_hit_ratio() - 1024.0 / 1224.0).abs() < 0.001);
}

#[tokio::test]
async fn openai_请求编码正确() {
    let s = MockServer::start(Protocol::OpenAi, Script::once(Reply::text("ok"))).unwrap();
    run_openai(&s, base_request()).await.unwrap();

    let req = s.request(0).expect("应抓到请求");
    assert_eq!(req.path, "/v1/chat/completions");
    assert_eq!(req.auth.as_deref(), Some("Bearer test-key"));
    // system 是 messages 的第一条
    assert_eq!(req.message(0).unwrap()["role"], "system");
    assert_eq!(
        req.message(0).unwrap()["content"],
        "你是测试用系统提示"
    );
    assert_eq!(req.body["stream"], true);
    assert_eq!(req.tool_names(), vec!["read_file"]);
}

#[tokio::test]
async fn openai_工具结果回填成_tool_角色() {
    let s = MockServer::start(Protocol::OpenAi, Script::once(Reply::text("ok"))).unwrap();
    let mut req = base_request();
    req.messages = vec![
        Message::user("读一下"),
        Message::assistant("").with_tool_calls(vec![ToolCall::new(
            "call_1",
            "read_file",
            serde_json::json!({"path": "a.txt"}),
        )]),
        Message::tool_result("call_1", "1 | 内容"),
    ];
    run_openai(&s, req).await.unwrap();

    let cap = s.request(0).unwrap();
    let msgs = cap.body["messages"].as_array().unwrap();
    // system, user, assistant, tool
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[2]["role"], "assistant");
    assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "read_file");
    // OpenAI 的参数是字符串
    assert!(msgs[2]["tool_calls"][0]["function"]["arguments"].is_string());
    assert_eq!(msgs[3]["role"], "tool");
    assert_eq!(msgs[3]["tool_call_id"], "call_1");
}

// ================================================================== Anthropic

fn anthropic_cfg(server: &MockServer) -> ProviderConfig {
    ProviderConfig {
        kind: Some("anthropic".into()),
        base_url: Some(server.anthropic_base()),
        api_key: Some("test-key".into()),
        model: Some("mock-model".into()),
        ..Default::default()
    }
}

async fn run_anthropic(
    server: &MockServer,
    req: ChatRequest,
) -> anyhow::Result<agent_core::ChatResponse> {
    let p = llm::anthropic::AnthropicProvider::new(&anthropic_cfg(server))?;
    let (tx, mut rx) = collector();
    let resp = p.stream(req, "mock-model", tx).await?;
    assert_eq!(drain(&mut rx), resp.content);
    Ok(resp)
}

#[tokio::test]
async fn anthropic_纯文本流式拼接() {
    let s =
        MockServer::start(Protocol::Anthropic, Script::once(Reply::text("你好，这是回复。"))).unwrap();
    let r = run_anthropic(&s, base_request()).await.unwrap();

    assert_eq!(r.content, "你好，这是回复。");
    assert_eq!(r.stop_reason, StopReason::EndTurn);
    assert!(r.tool_calls.is_empty());
}

#[tokio::test]
async fn anthropic_中文跨包不乱码() {
    let text = "第一段中文。第二段带 emoji 🚀。第三段结束。";
    let s = MockServer::start(Protocol::Anthropic, Script::once(Reply::text(text))).unwrap();
    let r = run_anthropic(&s, base_request()).await.unwrap();
    assert_eq!(r.content, text);
}

#[tokio::test]
async fn anthropic_工具参数从分片拼回() {
    let call = ToolCallSpec::new("toolu_01", "read_file", r#"{"path":"src/lib.rs"}"#).chunked(4);
    let s = MockServer::start(
        Protocol::Anthropic,
        Script::new(vec![Reply::text_then_tool("我读一下。", call)]),
    )
    .unwrap();

    let r = run_anthropic(&s, base_request()).await.unwrap();
    assert_eq!(r.content, "我读一下。");
    assert_eq!(r.tool_calls.len(), 1);
    assert_eq!(r.tool_calls[0].id, "toolu_01");
    assert_eq!(r.tool_calls[0].name, "read_file");
    assert_eq!(r.tool_calls[0].arguments["path"], "src/lib.rs");
    assert_eq!(r.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn anthropic_并行调用按_index归并不串台() {
    let s = MockServer::start(
        Protocol::Anthropic,
        Script::new(vec![Reply::parallel(vec![
            ToolCallSpec::new("t1", "read_file", r#"{"path":"a.txt"}"#).chunked(3),
            ToolCallSpec::new("t2", "grep", r#"{"pattern":"TODO"}"#).chunked(3),
        ])]),
    )
    .unwrap();

    let r = run_anthropic(&s, base_request()).await.unwrap();
    assert_eq!(r.tool_calls.len(), 2);
    assert_eq!(r.tool_calls[0].id, "t1");
    assert_eq!(r.tool_calls[0].arguments["path"], "a.txt");
    assert_eq!(r.tool_calls[1].id, "t2");
    assert_eq!(r.tool_calls[1].arguments["pattern"], "TODO");
}

#[tokio::test]
async fn anthropic_缓存用量分开记() {
    let usage = MUsage {
        input: 120,
        output: 40,
        cache_read: 800,
        cache_write: 64,
    };
    let s = MockServer::start(
        Protocol::Anthropic,
        Script::once(Reply::text("x").with_usage(usage)),
    )
    .unwrap();

    let r = run_anthropic(&s, base_request()).await.unwrap();
    assert_eq!(r.usage.input_tokens, 120);
    assert_eq!(r.usage.cache_read_tokens, 800);
    assert_eq!(r.usage.cache_write_tokens, 64);
    assert_eq!(r.usage.output_tokens, 40);
}

#[tokio::test]
async fn anthropic_请求编码正确() {
    let s = MockServer::start(Protocol::Anthropic, Script::once(Reply::text("ok"))).unwrap();
    run_anthropic(&s, base_request()).await.unwrap();

    let req = s.request(0).unwrap();
    assert_eq!(req.path, "/v1/messages");
    // key 走 x-api-key 头，不是 Bearer
    assert_eq!(req.anthropic_version.as_deref(), Some("2023-06-01"));
    assert!(req.auth.is_none());
    assert_eq!(req.any_system().as_deref(), Some("你是测试用系统提示"));
    // system 不在 messages 里
    assert!(req.body.get("messages").unwrap().as_array().unwrap().len() == 1);
    assert_eq!(req.body["stream"], true);
    assert_eq!(req.tool_names(), vec!["read_file"]);
    // 工具 schema 走 input_schema
    assert_eq!(req.body["tools"][0]["input_schema"]["type"], "object");
    // system 段应打了缓存断点
    assert_eq!(req.body["system"][0]["cache_control"]["type"], "ephemeral");
}

#[tokio::test]
async fn anthropic_工具结果转成_tool_result_block() {
    let s = MockServer::start(Protocol::Anthropic, Script::once(Reply::text("ok"))).unwrap();
    let mut req = base_request();
    req.messages = vec![
        Message::user("读一下"),
        Message::assistant("").with_tool_calls(vec![ToolCall::new(
            "toolu_9",
            "read_file",
            serde_json::json!({"path": "a.txt"}),
        )]),
        Message::tool_result("toolu_9", "1 | 内容"),
    ];
    run_anthropic(&s, req).await.unwrap();

    let cap = s.request(0).unwrap();
    let msgs = cap.body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    // assistant 的 tool_use
    assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
    assert_eq!(msgs[1]["content"][0]["id"], "toolu_9");
    // 工具结果走 user 角色的 tool_result block
    assert_eq!(msgs[2]["role"], "user");
    assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
    assert_eq!(msgs[2]["content"][0]["tool_use_id"], "toolu_9");
    assert!(msgs[2]["content"][0]["is_error"] == false);
}

#[tokio::test]
async fn anthropic_连续同角色消息被合并() {
    let s = MockServer::start(Protocol::Anthropic, Script::once(Reply::text("ok"))).unwrap();
    let mut req = base_request();
    // 三条都是 user 角色：user 消息 + tool_result(也是 user) + 又一条 user
    req.messages = vec![
        Message::user("第一句"),
        Message::tool_result("t1", "结果"),
        Message::user("第二句"),
    ];
    run_anthropic(&s, req).await.unwrap();

    let cap = s.request(0).unwrap();
    let msgs = cap.body["messages"].as_array().unwrap();
    // Anthropic 不接受连续同角色，必须全部并进一条
    assert_eq!(msgs.len(), 1);
    let blocks = msgs[0]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 3);
    assert_eq!(blocks[0]["text"], "第一句");
    assert_eq!(blocks[1]["type"], "tool_result");
    assert_eq!(blocks[2]["text"], "第二句");
}

#[tokio::test]
async fn anthropic_异角色不合并() {
    let s = MockServer::start(Protocol::Anthropic, Script::once(Reply::text("ok"))).unwrap();
    let mut req = base_request();
    req.messages = vec![
        Message::user("问"),
        Message::assistant("答"),
        Message::user("再问"),
    ];
    run_anthropic(&s, req).await.unwrap();

    let msgs = s.request(0).unwrap().body["messages"].as_array().unwrap().clone();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[1]["role"], "assistant");
}

// ==================================================================== Gemini

fn gemini_cfg(server: &MockServer) -> ProviderConfig {
    ProviderConfig {
        kind: Some("gemini".into()),
        base_url: Some(server.gemini_base()),
        api_key: Some("test-key".into()),
        model: Some("mock-model".into()),
        ..Default::default()
    }
}

async fn run_gemini(
    server: &MockServer,
    req: ChatRequest,
) -> anyhow::Result<agent_core::ChatResponse> {
    let p = llm::gemini::GeminiProvider::new(&gemini_cfg(server))?;
    let (tx, mut rx) = collector();
    let resp = p.stream(req, "mock-model", tx).await?;
    assert_eq!(drain(&mut rx), resp.content);
    Ok(resp)
}

#[tokio::test]
async fn gemini_纯文本流式拼接() {
    let s = MockServer::start(Protocol::Gemini, Script::once(Reply::text("你好，这是回复。"))).unwrap();
    let r = run_gemini(&s, base_request()).await.unwrap();
    assert_eq!(r.content, "你好，这是回复。");
    assert_eq!(r.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn gemini_中文跨包不乱码() {
    let text = "第一段中文。第二段带 emoji 🚀。第三段结束。";
    let s = MockServer::start(Protocol::Gemini, Script::once(Reply::text(text))).unwrap();
    let r = run_gemini(&s, base_request()).await.unwrap();
    assert_eq!(r.content, text);
}

#[tokio::test]
async fn gemini_函数调用转成中立_tool_call() {
    let s = MockServer::start(
        Protocol::Gemini,
        Script::new(vec![Reply::text_then_tool(
            "我读一下。",
            ToolCallSpec::new("ignored", "read_file", r#"{"path":"a.txt"}"#),
        )]),
    )
    .unwrap();

    let r = run_gemini(&s, base_request()).await.unwrap();
    assert_eq!(r.content, "我读一下。");
    assert_eq!(r.tool_calls.len(), 1);
    assert_eq!(r.tool_calls[0].name, "read_file");
    assert_eq!(r.tool_calls[0].arguments["path"], "a.txt");
    // Gemini 不给 id，我们合成一个可关联的
    assert!(!r.tool_calls[0].id.is_empty());
    assert_eq!(r.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn gemini_多个函数调用名字不同则不冲突() {
    let s = MockServer::start(
        Protocol::Gemini,
        Script::new(vec![Reply::parallel(vec![
            ToolCallSpec::new("x", "read_file", r#"{"path":"a"}"#),
            ToolCallSpec::new("x", "grep", r#"{"pattern":"b"}"#),
        ])]),
    )
    .unwrap();

    let r = run_gemini(&s, base_request()).await.unwrap();
    assert_eq!(r.tool_calls.len(), 2);
    assert_ne!(r.tool_calls[0].id, r.tool_calls[1].id);
    assert_eq!(r.tool_calls[0].name, "read_file");
    assert_eq!(r.tool_calls[1].name, "grep");
}

#[tokio::test]
async fn gemini_请求编码正确() {
    let s = MockServer::start(Protocol::Gemini, Script::once(Reply::text("ok"))).unwrap();
    run_gemini(&s, base_request()).await.unwrap();

    let req = s.request(0).unwrap();
    // key 走 query
    assert_eq!(req.api_key_query.as_deref(), Some("test-key"));
    assert!(req.path.contains(":streamGenerateContent"));
    assert_eq!(req.any_system().as_deref(), Some("你是测试用系统提示"));
    assert_eq!(req.tool_names(), vec!["read_file"]);
}

#[tokio::test]
async fn gemini_工具结果转成_function_response() {
    let s = MockServer::start(Protocol::Gemini, Script::once(Reply::text("ok"))).unwrap();
    let mut req = base_request();
    req.messages = vec![
        Message::user("读一下"),
        Message::assistant("").with_tool_calls(vec![ToolCall::new(
            "read_file",
            "read_file",
            serde_json::json!({"path": "a.txt"}),
        )]),
        // id 里带 #n 的也要还原成纯函数名
        Message::tool_result("grep#2", "命中 3 处"),
    ];
    run_gemini(&s, req).await.unwrap();

    let cap = s.request(0).unwrap();
    let contents = cap.body["contents"].as_array().unwrap();
    let fr = contents
        .iter()
        .flat_map(|c| c["parts"].as_array().unwrap())
        .find_map(|p| p.get("functionResponse"))
        .expect("应有 functionResponse");
    assert_eq!(fr["name"], "grep", "id 里的 #2 必须剥掉");
    assert_eq!(
        fr.pointer("/response/result").and_then(|v| v.as_str()),
        Some("命中 3 处")
    );
}

#[tokio::test]
async fn gemini_会剥掉_schema_里不支持的关键字() {
    let s = MockServer::start(Protocol::Gemini, Script::once(Reply::text("ok"))).unwrap();
    let mut req = base_request();
    req.tools[0].parameters = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "path": {"type": "string", "additionalProperties": false},
        },
        "required": ["path"]
    });
    run_gemini(&s, req).await.unwrap();

    let cap = s.request(0).unwrap();
    let decl = &cap.body["tools"][0]["functionDeclarations"][0];
    assert!(decl["parameters"].get("additionalProperties").is_none());
    assert!(
        decl.pointer("/parameters/properties/path")
            .unwrap()
            .get("additionalProperties")
            .is_none()
    );
    assert_eq!(decl["parameters"]["required"][0], "path");
}

// ======================================================= 跨协议一致性

/// 同一条回复，三个协议解码出来必须完全一样——这是"模型中立"的验收标准。
#[tokio::test]
async fn 三个协议对同一条回复解码一致() {
    let call = ToolCallSpec::new("call_1", "read_file", r#"{"path":"a.txt"}"#).chunked(4);
    let reply = Reply::text_then_tool("我要读文件。", call);
    let req = base_request();

    let so = MockServer::start(Protocol::OpenAi, Script::new(vec![reply.clone()])).unwrap();
    let ro = run_openai(&so, req.clone()).await.unwrap();

    let sa =
        MockServer::start(Protocol::Anthropic, Script::new(vec![reply.clone()])).unwrap();
    let ra = run_anthropic(&sa, req.clone()).await.unwrap();

    let sg = MockServer::start(Protocol::Gemini, Script::new(vec![reply])).unwrap();
    let rg = run_gemini(&sg, req).await.unwrap();

    assert_eq!(ro.content, ra.content);
    assert_eq!(ro.content, rg.content);
    assert_eq!(ro.stop_reason, ra.stop_reason);
    assert_eq!(ro.stop_reason, rg.stop_reason);

    // 工具名与参数跨协议一致（id 各家不同，不比）
    for r in [&ro, &ra, &rg] {
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].name, "read_file");
        assert_eq!(r.tool_calls[0].arguments["path"], "a.txt");
    }
}

/// 工具结果能被第二轮正确回填（跨协议）。
#[tokio::test]
async fn 第二轮工具结果能带上() {
    // 带上一轮的 tool_call 与 tool_result
    let req = || ChatRequest {
        messages: vec![
            Message::user("读一下"),
            Message::assistant("").with_tool_calls(vec![ToolCall::new(
                "call_1",
                "read_file",
                serde_json::json!({"path": "a.txt"}),
            )]),
            Message::tool_result("call_1", "1 | 内容"),
        ],
        ..base_request()
    };

    let s = MockServer::start(Protocol::OpenAi, Script::once(Reply::text("收到"))).unwrap();
    assert_eq!(run_openai(&s, req()).await.unwrap().content, "收到");
    assert!(s.request(0).unwrap().has_tool_result());

    let s =
        MockServer::start(Protocol::Anthropic, Script::once(Reply::text("收到"))).unwrap();
    assert_eq!(run_anthropic(&s, req()).await.unwrap().content, "收到");
    assert!(s.request(0).unwrap().has_tool_result());

    let s = MockServer::start(Protocol::Gemini, Script::once(Reply::text("收到"))).unwrap();
    assert_eq!(run_gemini(&s, req()).await.unwrap().content, "收到");
    assert!(s.request(0).unwrap().has_tool_result());
}

/// 流式增量必须被逐个推送，不能只在末尾一次性吐出来。
#[tokio::test]
async fn 增量是逐片推送的() {
    let s = MockServer::start(
        Protocol::OpenAi,
        Script::once(Reply::text("AAAABBBBCCCC")),
    )
    .unwrap();
    let p = llm::openai::OpenAiProvider::new(&openai_cfg(&s)).unwrap();
    let (tx, mut rx) = collector();
    p.stream(base_request(), "mock-model", tx).await.unwrap();

    let mut chunks = Vec::new();
    while let Ok(c) = rx.try_recv() {
        chunks.push(c);
    }
    assert!(chunks.len() > 1, "应当收到多个片段，实际 {}", chunks.len());
    assert_eq!(chunks.concat(), "AAAABBBBCCCC");
}

// ==================================================================== 用量类型

#[test]
fn usage_减法不会出负数() {
    let a = Usage {
        input_tokens: 100,
        output_tokens: 50,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    };
    let b = Usage {
        input_tokens: 30,
        output_tokens: 10,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    };
    let d = a.since(&b);
    assert_eq!(d.input_tokens, 70);
    assert_eq!(d.output_tokens, 40);

    // 反向减（不该出现，但不该 panic）
    let d2 = b.since(&a);
    assert_eq!(d2.input_tokens, 0);
    assert_eq!(d2.total(), 0);
}

#[test]
fn role_字符串() {
    assert_eq!(Role::User.as_str(), "user");
    assert_eq!(Role::Tool.as_str(), "tool");
}

