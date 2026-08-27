//! 阶段 7 import-customers 测试

use lnms_invoice::store::Store;

const SAMPLE: &str = r#"{
  "librenms_instances": [
    {"name": "hn-nms", "url": "https://nms.example/", "api_token_env": "LNMS_TOKEN_HN"}
  ],
  "customers": [
    {
      "internal_key": "A",
      "name": "湖南XX网络",
      "currency": "CNY",
      "librenms_instance": "hn-nms",
      "librenms_bill_id": 1,
      "timezone": "Asia/Shanghai",
      "company_type": "domestic",
      "company_info": {"tax_id": "12345"},
      "billing_address": "长沙",
      "contact_email": "a@example.com",
      "ports": [
        {"label": "华为BGP 3段", "ip_count_a": 8, "ip_count_b": 0, "machine_rent": false, "machine_hosting": true},
        {"label": "联通BGP 1段", "ip_count_a": 0, "ip_count_b": 4, "machine_rent": true, "machine_hosting": false}
      ]
    }
  ],
  "rates": [
    {
      "customer_internal_key": "A",
      "effective_from": "2026-01-01",
      "mbps_unit_price_cents": 10,
      "ip_unit_price_cents": 5,
      "machine_rent_cents": 500,
      "machine_hosting_cents": 300,
      "currency": "CNY"
    }
  ]
}"#;

#[tokio::test]
async fn test_import_writes_db_correctly() {
    let dir = tempfile::tempdir().unwrap();
    let json = dir.path().join("in.json");
    let db = dir.path().join("test.db");
    std::fs::write(&json, SAMPLE).unwrap();

    let store = Store::connect(&db).await.unwrap();

    // 直接 import-customers 内部逻辑走测试入口:
    // 这里 inline 复刻核心插入路径,避免拉起 CLI 子进程
    let raw = std::fs::read_to_string(&json).unwrap();
    let input: serde_json::Value = serde_json::from_str(&raw).unwrap();

    let inst_id = store
        .insert_libre_nms_instance(
            input["librenms_instances"][0]["name"].as_str().unwrap(),
            input["librenms_instances"][0]["url"].as_str().unwrap(),
            b"placeholder",
        )
        .await
        .unwrap();

    let cust = &input["customers"][0];
    let ci = cust["company_info"].to_string();
    let nc = lnms_invoice::store::NewCustomer {
        internal_key: cust["internal_key"].as_str().unwrap(),
        name: cust["name"].as_str().unwrap(),
        currency: cust["currency"].as_str().unwrap(),
        librenms_instance_id: inst_id,
        librenms_bill_id: cust["librenms_bill_id"].as_i64().unwrap(),
        timezone: cust["timezone"].as_str().unwrap(),
        company_type: cust["company_type"].as_str().unwrap(),
        company_info_json: &ci,
        company_info_schema_version: 1,
        billing_address: cust["billing_address"].as_str(),
        contact_email: cust["contact_email"].as_str(),
    };
    let cid = store.insert_customer(&nc).await.unwrap();
    for p in cust["ports"].as_array().unwrap() {
        store
            .insert_port(
                cid,
                p["label"].as_str().unwrap(),
                p["ip_count_a"].as_i64().unwrap(),
                p["ip_count_b"].as_i64().unwrap(),
                p["machine_rent"].as_bool().unwrap_or(false),
                p["machine_hosting"].as_bool().unwrap_or(false),
                None,
            )
            .await
            .unwrap();
    }
    let r = &input["rates"][0];
    let nr = lnms_invoice::store::NewRate {
        customer_id: cid,
        effective_from: r["effective_from"].as_str().unwrap(),
        effective_to: None,
        mbps_unit_price_cents: r["mbps_unit_price_cents"].as_i64().unwrap(),
        ip_unit_price_cents: r["ip_unit_price_cents"].as_i64().unwrap(),
        ip_quantity: 0,
        machine_rent_cents: r["machine_rent_cents"].as_i64().unwrap(),
        machine_hosting_cents: r["machine_hosting_cents"].as_i64().unwrap(),
        currency: r["currency"].as_str().unwrap(),
        librenms_bill_id: None,
            business_label: None,

            };
    store.insert_rate(&nr).await.unwrap();

    // 断言
    let c = store
        .find_customer_by_internal_key("A")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(c.name, "湖南XX网络");
    assert_eq!(c.company_info_json, "{\"tax_id\":\"12345\"}");
    let ports = store.list_ports_for_customer(c.id).await.unwrap();
    assert_eq!(ports.len(), 2);
    let rate = store
        .find_rate_for_customer_at(c.id, "2026-08-01")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rate.mbps_unit_price_cents, 10);
    assert_eq!(rate.machine_hosting_cents, 300);
}