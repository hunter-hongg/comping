//! 共享的 HTTP / SSE 基础设施。
//!
//! 各家适配器只写"格式转换"，重试、流式解析、错误归一都在这里。

use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use reqwest::{Client, StatusCode};

/// 默认超时。流式请求靠 idle timeout 兜底，不设整体上限。
pub fn build_client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!("comping/", env!("CARGO_PKG_VERSION")))
        .pool_max_idle_per_host(8)
        .connect_timeout(Duration::from_secs(10))
        .build()
        .context("构造 HTTP client 失败")
}

/// 重试策略。
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(20),
        }
    }
}

impl RetryPolicy {
    /// 判断状态码是否值得重试。
    pub fn should_retry(status: StatusCode) -> bool {
        status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
    }

    /// 服务端给了 `retry-after` 就听它的。
    pub fn delay_for(&self, attempt: u32, retry_after: Option<&str>) -> Duration {
        if let Some(v) = retry_after.and_then(|s| s.trim().parse::<u64>().ok()) {
            return Duration::from_secs(v.min(60));
        }
        let exp = self.base_delay * 2u32.saturating_pow(attempt.min(6));
        // full jitter，避免多客户端同时重试打爆上游
        let jitter = rand_jitter();
        std::cmp::min(exp, self.max_delay) / 2 + jitter
    }
}

fn rand_jitter() -> Duration {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    Duration::from_millis(nanos % 500)
}

/// 错误归一，让上层不需要认识各家的错误体。
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("网络请求失败: {0}")]
    Transport(String),
    #[error("HTTP {status}: {body}")]
    Status { status: StatusCode, body: String },
    #[error("响应解析失败: {0}")]
    Decode(String),
    #[error("{0}")]
    Other(String),
}

impl HttpError {
    pub fn is_retriable(&self) -> bool {
        match self {
            HttpError::Transport(_) => true,
            HttpError::Status { status, .. } => RetryPolicy::should_retry(*status),
            _ => false,
        }
    }
}

/// 带重试的流式 POST。返回原始字节流，由调用方做 SSE 解析。
pub async fn post_stream(
    client: &Client,
    url: &str,
    headers: &[(String, String)],
    body: &serde_json::Value,
) -> Result<reqwest::Response, HttpError> {
    let policy = RetryPolicy::default();
    let mut attempt = 0;

    loop {
        let mut retry_after: Option<String> = None;
        let mut req = client.post(url).json(body);
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }

        let err = match req.send().await {
            Ok(resp) if resp.status().is_success() => return Ok(resp),
            Ok(resp) => {
                let status = resp.status();
                retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let text = resp.text().await.unwrap_or_default();
                HttpError::Status {
                    status,
                    body: truncate(&text, 800),
                }
            }
            Err(e) => HttpError::Transport(e.to_string()),
        };

        attempt += 1;
        // 流式请求一旦收到 2xx 就不再重试；失败时 body 还没开始消费，可以安全重发。
        if attempt >= policy.max_attempts || !err.is_retriable() {
            return Err(err);
        }
        tokio::time::sleep(policy.delay_for(attempt, retry_after.as_deref())).await;
    }
}

/// 一个 SSE 事件：`event:` 名与 `data:` 负载。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// 把字节流切成 SSE 事件。
///
/// 厂商实现细节差异不小（有的不用 `event:`，有的用 CRLF，
/// 有的把整段 JSON 塞在一个 data 里），所以这里只做最保守的切分，
/// 语义解释交给各适配器。
pub fn sse_decoder(buffer: &mut String, chunk: &str) -> Vec<SseEvent> {
    buffer.push_str(chunk);
    let mut out = Vec::new();

    // 事件之间用空行分隔，兼容 \n\n 与 \r\n\r\n
    while let Some(pos) = find_separator(buffer) {
        let (raw, rest) = buffer.split_at(pos.0);
        let raw = raw.to_string();
        *buffer = rest[pos.1..].to_string();

        let mut ev = SseEvent::default();
        let mut data_lines: Vec<&str> = Vec::new();
        for line in raw.lines() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            match line.split_once(':') {
                Some(("event", v)) => ev.event = Some(v.trim().to_string()),
                Some(("data", v)) => data_lines.push(v.trim_start()),
                // id / retry 忽略
                _ => {}
            }
        }
        ev.data = data_lines.join("\n");
        if !ev.data.is_empty() || ev.event.is_some() {
            out.push(ev);
        }
    }

    out
}

/// 返回 (分隔符起点, 长度)。
fn find_separator(s: &str) -> Option<(usize, usize)> {
    if let Some(i) = s.find("\r\n\r\n") {
        return Some((i, 4));
    }
    if let Some(i) = s.find("\n\n") {
        return Some((i, 2));
    }
    None
}

/// SSE 结束的哨兵。
pub const SSE_DONE: &str = "[DONE]";

/// 消费一个 SSE 响应，对每个事件同步调用 `f`。
///
/// 刻意不做成 async 回调：三个适配器的解析都是纯 CPU 计算，
/// 同步闭包能省掉一层装箱，也让状态机可以直接借用捕获。
pub async fn consume_sse<F>(resp: reqwest::Response, mut f: F) -> Result<()>
where
    F: FnMut(SseEvent) -> Result<()>,
{
    let mut stream = Box::pin(resp.bytes_stream());
    let mut buffer = String::new();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| HttpError::Transport(e.to_string()))?;
        // SSE 允许在任意字节边界切断 UTF-8 序列，lossy 转换的替换符
        // 只影响被切断的那一个字符，下一块会补回来。
        let text = String::from_utf8_lossy(&bytes).to_string();
        for ev in sse_decoder(&mut buffer, &text) {
            f(ev)?;
        }
    }

    Ok(())
}

/// 截断长文本，保留头尾。
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let head = max * 2 / 3;
    let tail = max - head;
    format!(
        "{}…[省略 {} 字符]…{}",
        &s[..head],
        s.len() - head - tail,
        &s[s.len() - tail..]
    )
}

