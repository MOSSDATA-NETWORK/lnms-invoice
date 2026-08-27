//! 模块级错误定义(`thiserror`)。
//! `anyhow` 在 `main` / `bin` 顶层收口,提供友好的 `:#` 链式输出。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("config error: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("toml parse error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("env var error: {0}")]
    Env(#[from] std::env::VarError),

    #[error("database error: {0}")]
    Database(String),

    #[error("librenms api error: {0}")]
    LibreNms(String),

    #[error("template error: {0}")]
    Template(String),

    #[error("not implemented yet: {0}")]
    NotImplemented(&'static str),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid state transition: {0}")]
    InvalidTransition(String),

    #[error("already exists: {0}")]
    AlreadyExists(String),

    #[error("internal: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, Error>;
