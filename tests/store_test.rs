//! 阶段 2 单元测试
//!
//! 覆盖:
//! - Store::connect 创建数据库 + 跑迁移
//! - 9 张表 + 索引 + 约束存在
//! - 客户/端口/费率 CRUD
//! - 序号生成(事务化,多次调用递增)
//! - PRAGMA 配置(WAL / foreign_keys)

use lnms_invoice::store::{NewCustomer, NewRate, Store};
use tempfile::tempdir;

#[tokio::test]
async fn test_connect_creates_db_and_runs_migrations() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("test.sqlite");
    assert!(!db.exists());

    let store = Store::connect(&db).await.expect("connect");
    assert!(db.exists(), "数据库文件应被创建");

    // 9 张数据表 + sequences + template_versions(共 11 张)
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .fetch_all(store.pool())
    .await
    .expect("list tables");

    for required in [
        "librenms_instances",
        "customers",
        "ports",
        "rates",
        "invoices",
        "invoice_lines",
        "invoice_runs",
        "invoice_actions",
        "users",
        "sequences",
        "template_versions",
    ] {
        assert!(
            tables.contains(&required.to_string()),
            "缺少表 {required},实际表: {tables:?}"
        );
    }
}

#[tokio::test]
async fn test_pragma_wal_and_foreign_keys() {
    let dir = tempdir().unwrap();
    let store = Store::connect(&dir.path().join("t.sqlite")).await.unwrap();

    let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(journal.to_lowercase(), "wal", "PRAGMA journal_mode 应为 WAL");

    let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(fk, 1, "PRAGMA foreign_keys 应为 ON");
}

#[tokio::test]
async fn test_libre_nms_instance_crud() {
    let store = fresh_store().await;
    let id = store
        .insert_libre_nms_instance("main", "https://lnms.example.com", b"encrypted-token")
        .await
        .unwrap();

    let list = store.list_active_libre_nms_instances().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, id);
    assert_eq!(list[0].name, "main");
    assert!(list[0].is_active);
    assert_eq!(list[0].api_token_enc, b"encrypted-token");
}

#[tokio::test]
async fn test_customer_crud_and_internal_key_unique() {
    let store = fresh_store().await;
    let lnms_id = store
        .insert_libre_nms_instance("main", "https://x", b"t")
        .await
        .unwrap();

    let new = NewCustomer {
        internal_key: "c-hunan-xx",
        name: "湖南XX网络",
        currency: "CNY",
        librenms_instance_id: lnms_id,
        librenms_bill_id: 42,
        timezone: "Asia/Shanghai",
        company_type: "domestic",
        company_info_json: r#"{"tax_id":"91430111MABPNXXXX"}"#,
        company_info_schema_version: 1,
        billing_address: Some("长沙市开福区"),
        contact_email: Some("ops@example.com"),
    };
    let id = store.insert_customer(&new).await.unwrap();
    assert!(id > 0);

    let found = store
        .find_customer_by_internal_key("c-hunan-xx")
        .await
        .unwrap()
        .expect("客户应存在");
    assert_eq!(found.id, id);
    assert_eq!(found.currency, "CNY");
    assert_eq!(found.librenms_bill_id, 42);

    // 唯一键冲突
    let dup = store.insert_customer(&new).await;
    assert!(dup.is_err(), "internal_key 唯一约束应生效");
}

#[tokio::test]
async fn test_port_crud() {
    let store = fresh_store().await;
    let lnms_id = store
        .insert_libre_nms_instance("main", "https://x", b"t")
        .await
        .unwrap();
    let cid = store
        .insert_customer(&NewCustomer {
            internal_key: "c-1",
            name: "X",
            currency: "CNY",
            librenms_instance_id: lnms_id,
            librenms_bill_id: 1,
            timezone: "Asia/Shanghai",
            company_type: "domestic",
            company_info_json: "{}",
            company_info_schema_version: 1,
            billing_address: None,
            contact_email: None,
        })
        .await
        .unwrap();

    let pid = store
        .insert_port_with_bill(cid, "华为BGP 3段", 8, 0, false, true, None, Some("备注"))
        .await
        .unwrap();

    let ports = store.list_ports_for_customer(cid).await.unwrap();
    assert_eq!(ports.len(), 1);
    assert_eq!(ports[0].id, pid);
    assert_eq!(ports[0].port_label, "华为BGP 3段");
    assert_eq!(ports[0].ip_count_a, 8);
    assert!(!ports[0].machine_rent);
    assert!(ports[0].machine_hosting);
}

#[tokio::test]
async fn test_rate_lookup_at_period() {
    let store = fresh_store().await;
    let lnms_id = store
        .insert_libre_nms_instance("main", "https://x", b"t")
        .await
        .unwrap();
    let cid = store
        .insert_customer(&NewCustomer {
            internal_key: "c-2",
            name: "Y",
            currency: "CNY",
            librenms_instance_id: lnms_id,
            librenms_bill_id: 1,
            timezone: "Asia/Shanghai",
            company_type: "domestic",
            company_info_json: "{}",
            company_info_schema_version: 1,
            billing_address: None,
            contact_email: None,
        })
        .await
        .unwrap();

    store
        .insert_rate(&NewRate {
            customer_id: cid,
            effective_from: "2026-01-01",
            effective_to: Some("2026-06-30"),
            mbps_unit_price_yuan: 43.50, // ¥43.50/Mbps
            ip_unit_price_yuan: 50.00,
            ip_quantity: 0,
            machine_rent_yuan: 0.0,
            machine_hosting_yuan: 100.00,
            currency: "CNY",
            librenms_bill_id: None,
            business_label: None,
            notes: "",
        })
        .await
        .unwrap();
    store
        .insert_rate(&NewRate {
            customer_id: cid,
            effective_from: "2026-07-01",
            effective_to: None,
            mbps_unit_price_yuan: 50.00, // 涨价
            ip_unit_price_yuan: 50.00,
            ip_quantity: 0,
            machine_rent_yuan: 0.0,
            machine_hosting_yuan: 100.00,
            currency: "CNY",
            librenms_bill_id: None,
            business_label: None,
            notes: "",
        })
        .await
        .unwrap();

    // 2026-05 期 → 第一档(43.5)
    let r1 = store
        .find_rate_for_customer_at(cid, "2026-05-01")
        .await
        .unwrap()
        .expect("5 月应找到费率");
    assert_eq!(r1.mbps_unit_price_yuan, 43.50);

    // 2026-08 期 → 第二档(50)
    let r2 = store
        .find_rate_for_customer_at(cid, "2026-08-01")
        .await
        .unwrap()
        .expect("8 月应找到费率");
    assert_eq!(r2.mbps_unit_price_yuan, 50.00);

    // 2019 期 → 无
    let r3 = store
        .find_rate_for_customer_at(cid, "2019-01-01")
        .await
        .unwrap();
    assert!(r3.is_none());
}

#[tokio::test]
async fn test_next_sequence_atomic() {
    let store = fresh_store().await;
    let a = store.next_sequence("invoice_no_2026_08", 1).await.unwrap();
    let b = store.next_sequence("invoice_no_2026_08", 1).await.unwrap();
    let c = store.next_sequence("invoice_no_2026_08", 1).await.unwrap();
    assert_eq!((a, b, c), (1, 2, 3));

    // 不同 name 独立计数
    let d = store.next_sequence("other", 100).await.unwrap();
    assert_eq!(d, 100);
    let e = store.next_sequence("other", 100).await.unwrap();
    assert_eq!(e, 101);
}

#[tokio::test]
async fn test_concurrent_sequence_increments() {
    let store = fresh_store().await;
    // 模拟 50 个并发取号,应得到 50 个不同值(无重复)
    let mut handles = Vec::new();
    for _ in 0..50 {
        let s = store.clone();
        handles.push(tokio::spawn(async move { s.next_sequence("concurrent", 1).await.unwrap() }));
    }
    let mut results: Vec<i64> = Vec::new();
    for h in handles {
        results.push(h.await.unwrap());
    }
    results.sort();
    let unique: std::collections::HashSet<i64> = results.iter().copied().collect();
    assert_eq!(unique.len(), 50, "并发取号应全部唯一,实际: {results:?}");
    assert_eq!(results[0], 1);
    assert_eq!(results[49], 50);
}

// ============================================================
// helpers
// ============================================================

async fn fresh_store() -> Store {
    let dir = tempdir().expect("tempdir");
    Store::connect(&dir.path().join("t.sqlite"))
        .await
        .expect("connect")
}

// ============================================================
// 全局设置(settings 表;出账日/时刻/发票号模板的存储层)
// ============================================================

#[tokio::test]
async fn test_setting_get_returns_none_when_absent() {
    let store = fresh_store().await;
    let v = store.get_setting("billing_day").await.unwrap();
    assert!(v.is_none(), "未设置时应返回 None");
}

#[tokio::test]
async fn test_setting_set_then_get_roundtrip() {
    let store = fresh_store().await;
    store.set_setting("billing_day", "5").await.unwrap();
    let v = store.get_setting("billing_day").await.unwrap();
    assert_eq!(v.as_deref(), Some("5"));

    // upsert 覆盖
    store.set_setting("billing_day", "10").await.unwrap();
    let v2 = store.get_setting("billing_day").await.unwrap();
    assert_eq!(v2.as_deref(), Some("10"));
}

#[tokio::test]
async fn test_setting_keys_are_independent() {
    let store = fresh_store().await;
    store.set_setting("billing_day", "3").await.unwrap();
    store.set_setting("billing_hour", "14").await.unwrap();
    store
        .set_setting("invoice_no_template", "INV-{KEY}-{YYYY}-{MM}-{SEQ}")
        .await
        .unwrap();
    assert_eq!(
        store.get_setting("billing_day").await.unwrap().as_deref(),
        Some("3")
    );
    assert_eq!(
        store.get_setting("billing_hour").await.unwrap().as_deref(),
        Some("14")
    );
    assert_eq!(
        store
            .get_setting("invoice_no_template")
            .await
            .unwrap()
            .as_deref(),
        Some("INV-{KEY}-{YYYY}-{MM}-{SEQ}")
    );
    // 不存在的 key 仍然 None
    assert!(store.get_setting("not_a_thing").await.unwrap().is_none());
}

// ============================================================
// 幂等检查(has_invoice_for_period;定时自检模式防重复出账)
// ============================================================

#[tokio::test]
async fn test_has_invoice_for_period_false_when_no_invoices() {
    let store = fresh_store().await;
    let has = store
        .has_invoice_for_period(/* customer_id = */ 1, 2026, 8)
        .await
        .unwrap();
    assert!(!has, "无发票时必须返回 false(否则会重复出账)");
}

#[tokio::test]
async fn test_has_invoice_for_period_true_after_insert() {
    let store = fresh_store().await;
    let lnms_id = store
        .insert_libre_nms_instance("main", "https://x", b"t")
        .await
        .unwrap();
    let customer_id = store
        .insert_customer(&NewCustomer {
            internal_key: "c-1",
            name: "C",
            currency: "CNY",
            librenms_instance_id: lnms_id,
            librenms_bill_id: 42,
            timezone: "Asia/Shanghai",
            company_type: "domestic",
            company_info_json: "{}",
            company_info_schema_version: 1,
            billing_address: None,
            contact_email: None,
        })
        .await
        .unwrap();

    // 同账期不同客户不同,但没插 → false
    let other = store
        .insert_customer(&NewCustomer {
            internal_key: "c-2",
            name: "C2",
            currency: "CNY",
            librenms_instance_id: lnms_id,
            librenms_bill_id: 43,
            timezone: "Asia/Shanghai",
            company_type: "domestic",
            company_info_json: "{}",
            company_info_schema_version: 1,
            billing_address: None,
            contact_email: None,
        })
        .await
        .unwrap();

    store
        .upsert_invoice_generating(
            customer_id,
            2026,
            8,
            "INV-c-1-2026-08-0001",
            "stub",
            "{}",
            "CNY",
        )
        .await
        .unwrap();

    assert!(
        store
            .has_invoice_for_period(customer_id, 2026, 8)
            .await
            .unwrap(),
        "已插发票必须返回 true"
    );

    // 其它账期仍未出
    assert!(!store.has_invoice_for_period(customer_id, 2026, 7).await.unwrap());
    // 其它客户仍未出
    assert!(!store.has_invoice_for_period(other, 2026, 8).await.unwrap());
}

// ============================================================
// 费率自动收尾(insert_rate 自动把上一条 effective_to 设为新 effective_from - 1 天)
// ============================================================

#[tokio::test]
async fn test_insert_rate_closes_overlapping_open_prev() {
    let store = fresh_store().await;
    let lnms_id = store
        .insert_libre_nms_instance("main", "https://x", b"t")
        .await
        .unwrap();
    let cid = store
        .insert_customer(&NewCustomer {
            internal_key: "auto-close",
            name: "AC",
            currency: "CNY",
            librenms_instance_id: lnms_id,
            librenms_bill_id: 1,
            timezone: "Asia/Shanghai",
            company_type: "domestic",
            company_info_json: "{}",
            company_info_schema_version: 1,
            billing_address: None,
            contact_email: None,
        })
        .await
        .unwrap();

    // 第一条:open-ended
    store
        .insert_rate(&NewRate {
            customer_id: cid,
            effective_from: "2026-01-01",
            effective_to: None,
            mbps_unit_price_yuan: 10.0,
            ip_unit_price_yuan: 0.0,
            ip_quantity: 0,
            machine_rent_yuan: 0.0,
            machine_hosting_yuan: 0.0,
            currency: "CNY",
            librenms_bill_id: None,
            business_label: None,
            notes: "",
        })
        .await
        .unwrap();

    // 第二条 effective_from = 2026-08-15,应自动把第一条收尾到 2026-08-14
    store
        .insert_rate(&NewRate {
            customer_id: cid,
            effective_from: "2026-08-15",
            effective_to: None,
            mbps_unit_price_yuan: 20.0,
            ip_unit_price_yuan: 0.0,
            ip_quantity: 0,
            machine_rent_yuan: 0.0,
            machine_hosting_yuan: 0.0,
            currency: "CNY",
            librenms_bill_id: None,
            business_label: None,
            notes: "",
        })
        .await
        .unwrap();

    let rates = store.list_rates_for_customer(cid).await.unwrap();
    assert_eq!(rates.len(), 2, "应有 2 条费率");
    let prev = rates.iter().find(|r| r.effective_from == "2026-01-01").unwrap();
    assert_eq!(
        prev.effective_to.as_deref(),
        Some("2026-08-14"),
        "上一条 open-ended 应收尾为新 effective_from - 1 天"
    );
    let newer = rates.iter().find(|r| r.effective_from == "2026-08-15").unwrap();
    assert_eq!(newer.effective_to, None, "新插入那条 effective_to 不变");

    // 出账查询 2026-08-15:新费率;2026-08-14:旧费率;2026-08-13:旧费率
    let r_aug = store
        .find_rate_for_customer_at(cid, "2026-08-15")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r_aug.mbps_unit_price_yuan, 20.0);
    let r_jul = store
        .find_rate_for_customer_at(cid, "2026-07-01")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r_jul.mbps_unit_price_yuan, 10.0);
}

#[tokio::test]
async fn test_insert_rate_skips_when_prev_already_closed() {
    let store = fresh_store().await;
    let lnms_id = store
        .insert_libre_nms_instance("main", "https://x", b"t")
        .await
        .unwrap();
    let cid = store
        .insert_customer(&NewCustomer {
            internal_key: "already-closed",
            name: "AC",
            currency: "CNY",
            librenms_instance_id: lnms_id,
            librenms_bill_id: 1,
            timezone: "Asia/Shanghai",
            company_type: "domestic",
            company_info_json: "{}",
            company_info_schema_version: 1,
            billing_address: None,
            contact_email: None,
        })
        .await
        .unwrap();

    // 上一条已明确收尾到 2026-06-30
    store
        .insert_rate(&NewRate {
            customer_id: cid,
            effective_from: "2026-01-01",
            effective_to: Some("2026-06-30"),
            mbps_unit_price_yuan: 10.0,
            ip_unit_price_yuan: 0.0,
            ip_quantity: 0,
            machine_rent_yuan: 0.0,
            machine_hosting_yuan: 0.0,
            currency: "CNY",
            librenms_bill_id: None,
            business_label: None,
            notes: "",
        })
        .await
        .unwrap();

    // 新一条 2026-07-01:不应动 2026-06-30 那条
    store
        .insert_rate(&NewRate {
            customer_id: cid,
            effective_from: "2026-07-01",
            effective_to: None,
            mbps_unit_price_yuan: 20.0,
            ip_unit_price_yuan: 0.0,
            ip_quantity: 0,
            machine_rent_yuan: 0.0,
            machine_hosting_yuan: 0.0,
            currency: "CNY",
            librenms_bill_id: None,
            business_label: None,
            notes: "",
        })
        .await
        .unwrap();

    let prev = store
        .list_rates_for_customer(cid)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.effective_from == "2026-01-01")
        .unwrap();
    assert_eq!(
        prev.effective_to.as_deref(),
        Some("2026-06-30"),
        "已明确收尾的不应被覆盖"
    );
}
