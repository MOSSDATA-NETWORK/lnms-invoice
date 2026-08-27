//! 配置加载:TOML 文件 + 环境变量覆盖。
//!
//! 加载顺序(后者覆盖前者):
//! 1. 内置默认值(`config/default.toml`)
//! 2. `LNMS_INVOICE_CONFIG` 环境变量指向的文件
//! 3. 系统环境变量(`LNMS_INVOICE_DB` / `LNMS_INVOICE_OUTPUT` / `LNMS_API_TOKEN` 等)
//!
//! 敏感凭据(API token)**绝不**写入 TOML,只走环境变量或 systemd `LoadCredentialEncrypted`。

use crate::error::{Error, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub libre_nms: LibreNmsConfig,
    pub billing: BillingConfig,
    pub output: OutputConfig,
    #[serde(default)]
    pub web: WebConfig,
}

/// Web 层配置。`session_secret` 不写入 TOML 仓库(详见 DESIGN.md 决策 #24),
/// 阶段 7 由 systemd LoadCredential 注入;本地 dev 从环境变��� `LNMS_INVOICE_SESSION_SECRET` 读。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct WebConfig {
    #[serde(default = "default_session_secret")]
    pub session_secret: String,
}

fn default_session_secret() -> String {
    String::new()
}

pub type Config = AppConfig;

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub bind: String,
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LibreNmsConfig {
    pub timeout_seconds: u64,
    pub retry_max: u32,
    pub sleep_ms_between_requests: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BillingConfig {
    /// 每月几号(1..=28,避免 2 月与大小月越界;UTC 时区跑一次,客户本地时区在账单上另行标注)
    pub run_at_day_of_month: u8,
    pub run_at_hour: u8,
    pub run_at_minute: u8,
    pub timezone: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OutputConfig {
    pub root: PathBuf,
    pub soffice_user_installation: PathBuf,
}

impl AppConfig {
    /// 测试用默认值(不走文件系统)。生产路径走 `load()`。
    pub fn default_for_test() -> Self {
        Self {
            server: ServerConfig {
                bind: "127.0.0.1".into(),
                port: 8080,
            },
            database: DatabaseConfig {
                path: PathBuf::from(":memory:"),
            },
            libre_nms: LibreNmsConfig {
                timeout_seconds: 5,
                retry_max: 1,
                sleep_ms_between_requests: 100,
            },
            billing: BillingConfig {
                run_at_day_of_month: 1,
                run_at_hour: 10,
                run_at_minute: 0,
                timezone: "Asia/Shanghai".into(),
            },
            output: OutputConfig {
                root: PathBuf::from("/tmp/output"),
                soffice_user_installation: PathBuf::from("/tmp/soffice-profile"),
            },
            web: WebConfig {
                session_secret: String::new(),
            },
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self::default_for_test()
    }
}

impl AppConfig {
    /// 从默认位置或 `LNMS_INVOICE_CONFIG` 指向的文件加载,并叠加环境变量覆盖。
    pub fn load() -> Result<Self> {
        let _ = dotenvy::dotenv(); // 静默失败:生产环境通常没有 .env

        let config_path = std::env::var("LNMS_INVOICE_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("config/default.toml"));

        let mut cfg = Self::from_file(&config_path)?;

        // 环境变量覆盖(可选)
        if let Ok(db) = std::env::var("LNMS_INVOICE_DB") {
            cfg.database.path = PathBuf::from(db);
        }
        if let Ok(out) = std::env::var("LNMS_INVOICE_OUTPUT") {
            cfg.output.root = PathBuf::from(out);
        }
        if let Ok(secret) = std::env::var("LNMS_INVOICE_SESSION_SECRET") {
            cfg.web.session_secret = secret;
        }
        if cfg.web.session_secret.is_empty() {
            return Err(Error::Config(
                "session_secret 未设置:请通过环境变量 LNMS_INVOICE_SESSION_SECRET 注入(生产由 systemd LoadCredential 提供)"
                    .into(),
            ));
        }

        cfg.validate()?;
        Ok(cfg)
    }

    /// 校验字段范围(day_of_month 1..=28,hour/minute 合法,port 合法)。
    pub fn validate(&self) -> Result<()> {
        let b = &self.billing;
        if !(1..=28).contains(&b.run_at_day_of_month) {
            return Err(Error::Config(format!(
                "billing.run_at_day_of_month 必须在 1..=28,实际 {}",
                b.run_at_day_of_month
            )));
        }
        if b.run_at_hour > 23 {
            return Err(Error::Config(format!(
                "billing.run_at_hour 必须在 0..=23,实际 {}",
                b.run_at_hour
            )));
        }
        if b.run_at_minute > 59 {
            return Err(Error::Config(format!(
                "billing.run_at_minute 必须在 0..=59,实际 {}",
                b.run_at_minute
            )));
        }
        if b.timezone.is_empty() {
            return Err(Error::Config("billing.timezone 不能为空".into()));
        }
        if self.server.port == 0 {
            return Err(Error::Config("server.port 不能为 0".into()));
        }
        Ok(())
    }

    /// 从指定 TOML 文件加载(纯文件,无环境变量覆盖)。
    pub fn from_file(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(Error::Config(format!("config file not found: {}", path.display())));
        }
        let content = std::fs::read_to_string(path)?;
        let cfg: AppConfig = toml::from_str(&content)?;
        Ok(cfg)
    }

    pub fn environment(&self) -> &'static str {
        if cfg!(debug_assertions) { "dev" } else { "release" }
    }
}
