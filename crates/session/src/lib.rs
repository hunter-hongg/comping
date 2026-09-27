//! `session` —— 配置与会话持久化。

pub mod config;
pub mod store;

pub use config::{AgentSection, Config, Providers, UiSection};
pub use store::{Record, Session, SessionMeta};

/// 会话存放目录：`$XDG_DATA_HOME/comping/sessions`。
pub fn sessions_dir() -> std::path::PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("comping")
        .join("sessions")
}
