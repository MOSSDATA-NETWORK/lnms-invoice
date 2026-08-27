//! 阶段 7 Config::validate 测试

use lnms_invoice::config::{AppConfig, WebConfig};

fn base() -> AppConfig {
    AppConfig {
        server: lnms_invoice::config::ServerConfig {
            bind: "127.0.0.1".into(),
            port: 8080,
        },
        database: lnms_invoice::config::DatabaseConfig {
            path: "/tmp/x".into(),
        },
        libre_nms: lnms_invoice::config::LibreNmsConfig {
            timeout_seconds: 5,
            retry_max: 1,
            sleep_ms_between_requests: 100,
        },
        billing: lnms_invoice::config::BillingConfig {
            run_at_day_of_month: 1,
            run_at_hour: 10,
            run_at_minute: 0,
            timezone: "Asia/Shanghai".into(),
        },
        output: lnms_invoice::config::OutputConfig {
            root: "/tmp/out".into(),
            soffice_user_installation: "/tmp/sof".into(),
        },
        web: WebConfig {
            session_secret: "x".repeat(48),
        },
    }
}

#[test]
fn test_validate_default_ok() {
    base().validate().expect("default should validate");
}

#[test]
fn test_validate_rejects_day_29() {
    let mut c = base();
    c.billing.run_at_day_of_month = 29;
    assert!(c.validate().is_err());
}

#[test]
fn test_validate_rejects_day_0() {
    let mut c = base();
    c.billing.run_at_day_of_month = 0;
    assert!(c.validate().is_err());
}

#[test]
fn test_validate_rejects_hour_24() {
    let mut c = base();
    c.billing.run_at_hour = 24;
    assert!(c.validate().is_err());
}

#[test]
fn test_validate_rejects_minute_60() {
    let mut c = base();
    c.billing.run_at_minute = 60;
    assert!(c.validate().is_err());
}

#[test]
fn test_validate_rejects_empty_timezone() {
    let mut c = base();
    c.billing.timezone.clear();
    assert!(c.validate().is_err());
}

#[test]
fn test_validate_rejects_zero_port() {
    let mut c = base();
    c.server.port = 0;
    assert!(c.validate().is_err());
}

#[test]
fn test_validate_accepts_day_28() {
    let mut c = base();
    c.billing.run_at_day_of_month = 28;
    c.validate().expect("day 28 should validate");
}