//! `lnms-invoice` 主入口
//!
//! 子命令:
//! - `lnms-invoice`           打印 config(诊断)
//! - `lnms-invoice serve`     启动 axum web(常驻 systemd service)
//! - 其他子命令走独立 binary:run-billing / import-customers / set-instance-token / template-audit

use lnms_invoice::config::AppConfig;
use lnms_invoice::runner::InvoiceService;
use lnms_invoice::store::Store;
use lnms_invoice::web::{router, WebState};
use std::net::SocketAddr;
use std::process::ExitCode;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().collect();
    let sub = args.get(1).map(String::as_str).unwrap_or("");

    match sub {
        "" | "status" => run_status(),
        "serve" => run_serve(),
        "version" | "--version" | "-V" => {
            println!("lnms-invoice {}", lnms_invoice::VERSION);
            ExitCode::SUCCESS
        }
        "help" | "--help" | "-h" => {
            println!(
                "lnms-invoice {ver}\n\n子命令:\n  serve              启动 web UI(axum,常驻)\n  status             打印当前 config(诊断)\n  version            版本号\n\n其他子命令是独立二进制:run-billing / import-customers / set-instance-token / template-audit",
                ver = lnms_invoice::VERSION
            );
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("未知子命令: {other};运行 `lnms-invoice help`");
            ExitCode::from(2)
        }
    }
}

fn run_status() -> ExitCode {
    let cfg = match AppConfig::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config load failed: {e:#}");
            return ExitCode::from(2);
        }
    };
    log::info!("lnms-invoice {} starting ({})", lnms_invoice::VERSION, cfg.environment());
    println!("lnms-invoice {} ({})", lnms_invoice::VERSION, cfg.environment());
    println!("  bind:        {}:{}", cfg.server.bind, cfg.server.port);
    println!("  db:          {}", cfg.database.path.display());
    println!("  output:      {}", cfg.output.root.display());
    println!("  soffice UI:  {}", cfg.output.soffice_user_installation.display());
    println!(
        "  billing:     每月 {} 日 {:02}:{:02} {}",
        cfg.billing.run_at_day_of_month, cfg.billing.run_at_hour, cfg.billing.run_at_minute, cfg.billing.timezone
    );
    ExitCode::SUCCESS
}

fn run_serve() -> ExitCode {
    let cfg = match AppConfig::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config load failed: {e:#}");
            return ExitCode::from(2);
        }
    };
    log::info!("lnms-invoice serve: starting on {}:{}", cfg.server.bind, cfg.server.port);

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("tokio runtime: {e}");
            return ExitCode::from(2);
        }
    };
    let result: anyhow::Result<()> = runtime.block_on(async {
        let store = Store::connect(&cfg.database.path).await?;
        let template_root = std::env::var("LNMS_INVOICE_TEMPLATE_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                // macOS: /var/lib requires root; fall back to /tmp for dev
                if cfg!(target_os = "macos") {
                    std::path::PathBuf::from("/tmp/lnms-invoice/templates")
                } else {
                    std::path::PathBuf::from("/var/lib/lnms-invoice/templates")
                }
            });
        std::fs::create_dir_all(&template_root)?;
        std::fs::create_dir_all(&cfg.output.root)?;
        std::fs::create_dir_all(&cfg.output.soffice_user_installation)?;
        let svc = InvoiceService::new(
            store,
            template_root,
            cfg.output.root.clone(),
            cfg.output.soffice_user_installation.clone(),
        );
        let state = WebState::from_config(svc, &cfg);
        let app = router(state);
        let addr: SocketAddr = format!("{}:{}", cfg.server.bind, cfg.server.port)
            .parse()
            .map_err(|e: std::net::AddrParseError| anyhow::anyhow!("bad bind addr: {e}"))?;
        let listener = tokio::net::TcpListener::bind(addr).await?;
        log::info!("serving on http://{addr}");
        axum::serve(listener, app).await?;
        Ok(())
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("serve failed: {e:#}");
            ExitCode::from(1)
        }
    }
}