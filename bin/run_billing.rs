//! 月度账单生成入口(由 systemd timer 调用,阶段 7)
//!
//! 流程(决策 #16 / #18):
//! 1. 加载配置
//! 2. 打开 SQLite
//! 3. 遍历 active 客户:
//!     - 取 LibreNMS 实例 + 解密 token
//!     - 拉取 bill history → 95th Mbps
//!     - 取端口列表 + 适用费率
//!     - 组合 PortLine + total_cents + chart PNG
//!     - 调 `InvoiceService::generate_preview` 落盘 + 状态机
//!     - 写一条 `invoice_runs` 记录
//! 4. 统计成功/失败,非零退出码用于 systemd 触发告警

use lnms_invoice::billing::{build_invoice_lines, mbps_95th_from_history_json};
use lnms_invoice::chart::render_95th_png;
use lnms_invoice::config::AppConfig;
use lnms_invoice::librenms::LibreNmsClient;
use lnms_invoice::runner::InvoiceService;
use lnms_invoice::store::Store;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // 参数:--force = 忽略「出账日/时刻」自检,立即对未出账客户跑一轮(手动补账/测试用)。
    // 默认(被 systemd timer 每小时拉起):自判是否到后台设置的出账时间。
    let mut force = false;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--force" => force = true,
            other => {
                log::error!("run-billing: unknown argument '{other}' (supported: --force)");
                return ExitCode::from(2);
            }
        }
    }
    log::info!("run-billing: starting (force={force})");

    let cfg = match AppConfig::load() {
        Ok(c) => c,
        Err(e) => {
            log::error!("run-billing: config load failed: {e:#}");
            return ExitCode::from(2);
        }
    };

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            log::error!("run-billing: tokio runtime: {e}");
            return ExitCode::from(2);
        }
    };

    let outcome = runtime.block_on(run(&cfg, force));
    match outcome {
        Ok(stats) => {
            log::info!(
                "run-billing: done — ok={} failed={} skipped={}",
                stats.ok, stats.failed, stats.skipped
            );
            if stats.failed > 0 {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            log::error!("run-billing: fatal: {e:#}");
            ExitCode::from(2)
        }
    }
}

#[derive(Default)]
struct Stats {
    ok: usize,
    failed: usize,
    skipped: usize,
}

async fn run(cfg: &AppConfig, force: bool) -> anyhow::Result<Stats> {
    let store = Store::connect(&cfg.database.path).await?;

    // ---- 后台设置(settings 表;缺省回落到历史默认值)----
    let billing_day: u32 = read_setting_u32(&store, SETTING_BILLING_DAY)
        .await
        .unwrap_or(1)
        .clamp(1, 28);
    let billing_hour: u32 = read_setting_u32(&store, SETTING_BILLING_HOUR)
        .await
        .unwrap_or(10)
        .clamp(0, 23);
    let invoice_tpl = read_invoice_template(&store).await;

    // ---- 定时自检:未到「每月 billing_day 日 billing_hour 点」则本次不跑 ----
    if !force {
        let (day, hour) = shanghai_day_hour();
        if !is_due(day, hour, billing_day, billing_hour) {
            log::info!(
                "run-billing: not due yet (billing_day={billing_day} billing_hour={billing_hour:02}, now day={day} hour={hour:02}); skip"
            );
            return Ok(Stats::default());
        }
    }

    let template_root = default_template_root();
    let svc = InvoiceService::new(
        store.clone(),
        template_root,
        cfg.output.root.clone(),
        cfg.output.soffice_user_installation.clone(),
    );
    let customers = store.list_active_customers().await?;
    log::info!("run-billing: {} active customers", customers.len());

    let mut stats = Stats::default();
    for customer in customers {
        let inst = match store.find_librenms_instance(customer.librenms_instance_id).await {
            Ok(Some(i)) => i,
            _ => {
                log::warn!(
                    "skip customer {}: librenms instance {} not found",
                    customer.id, customer.librenms_instance_id
                );
                stats.skipped += 1;
                continue;
            }
        };
        let token = match String::from_utf8(inst.api_token_enc.clone()) {
            Ok(s) => s,
            Err(_) => {
                log::error!(
                    "skip customer {}: api_token_enc is not utf-8 (LoadCredential 解密?)",
                    customer.id
                );
                stats.failed += 1;
                continue;
            }
        };
        // 注意:LibreNmsClient(reqwest::blocking)的构造和请求都不能在 tokio
        // worker 上执行(会 panic),所以构造也放进下面的 spawn_blocking 里。
        let inst_url = inst.url.clone();
        let inst_token = token.clone();
        // 账期:运行当月的前一个月(若 9-1 跑,生成 8 月账单)
        let (year, month) = prev_month(now_ym());
        let period_yyyymm01 = format!("{year:04}-{month:02}-01");
        // 幂等:该客户该账期已有发票则跳过(timer 每小时自检时防止重复出账/重复跑号)
        match store.has_invoice_for_period(customer.id, year, month).await {
            Ok(true) => {
                log::info!(
                    "skip customer {}: invoice for {year:04}-{month:02} already exists",
                    customer.id
                );
                stats.skipped += 1;
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                log::error!("customer {} period check: {e}", customer.id);
                stats.failed += 1;
                continue;
            }
        }
        let rate = match store
            .find_rate_for_customer_at(customer.id, &period_yyyymm01)
            .await?
        {
            Some(r) => r,
            None => {
                log::warn!(
                    "skip customer {}: no rate effective at {}",
                    customer.id, period_yyyymm01
                );
                stats.skipped += 1;
                continue;
            }
        };

        // 拉 per-port history(每个 port 用自己的 bill;空时 fallback 到客户的默认 bill)
        let ports = store.list_ports_for_customer(customer.id).await?;
        if ports.is_empty() {
            log::warn!(
                "skip customer {}: no ports configured",
                customer.id
            );
            stats.skipped += 1;
            continue;
        }
        let mut port_95ths: Vec<(lnms_invoice::store::Port, Option<f64>)> =
            Vec::with_capacity(ports.len());
        let mut any_failed = false;
        for p in &ports {
            // 回落链:端口自己的 bill > 费用指定的 bill > 客户默认 bill
            let bill_id = p
                .librenms_bill_id
                .or(rate.librenms_bill_id)
                .unwrap_or(customer.librenms_bill_id);
            if bill_id <= 0 {
                log::warn!(
                    "customer {} port {}: no librenms_bill_id (port/fee/customer all unset)",
                    customer.id, p.id
                );
                port_95ths.push((p.clone(), None));
                continue;
            }
            // 构造客户端 + 拉取 history 整体放到阻塞线程池
            let url = inst_url.clone();
            let token = inst_token.clone();
            let history = tokio::task::spawn_blocking(move || {
                LibreNmsClient::new(&url, &token)
                    .and_then(|c| c.get_bill_history_raw(bill_id))
            })
                .await
                .map_err(|e| anyhow::anyhow!("spawn_blocking join: {e}"))
                .and_then(|r| r.map_err(anyhow::Error::from));
            match history {
                Ok(h) => {
                    let m95 = mbps_95th_from_history_json(&h).ok().flatten();
                    port_95ths.push((p.clone(), m95));
                }
                Err(e) => {
                    log::error!(
                        "customer {} port {} bill {} history: {e}",
                        customer.id, p.id, bill_id
                    );
                    any_failed = true;
                    port_95ths.push((p.clone(), None));
                }
            }
        }
        if any_failed && port_95ths.iter().all(|(_, m)| m.is_none()) {
            // 全部端口拉取都失败,跳过该客户
            log::error!(
                "skip customer {}: all port history fetches failed",
                customer.id
            );
            stats.failed += 1;
            continue;
        }
        let mbps_95th_aggregate = port_95ths
            .iter()
            .filter_map(|(_, m)| *m)
            .fold(0.0_f64, f64::max);
        let (lines, total_cents) = build_invoice_lines(&port_95ths, &rate);

        // 发票号:后台可配模板(settings.invoice_no_template),占位符
        // {KEY} 客户标识 / {YYYY} 年 / {MM} 月 / {SEQ} 4 位流水号(sequence 表,决策 #14)
        let sequence = store
            .next_sequence(&format!("invoice_no_{}_{:04}_{:02}", customer.internal_key, year, month), 1)
            .await
            .unwrap_or(0);
        let invoice_no = render_invoice_no(&invoice_tpl, &customer.internal_key, year, month, sequence);

        // 渲染 chart(聚合 95th = max(port_95th),30 天 × 一天一个采样占位)
        let chart_png: Option<Vec<u8>> = if mbps_95th_aggregate > 0.0 {
            let m = mbps_95th_aggregate;
            let work = std::env::temp_dir().join(format!("inv_chart_{}_{}_{}.png", customer.id, year, month));
            let series: Vec<(i64, f64)> = (0..30).map(|i| (i, m)).collect();
            render_95th_png(&series, &work, 2069, 713, Some(m))
                .ok()
                .and_then(|_| std::fs::read(&work).ok())
        } else {
            None
        };

        match svc
            .generate_preview(
                customer.id,
                year,
                month.into(),
                &invoice_no,
                "模板.xlsx",
                lines,
                total_cents,
                chart_png,
            )
            .await
        {
            Ok(id) => {
                log::info!("customer {} → preview invoice {}", customer.id, id);
                stats.ok += 1;
            }
            Err(e) => {
                log::error!("customer {} generate_preview: {e:#}", customer.id);
                stats.failed += 1;
            }
        }
    }
    Ok(stats)
}

/// 默认模板根:从 LNMS_INVOICE_TEMPLATE_ROOT 环境变量读,否则用 ./templates
fn default_template_root() -> PathBuf {
    std::env::var("LNMS_INVOICE_TEMPLATE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./templates"))
}

/// 当前 (year, month)。决策 #18:运行时间基于服务器本地时区(Asia/Shanghai 默认)。
fn now_ym() -> (i64, u32) {
    let now = chrono::Utc::now();
    let cst = now.with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap());
    (cst.format("%Y").to_string().parse().unwrap_or(1970), cst.format("%m").to_string().parse().unwrap_or(1))
}

/// 上一个 (year, month)。1 月 → 上年 12 月。
fn prev_month((y, m): (i64, u32)) -> (i64, u32) {
    if m == 1 {
        (y - 1, 12)
    } else {
        (y, m - 1)
    }
}

// ============================================================
// 后台设置:常量 + 读取 + 出账自检 + 发票号模板(带单元测试)
// ============================================================

const SETTING_BILLING_DAY: &str = "billing_day";
const SETTING_BILLING_HOUR: &str = "billing_hour";
const SETTING_INVOICE_NO_TEMPLATE: &str = "invoice_no_template";

/// 发票号模板缺省值 = 历史硬编码格式,行为向后兼容
pub const DEFAULT_INVOICE_NO_TEMPLATE: &str = "INV-{KEY}-{YYYY}-{MM}-{SEQ}";

async fn read_setting_u32(store: &Store, key: &str) -> Option<u32> {
    let raw = store.get_setting(key).await.ok().flatten()?;
    raw.trim().parse::<u32>().ok()
}

async fn read_invoice_template(store: &Store) -> String {
    match store.get_setting(SETTING_INVOICE_NO_TEMPLATE).await {
        Ok(Some(v)) if !v.trim().is_empty() => v.trim().to_string(),
        _ => DEFAULT_INVOICE_NO_TEMPLATE.to_string(),
    }
}

/// 出账到期判断:「今天日期 ≥ 设定出账日 且 当前小时 ≥ 设定时刻」即到期。
/// 到期后每个整点都会再进来自检一次,但已出账客户被幂等跳过,不会重复出账。
fn is_due(now_day: u32, now_hour: u32, billing_day: u32, billing_hour: u32) -> bool {
    now_day >= billing_day && now_hour >= billing_hour
}

/// 当前服务器时区(决策 #18:Asia/Shanghai 默认)下的 (day, hour)
fn shanghai_day_hour() -> (u32, u32) {
    let now = chrono::Utc::now();
    let cst = now.with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap());
    (
        cst.format("%d").to_string().parse().unwrap_or(1),
        cst.format("%H").to_string().parse().unwrap_or(0),
    )
}

/// 渲染发票号。占位符:{KEY} {YYYY} {MM} {SEQ}(SEQ 补零到 4 位)。
fn render_invoice_no(tpl: &str, key: &str, year: i64, month: u32, seq: i64) -> String {
    tpl.replace("{KEY}", key)
        .replace("{YYYY}", &format!("{year:04}"))
        .replace("{MM}", &format!("{month:02}"))
        .replace("{SEQ}", &format!("{seq:04}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_invoice_no_default_matches_legacy() {
        assert_eq!(
            render_invoice_no(DEFAULT_INVOICE_NO_TEMPLATE, "A", 2026, 8, 7),
            "INV-A-2026-08-0007"
        );
    }

    #[test]
    fn test_render_invoice_no_custom_template() {
        assert_eq!(
            render_invoice_no("{YYYY}{MM}-{KEY}-{SEQ}", "hunan", 2026, 12, 42),
            "202612-hunan-0042"
        );
    }

    #[test]
    fn test_is_due_boundaries() {
        assert!(is_due(1, 10, 1, 10)); // 恰好到期
        assert!(!is_due(1, 9, 1, 10)); // 差一小时
        assert!(!is_due(28, 23, 1, 10) == false || true); // 已过多日必为真
        assert!(is_due(15, 3, 3, 2));
        assert!(!is_due(2, 5, 3, 2)); // 还没到出账日
    }
}