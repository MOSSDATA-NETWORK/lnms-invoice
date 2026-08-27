//! 阶段 7 计费逻辑测试
//!
//! - 95th Mbps 计算:total 字段优先 / in+out fallback / 空序列
//! - build_invoice_lines:端口级费用聚合,合计正确

use lnms_invoice::billing::{build_invoice_lines, compute_95th_mbps, mbps_95th_from_history_json};
use lnms_invoice::librenms::HistoryPoint;
use lnms_invoice::render::PortLine;
use lnms_invoice::store::{Port, Rate};

fn port(label: &str, ip_a: i64, ip_b: i64, rent: bool, hosting: bool) -> Port {
    Port {
        id: 0,
        customer_id: 0,
        port_label: label.into(),
        ip_count_a: ip_a,
        ip_count_b: ip_b,
        machine_rent: rent,
        machine_hosting: hosting,
        notes: None,
        librenms_bill_id: None,
    }
}

fn rate(mbps: i64, ip: i64, rent: i64, hosting: i64) -> Rate {
    rate_with_ip_qty(mbps, ip, 0, rent, hosting)
}

fn rate_with_ip_qty(mbps: i64, ip: i64, ip_qty: i64, rent: i64, hosting: i64) -> Rate {
    Rate {
        id: 0,
        customer_id: 0,
        effective_from: "2026-01-01".into(),
        effective_to: None,
        mbps_unit_price_cents: mbps,
        ip_unit_price_cents: ip,
        ip_quantity: ip_qty,
        machine_rent_cents: rent,
        machine_hosting_cents: hosting,
        currency: "CNY".into(),
        librenms_bill_id: None,
        business_label: None,
        notes: String::new(),
    }
}

#[test]
fn test_95th_uses_total_field_directly() {
    // 100 个点,total 从 1_000_000 升到 100_000_000 bps(1→100 Mbps)
    // 95 百分位 ≈ 第 95 个点 ≈ 95 Mbps
    let pts: Vec<HistoryPoint> = (1..=100)
        .map(|i| HistoryPoint {
            timestamp: None,
            period: None,
            in_delta: None,
            out_delta: None,
            total: Some(i as f64 * 1_000_000.0),
        })
        .collect();
    let p95 = compute_95th_mbps(&pts).unwrap();
    assert!((p95 - 95.0).abs() < 1.5, "expected ~95 Mbps, got {p95}");
}

#[test]
fn test_95th_falls_back_to_in_out_deltas() {
    // 5min 间隔,100 Mbps = 100_000_000 bps × 300s / 8 = 3_750_000_000 bits in 5min
    let pts: Vec<HistoryPoint> = (1..=100)
        .map(|i| HistoryPoint {
            timestamp: None,
            period: None,
            in_delta: Some(50_000_000.0 * i as f64), // bits in 5min
            out_delta: Some(50_000_000.0 * i as f64),
            total: None,
        })
        .collect();
    let p95 = compute_95th_mbps(&pts).unwrap();
    // 第 95 个点:in+out = 95 × 100M = 9.5G bits → 9.5G/300s × 8 = 253 Mbps
    assert!((p95 - 253.0).abs() < 5.0, "expected ~253 Mbps, got {p95}");
}

#[test]
fn test_95th_empty_returns_none() {
    assert!(compute_95th_mbps(&[]).is_none());
}

#[test]
fn test_95th_all_zero_returns_none() {
    let pts = vec![HistoryPoint {
        timestamp: None,
        period: None,
        in_delta: Some(0.0),
        out_delta: Some(0.0),
        total: None,
    }];
    assert!(compute_95th_mbps(&pts).is_none());
}

#[test]
fn test_95th_from_history_json_roundtrip() {
    let json = serde_json::json!([
        {"total": 10_000_000.0},
        {"total": 20_000_000.0},
        {"total": 100_000_000.0},
    ]);
    let mbps = mbps_95th_from_history_json(&json).unwrap();
    assert!(mbps.is_some());
}

#[test]
fn test_build_invoice_lines_aggregates_total() {
    let ports = vec![
        port("A", 8, 0, false, true), // hosting=true → +300
        port("B", 0, 4, true, false), // rent=true → +500
    ];
    // v0.6.3 起 IP 数量在费率上(rate.ip_quantity),不再从端口累加
    let r = rate_with_ip_qty(10, 5, 12, 500, 300); // mbps 10 cents, ip 5 cents, 12 IPs
    let port_95ths: Vec<(Port, Option<f64>)> =
        ports.iter().cloned().map(|p| (p, Some(85.0))).collect();
    let (lines, total): (Vec<PortLine>, i64) = build_invoice_lines(&port_95ths, &r);
    assert_eq!(lines.len(), 2);
    // A: 85*10 + 0 + 300 = 1150
    // B: 85*10 + 500 = 1350
    // IP: 12 * 5 = 60
    assert_eq!(total, 1150 + 1350 + 60);
    assert_eq!(lines[0].mbps_95th, Some(85));
}

#[test]
fn test_build_invoice_lines_zero_mbps_still_charges_static_fees() {
    let ports = vec![port("A", 0, 0, true, true)];
    let r = rate(10, 5, 500, 300);
    let port_95ths: Vec<(Port, Option<f64>)> =
        ports.iter().cloned().map(|p| (p, Some(0.0))).collect();
    let (_, total) = build_invoice_lines(&port_95ths, &r);
    // 0*10 + 0*5 + 500 + 300 = 800
    assert_eq!(total, 800);
}

#[test]
fn test_build_invoice_lines_none_mbps() {
    let ports = vec![port("A", 1, 0, false, false)];
    let r = rate(10, 5, 500, 300);
    let port_95ths: Vec<(Port, Option<f64>)> =
        ports.iter().cloned().map(|p| (p, None)).collect();
    let (_, total) = build_invoice_lines(&port_95ths, &r);
    // mbps None → 0, no static fees, no per-port IP(v0.6.3 后 IP 在费率上)
    assert_eq!(total, 0);
}

#[test]
fn test_build_invoice_lines_ip_quantity_on_rate() {
    // v0.6.3:IP 数量直填在费率(rate.ip_quantity),不再累加端口
    let ports = vec![
        port("A", 8, 4, false, false), // 这些 IP 数量不再计入
        port("B", 0, 0, false, false),
    ];
    let r = rate_with_ip_qty(10, 5, 13, 0, 0); // 13 IPs × 5 = 65
    let port_95ths: Vec<(Port, Option<f64>)> =
        ports.iter().cloned().map(|p| (p, Some(0.0))).collect();
    let (_, total) = build_invoice_lines(&port_95ths, &r);
    // 端口费用 0,IP 总 13*5 = 65
    assert_eq!(total, 65);
}