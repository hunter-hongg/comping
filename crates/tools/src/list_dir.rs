//! 目录列举与内容检索。
//!
//! `list_dir` 负责"先看清项目长什么样"，`grep` 负责"按模式找代码"，
//! 两者都是只读的，注册为 Safe 等级、自动执行。

use agent_core::{Tool, ToolError, ToolOutcome, object_schema};
use async_trait::async_trait;
use globset::{Glob, GlobMatcher};
use serde_json::{Value, json};

use crate::workspace::{Workspace, display_relative};

/// 一次列举最多返回多少项，剩下的提示模型换更精确的 path。
const MAX_ENTRIES: usize = 500;

/// 永远跳过的目录，省 token 也避免误读构建产物。
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".next",
    ".cache",
];

pub struct ListDir {
    ws: Workspace,
}

impl ListDir {
    pub fn new(ws: Workspace) -> Self {
        Self { ws }
    }
}

#[async_trait]
impl Tool for ListDir {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "列出工作区内某个目录的内容。path 省略时列根目录。可用 glob 过滤，如 `src/**/*.rs`。"
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "path": { "type": "string", "description": "相对工作区根目录的目录路径，默认为根目录" },
                "glob": { "type": "string", "description": "可选的 glob 过滤，如 `**/*.rs`" },
                "recursive": { "type": "boolean", "description": "是否递归，默认 false" }
            }),
            &[],
        )
    }

    fn preview(&self, args: &Value) -> String {
        args.get("path")
            .and_then(|p| p.as_str())
            .unwrap_or(".")
            .to_string()
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        let raw = args.get("path").and_then(|p| p.as_str()).unwrap_or(".");
        let dir = self
            .ws
            .resolve(raw)
            .map_err(|e| ToolError::OutsideWorkspace(e.to_string()))?;

        let meta = tokio::fs::metadata(&dir)
            .await
            .map_err(|e| ToolError::NotFound(format!("{raw}: {e}")))?;
        if !meta.is_dir() {
            return Err(ToolError::InvalidParams(format!(
                "{raw} 不是目录，用 read_file 读它"
            )));
        }

        let recursive = args
            .get("recursive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let matcher = build_matcher(args.get("glob").and_then(|g| g.as_str()))?;

        let entries = walk(&dir, recursive, matcher.as_ref(), 0).await?;
        let rel_dir = display_relative(self.ws.root(), &dir);

        if entries.is_empty() {
            return Ok(ToolOutcome::ok(
                format!("{rel_dir} (空)"),
                "没有匹配的条目。".to_string(),
            ));
        }

        let total = entries.len();
        let shown: Vec<&Entry> = entries.iter().take(MAX_ENTRIES).collect();

        let mut out = String::new();
        for e in &shown {
            out.push_str(&e.render(&dir));
        }
        if total > shown.len() {
            out.push_str(&format!(
                "\n[共 {total} 项，已显示 {} 项。请用 glob 缩小范围。]\n",
                shown.len()
            ));
        }

        Ok(ToolOutcome::ok(
            format!("{rel_dir} ({total} 项)"),
            out,
        ))
    }
}

struct Entry {
    path: std::path::PathBuf,
    is_dir: bool,
    size: u64,
}

impl Entry {
    fn render(&self, base: &std::path::Path) -> String {
        let rel = display_relative(base, &self.path);
        if self.is_dir {
            format!("{rel}/")
        } else {
            format!("{rel}  ({} 字节)", human_size(self.size))
        }
    }
}

/// 递归深度上限，防止误扫超大目录。
const MAX_DEPTH: usize = 6;

fn walk<'a>(
    dir: &'a std::path::Path,
    recursive: bool,
    matcher: Option<&'a GlobMatcher>,
    depth: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Entry>, ToolError>> + Send + 'a>> {
    Box::pin(walk_inner(dir, recursive, matcher, depth))
}

async fn walk_inner(
    dir: &std::path::Path,
    recursive: bool,
    matcher: Option<&GlobMatcher>,
    depth: usize,
) -> Result<Vec<Entry>, ToolError> {
    if depth > MAX_DEPTH {
        return Ok(Vec::new());
    }

    let mut rd = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| ToolError::Failed(format!("读取目录失败: {e}")))?;

    let mut out = Vec::new();
    while let Some(ent) = rd
        .next_entry()
        .await
        .map_err(|e| ToolError::Failed(format!("读取目录失败: {e}")))?
    {
        let name = ent.file_name().to_string_lossy().to_string();
        if SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }

        let meta = match ent.metadata().await {
            Ok(m) => m,
            // 断掉的符号链接之类的，跳过而不是整体失败
            Err(_) => continue,
        };
        let is_dir = meta.is_dir();
        let path = ent.path();

        // 目录只作为递归入口，不单独出现在结果里（除非是根目录本身）
        if is_dir {
            if recursive {
                let sub = walk(&path, recursive, matcher, depth + 1).await?;
                out.extend(sub);
            }
            continue;
        }

        if let Some(m) = matcher {
            if !m.is_match(&path) {
                continue;
            }
        }

        out.push(Entry {
            path,
            is_dir,
            size: meta.len(),
        });

        if out.len() > MAX_ENTRIES * 4 {
            break; // 硬停，最终输出时再截断
        }
    }

    // 排序让结果稳定：目录优先，再按名字
    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.path.file_name().cmp(&b.path.file_name()))
    });
    Ok(out)
}

fn build_matcher(glob: Option<&str>) -> Result<Option<GlobMatcher>, ToolError> {
    let Some(g) = glob else { return Ok(None) };
    let matcher = Glob::new(g)
        .map_err(|e| ToolError::InvalidParams(format!("glob `{g}` 无效: {e}")))?
        .compile_matcher();
    Ok(Some(matcher))
}

fn human_size(n: u64) -> String {
    const UNIT: u64 = 1024;
    if n < UNIT {
        return format!("{n}");
    }
    let mut v = n as f64;
    let mut i = 0;
    while v >= UNIT as f64 && i < 4 {
        v /= UNIT as f64;
        i += 1;
    }
    let suffix = ["B", "K", "M", "G", "T"][i];
    format!("{v:.1}{suffix}")
}

// ---------------------------------------------------------------- grep

/// ripgrep 风格的正则检索。
pub struct Grep {
    ws: Workspace,
}

impl Grep {
    pub fn new(ws: Workspace) -> Self {
        Self { ws }
    }
}

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "在工作区内按正则搜索文本，输出 `文件:行号:内容`。先 grep 定位，再 read_file 看上下文，比反复 read 更快。"
    }

    fn parameters(&self) -> Value {
        object_schema(
            json!({
                "pattern": { "type": "string", "description": "正则表达式（Rust regex 语法）" },
                "path": { "type": "string", "description": "限定搜索目录，默认为工作区根" },
                "glob": { "type": "string", "description": "只搜索匹配该 glob 的文件，如 `**/*.rs`" },
                "ignore_case": { "type": "boolean", "description": "忽略大小写，默认 false" },
                "max_results": { "type": "integer", "description": "最多返回多少行，默认 100", "minimum": 1 }
            }),
            &["pattern"],
        )
    }

    fn preview(&self, args: &Value) -> String {
        let p = args.get("pattern").and_then(|p| p.as_str()).unwrap_or("");
        let path = args.get("path").and_then(|p| p.as_str()).unwrap_or(".");
        format!("{p} in {path}")
    }

    async fn call(&self, args: Value) -> Result<ToolOutcome, ToolError> {
        let pattern = args
            .get("pattern")
            .and_then(|p| p.as_str())
            .ok_or_else(|| ToolError::InvalidParams("缺少 `pattern`".into()))?;

        let raw_path = args.get("path").and_then(|p| p.as_str()).unwrap_or(".");
        let root = self
            .ws
            .resolve(raw_path)
            .map_err(|e| ToolError::OutsideWorkspace(e.to_string()))?;

        let ignore_case = args
            .get("ignore_case")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let max = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(100) as usize;

        // regex 语法和 ripgrep 一致（Rust regex）
        let re = regex::RegexBuilder::new(pattern)
            .case_insensitive(ignore_case)
            .build()
            .map_err(|e| ToolError::InvalidParams(format!("正则无效: {e}")))?;

        let glob_matcher = build_matcher(args.get("glob").and_then(|g| g.as_str()))?;
        let mut files = Vec::new();
        collect_files(&root, glob_matcher.as_ref(), 0, &mut files).await?;

        let mut out = String::new();
        let mut hits = 0usize;
        let mut files_hit = 0usize;
        let mut truncated = false;

        for f in files {
            if truncated {
                break;
            }
            let Ok(content) = tokio::fs::read_to_string(&f).await else {
                continue;
            };
            // 二进制/超大文件直接跳过
            if content.contains('\0') || content.len() > 4 * 1024 * 1024 {
                continue;
            }

            let mut file_hits = 0usize;
            for (i, line) in content.lines().enumerate() {
                if !re.is_match(line) {
                    continue;
                }
                if hits >= max {
                    truncated = true;
                    break;
                }
                let rel = display_relative(self.ws.root(), &f);
                out.push_str(&format!("{rel}:{}: {}\n", i + 1, line.trim_end()));
                hits += 1;
                file_hits += 1;
            }
            if file_hits > 0 {
                files_hit += 1;
            }
        }

        if hits == 0 {
            return Ok(ToolOutcome::ok(
                format!("无匹配 ({pattern})"),
                format!("没有文件匹配 `{pattern}`。检查拼写，或放宽 glob。"),
            ));
        }

        if truncated {
            out.push_str(&format!(
                "\n[已达上限 {max} 行，匹配可能还有更多。缩小 path/glob 或细化 pattern。]\n"
            ));
        }

        let summary = if truncated {
            format!("{hits}+ 处匹配 / {files_hit} 个文件")
        } else {
            format!("{hits} 处匹配 / {files_hit} 个文件")
        };
        Ok(ToolOutcome::ok(summary, out))
    }
}

fn collect_files<'a>(
    dir: &'a std::path::Path,
    matcher: Option<&'a GlobMatcher>,
    depth: usize,
    out: &'a mut Vec<std::path::PathBuf>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ToolError>> + Send + 'a>> {
    Box::pin(collect_files_inner(dir, matcher, depth, out))
}

async fn collect_files_inner(
    dir: &std::path::Path,
    matcher: Option<&GlobMatcher>,
    depth: usize,
    out: &mut Vec<std::path::PathBuf>,
) -> Result<(), ToolError> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(_) => return Ok(()),
    };
    while let Ok(Some(ent)) = rd.next_entry().await {
        let name = ent.file_name().to_string_lossy().to_string();
        if SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        let Ok(meta) = ent.metadata().await else {
            continue;
        };
        if meta.is_dir() {
            collect_files(&ent.path(), matcher, depth + 1, out).await?;
        } else if let Some(m) = matcher {
            if m.is_match(&ent.path()) {
                out.push(ent.path());
            }
        } else {
            out.push(ent.path());
        }
    }
    Ok(())
}
