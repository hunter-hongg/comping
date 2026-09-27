//! 路径解析：所有文件工具都要过这一层，防止模型跑到工作区外面乱写。

use std::path::{Path, PathBuf};

use anyhow::Result;

/// 工作区根。
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            root: root.canonicalize().unwrap_or(root),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 把模型给的相对路径解析成绝对路径，并确保不越界。
    ///
    /// 模型经常给绝对路径或 `../../etc/passwd`，这里统一收敛。
    pub fn resolve(&self, raw: &str) -> Result<PathBuf> {
        let raw = raw.trim();
        if raw.is_empty() {
            anyhow::bail!("路径为空");
        }

        let candidate = if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            self.root.join(raw)
        };

        // 词法规范化：消掉 `.` 和 `..`，不要求路径已存在
        let normalized = normalize(&candidate);

        if !normalized.starts_with(&self.root) {
            anyhow::bail!(
                "路径 `{}` 超出工作区范围（根目录 {}）",
                raw,
                self.root.display()
            );
        }
        Ok(normalized)
    }
}

/// 纯词法的路径规范化。
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        out
    }
}

/// 输出给模型看的相对路径，减少 token 浪费。
pub fn display_relative(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 相对路径落在工作区内() {
        let ws = Workspace::new("/tmp/ws");
        assert_eq!(ws.resolve("src/main.rs").unwrap(), PathBuf::from("/tmp/ws/src/main.rs"));
    }

    #[test]
    fn 点号被消掉() {
        let ws = Workspace::new("/tmp/ws");
        assert_eq!(ws.resolve("./a/./b").unwrap(), PathBuf::from("/tmp/ws/a/b"));
    }

    #[test]
    fn 越界被拒绝() {
        let ws = Workspace::new("/tmp/ws");
        assert!(ws.resolve("../../etc/passwd").is_err());
    }

    #[test]
    fn 工作区内的上跳不越界() {
        let ws = Workspace::new("/tmp/ws");
        assert!(ws.resolve("a/../b").is_ok());
    }

    #[test]
    fn 上跳到根目录本身是允许的() {
        let ws = Workspace::new("/tmp/ws");
        // 恰好回到根，不算越界
        assert_eq!(ws.resolve("a/..").unwrap(), PathBuf::from("/tmp/ws"));
    }

    #[test]
    fn 绝对路径越界被拒绝() {
        let ws = Workspace::new("/tmp/ws");
        assert!(ws.resolve("/etc/passwd").is_err());
        assert!(ws.resolve("/tmp").is_err());
        // 根目录自身的绝对路径应当允许
        assert!(ws.resolve("/tmp/ws").is_ok());
    }

    #[test]
    fn 同前缀的不同目录不算在工作区内() {
        // /tmp/ws-evil 以 /tmp/ws 开头，但 components 不同，必须拒绝
        let ws = Workspace::new("/tmp/ws");
        assert!(ws.resolve("/tmp/ws-evil/secret").is_err());
    }

    #[test]
    fn 空路径被拒绝() {
        let ws = Workspace::new("/tmp/ws");
        assert!(ws.resolve("").is_err());
        assert!(ws.resolve("   ").is_err());
    }

    #[test]
    fn 深层上跳不能逃出() {
        let ws = Workspace::new("/tmp/ws");
        assert!(ws.resolve("a/b/c/../../../../../../etc").is_err());
    }

    #[test]
    fn 显示相对路径() {
        assert_eq!(
            display_relative(Path::new("/tmp/ws"), Path::new("/tmp/ws/a/b.txt")),
            "a/b.txt"
        );
    }
}
