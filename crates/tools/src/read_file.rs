//! 文件读取工具。
//!
//! 输出带行号，因为后续 `edit_file` 的报错要靠行号让模型精确定位。

use agent_core::{Tool, ToolError, ToolOutcome, object_schema};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::workspace::{Workspace, display_relative};

/// 超过这个行数就提示模型用 offset/limit 分段读，别一次性灌爆上下文。
const LARGE_FILE_LINES: usize = 2000;

pub struct ReadFile {
    ws: Workspace,
}

impl ReadFile {
    pub fn new(ws: Workspace) -> Self {
        Self { ws }
    }
}

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "读取工作区内一个文本文件，返回带行号的内容。文件很大时用 offset/limit 分段读取。"
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "path": { "type": "string", "description": "相对工作区根目录的路径" },
                "offset": { "type": "integer", "description": "起始行（1 起），默认 1", "minimum": 1 },
                "limit": { "type": "integer", "description": "最多读多少行，默认全部", "minimum": 1 }
            }),
            &["path"],
        )
    }

    fn preview(&self, args: &Value) -> String {
        args.get("path")
            .and_then(|p| p.as_str())
            .unwrap_or("<无路径>")
            .to_string()
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        let raw = args
            .get("path")
            .and_then(|p| p.as_str())
            .ok_or_else(|| ToolError::InvalidParams("缺少 `path`".into()))?;

        let path = self
            .ws
            .resolve(raw)
            .map_err(|e| ToolError::OutsideWorkspace(e.to_string()))?;

        let meta = tokio::fs::metadata(&path)
            .await
            .map_err(|e| ToolError::NotFound(format!("{raw}: {e}")))?;

        if meta.is_dir() {
            return Err(ToolError::InvalidParams(format!(
                "{raw} 是目录，请用 list_dir"
            )));
        }

        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::Failed(format!("读取 {raw} 失败: {e}")))?;

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();

        let offset = args
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(1) as usize;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(usize::MAX);

        let start = offset.saturating_sub(1).min(total);
        let end = (start + limit).min(total);

        // 行号按实际宽度右对齐，方便模型数位数
        let width = end.to_string().len();
        let mut out = String::with_capacity((end - start) * (width + content.len() / total.max(1) + 2));
        for (i, line) in lines[start..end].iter().enumerate() {
            out.push_str(&format!("{:>w$} | {}\n", start + i + 1, line, w = width));
        }

        let rel = display_relative(self.ws.root(), &path);
        let mut summary = format!("{rel} ({total} 行)");
        if end < total {
            summary.push_str(&format!(", 返回 {}-{}", start + 1, end));
        }
        if total > LARGE_FILE_LINES && limit == usize::MAX {
            out.push_str(&format!(
                "\n[文件共 {total} 行，已全部返回。若只需局部，请用 offset/limit。]\n"
            ));
        }

        Ok(ToolOutcome::ok(summary, out))
    }
}
