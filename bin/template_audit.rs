//! 模板预检 CLI(阶段 3)
//!
//! 用法:
//!   template-audit <xlsx_path> <template_name> [--db <sqlite_path>]
//!
//! - 打开 xlsx,产出 TemplateAudit(SHA256 + 单元格清单 + drawing 锚点 + 媒体清单)
//! - 打印摘要到 stdout(JSON)
//! - 若指定 --db,落 template_versions 表

use lnms_invoice::store::Store;
use lnms_invoice::template::{inspect, write_template_version};
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("用法: {} <xlsx_path> <template_name> [--db <sqlite_path>]", args[0]);
        return ExitCode::from(2);
    }

    let xlsx = PathBuf::from(&args[1]);
    let name = &args[2];

    let db_path: Option<PathBuf> = args
        .iter()
        .position(|a| a == "--db")
        .and_then(|i| args.get(i + 1).map(PathBuf::from));

    let audit = match inspect(&xlsx, name) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("template-audit: inspect failed: {e:#}");
            return ExitCode::from(1);
        }
    };

    // stdout: 摘要
    let summary = serde_json::json!({
        "template_name": audit.template_name,
        "sha256": audit.sha256,
        "bytes": audit.bytes,
        "sheets": audit.sheets,
        "cell_count": audit.cell_map.len(),
        "drawing_count": audit.drawings.len(),
        "media": audit.media.iter().map(|m| serde_json::json!({
            "path": m.media_path,
            "sha256": m.sha256,
            "bytes": m.bytes,
            "width": m.width,
            "height": m.height,
        })).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());

    // 可选:落 SQLite
    if let Some(db) = db_path {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let result: anyhow::Result<()> = rt.block_on(async {
            let store = Store::connect(&db).await?;
            write_template_version(&store, &audit).await?;
            Ok(())
        });
        if let Err(e) = result {
            eprintln!("template-audit: 写 SQLite 失败: {e:#}");
            return ExitCode::from(1);
        }
        eprintln!("template-audit: 已落 template_versions (db = {})", db.display());
    }

    ExitCode::SUCCESS
}
