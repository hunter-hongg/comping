//! 文件写入与精确替换。
//!
//! 两个工具都要求用户确认：写入会把已有内容覆盖掉，替换会改动源码。
//! 批准面板里给模型看到的是最终内容或 diff，不是一句「确认执行」。

use agent_core::{PendingApproval, Tool, ToolError, ToolOutcome, object_schema};
use async_trait::async_trait;
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};

use crate::workspace::{Workspace, display_relative};

/// 单次写入的新增内容上限，防止模型一次灌 10 万行。
const MAX_WRITE_BYTES: usize = 2 * 1024 * 1024;

pub struct WriteFile {
    ws: Workspace,
}

impl WriteFile {
    pub fn new(ws: Workspace) -> Self {
        Self { ws }
    }
}

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "把给定内容完整写入工作区内的文件，自动创建父目录。会覆盖已有内容——要局部修改请用 edit_file。"
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "path": { "type": "string", "description": "相对工作区根目录的路径" },
                "content": { "type": "string", "description": "文件的完整新内容" }
            }),
            &["path", "content"],
        )
    }

    fn preview(&self, args: &Value) -> String {
        let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("<无路径>");
        let content = args.get("content").and_then(|p| p.as_str()).unwrap_or("");
        let lines = content.lines().count();
        format!("{path} ({lines} 行, {} 字节)", content.len())
    }

    async fn request_approval(&self, args: &Value) -> Option<PendingApproval> {
        let path = args.get("path")?.as_str()?;
        let content = args.get("content")?.as_str()?;
        let abs = self.ws.resolve(path).ok()?;

        let existing = tokio::fs::read_to_string(&abs).await.unwrap_or_default();
        let (title, body) = if existing.is_empty() {
            (
                format!("新建 {path}"),
                format!("+{} 行\n\n{}", content.lines().count(), preview_text(content)),
            )
        } else {
            (
                format!("覆盖 {path}"),
                format!(
                    "现有 {} 行 → 新 {} 行\n\n{}",
                    existing.lines().count(),
                    content.lines().count(),
                    diff_text(&existing, content)
                ),
            )
        };
        Some(PendingApproval { title, body })
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        let raw = args
            .get("path")
            .and_then(|p| p.as_str())
            .ok_or_else(|| ToolError::InvalidParams("缺少 `path`".into()))?;
        let content = args
            .get("content")
            .and_then(|p| p.as_str())
            .ok_or_else(|| ToolError::InvalidParams("缺少 `content`".into()))?;

        if content.len() > MAX_WRITE_BYTES {
            return Err(ToolError::InvalidParams(format!(
                "内容 {} 字节超过上限 {}",
                content.len(),
                MAX_WRITE_BYTES
            )));
        }

        let path = self
            .ws
            .resolve(raw)
            .map_err(|e| ToolError::OutsideWorkspace(e.to_string()))?;

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ToolError::Failed(format!("创建目录失败: {e}")))?;
        }

        let existed = path.exists();
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| ToolError::Failed(format!("写入 {raw} 失败: {e}")))?;

        let rel = display_relative(self.ws.root(), &path);
        let lines = content.lines().count();
        let verb = if existed { "覆盖写入" } else { "新建" };
        Ok(ToolOutcome::ok(
            format!("{verb} {rel} ({lines} 行)"),
            format!("{verb}成功: {rel}，共 {lines} 行。"),
        ))
    }
}

/// 局部替换。
pub struct EditFile {
    ws: Workspace,
}

impl EditFile {
    pub fn new(ws: Workspace) -> Self {
        Self { ws }
    }
}

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "在工作区文件里把 old_string 精确替换成 new_string。old_string 必须唯一出现，否则报错。多处要改就多次调用。"
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "path": { "type": "string", "description": "相对工作区根目录的路径" },
                "old_string": { "type": "string", "description": "要被替换的原文，必须在文件中唯一出现" },
                "new_string": { "type": "string", "description": "替换后的新文本" }
            }),
            &["path", "old_string", "new_string"],
        )
    }

    fn preview(&self, args: &Value) -> String {
        let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("<无路径>");
        let old = args.get("old_string").and_then(|p| p.as_str()).unwrap_or("");
        let new = args.get("new_string").and_then(|p| p.as_str()).unwrap_or("");
        format!("{path}: {} → {}", one_line(old), one_line(new))
    }

    async fn request_approval(&self, args: &Value) -> Option<PendingApproval> {
        let path = args.get("path")?.as_str()?;
        let old = args.get("old_string")?.as_str()?;
        let new = args.get("new_string")?.as_str()?;
        Some(PendingApproval {
            title: self.preview(args),
            body: format!("{path}\n-{}\n+{}", preview_text(old), preview_text(new)),
        })
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        let raw = args
            .get("path")
            .and_then(|p| p.as_str())
            .ok_or_else(|| ToolError::InvalidParams("缺少 `path`".into()))?;
        let old = args
            .get("old_string")
            .and_then(|p| p.as_str())
            .ok_or_else(|| ToolError::InvalidParams("缺少 `old_string`".into()))?;
        let new = args
            .get("new_string")
            .and_then(|p| p.as_str())
            .ok_or_else(|| ToolError::InvalidParams("缺少 `new_string`".into()))?;

        if old == new {
            return Err(ToolError::InvalidParams("old_string 与 new_string 相同".into()));
        }

        let path = self
            .ws
            .resolve(raw)
            .map_err(|e| ToolError::OutsideWorkspace(e.to_string()))?;

        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::NotFound(format!("{raw}: {e}")))?;

        let hits = count_occurrences(&content, old);
        match hits {
            0 => {
                return Err(ToolError::Failed(format!(
                    "在 {raw} 中找不到 old_string。请先 read_file 确认当前内容（文件可能已被改动）。"
                )));
            }
            1 => {}
            n => {
                return Err(ToolError::Failed(format!(
                    "old_string 在 {raw} 中出现 {n} 次，无法确定改哪一处。请加入更多上下文让它唯一，\
                     或改用 write_file 重写整个文件。候选行号：{}",
                    locate_lines(&content, old)
                        .iter()
                        .map(|l| l.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        }

        let updated = content.replacen(old, new, 1);
        tokio::fs::write(&path, &updated)
            .await
            .map_err(|e| ToolError::Failed(format!("写入 {raw} 失败: {e}")))?;

        let rel = display_relative(self.ws.root(), &path);
        let delta = new.lines().count() as i64 - old.lines().count() as i64;
        let summary = match delta {
            0 => format!("编辑 {rel}"),
            d if d > 0 => format!("编辑 {rel} (+{d} 行)"),
            d => format!("编辑 {rel} ({d} 行)"),
        };
        Ok(ToolOutcome::ok(summary, format!("替换成功: {rel}")))
    }
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack.matches(needle).count()
}

/// 每个匹配位置所在的行号（1 起）。
fn locate_lines(content: &str, needle: &str) -> Vec<usize> {
    content
        .match_indices(needle)
        // content[..i] 的行数 = 已出现的换行符数，再 +1 得到 1 起的行号
        .map(|(byte_idx, _)| content[..byte_idx].lines().count() + 1)
        .collect()
}

fn preview_text(s: &str) -> String {
    const MAX: usize = 1200;
    if s.len() <= MAX {
        s.to_string()
    } else {
        format!("{}…[截断]", &s[..MAX])
    }
}

fn one_line(s: &str) -> String {
    let t = s.lines().next().unwrap_or("").trim();
    if t.chars().count() > 60 {
        format!("{}…", t.chars().take(60).collect::<String>())
    } else {
        t.to_string()
    }
}

/// 生成带前后文的行级 diff，用于批准面板。
pub fn diff_text(old: &str, new: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut out = String::new();

    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        out.push_str(sign);
        out.push_str(change.value().trim_end_matches('\n'));
        out.push('\n');
    }

    if out.is_empty() {
        "(无变化)".into()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 出现多次时报出行号() {
        let c = "a\nX\nb\nX\nc";
        assert_eq!(count_occurrences(c, "X"), 2);
        assert_eq!(locate_lines(c, "X"), vec![2, 4]);
    }

    #[test]
    fn 不出现时报错信息可用() {
        assert_eq!(count_occurrences("abc", "z"), 0);
    }

    #[test]
    fn diff_标出增删() {
        let d = diff_text("a\nb\nc", "a\nB\nc");
        assert!(d.contains("-b"));
        assert!(d.contains("+B"));
        assert!(d.contains(" a"));
    }
}
