//! 月度账单数据流(阶段 7)
//!
//! 从 LNMS 历史数据 → 95th → 应用费率 → 生成 InvoiceData。
//!
//! 算法(决策 #16,#17):
//! 1. `/bills/{id}/history` 返回 5min 序列;按 `total`(bits)求 95 百分位;
//!    (in+out) = bits,× 5min 时长得到 bytes;再 /300s = bps。
//! 2. 实际 LibreNMS 字段形态待生产环境实测;`history_total_bps` 用 `total` 字段,
//!    若 `total` 不存在则用 `in_delta + out_delta`(bits per 5min interval)。
//! 3. 单个 bill 的 95th 应用于该客户的所有端口(决策 #16 简化);
//!    多 bill(每个端口一个)在阶段 8+ 接入。

use crate::error::Result;
use crate::librenms::HistoryPoint;
use crate::render::PortLine;
use crate::store::{Port, Rate};

/// 把 5min 序列点转成 95th Mbps(bps ÷ 1_000_000)。
///
/// 优先级:
/// 1. `total` 字段已存在 → 当作 bps 直接用
/// 2. `in_delta + out_delta` 当作 bits-per-5min → × 8 ÷ 300s = bps
/// 3. 否则返回 None(数据缺失)
pub fn compute_95th_mbps(points: &[HistoryPoint]) -> Option<f64> {
    if points.is_empty() {
        return None;
    }
    let mut bps_values: Vec<f64> = points
        .iter()
        .filter_map(|p| {
            if let Some(t) = p.total {
                Some(t)
            } else {
                let in_v = p.in_delta.unwrap_or(0.0);
                let out_v = p.out_delta.unwrap_or(0.0);
                let bits = in_v + out_v;
                if bits > 0.0 {
                    Some(bits * 8.0 / 300.0)
                } else {
                    None
                }
            }
        })
        .collect();
    if bps_values.is_empty() {
        return None;
    }
    bps_values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((bps_values.len() as f64 * 0.95).ceil() as usize)
        .saturating_sub(1)
        .min(bps_values.len() - 1);
    Some(bps_values[idx] / 1_000_000.0)
}

/// 给定端口(各绑各的 bill) + 费率,生成 PortLine 列表 + 合计 cents。
///
/// 每个端口有自己的 95th Mbps(可能为 None,表示拉取失败或无数据)。
///
/// 合计 = Σ 各端口费用(mbps × 单价 + rent + hosting) + 客户级 IP 费用(ip_quantity × ip_unit_price_cents)。
/// IP 数量 v0.6.3 起在费用表单上直接维护,不再从端口累加。
pub fn build_invoice_lines(
    port_95ths: &[(Port, Option<f64>)],
    rate: &Rate,
) -> (Vec<PortLine>, i64) {
    let mut lines = Vec::with_capacity(port_95ths.len());
    let mut total = 0i64;
    for (p, mbps_95th) in port_95ths {
        let mbps = mbps_95th.unwrap_or(0.0).round() as i64;
        // 端口级 Mbps 费用:mbps × 单价
        let port_mbps_cents = mbps.max(0) * rate.mbps_unit_price_cents;
        // 机柜租 / 托管费用(布尔,只用一次,不走端口重复)
        let rent_cents = if p.machine_rent { rate.machine_rent_cents } else { 0 };
        let hosting_cents = if p.machine_hosting { rate.machine_hosting_cents } else { 0 };
        let port_total = port_mbps_cents + rent_cents + hosting_cents;
        total += port_total;
        lines.push(PortLine {
            label: p.port_label.clone(),
            mbps_95th: mbps_95th.map(|m| m.round() as i64),
            machine_rent: p.machine_rent,
            machine_hosting: p.machine_hosting,
        });
    }
    // 客户级 IP 费用(单笔,直接维护在费用表单上,不再从端口累加)
    total += rate.ip_quantity.max(0) * rate.ip_unit_price_cents;
    (lines, total)
}
/// 从 history JSON 原始值直接计算 95th Mbps(对外便捷入口)。
pub fn mbps_95th_from_history_json(raw: &serde_json::Value) -> Result<Option<f64>> {
    let arr = raw.as_array().ok_or_else(|| {
        crate::error::Error::LibreNms("history root is not array".into())
    })?;
    let points: Vec<HistoryPoint> = serde_json::from_value(serde_json::Value::Array(
        arr.clone(),
    ))
    .map_err(|e| crate::error::Error::LibreNms(format!("history decode: {e}")))?;
    Ok(compute_95th_mbps(&points))
}

#[allow(dead_code)]
fn _phantom_rate(_r: &Rate) {}