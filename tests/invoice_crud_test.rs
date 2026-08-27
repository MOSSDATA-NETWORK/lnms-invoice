//! 阶段 6 store 扩展测试
//!
//! - 用户 CRUD
//! - 账单状态机:generating → preview → final / rejected / failed
//! - 幂等 upsert:同 customer + 同月第二次覆盖前一次
//! - 唯一约束:同 customer + 同月只能有 1 行(由 ON CONFLICT 体现)

use lnms_invoice::store::{InvoiceStatus, NewCustomer, Store};

async fn fresh_store() -> Store {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    // 临时目录在 test 结束时 drop,Store 持有路径的所有权不影响
    Store::connect(&path).await.unwrap()
}

/// 创建一个 LibreNMS 实例 + 一个客户,返回 customer_id。
/// 仅测试用;token 是占位字节。
async fn make_customer(store: &Store, key: &str) -> i64 {
    let inst_id = store
        .insert_libre_nms_instance("inst1", "https://nms.example/", &[0u8])
        .await
        .unwrap();
    store
        .insert_customer(&NewCustomer {
            internal_key: key,
            name: &format!("{key} 客户"),
            currency: "CNY",
            librenms_instance_id: inst_id,
            librenms_bill_id: 1,
            timezone: "Asia/Shanghai",
            company_type: "domestic",
            company_info_json: "{}",
            company_info_schema_version: 1,
            billing_address: None,
            contact_email: None,
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn test_user_insert_and_lookup() {
    let store = fresh_store().await;
    let id = store
        .insert_user("admin", "$argon2id$dummy", "admin")
        .await
        .unwrap();
    let u = store.find_user_by_username("admin").await.unwrap().unwrap();
    assert_eq!(u.id, id);
    assert_eq!(u.username, "admin");
    assert_eq!(u.role, "admin");
    assert!(u.is_active);

    assert!(store.find_user_by_username("nope").await.unwrap().is_none());
}

#[tokio::test]
async fn test_user_last_login() {
    let store = fresh_store().await;
    let id = store
        .insert_user("ops", "hash", "operator")
        .await
        .unwrap();
    store.update_user_last_login(id).await.unwrap();
}

#[tokio::test]
async fn test_invoice_lifecycle_happy_path() {
    let store = fresh_store().await;
    let cust = make_customer(&store, "A").await;
    let user_id = store.insert_user("admin", "hash", "admin").await.unwrap();
    let id = store
        .upsert_invoice_generating(
            cust, 2026, 8,
            "INV-A-2026-08-0001",
            "v0.4",
            r#"{"customer":"A","total":12700}"#,
            "CNY",
        )
        .await
        .unwrap();

    store
        .update_invoice_preview(id, 12700.0, "/tmp/preview.pdf")
        .await
        .unwrap();

    let inv = store.find_invoice(id).await.unwrap().unwrap();
    assert_eq!(inv.status, InvoiceStatus::Preview);
    assert_eq!(inv.total_yuan, Some(12700.0));
    assert_eq!(inv.pdf_path_preview.as_deref(), Some("/tmp/preview.pdf"));
    assert!(inv.pdf_path_final.is_none());
    assert!(inv.confirmed_at.is_none());

    store
        .record_action(id, "preview_generated", None, None)
        .await
        .unwrap();

    store
        .update_invoice_confirmed(id, "/tmp/final.pdf", user_id)
        .await
        .unwrap();

    let inv = store.find_invoice(id).await.unwrap().unwrap();
    assert_eq!(inv.status, InvoiceStatus::Final);
    assert_eq!(inv.pdf_path_final.as_deref(), Some("/tmp/final.pdf"));
    assert_eq!(inv.confirmed_by, Some(user_id));
    assert!(inv.confirmed_at.is_some());

    store
        .record_action(id, "confirmed", Some(user_id), None)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_invoice_rejected_branch() {
    let store = fresh_store().await;
    let cust = make_customer(&store, "B").await;
    let user_id = store.insert_user("ops", "hash", "operator").await.unwrap();
    let id = store
        .upsert_invoice_generating(
            cust, 2026, 8,
            "INV-B-2026-08-0002",
            "v0.4",
            "{}",
            "CNY",
        )
        .await
        .unwrap();
    store
        .update_invoice_preview(id, 1.0, "/tmp/p.pdf")
        .await
        .unwrap();
    store
        .update_invoice_rejected(id, "金额对不上")
        .await
        .unwrap();
    store
        .record_action(id, "rejected", Some(user_id), Some("金额对不上"))
        .await
        .unwrap();

    let inv = store.find_invoice(id).await.unwrap().unwrap();
    assert_eq!(inv.status, InvoiceStatus::Rejected);
    assert_eq!(inv.rejected_reason.as_deref(), Some("金额对不上"));
}

#[tokio::test]
async fn test_invoice_failed_branch() {
    let store = fresh_store().await;
    let cust = make_customer(&store, "C").await;
    let id = store
        .upsert_invoice_generating(
            cust, 2026, 8,
            "INV-2026-08-0003",
            "v0.4",
            "{}",
            "CNY",
        )
        .await
        .unwrap();
    store.update_invoice_failed(id).await.unwrap();
    let inv = store.find_invoice(id).await.unwrap().unwrap();
    assert_eq!(inv.status, InvoiceStatus::Failed);
}

#[tokio::test]
async fn test_invoice_upsert_same_month_overwrites() {
    let store = fresh_store().await;
    let cust = make_customer(&store, "D").await;
    // 第一次
    let id1 = store
        .upsert_invoice_generating(
            cust, 2026, 8,
            "INV-2026-08-OVERWRITE",
            "v0.4",
            "{\"first\":true}",
            "CNY",
        )
        .await
        .unwrap();
    // 第二次同 customer+月:UNIQUE 触发,ON CONFLICT 应该 UPDATE 而不是 INSERT 新行
    let id2 = store
        .upsert_invoice_generating(
            cust, 2026, 8,
            "INV-2026-08-OVERWRITE-V2",
            "v0.4",
            "{\"second\":true}",
            "CNY",
        )
        .await
        .unwrap();
    assert_eq!(id1, id2, "同 customer+月应覆盖而非新建");

    let fetched = store.find_invoice_for_customer_month(cust, 2026, 8)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched.invoice_no, "INV-2026-08-OVERWRITE-V2");
    assert!(fetched.source_snapshot_json.contains("second"));
}

#[tokio::test]
async fn test_invoice_list_ordering_desc() {
    let store = fresh_store().await;
    let cust = make_customer(&store, "E").await;
    store.upsert_invoice_generating(cust, 2026, 6, "x-6", "v", "{}", "CNY").await.unwrap();
    store.upsert_invoice_generating(cust, 2026, 8, "x-8", "v", "{}", "CNY").await.unwrap();
    store.upsert_invoice_generating(cust, 2026, 7, "x-7", "v", "{}", "CNY").await.unwrap();
    let list = store.list_invoices_for_customer(cust).await.unwrap();
    assert_eq!(list.len(), 3);
    assert_eq!((list[0].period_year, list[0].period_month), (2026, 8));
    assert_eq!((list[1].period_year, list[1].period_month), (2026, 7));
    assert_eq!((list[2].period_year, list[2].period_month), (2026, 6));
}

#[tokio::test]
async fn test_invoice_status_parse_roundtrip() {
    for s in [
        InvoiceStatus::Generating,
        InvoiceStatus::Preview,
        InvoiceStatus::Confirming,
        InvoiceStatus::Final,
        InvoiceStatus::Failed,
        InvoiceStatus::Rejected,
    ] {
        let parsed = InvoiceStatus::parse(s.as_str()).unwrap();
        assert_eq!(parsed, s);
    }
    assert!(InvoiceStatus::parse("nope").is_err());
}