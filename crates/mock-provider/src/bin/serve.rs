//! 手动跑起来的假服务，方便用 curl / 真实客户端调试。
//!
//! ```sh
//! mock-provider --port 8977 --protocol openai
//! curl -N -X POST http://127.0.0.1:8977/v1/chat/completions \
//!   -d '{"model":"m","messages":[],"stream":true}'
//! ```

use mock_provider::{Protocol, Reply, Script, ToolCallSpec};

fn main() {
    let mut port: u16 = 8977;
    let mut protocol = Protocol::OpenAi;
    let mut scenario = String::from("tool");

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" | "-p" => port = it.next().and_then(|s| s.parse().ok()).unwrap_or(port),
            "--protocol" => {
                protocol = match it.next().as_deref() {
                    Some("anthropic") => Protocol::Anthropic,
                    Some("gemini") => Protocol::Gemini,
                    _ => Protocol::OpenAi,
                }
            }
            // text | tool | text-only
            "--scenario" | "-s" => scenario = it.next().unwrap_or(scenario),
            "--help" | "-h" => {
                eprintln!(
                    "mock-provider [--port N] [--protocol openai|anthropic|gemini] \
                     [--scenario text|tool|text-only]"
                );
                return;
            }
            other => eprintln!("忽略未知参数 {other}"),
        }
    }

    let script = match scenario.as_str() {
        "text" => Script::new(vec![Reply::text("你好，我是 mock 模型。")]).repeat(),
        "text-only" => Script::new(vec![Reply::text("只有文本。")]).repeat(),
        "tool" => Script::tool_then_text(
            "我先读一下文件。",
            ToolCallSpec::new("call_1", "read_file", r#"{"path":"README.md"}"#).chunked(6),
        ),
        _ => Script::new(vec![Reply::text("默认回复。")]).repeat(),
    };

    let listener = std::net::TcpListener::bind(("127.0.0.1", port)).expect("绑定端口失败");
    let addr = listener.local_addr().unwrap();

    println!("mock-provider 已在 {addr} 上运行（{protocol:?}, 场景 {scenario}）");
    println!("试一下：");
    match protocol {
        Protocol::OpenAi => println!(
            "  curl -N -X POST {addr}/v1/chat/completions \
             -d '{{\"model\":\"m\",\"messages\":[],\"stream\":true}}'"
        ),
        Protocol::Anthropic => println!(
            "  curl -N -X POST {addr}/v1/messages \
             -H 'anthropic-version: 2023-06-01' -H 'x-api-key: k' \
             -d '{{\"model\":\"m\",\"messages\":[],\"stream\":true}}'"
        ),
        Protocol::Gemini => println!(
            "  curl -N -X POST '{addr}/v1beta/models/m:streamGenerateContent?alt=sse' \
             -d '{{\"contents\":[]}}'"
        ),
    }
    use std::io::Write;
    let _ = std::io::stdout().flush();

    mock_provider::server::serve(listener, protocol, script);
}
