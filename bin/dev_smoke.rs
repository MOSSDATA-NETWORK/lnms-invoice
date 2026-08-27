//! 开发期烟雾测试入口
//!
//! 用途:阶段 1 验证配置加载 + 模块导入无误。
//! 阶段 4+ 将扩展为:连通 LNMS API + 拉一份 bills 列表 + 写 mock xlsx。

use lnms_invoice::config::AppConfig;
use std::process::ExitCode;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    log::info!("dev-smoke: starting");

    let cfg = match AppConfig::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("dev-smoke: config load failed: {e:#}");
            return ExitCode::from(2);
        }
    };

    println!("dev-smoke: lnms-invoice {} OK", env!("CARGO_PKG_VERSION"));
    println!("  bind:    {}:{}", cfg.server.bind, cfg.server.port);
    println!("  db:      {}", cfg.database.path.display());
    println!("  output:  {}", cfg.output.root.display());
    println!(
        "  billing: 每月 {} 日 {:02}:{:02} {}",
        cfg.billing.run_at_day_of_month, cfg.billing.run_at_hour, cfg.billing.run_at_minute, cfg.billing.timezone
    );

    log::info!("dev-smoke: done");
    ExitCode::SUCCESS
}
