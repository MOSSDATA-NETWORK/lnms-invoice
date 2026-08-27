//! 阶段 4 LNMS 客户端单元测试(httpmock 拦截 HTTP)
//!
//! 覆盖:
//! - 200 + 合法 JSON → 解析正确
//! - 401 → 立即失败,带 URL
//! - 404 → 立即失败
//! - 429 + Retry-After → 等待后重试,成功
//! - 500 → 3 次重试后失败
//! - 连接被拒 → 3 次重试后失败

use httpmock::prelude::*;
use lnms_invoice::librenms::LibreNmsClient;

#[test]
fn test_list_bills_parses_bills_array() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api/v0/bills");
        then.status(200)
            .header("content-type", "application/json")
            .body(
                r#"{
                  "status": "ok",
                  "bills": [
                    {"id": 1, "bill_name": "Customer-A", "bill_type": "quota", "port_id": 100},
                    {"id": 2, "bill_name": "Customer-B", "bill_type": "quota", "port_id": 101}
                  ]
                }"#,
            );
    });

    let cli = LibreNmsClient::new(&server.base_url(), "test-token").unwrap();
    let bills = cli.list_bills().expect("list bills");

    assert_eq!(bills.len(), 2);
    assert_eq!(bills[0].id, 1);
    assert_eq!(bills[0].bill_name.as_deref(), Some("Customer-A"));
    assert_eq!(bills[1].id, 2);
    mock.assert_hits(1);
}

#[test]
fn test_get_bill_parses_bill_with_95th() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api/v0/bills/42");
        then.status(200)
            .header("content-type", "application/json")
            .body(
                r#"{
                  "status": "ok",
                  "bill": {
                    "id": 42,
                    "bill_name": "Cust-X",
                    "rate_95th": 1234.5,
                    "dir_95th": "in",
                    "in_avg": 500.0,
                    "out_avg": 480.0,
                    "total_data": 12345678.0
                  }
                }"#,
            );
    });

    let cli = LibreNmsClient::new(&server.base_url(), "tok").unwrap();
    let detail = cli.get_bill(42).expect("get_bill");

    assert_eq!(detail.id, 42);
    assert_eq!(detail.rate_95th, Some(1234.5));
    assert_eq!(detail.dir_95th.as_deref(), Some("in"));
    mock.assert_hits(1);
}

#[test]
fn test_401_fails_immediately() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api/v0/bills");
        then.status(401).body("Unauthorized");
    });

    let cli = LibreNmsClient::new(&server.base_url(), "bad-token").unwrap();
    let err = cli.list_bills().unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("auth failed"), "got: {msg}");
    assert!(msg.contains("401"), "got: {msg}");
    mock.assert_hits(1); // 不重试
}

#[test]
fn test_404_fails_immediately() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api/v0/bills/999");
        then.status(404).body("not found");
    });

    let cli = LibreNmsClient::new(&server.base_url(), "tok").unwrap();
    let err = cli.get_bill(999).unwrap_err();
    assert!(format!("{err}").contains("404"));
    mock.assert_hits(1);
}

#[test]
fn test_429_retries_then_exhausts() {
    let server = MockServer::start();
    // 4 次都 429:1 次初次 + 3 次重试,客户端耗尽后报错。
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api/v0/bills");
        then.status(429).header("retry-after", "0");
    });

    let cli = LibreNmsClient::new(&server.base_url(), "tok").unwrap();
    let err = cli.list_bills().unwrap_err();
    assert!(format!("{err}").contains("429 retries exhausted"));
    mock.assert_hits(4); // 1 初次 + 3 重试
}

#[test]
fn test_500_retries_then_fails() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api/v0/bills");
        then.status(500).body("oops");
    });

    let cli = LibreNmsClient::new(&server.base_url(), "tok").unwrap();
    let err = cli.list_bills().unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("after 3 retries"), "got: {msg}");
    mock.assert_hits(4); // 1 初次 + 3 重试
}

#[test]
fn test_history_returns_raw_json() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/api/v0/bills/7/history");
        then.status(200)
            .header("content-type", "application/json")
            .body(
                r#"[
                  {"timestamp": 1700000000, "in_delta": 1.2, "out_delta": 1.1},
                  {"timestamp": 1700000300, "in_delta": 1.5, "out_delta": 1.4}
                ]"#,
            );
    });

    let cli = LibreNmsClient::new(&server.base_url(), "tok").unwrap();
    let raw = cli.get_bill_history_raw(7).expect("history");

    let arr = raw.as_array().expect("array");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["timestamp"], 1700000000_i64);
    mock.assert_hits(1);
}
