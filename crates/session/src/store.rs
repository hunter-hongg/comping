//! 会话持久化。
//!
//! 每个会话一个 JSONL 文件，**追加写**：每条记录是一个事件快照。
//! 好处是崩溃后不丢已完成的部分，读的时候只要扫一遍重建。

use std::path::{Path, PathBuf};

use agent_core::{Conversation, Message, Usage};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 新会话的占位标题。
pub const DEFAULT_TITLE: &str = "新会话";

/// 会话元信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub created_at: u64,
    pub updated_at: u64,
    /// 使用过的 provider / 模型，写进元信息方便 `--resume` 复现环境。
    pub provider: String,
    pub model: String,
}

/// 一行记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    Meta(SessionMeta),
    /// 追加一条消息。
    Message { message: Message },
    /// 记一次用量汇总。
    Usage { usage: Usage },
}

/// 一个会话文件。
#[derive(Debug, Clone)]
pub struct Session {
    path: PathBuf,
    meta: SessionMeta,
    messages: Vec<Message>,
}

impl Session {
    /// 在 `dir` 下新建一个会话文件。
    pub async fn open(dir: &Path, id: Option<&str>, provider: &str, model: &str) -> Result<Self> {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("创建会话目录 {}", dir.display()))?;

        let id = match id {
            Some(i) => i.to_string(),
            None => new_id(),
        };
        let path = dir.join(format!("{id}.jsonl"));

        let now = now_secs();
        let meta = SessionMeta {
            id,
            title: DEFAULT_TITLE.into(),
            created_at: now,
            updated_at: now,
            provider: provider.to_string(),
            model: model.to_string(),
        };

        Ok(Self {
            path,
            meta,
            messages: Vec::new(),
        })
    }

    /// 新建后立刻把元信息写进文件。
    ///
    /// 没有这条 `meta` 行的话，`load` 无法重建会话，`--resume` 直接失败。
    pub async fn create(dir: &Path, provider: &str, model: &str) -> Result<Self> {
        let s = Self::open(dir, None, provider, model).await?;
        s.write(&Record::Meta(s.meta.clone())).await?;
        Ok(s)
    }

    /// 载入已有会话文件。
    pub async fn load(path: &Path) -> Result<Self> {
        let text = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("读取会话 {}", path.display()))?;

        let mut meta = None;
        let mut messages = Vec::new();

        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            // 单行损坏不该让整个会话打不开
            let Ok(rec) = serde_json::from_str::<Record>(line) else {
                tracing::warn!(path = %path.display(), line = i, "跳过无法解析的记录");
                continue;
            };
            match rec {
                Record::Meta(m) => meta = Some(m),
                Record::Message { message } => messages.push(message),
                _ => {}
            }
        }

        let meta = meta.ok_or_else(|| anyhow::anyhow!("会话文件缺少元信息: {}", path.display()))?;

        Ok(Self {
            path: path.to_path_buf(),
            meta,
            messages,
        })
    }

    pub fn meta(&self) -> &SessionMeta {
        &self.meta
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// 把已有消息转成可继续的会话。
    pub fn to_conversation(&self, budget: agent_core::ContextBudget) -> Conversation {
        let mut c = Conversation::new(budget);
        c.messages = self.messages.clone();
        c
    }

    /// 追加一条消息并落盘。
    pub async fn append(&mut self, message: Message) -> Result<()> {
        let rec = Record::Message {
            message: message.clone(),
        };
        self.messages.push(message);
        self.write(&rec).await
    }

    /// 记一次用量。
    pub async fn record_usage(&mut self, usage: &Usage) -> Result<()> {
        self.write(&Record::Usage { usage: *usage }).await
    }

    /// 改标题。只在标题还是占位符时才生效，且必须落盘——否则下次 `-l` 看到的还是「新会话」。
    pub async fn retitle(&mut self, title: impl Into<String>) -> Result<()> {
        let t = title.into();
        if t.trim().is_empty() {
            return Ok(());
        }
        if self.meta.title != DEFAULT_TITLE {
            return Ok(());
        }
        self.meta.title = t;
        self.rewrite_meta().await
    }

    /// 重写文件首行的 meta 记录。
    ///
    /// JSONL 是追加写的，改元信息只能"重写首行 + 保留其余"。
    /// 行数不多，整体读回来再写回去最不容易出错。
    async fn rewrite_meta(&self) -> Result<()> {
        let existing = tokio::fs::read_to_string(&self.path).await.unwrap_or_default();

        let mut body = String::new();
        let mut seen_meta = false;
        for line in existing.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if !seen_meta && serde_json::from_str::<Record>(line).is_ok_and(|r| matches!(r, Record::Meta(_))) {
                seen_meta = true;
                continue; // 用新值替换
            }
            body.push_str(line);
            body.push('\n');
        }

        let head = serde_json::to_string(&Record::Meta(self.meta.clone()))?;
        let mut out = String::with_capacity(head.len() + body.len() + 1);
        out.push_str(&head);
        out.push('\n');
        out.push_str(&body);

        tokio::fs::write(&self.path, out).await?;
        Ok(())
    }

    async fn write(&self, rec: &Record) -> Result<()> {
        let line = serde_json::to_string(rec)?;
        let mut f = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .with_context(|| format!("打开会话文件 {}", self.path.display()))?;
        use tokio::io::AsyncWriteExt;
        f.write_all(line.as_bytes()).await?;
        f.write_all(b"\n").await?;
        // 必须显式 flush：tokio 的 File 在 drop 时才在后台刷缓冲，且不等待完成。
        // 不 flush 的话 write 返回 Ok 时数据可能还没落盘。
        f.flush().await?;
        Ok(())
    }

    /// 列出目录下所有会话，按更新时间倒序。
    pub async fn list(dir: &Path) -> Result<Vec<SessionMeta>> {
        let mut out = Vec::new();
        let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
            return Ok(out);
        };
        while let Ok(Some(ent)) = rd.next_entry().await {
            if ent.path().extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Ok(s) = Session::load(&ent.path()).await {
                out.push(s.meta);
            }
        }
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(out)
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::Message;

    /// 每个测试用独立目录：测试默认并发跑，共用固定路径会互相覆盖。
    fn tmpdir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("comping-test-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn 新会话_创建后能立刻载回() {
        let dir = tmpdir("create");
        let s = Session::create(&dir, "mock", "m1").await.unwrap();
        let path = s.path().to_path_buf();
        drop(s);

        // 这正是之前 --resume 失败的原因：没有 meta 行
        let loaded = Session::load(&path).await.unwrap();
        assert_eq!(loaded.meta().id, loaded.meta().id);
        assert_eq!(loaded.meta().provider, "mock");
    }

    #[tokio::test]
    async fn 追加的消息能被载回() {
        let dir = tmpdir("append");
        let mut s = Session::create(&dir, "mock", "m1").await.unwrap();
        let path = s.path().to_path_buf();

        s.append(Message::user("第一句")).await.unwrap();
        s.append(Message::assistant("第二句")).await.unwrap();
        s.append(Message::user("第三句")).await.unwrap();
        drop(s);

        let loaded = Session::load(&path).await.unwrap();
        let roles: Vec<_> = loaded.messages().iter().map(|m| m.role).collect();
        assert_eq!(roles.len(), 3);
        assert_eq!(loaded.messages()[0].content, "第一句");
        assert_eq!(loaded.messages()[2].content, "第三句");
    }

    #[tokio::test]
    async fn 改标题只改一次且不影响消息() {
        let dir = tmpdir("retitle");
        let mut s = Session::create(&dir, "mock", "m1").await.unwrap();
        let path = s.path().to_path_buf();

        s.append(Message::user("原始问题")).await.unwrap();
        s.retitle("原始问题").await.unwrap();
        // 第二次改标题应被忽略
        s.retitle("不该生效").await.unwrap();
        s.append(Message::assistant("回答")).await.unwrap();
        drop(s);

        let loaded = Session::load(&path).await.unwrap();
        assert_eq!(loaded.meta().title, "原始问题");
        assert_eq!(loaded.messages().len(), 2);
        assert_eq!(loaded.messages()[1].content, "回答");
    }

    #[tokio::test]
    async fn 损坏的行被跳过而不影响其余() {
        let dir = tmpdir("corrupt");
        let mut s = Session::create(&dir, "mock", "m1").await.unwrap();
        let path = s.path().to_path_buf();
        s.append(Message::user("好的行")).await.unwrap();
        drop(s);

        // 手动插一行垃圾
        let mut text = std::fs::read_to_string(&path).unwrap();
        assert!(text.ends_with('\n'), "写入的每行都应以换行结尾");
        text.push_str("{这不是合法 json\n");
        text.push_str(&format!("{}\n", serde_json::to_string(&Record::Message {
            message: Message::user("垃圾之后的行"),
        }).unwrap()));
        std::fs::write(&path, text).unwrap();

        let loaded = Session::load(&path).await.unwrap();
        let contents: Vec<_> = loaded.messages().iter().map(|m| &m.content).collect();
        assert_eq!(contents, vec!["好的行", "垃圾之后的行"]);
    }

    #[tokio::test]
    async fn 列表按更新时间倒序() {
        let dir = tmpdir("list");
        Session::create(&dir, "a", "m").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        Session::create(&dir, "b", "m").await.unwrap();

        let list = Session::list(&dir).await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].updated_at >= list[1].updated_at, true);
    }
}

/// 会话 id：时间戳 + 随机后缀，人眼可辨且不撞。
fn new_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!("{:x}{}", d.as_secs(), &suffix[..6])
}
