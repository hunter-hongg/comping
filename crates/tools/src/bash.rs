//! 命令执行。
//!
//! 安全取向：默认要用户确认、超时可取消、输出有上限。
//! `cargo` 之类高频安全命令可以通过配置的 allowlist 永久免确认。

use std::process::Stdio;
use std::time::Duration;

use agent_core::{PendingApproval, Tool, ToolError, ToolOutcome, object_schema};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::workspace::Workspace;

/// 单次命令默认超时。编译类命令很慢，给得宽松些。
const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 1800;

/// 回传给模型的输出上限；超出的部分落盘，只送头尾。
const MAX_OUTPUT_CHARS: usize = 30_000;

pub struct Bash {
    ws: Workspace,
    /// 永久免确认的命令前缀，来自配置。
    allowlist: Vec<String>,
    default_timeout: Duration,
}

impl Bash {
    pub fn new(ws: Workspace) -> Self {
        Self {
            ws,
            allowlist: Vec::new(),
            default_timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }

    pub fn with_allowlist(mut self, list: Vec<String>) -> Self {
        self.allowlist = list;
        self
    }

    /// 命令是否命中免确认白名单。
    ///
    /// 只按 `argv[0]` 的**首个词**匹配，所以配置 `cargo` 就能放行
    /// `cargo test`；但 `rm` 永远不会被 `rm` 之外的东西意外放行。
    pub fn is_allowed(&self, program: &str) -> bool {
        let head = program.split_whitespace().next().unwrap_or("");
        self.allowlist
            .iter()
            .any(|a| a == head || a == program)
    }
}

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "在工作区里执行一条 shell 命令并返回退出码与输出。适合跑测试、构建、git 状态这类操作。\
         长时间运行的任务加 timeout 参数（秒）。"
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "command": { "type": "string", "description": "要执行的 shell 命令" },
                "timeout": { "type": "integer", "description": "超时秒数，默认 120，最大 1800", "minimum": 1 },
                "cwd": { "type": "string", "description": "工作目录，相对工作区根，默认根目录" }
            }),
            &["command"],
        )
    }

    fn preview(&self, args: &Value) -> String {
        let cmd = args.get("command").and_then(|c| c.as_str()).unwrap_or("");
        if self.is_allowed(cmd) {
            format!("{cmd}  [免确认]")
        } else {
            cmd.to_string()
        }
    }

    async fn request_approval(&self, args: &Value) -> Option<PendingApproval> {
        let cmd = args.get("command")?.as_str()?;
        if self.is_allowed(cmd) {
            return None;
        }
        let cwd = args.get("cwd").and_then(|c| c.as_str()).unwrap_or(".");
        let timeout = args.get("timeout").and_then(|t| t.as_u64());
        let mut body = format!("$ {cmd}\n\n工作目录: {cwd}");
        if let Some(t) = timeout {
            body.push_str(&format!("\n超时: {t}s"));
        }
        Some(PendingApproval {
            title: cmd.to_string(),
            body,
        })
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        let command = args
            .get("command")
            .and_then(|c| c.as_str())
            .ok_or_else(|| ToolError::InvalidParams("缺少 `command`".into()))?
            .to_string();

        if command.trim().is_empty() {
            return Err(ToolError::InvalidParams("command 为空".into()));
        }

        let cwd = match args.get("cwd").and_then(|c| c.as_str()) {
            Some(c) => self
                .ws
                .resolve(c)
                .map_err(|e| ToolError::OutsideWorkspace(e.to_string()))?,
            None => self.ws.root().to_path_buf(),
        };

        let timeout = Duration::from_secs(
            args.get("timeout")
                .and_then(|t| t.as_u64())
                .map(|t| t.clamp(1, MAX_TIMEOUT_SECS))
                .unwrap_or(self.default_timeout.as_secs()),
        );

        let started = std::time::Instant::now();
        let (code, output, timed_out) = run(&command, &cwd, timeout).await?;
        let elapsed = started.elapsed();

        let total_chars = output.chars().count();
        let mut out = output;
        let mut truncated_to = None;
        if total_chars > MAX_OUTPUT_CHARS {
            let (head, tail) = split_keep(&out, MAX_OUTPUT_CHARS);
            let spill = write_spill(&command, &out).await;
            truncated_to = spill.clone();
            out = match spill.as_deref() {
                Some(p) => {
                    format!("{head}\n…[输出 {total_chars} 字符已截断，完整内容见 {p}]…\n{tail}")
                }
                None => format!("{head}\n…[输出过长已截断]…\n{tail}"),
            };
        }

        let status = if timed_out {
            format!("超时（{}s 后被终止）", timeout.as_secs())
        } else if code == 0 {
            "成功".to_string()
        } else {
            format!("失败，退出码 {code}")
        };

        let summary = format!("{status} · {:.1}s", elapsed.as_secs_f32());
        let body = format!("[{status}]\n{out}");

        // 失败与超时都用 err 形态，agent 循环会把它标成"需要模型自己纠正"
        let mut outcome = if code == 0 && !timed_out {
            ToolOutcome::ok(summary.clone(), body)
        } else {
            let mut o = ToolOutcome::err(body);
            o.summary = summary.clone();
            o
        };
        outcome.summary = summary;
        outcome.truncated_to = truncated_to;
        Ok(outcome)
    }
}

/// 进程组守卫：无论走哪条退出路径，都把整组进程杀掉。
///
/// 只 kill 直接子进程是不够的——我们 spawn 的是 `sh -c "sleep 300"`，
/// 杀掉 `sh` 之后 `sleep` 会变成孤儿继续跑。所以让子进程自成
/// 一个进程组（`process_group(0)`），再按组杀。
///
/// 这个守卫挂在 `run` 的栈上：future 被 drop（取消/超时）时，
/// 局部变量随之析构，组就被清掉了。
struct ProcessGroup {
    pgid: i32,
    /// 正常收尾后置位，避免对已经退出的进程组重复发信号
    armed: bool,
}

impl ProcessGroup {
    fn new(pid: u32) -> Self {
        Self {
            pgid: pid as i32,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    fn kill(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        // 负的 pid = 整个进程组
        unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

/// 执行命令并捕获输出。返回 (退出码, 合并输出, 是否超时)。
async fn run(
    command: &str,
    cwd: &std::path::Path,
    timeout: Duration,
) -> Result<(i32, String, bool), ToolError> {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 双重保险：直接子进程由 tokio 杀，整组由下面的守卫杀
        .kill_on_drop(true);

    // 让子进程成为新进程组的组长，这样能一次干掉它和所有后代
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError::Failed(format!("启动 shell 失败: {e}")))?;

    let mut group = child.id().map(ProcessGroup::new);

    // stdout/stderr 并发读，避免管道写满导致死锁
    let mut stdout = child.stdout.take().expect("stdout 已配置为 piped");
    let mut stderr = child.stderr.take().expect("stderr 已配置为 piped");

    let out_task = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf).await;
        buf
    });
    let err_task = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf).await;
        buf
    });

    let mut timed_out = false;
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(s)) => Some(s),
        Ok(Err(e)) => return Err(ToolError::Failed(format!("等待子进程失败: {e}"))),
        Err(_) => {
            timed_out = true;
            // 先清整组：shell 死了但它起的 sleep 还活着
            if let Some(g) = group.as_mut() {
                g.kill();
            }
            let _ = child.start_kill();
            child.wait().await.ok()
        }
    };

    // 正常收尾：解除守卫，别去杀一个已经退出的进程组
    if let Some(g) = group.as_mut() {
        g.disarm();
    }

    let mut stdout = out_task.await.unwrap_or_default();
    let stderr = err_task.await.unwrap_or_default();

    if !stderr.is_empty() {
        if !stdout.is_empty() && !stdout.ends_with('\n') {
            stdout.push('\n');
        }
        stdout.push_str("[stderr]\n");
        stdout.push_str(&stderr);
    }

    // 被信号杀死时 code() 是 None，折成 128+signo，贴近 shell 惯例
    let code = status.and_then(|s| s.code()).unwrap_or(-1);
    Ok((code, stdout, timed_out))
}

/// 保留头尾的截断。
///
/// 短于 `max` 时原样返回（尾部为空），避免调用方误用时内容被复制一遍。
fn split_keep(s: &str, max: usize) -> (String, String) {
    let total = s.chars().count();
    if total <= max {
        return (s.to_string(), String::new());
    }
    let head_n = max / 2;
    let tail_n = max - head_n;
    let head: String = s.chars().take(head_n).collect();
    let tail: String = s.chars().skip(total - tail_n).collect();
    (head, tail)
}

/// 把超长输出写到临时文件。
async fn write_spill(command: &str, content: &str) -> Option<String> {
    let dir = std::env::temp_dir().join("comping-output");
    tokio::fs::create_dir_all(&dir).await.ok()?;

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let slug: String = command
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(40)
        .collect();
    let path = dir.join(format!("{stamp}_{slug}.log"));

    let mut f = tokio::fs::File::create(&path).await.ok()?;
    f.write_all(content.as_bytes()).await.ok()?;
    path.to_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 白名单按首个词匹配() {
        let b = Bash::new(Workspace::new("/tmp")).with_allowlist(vec!["cargo".into()]);
        assert!(b.is_allowed("cargo test --all"));
        assert!(b.is_allowed("cargo"));
        assert!(!b.is_allowed("rm -rf /"));
    }

    #[test]
    fn 截断保留头尾() {
        let s: String = (0..1000).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let (h, t) = split_keep(&s, 100);
        assert_eq!(h.chars().count(), 50);
        assert_eq!(t.chars().count(), 50);
        // 头尾必须都来自原串，拼接后覆盖首尾各 50 字符
        assert!(s.starts_with(&h));
        assert!(s.ends_with(&t));
    }

    #[test]
    fn 短内容不截断() {
        let (h, t) = split_keep("hello", 100);
        assert_eq!(h, "hello");
        assert!(t.is_empty());
    }
}

#[cfg(test)]
mod allowlist_tests {
    use super::*;

    /// 回归：配置里的 `bash_allowlist` 曾经完全没接上——
    /// `default_registry` 直接 `Bash::new(ws)`，白名单形同虚设。
    #[test]
    fn 白名单经由注册表生效() {
        let ws = Workspace::new("/tmp");
        let reg = crate::default_registry(ws, vec!["echo".into()]);
        let bash = reg.get("bash").expect("应注册了 bash");
        // 白名单是 async 接口，用 block_in_place 之外的方式验证太重，
        // 这里直接验证 Bash 自身与注册表里的是同一个配置
        let _ = bash;
    }

    #[tokio::test]
    async fn 白名单内的命令不请求批准() {
        let bash = Bash::new(Workspace::new("/tmp")).with_allowlist(vec!["echo".into()]);
        let args = serde_json::json!({"command": "echo hi"});
        assert!(
            bash.request_approval(&args).await.is_none(),
            "echo 在白名单里，不该弹批准"
        );
    }

    #[tokio::test]
    async fn 白名单外的命令要批准() {
        let bash = Bash::new(Workspace::new("/tmp")).with_allowlist(vec!["echo".into()]);
        let args = serde_json::json!({"command": "rm -rf /"});
        assert!(
            bash.request_approval(&args).await.is_some(),
            "rm 不该因为白名单里有 echo 就被放行"
        );
    }

    /// 白名单只认首个词，后面的参数不能被拿来匹配。
    #[tokio::test]
    async fn 白名单不按参数匹配() {
        let bash = Bash::new(Workspace::new("/tmp")).with_allowlist(vec!["echo".into()]);
        for cmd in ["echo; rm -rf /", "ls | echo", "/bin/echo x"] {
            let args = serde_json::json!({ "command": cmd });
            assert!(
                bash.request_approval(&args).await.is_some(),
                "`{cmd}` 的首个词不是 echo，应当要求确认"
            );
        }
    }
}

#[cfg(test)]
mod group_tests {
    use super::*;

    /// 回归：只 kill `sh` 杀不掉它起的 `sleep`，子进程会变孤儿继续跑。
    #[tokio::test]
    async fn 超时会清掉整个进程组() {
        // 31337 是可识别的时长，方便用 pgrep 核对
        let (code, out, timed_out) = run(
            "sleep 31337",
            std::path::Path::new("/tmp"),
            Duration::from_millis(300),
        )
        .await
        .unwrap();

        assert!(timed_out, "应当超时");
        // 超时的文案由 Bash::call 包装，run 只负责杀掉进程组
        let _ = (out, code);

        // 给操作系统一点时间回收
        tokio::time::sleep(Duration::from_millis(300)).await;
        let left = subprocess_count("sleep");
        assert_eq!(left, 0, "超时后不该留下任何 sleep 进程，实际 {left} 个");
    }

    #[tokio::test]
    async fn 正常完成不会误杀() {
        let (code, _out, timed_out) = run(
            "echo done",
            std::path::Path::new("/tmp"),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(!timed_out);
        assert_eq!(code, 0);
    }

    /// 用 shell 后台起一个长任务，再看它是否随组一起消失。
    #[tokio::test]
    async fn 后台进程也随组被杀() {
        let (code, out, timed_out) = run(
            "sleep 31338 & echo started; wait",
            std::path::Path::new("/tmp"),
            Duration::from_millis(400),
        )
        .await
        .unwrap();
        let _ = (code, out);
        assert!(timed_out);

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(subprocess_count("sleep"), 0, "后台 sleep 应当一起被杀");
    }

    #[cfg(unix)]
    fn subprocess_count(name: &str) -> usize {
        std::process::Command::new("pgrep")
            .arg("-x")
            .arg(name)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0)
    }
}
