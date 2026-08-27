//! 本地 dev 预览用的 SQLite 引导器。
//!
//! **不在生产用** ——只为让运维/开发快速看到 Web UI 跑起来的样子。
//!
//! 作用:
//! 1. 在指定路径创建 SQLite,跑 migrations
//! 2. 写入 1 个 LibreNMS 实例(URL 占位,无 token)
//! 3. 写入 1 个客户 + 2 个端口 + 1 个费率
//! 4. 写入 1 个 admin 用户(密码 `--password`,默认 `admin123`)和 1 个 operator
//!
//! 用法:
//!   dev-bootstrap <db.sqlite> [--password <pwd>]
//!
//! 跑完后用以下环境变量启动 web:
//!   LNMS_INVOICE_DB=<db.sqlite>
//!   LNMS_INVOICE_SESSION_SECRET=$(openssl rand -hex 32)
//!   LNMS_INVOICE_BIND=127.0.0.1:18080  (避免与生产 8080 撞)
//!   LNMS_INVOICE_PORT=18080
//!   LNMS_INVOICE_TEMPLATE_ROOT=<任意可写目录>
//!   LNMS_INVOICE_OUTPUT=<任意可写目录>
//!   LNMS_INVOICE_SOFFICE_PROFILE=<任意可写目��>

use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
use argon2::Argon2;
use lnms_invoice::store::{NewCustomer, NewRate, Store};
use std::path::PathBuf;
use std::process::ExitCode;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut positional: Vec<String> = Vec::new();
    let mut password = "admin123".to_string();
    let mut args = std::env::args().skip(1).peekable();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--password" => {
                password = args.next().unwrap_or_else(|| {
                    eprintln!("--password 需要值");
                    std::process::exit(2);
                });
            }
            other if other.starts_with("--") => {
                eprintln!("未知选项: {other}");
                return ExitCode::from(2);
            }
            other => positional.push(other.to_string()),
        }
    }
    if positional.len() != 1 {
        eprintln!("用法: dev-bootstrap <db.sqlite> [--password <pwd>]");
        return ExitCode::from(2);
    }
    let db = PathBuf::from(&positional[0]);

    if let Some(parent) = db.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }

    let store = match Store::connect(&db).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("connect db: {e:#}");
            return ExitCode::from(1);
        }
    };

    // 1. LibreNMS 实例(URL 占位;token 不会写库)
    let inst_id = match store
        .insert_libre_nms_instance("hn-nms", "https://nms.example/", b"dev-placeholder-no-token")
        .await
    {
        Ok(i) => i,
        Err(e) => {
            eprintln!("insert instance: {e:#}");
            return ExitCode::from(1);
        }
    };

    // 2. 客户
    let cust_id = match store
        .insert_customer(&NewCustomer {
            internal_key: "A",
            name: "湖南XX网络(预览数据)",
            currency: "CNY",
            librenms_instance_id: inst_id,
            librenms_bill_id: 1,
            timezone: "Asia/Shanghai",
            company_type: "domestic",
            company_info_json: r#"{"tax_id":"91430100MA00000000","bank":"招商银行长沙分行","bank_account":"1234 5678 9012 3456"}"#,
            company_info_schema_version: 1,
            billing_address: Some("湖南省长沙市岳麓区XX路 1 号"),
            contact_email: Some("billing@a.example.com"),
        })
        .await
    {
        Ok(i) => i,
        Err(e) => {
            eprintln!("insert customer: {e:#}");
            return ExitCode::from(1);
        }
    };

    // 3. 端口
    if let Err(e) = store
        .insert_port(cust_id, "华为BGP 3段", 8, 0, false, true, None)
        .await
    {
        eprintln!("insert port 1: {e:#}");
        return ExitCode::from(1);
    }
    if let Err(e) = store
        .insert_port(cust_id, "联通BGP 1段", 0, 4, true, false, None)
        .await
    {
        eprintln!("insert port 2: {e:#}");
        return ExitCode::from(1);
    }

    // 4. 费率
    if let Err(e) = store
        .insert_rate(&NewRate {
            customer_id: cust_id,
            effective_from: "2026-01-01",
            effective_to: None,
            mbps_unit_price_cents: 10,
            ip_unit_price_cents: 5,
            ip_quantity: 0,
            machine_rent_cents: 50000,
            machine_hosting_cents: 30000,
            currency: "CNY",
            librenms_bill_id: None,
            business_label: None,
            notes: "",
        })
        .await
    {
        eprintln!("insert rate: {e:#}");
        return ExitCode::from(1);
    }

    // 5. 用户(admin + operator)
    for (uname, role) in [("admin", "admin"), ("operator", "operator")] {
        let salt = SaltString::generate(&mut OsRng);
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .unwrap()
            .to_string();
        if let Err(e) = store.insert_user(uname, &hash, role).await {
            eprintln!("insert user {uname}: {e:#}");
            return ExitCode::from(1);
        }
    }

    println!("[dev-bootstrap] db = {}", db.display());
    println!("[dev-bootstrap] admin    账号: admin    密码: {password}");
    println!("[dev-bootstrap] operator 账号: operator 密码: {password}");
    println!();
    println!("启动 web:");
    println!("  LNMS_INVOICE_DB={} \\", db.display());
    println!("  LNMS_INVOICE_SESSION_SECRET=$(openssl rand -hex 32) \\");
    println!("  LNMS_INVOICE_BIND=127.0.0.1 \\");
    println!("  LNMS_INVOICE_PORT=18080 \\");
    println!("  LNMS_INVOICE_TEMPLATE_ROOT=/tmp/lnms-invoice/templates \\");
    println!("  LNMS_INVOICE_OUTPUT=/tmp/lnms-invoice/output \\");
    println!("  LNMS_INVOICE_SOFFICE_PROFILE=/tmp/lnms-invoice/soffice-profile \\");
    println!("  cargo run --release -- serve");
    println!();
    println!("(注:本 bootstrap 不会跑 LNMS 拉数,也不生成 PDF;只让你看到 web 页面)");
    ExitCode::SUCCESS
}