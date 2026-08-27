//! `lnms_invoice` 库入口
//!
//! 阶段 1 骨架:模块占位,业务逻辑在后续阶段填充。
//! 详见 [`DESIGN.md`](../DESIGN.md) 阶段划分。

pub mod config;
pub mod error;
pub mod store;
pub mod librenms;
pub mod domain;
pub mod template;
pub mod runner;
pub mod chart;
pub mod render;
pub mod web;
pub mod billing;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 重新导出常用类型,便于调用方 `use lnms_invoice::*;`
pub use error::{Error, Result};
