//! 一个只依赖 std 的 HTTP/1.1 服务，用来给适配器造流量。
//!
//! 刻意不用任何 HTTP 框架：依赖越少，这个"测试替身"越不容易
//! 自己出 bug 而误导我们。
//!
//! 响应用 chunked 传输编码逐事件 flush，这样适配器的
//! "边解析边拼接"路径才真的被执行到——如果一次性吐完，
//! 分片拼接的 bug 是测不出来的。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::Value;

use crate::protocol;
use crate::scenario::Script;
use crate::State;

/// 假服务支持的三种协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    OpenAi,
    Anthropic,
    Gemini,
}

/// 一个跑起来的假服务。
pub struct MockServer {
    addr: SocketAddr,
    state: Arc<State>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MockServer {
    /// 绑定到 `127.0.0.1` 的随机端口。
    ///
    /// 端口 0 让内核分配，测试并发跑也不会撞。
    pub fn start(protocol: Protocol, script: Script) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;

        let state = State::new();
        let stop = Arc::new(AtomicBool::new(false));

        let t_stop = stop.clone();
        let t_state = state.clone();
        let handle = std::thread::spawn(move || {
            for conn in listener.incoming() {
                if t_stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(stream) = conn else { continue };
                let s = t_state.clone();
                let p = protocol;
                let sc = script.clone();
                // 每次请求一个线程：足够测试用，也免去异步锁
                std::thread::spawn(move || {
                    let _ = serve_one(stream, p, &sc, &s);
                });
            }
        });

        Ok(Self {
            addr,
            state,
            stop,
            handle: Some(handle),
        })
    }

    /// 基础地址，如 `http://127.0.0.1:34567`。
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// OpenAI 风格路径，如 `{base}/v1`。
    pub fn openai_base(&self) -> String {
        format!("{}/v1", self.base_url())
    }

    pub fn anthropic_base(&self) -> String {
        format!("{}/v1", self.base_url())
    }

    pub fn gemini_base(&self) -> String {
        format!("{}/v1beta", self.base_url())
    }

    pub fn state(&self) -> Arc<State> {
        self.state.clone()
    }

    /// 收到的请求，按时间顺序。
    pub fn requests(&self) -> Vec<crate::Captured> {
        self.state.requests()
    }

    /// 第 n 个请求。
    pub fn request(&self, n: usize) -> Option<crate::Captured> {
        self.requests().into_iter().nth(n)
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // 唤醒 accept 循环，否则 JoinHandle 永远不返回
        let _ = TcpStream::connect(self.addr);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// 在给定监听器上服务，直到进程结束。手动模式（`serve.rs` 二进制）用。
pub fn serve(listener: TcpListener, protocol: Protocol, script: Script) -> ! {
    let state = State::new();
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let s = state.clone();
        let p = protocol;
        let sc = script.clone();
        // 每次连接一个线程：测试规模下够用，也免掉异步锁
        std::thread::spawn(move || {
            let _ = serve_one(stream, p, &sc, &s);
        });
    }
    unreachable!("listener closed")
}

struct Request {
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        let lname = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k == &lname)
            .map(|(_, v)| v.as_str())
    }

    fn query(&self) -> Vec<(String, String)> {
        let Some(q) = self.path.split_once('?').map(|(_, q)| q) else {
            return Vec::new();
        };
        q.split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), percent_decode(v)))
            .collect()
    }

    fn query_value(&self, key: &str) -> Option<String> {
        self.query()
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(b[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn serve_one(
    mut stream: TcpStream,
    protocol: Protocol,
    script: &Script,
    state: &State,
) -> std::io::Result<()> {
    let req = match read_request(&mut stream)? {
        Some(r) => r,
        None => return Ok(()),
    };

    let turn = state.record(crate::Captured {
        path: req.path.clone(),
        auth: req.header("authorization").map(str::to_string),
        api_key_query: req.query_value("key"),
        anthropic_version: req.header("anthropic-version").map(str::to_string),
        body: req.json(),
        raw: req.body.clone(),
    });

    let reply = script.reply_at(turn);
    let streaming = req
        .json()
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(true);

    match (protocol, streaming) {
        (Protocol::OpenAi, true) => {
            write_sse(&mut stream, &protocol::openai(&reply), Duration::ZERO)
        }
        (Protocol::OpenAi, false) => write_json(&mut stream, 200, &protocol::openai_completion(&reply)),
        (Protocol::Anthropic, true) => {
            write_sse(&mut stream, &protocol::anthropic(&reply), Duration::ZERO)
        }
        (Protocol::Anthropic, false) => {
            // 非流式时拼成一个完整 message 对象
            write_json(&mut stream, 200, &anthropic_message(&reply))
        }
        (Protocol::Gemini, true) => write_sse(&mut stream, &protocol::gemini(&reply), Duration::ZERO),
        (Protocol::Gemini, false) => {
            write_json(&mut stream, 200, &gemini_response(&reply))
        }
    }
}

fn anthropic_message(reply: &crate::Reply) -> Value {
    let u = reply.usage();
    let mut content = Vec::new();
    if !reply.text.is_empty() {
        content.push(serde_json::json!({"type": "text", "text": reply.text}));
    }
    for c in &reply.tool_calls {
        content.push(serde_json::json!({
            "type": "tool_use", "id": c.id, "name": c.name, "input": c.args_value(),
        }));
    }
    serde_json::json!({
        "id": "msg_mock", "type": "message", "role": "assistant",
        "model": "mock", "content": content,
        "stop_reason": match reply.stop() {
            crate::Stop::ToolUse => "tool_use",
            crate::Stop::MaxTokens => "max_tokens",
            crate::Stop::EndTurn => "end_turn",
        },
        "usage": {
            "input_tokens": u.input,
            "cache_read_input_tokens": u.cache_read,
            "cache_creation_input_tokens": u.cache_write,
            "output_tokens": u.output,
        },
    })
}

fn gemini_response(reply: &crate::Reply) -> Value {
    let u = reply.usage();
    let mut parts = Vec::new();
    if !reply.text.is_empty() {
        parts.push(serde_json::json!({"text": reply.text}));
    }
    for c in &reply.tool_calls {
        parts.push(serde_json::json!({
            "functionCall": {"name": c.name, "args": c.args_value()}
        }));
    }
    serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": parts},
            "index": 0,
            "finishReason": "STOP",
        }],
        "usageMetadata": {
            "promptTokenCount": u.input + u.cache_read,
            "candidatesTokenCount": u.output,
            "cachedContentTokenCount": u.cache_read,
        },
    })
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let _method = parts.next();
    let target = parts.next().unwrap_or("/").to_string();

    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 {
            break;
        }
        let t = h.trim_end();
        if t.is_empty() {
            break;
        }
        if let Some((k, v)) = t.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }

    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);

    let mut body = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut body)?;
    }

    Ok(Some(Request {
        path: target,
        headers,
        body: String::from_utf8_lossy(&body).to_string(),
    }))
}

/// 逐事件写 chunk，每条之间 sleep `delay`，逼出真实的增量行为。
fn write_sse(stream: &mut TcpStream, events: &[String], delay: Duration) -> std::io::Result<()> {
    let head = "HTTP/1.1 200 OK\r\n\
                Content-Type: text/event-stream\r\n\
                Cache-Control: no-cache\r\n\
                Transfer-Encoding: chunked\r\n\
                Connection: close\r\n\r\n";
    stream.write_all(head.as_bytes())?;
    stream.flush()?;

    for ev in events {
        let mut chunk: Vec<u8> = Vec::with_capacity(ev.len() + 16);
        chunk.extend_from_slice(format!("{:x}\r\n", ev.len()).as_bytes());
        chunk.extend_from_slice(ev.as_bytes());
        chunk.extend_from_slice(b"\r\n");
        stream.write_all(&chunk)?;
        stream.flush()?;
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
    }

    stream.write_all(b"0\r\n\r\n")?;
    stream.flush()?;
    Ok(())
}

fn write_json(stream: &mut TcpStream, status: u16, body: &Value) -> std::io::Result<()> {
    let text = serde_json::to_string(body)?;
    let head = format!(
        "HTTP/1.1 {status} OK\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        text.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(text.as_bytes())?;
    stream.flush()?;
    Ok(())
}
