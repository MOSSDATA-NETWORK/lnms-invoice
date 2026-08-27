//! 客户档案导入 CLI(阶段 7)
//!
//! 用法:
//!   import-customers <db.sqlite> <customers.json>
//!
//! customers.json 结构:
//! ```json
//! {
//!   "librenms_instances": [
//!     {"name": "hn-nms", "url": "https://nms.example/", "api_token_env": "LNMS_TOKEN_HN"}
//!   ],
//!   "customers": [
//!     {
//!       "internal_key": "A",
//!       "name": "湖南XX网络",
//!       "currency": "CNY",
//!       "librenms_instance": "hn-nms",
//!       "librenms_bill_id": 1,
//!       "timezone": "Asia/Shanghai",
//!       "company_type": "domestic",
//!       "company_info": {"tax_id": "..."},
//!       "billing_address": "...",
//!       "contact_email": "...",
//!       "ports": [
//!         {"label": "华为BGP 3段", "ip_count_a": 8, "ip_count_b": 0, "machine_rent": false, "machine_hosting": true}
//!       ]
//!     }
//!   ],
//!   "rates": [
//!     {
//!       "customer_internal_key": "A",
//!       "effective_from": "2026-01-01",
//!       "mbps_unit_price_cents": 10,
//!       "ip_unit_price_cents": 5,
//!       "machine_rent_cents": 500,
//!       "machine_hosting_cents": 300,
//!       "currency": "CNY"
//!     }
//!   ]
//! }
//! ```
//!
//! **不**接受明文 token;token 走 `set-instance-token` CLI(读 stdin)单独设置。
//! 这里只校验 `api_token_env` 字段非空(指向的环境变量当前是否存在由运维保证)。

use lnms_invoice::config::AppConfig;
use lnms_invoice::store::{NewCustomer, NewRate, Store};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Deserialize)]
struct Input {
    #[serde(default)]
    librenms_instances: Vec<InstanceIn>,
    customers: Vec<CustomerIn>,
    #[serde(default)]
    rates: Vec<RateIn>,
}

#[derive(Debug, Deserialize)]
struct InstanceIn {
    name: String,
    url: String,
    #[serde(default)]
    api_token_env: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CustomerIn {
    internal_key: String,
    name: String,
    currency: String,
    librenms_instance: String,
    librenms_bill_id: i64,
    timezone: String,
    company_type: String,
    #[serde(default)]
    company_info: serde_json::Value,
    #[serde(default)]
    company_info_schema_version: Option<i64>,
    #[serde(default)]
    billing_address: Option<String>,
    #[serde(default)]
    contact_email: Option<String>,
    #[serde(default)]
    ports: Vec<PortIn>,
}

#[derive(Debug, Deserialize)]
struct PortIn {
    label: String,
    #[serde(default)]
    ip_count_a: i64,
    #[serde(default)]
    ip_count_b: i64,
    #[serde(default)]
    machine_rent: bool,
    #[serde(default)]
    machine_hosting: bool,
    #[serde(default)]
    notes: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RateIn {
    customer_internal_key: String,
    effective_from: String,
    #[serde(default)]
    effective_to: Option<String>,
    mbps_unit_price_cents: i64,
    ip_unit_price_cents: i64,
    machine_rent_cents: i64,
    machine_hosting_cents: i64,
    currency: String,
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("用法: import-customers <db.sqlite> <customers.json>");
        return ExitCode::from(2);
    }
    let db_path = PathBuf::from(&args[1]);
    let json_path = PathBuf::from(&args[2]);

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("tokio runtime: {e}");
            return ExitCode::from(2);
        }
    };
    let cfg = match AppConfig::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config: {e}");
            return ExitCode::from(2);
        }
    };
    let outcome = runtime.block_on(import(&db_path, &json_path, &cfg));
    match outcome {
        Ok(stats) => {
            log::info!(
                "import-customers: instances={} customers={} ports={} rates={}",
                stats.instances, stats.customers, stats.ports, stats.rates
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("import-customers failed: {e:#}");
            ExitCode::from(1)
        }
    }
}

#[derive(Default)]
struct Stats {
    instances: usize,
    customers: usize,
    ports: usize,
    rates: usize,
}

async fn import(
    db: &PathBuf,
    json_path: &PathBuf,
    _cfg: &AppConfig,
) -> anyhow::Result<Stats> {
    let raw = std::fs::read_to_string(json_path)?;
    let input: Input = serde_json::from_str(&raw)?;

    // session_secret 必须存在(不写到 DB),只是借用 config 触发 load
    let store = Store::connect(db).await?;

    let mut stats = Stats::default();
    let mut name_to_id: HashMap<String, i64> = HashMap::new();

    // 1. LibreNMS 实例(URL only;token 走 set-instance-token)
    for inst in &input.librenms_instances {
        if let Some(env) = &inst.api_token_env {
            if std::env::var(env).is_err() {
                log::warn!(
                    "实例 {} 引用的环境变量 {} 当前不存在(由运维/部署保证存在;token 走 set-instance-token 设置)",
                    inst.name, env
                );
            }
        }
        // 占位 token:用 env 名字本身作为标记,让 store 不为空;运行时由 set-instance-token 覆写
        let placeholder = format!("env:{}", inst.api_token_env.clone().unwrap_or_default()).into_bytes();
        let id = store
            .insert_libre_nms_instance(&inst.name, &inst.url, &placeholder)
            .await?;
        name_to_id.insert(inst.name.clone(), id);
        stats.instances += 1;
    }

    // 2. 客户 + 端口
    for c in &input.customers {
        let librenms_instance_id = *name_to_id
            .get(&c.librenms_instance)
            .ok_or_else(|| anyhow::anyhow!("unknown librenms_instance: {}", c.librenms_instance))?;
        let schema_version = c.company_info_schema_version.unwrap_or(1);
        let company_info_json = serde_json::to_string(&c.company_info)?;
        let customer_id = store
            .insert_customer(&NewCustomer {
                internal_key: &c.internal_key,
                name: &c.name,
                currency: &c.currency,
                librenms_instance_id,
                librenms_bill_id: c.librenms_bill_id,
                timezone: &c.timezone,
                company_type: &c.company_type,
                company_info_json: &company_info_json,
                company_info_schema_version: schema_version,
                billing_address: c.billing_address.as_deref(),
                contact_email: c.contact_email.as_deref(),
            })
            .await?;
        stats.customers += 1;
        for p in &c.ports {
            store
                .insert_port(
                    customer_id,
                    &p.label,
                    p.ip_count_a,
                    p.ip_count_b,
                    p.machine_rent,
                    p.machine_hosting,
                    p.notes.as_deref(),
                )
                .await?;
            stats.ports += 1;
        }
    }

    // 3. 费率(按 internal_key ��射)
    let mut key_to_id: HashMap<String, i64> = HashMap::new();
    for c in &input.customers {
        let id = store
            .find_customer_by_internal_key(&c.internal_key)
            .await?
            .map(|c| c.id)
            .ok_or_else(|| anyhow::anyhow!("customer {} not inserted", c.internal_key))?;
        key_to_id.insert(c.internal_key.clone(), id);
    }
    for r in &input.rates {
        let customer_id = *key_to_id
            .get(&r.customer_internal_key)
            .ok_or_else(|| anyhow::anyhow!("unknown customer for rate: {}", r.customer_internal_key))?;
        store
            .insert_rate(&NewRate {
                customer_id,
                effective_from: &r.effective_from,
                effective_to: r.effective_to.as_deref(),
                mbps_unit_price_cents: r.mbps_unit_price_cents,
                ip_unit_price_cents: r.ip_unit_price_cents,
                ip_quantity: 0,
                machine_rent_cents: r.machine_rent_cents,
                machine_hosting_cents: r.machine_hosting_cents,
                currency: &r.currency,
                librenms_bill_id: None,
            business_label: None,
        })
            .await?;
        stats.rates += 1;
    }
    Ok(stats)
}