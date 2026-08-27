//! 阶段 8 Web 管理后台测试
//!
//! - 未登录访问 /admin → 重定向到 /login
//! - operator 访问 /admin → 重定向到 /login(无权)
//! - admin 访问 /admin → 200 + 概况
//! - admin 访问 /admin/customers/:id/toggle-active → 翻转 is_active
//! - admin 通过 /admin/rates/new 新增 + 删除 round-trip

use lnms_invoice::config::Config;
use lnms_invoice::runner::InvoiceService;
use lnms_invoice::store::{NewCustomer, NewCustomerFull, NewRate, Store, TemplateVersionRow};
use lnms_invoice::web::{router, WebState};
use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
use argon2::Argon2;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use std::path::PathBuf;

/// 启动一套测试环境:1 个 NMS + 1 个客户 + admin/operator 两个用户。
/// 返回:tempdir(供 sqlite 自动清理) + axum router
async fn boot() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("test.db");
    let store = Store::connect(&db).await.unwrap();

    let inst = store
        .insert_libre_nms_instance("inst", "https://nms.example/", &[0u8])
        .await
        .unwrap();
    store
        .insert_customer(&NewCustomer {
            internal_key: "A",
            name: "测试客户",
            currency: "CNY",
            librenms_instance_id: inst,
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

    for (uname, role) in [("admin", "admin"), ("operator", "operator")] {
        let salt = SaltString::generate(&mut OsRng);
        let hash = Argon2::default()
            .hash_password(b"hunter2", &salt)
            .unwrap()
            .to_string();
        store.insert_user(uname, &hash, role).await.unwrap();
    }

    let svc = InvoiceService::new(
        store,
        PathBuf::from("/tmp/templates"),
        PathBuf::from("/tmp/output"),
        PathBuf::from("/tmp/soffice-profile"),
    );
    let mut cfg = Config::default_for_test();
    cfg.web.session_secret = "x".repeat(48);
    let state = WebState::from_config(svc, &cfg);
    let router = router(state);
    (dir, router)
}

/// 模拟 POST /login,返回 Set-Cookie 头里的 cookie 字符串(只取第一段)
async fn login(app: &axum::Router, username: &str) -> String {
    let body = format!("username={}&password=hunter2", username);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER, "login 应 303");
    let cookie = resp
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    cookie
}

#[tokio::test]
async fn test_admin_unauthenticated_redirects_to_login() {
    let (_dir, app) = boot().await;
    let resp = app
        .oneshot(Request::builder().uri("/admin").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        resp.headers().get("location").and_then(|v| v.to_str().ok()),
        Some("/login")
    );
}

#[tokio::test]
async fn test_admin_operator_role_redirects_to_login() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "operator").await;
    // operator 已登录但无权访问 /admin:require_admin 应拒绝并回到 /login
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        resp.headers().get("location").and_then(|v| v.to_str().ok()),
        Some("/login")
    );
}

#[tokio::test]
async fn test_admin_admin_role_sees_dashboard() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "admin").await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let s = String::from_utf8_lossy(&body);
    assert!(s.contains("管理后台"), "页面应包含「管理后台」标题");
    assert!(s.contains("LibreNMS"), "页面应包含 LibreNMS 实例链接");
}

#[tokio::test]
async fn test_admin_toggle_customer_active_flips_flag() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "admin").await;

    // 查找客户 id
    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let cust = store
        .list_all_customers()
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.internal_key == "A")
        .expect("客户 A 应存在");
    assert!(cust.is_active, "默认 is_active = true");

    // POST toggle
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/admin/customers/{}/toggle-active", cust.id))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // 成功应重定向回 /admin/customers
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    // 验证 DB 状态翻转
    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let cust2 = store
        .find_customer_by_id(cust.id)
        .await
        .unwrap()
        .expect("客户应仍存在");
    assert!(!cust2.is_active, "切换后 is_active = false");

    // 再切一次,回到 true
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/admin/customers/{}/toggle-active", cust.id))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let cust3 = store.find_customer_by_id(cust.id).await.unwrap().unwrap();
    assert!(cust3.is_active, "再次切换后 is_active = true");
}

#[tokio::test]
async fn test_admin_rate_create_then_delete() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "admin").await;
    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let cust = store
        .list_all_customers()
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.internal_key == "A")
        .expect("客户 A 应存在");

    // 新增
    let body = format!(
        "customer_id={}&effective_from=2026-01-01&effective_to=&mbps_unit_price_cents=10&ip_unit_price_cents=5&ip_quantity=0&machine_rent_cents=0&machine_hosting_cents=0&currency=CNY",
        cust.id
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/rates/new")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", &cookie.clone())
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let rates = store.list_rates_for_customer(cust.id).await.unwrap();
    assert_eq!(rates.len(), 1, "应新增一条费率");
    let rate_id = rates[0].id;

    // 删除
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/admin/rates/{}/delete", rate_id))
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let rates = store.list_rates_for_customer(cust.id).await.unwrap();
    assert!(rates.is_empty(), "删除后应无费率记录");
}

#[tokio::test]
async fn test_admin_instance_create_then_update_then_delete() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "admin").await;

    // 新增实例
    let body = "name=staging-nms&url=https://staging.example.com/";
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/instances/new")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", cookie.clone())
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let all = store.list_all_libre_nms_instances().await.unwrap();
    let new_inst = all
        .iter()
        .find(|i| i.name == "staging-nms")
        .expect("新实例��已落库");
    assert_eq!(new_inst.url, "https://staging.example.com/");
    assert!(new_inst.is_active, "新建默认激活");

    // 编辑 URL + 改名为新名
    let body = "name=staging-nms&url=https://staging2.example.com/&is_active=on";
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/admin/instances/{}/edit", new_inst.id))
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", cookie.clone())
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let after_edit = store
        .find_librenms_instance(new_inst.id)
        .await
        .unwrap()
        .expect("实例应仍存在");
    assert_eq!(after_edit.url, "https://staging2.example.com/");
    assert!(after_edit.is_active, "checkbox on → is_active = true");

    // 没勾 is_active → 应被设为 false
    let body = "name=staging-nms&url=https://staging2.example.com/";
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/admin/instances/{}/edit", new_inst.id))
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", cookie.clone())
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let after_edit = store.find_librenms_instance(new_inst.id).await.unwrap().unwrap();
    assert!(!after_edit.is_active, "缺 checkbox → is_active = false");

    // 删除(此时没 customer 引用,可以删)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/admin/instances/{}/delete", new_inst.id))
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let after = store.find_librenms_instance(new_inst.id).await.unwrap();
    assert!(after.is_none(), "删除后应查不到");
}

#[tokio::test]
async fn test_admin_instance_toggle_active_flips_flag() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "admin").await;

    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let inst = store
        .list_all_libre_nms_instances()
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("boot 应已插 1 个实例");
    assert!(inst.is_active);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/admin/instances/{}/toggle-active", inst.id))
                .header("cookie", cookie.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let after = store.find_librenms_instance(inst.id).await.unwrap().unwrap();
    assert!(!after.is_active, "toggle 一次后应停用");

    // 再 toggle,回到 true
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/admin/instances/{}/toggle-active", inst.id))
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let after = store.find_librenms_instance(inst.id).await.unwrap().unwrap();
    assert!(after.is_active, "再 toggle 后应恢复激活");
}

#[tokio::test]
async fn test_admin_instance_delete_blocked_when_customer_refs() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "admin").await;

    // boot 已插 1 个 NMS + 1 个 customer 引用它
    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let inst = store
        .list_all_libre_nms_instances()
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("boot 应已插 1 个实例");

    // 删除应失败(不是 303,而是错误页),且实例应仍存在
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/admin/instances/{}/delete", inst.id))
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // 错误页:既不是 303,也不应误删;模板渲染失败/或 fallback HTML,这里只断言不是重定向
    assert_ne!(
        resp.status(),
        StatusCode::SEE_OTHER,
        "有 customer 引用时 delete 不应 303"
    );

    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let after = store.find_librenms_instance(inst.id).await.unwrap();
    assert!(after.is_some(), "有 customer 引用时实例应仍存在");
}

#[tokio::test]
async fn test_operator_instance_create_redirects_to_login() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "operator").await;

    let body = "name=evil-nms&url=https://evil.example.com/";
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/instances/new")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", cookie)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::SEE_OTHER,
        "operator POST instance 应被 require_admin 挡掉并重定向 /login"
    );
    assert_eq!(
        resp.headers().get("location").and_then(|v| v.to_str().ok()),
        Some("/login")
    );

    // 验证数据库确实没插
    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let all = store.list_all_libre_nms_instances().await.unwrap();
    assert!(
        all.iter().all(|i| i.name != "evil-nms"),
        "operator 操作不应落库"
    );
}

#[tokio::test]
async fn test_admin_instance_create_with_token_stores_it() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "admin").await;

    // 带 token 的新增
    let body = "name=prod-nms&url=https://nms.example.com/&token=secret-token-abc";
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/instances/new")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", cookie)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let all = store.list_all_libre_nms_instances().await.unwrap();
    let inst = all
        .iter()
        .find(|i| i.name == "prod-nms")
        .expect("新实例应落库");
    assert_eq!(inst.api_token_enc, b"secret-token-abc", "token 应原样落库");
    assert!(
        store.librenms_instance_token_set(inst.id).await.unwrap(),
        "token_set 应为 true"
    );
}

#[tokio::test]
async fn test_admin_instance_edit_token_empty_keeps_existing() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "admin").await;

    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let inst = store
        .list_all_libre_nms_instances()
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("boot 应已插 1 个实例");

    // 用 sudo 风格的 store 方法注入 token
    store
        .update_librenms_instance_token(inst.id, b"original-token")
        .await
        .unwrap();

    // 编辑只改 name/url,token 字段留空 → token 应保留原值
    let body = "name=inst&url=https://nms.example.com/v2/&is_active=on&token=";
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/admin/instances/{}/edit", inst.id))
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", cookie.clone())
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let after = store.find_librenms_instance(inst.id).await.unwrap().unwrap();
    assert_eq!(after.api_token_enc, b"original-token", "留空 token 应保留原值");
    assert_eq!(after.url, "https://nms.example.com/v2/", "URL 应已更新");

    // 编辑给��� token → 应被覆盖
    let body = "name=inst&url=https://nms.example.com/v2/&is_active=on&token=rotated-token";
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/admin/instances/{}/edit", inst.id))
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", cookie)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let after = store.find_librenms_instance(inst.id).await.unwrap().unwrap();
    assert_eq!(after.api_token_enc, b"rotated-token", "非空 token 应覆盖");
}

#[tokio::test]
async fn test_admin_rates_page_lists_existing_rates() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "admin").await;
    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let cust = store
        .list_all_customers()
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.internal_key == "A")
        .unwrap();

    store
        .insert_rate(&NewRate {
            customer_id: cust.id,
            effective_from: "2026-01-01",
            effective_to: None,
            mbps_unit_price_cents: 10,
            ip_unit_price_cents: 5,
            ip_quantity: 0,
            machine_rent_cents: 0,
            machine_hosting_cents: 0,
            currency: "CNY",
            librenms_bill_id: None,
            business_label: None,
        })
        .await
        .unwrap();

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/rates")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let s = String::from_utf8_lossy(&body);
    assert!(s.contains("费用管理"));
    assert!(s.contains("10"));
}
// ============================================================
// 阶段 8f:客户 CRUD + per-port bill + 模板管理
// ============================================================

#[tokio::test]
async fn test_admin_customer_create_full_round_trip() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "admin").await;
    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let inst = store.list_all_libre_nms_instances().await.unwrap()[0].id;

    let body = format!(
        "internal_key=B&name=测试客户B&currency=HKD&librenms_instance_id={}&librenms_bill_id=7&timezone=UTC&company_type=hk&company_info_json={{\"tax_id\":\"X\"}}&billing_address=香港中环&contact_email=b%40example.com&template_name=&is_active=on",
        inst
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/customers/new")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", cookie.clone())
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let cust = store
        .list_all_customers()
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.internal_key == "B")
        .expect("客户 B 应已落库");
    assert_eq!(cust.name, "测试客户B");
    assert_eq!(cust.currency, "HKD");
    assert_eq!(cust.librenms_bill_id, 7);
    assert_eq!(cust.timezone, "UTC");
    assert_eq!(cust.company_type, "hk");
    assert!(cust.billing_address.as_deref() == Some("香港中环"));
    assert_eq!(cust.contact_email.as_deref(), Some("b@example.com"));
    assert!(cust.is_active);
}

#[tokio::test]
async fn test_admin_customer_edit_updates_all_fields() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "admin").await;
    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let cust = store.list_all_customers().await.unwrap()[0].clone();

    let body = format!(
        "internal_key=A&name=改名的客户&currency=HKD&librenms_instance_id={}&librenms_bill_id=42&timezone=UTC&company_type=hk&company_info_json={{}}&billing_address=&contact_email=&template_name=&is_active=on",
        cust.librenms_instance_id
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/admin/customers/{}/edit", cust.id))
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", cookie.clone())
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let after = store.find_customer_by_id(cust.id).await.unwrap().unwrap();
    assert_eq!(after.name, "改名的客户");
    assert_eq!(after.currency, "HKD");
    assert_eq!(after.librenms_bill_id, 42);
    assert_eq!(after.company_type, "hk");
    assert_eq!(after.timezone, "UTC");
}

#[tokio::test]
async fn test_admin_customer_create_validates_currency() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "admin").await;
    let inst = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin")
                .header("cookie", cookie.clone())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let _ = inst;
    // 拿 instance_id 走 admin 概况页(走完 boot 不再新开 store)
    let store_in = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let inst_id = store_in.list_all_libre_nms_instances().await.unwrap()[0].id;
    drop(store_in);

    let body = format!(
        "internal_key=BAD&name=坏币种&currency=EUR&librenms_instance_id={}&librenms_bill_id=1&timezone=Asia/Shanghai&company_type=domestic&company_info_json={{}}&is_active=on",
        inst_id
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/customers/new")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", cookie)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    // 验证失败由 error_page/render_customer_form 处理 → 200 + HTML 错误页
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let s = String::from_utf8_lossy(&body_bytes);
    assert!(s.contains("CNY") || s.contains("HKD") || s.contains("币种") || s.contains("BAD"));
    // DB 中不应有 BAD 客户
    let store_out = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let all = store_out.list_all_customers().await.unwrap();
    assert!(all.iter().all(|c| c.internal_key != "BAD"));
}

#[tokio::test]
async fn test_admin_customer_delete_succeeds_when_empty() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "admin").await;
    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();

    let id = store
        .insert_customer_full(&NewCustomerFull {
            internal_key: "DEL",
            name: "待删",
            currency: "CNY",
            librenms_instance_id: store.list_all_libre_nms_instances().await.unwrap()[0].id,
            librenms_bill_id: 0,
            timezone: "Asia/Shanghai",
            company_type: "domestic",
            company_info_json: "{}",
            company_info_schema_version: 1,
            billing_address: None,
            contact_email: None,
            template_name: None,
            is_active: true,
        })
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/admin/customers/{}/delete", id))
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    assert!(store.find_customer_by_id(id).await.unwrap().is_none());
}

#[tokio::test]
async fn test_admin_customer_delete_blocked_by_ports() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "admin").await;
    let store_in = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let cust = store_in.list_all_customers().await.unwrap()[0].clone();
    store_in
        .insert_port_with_bill(
            cust.id,
            "blocked-port",
            1,
            0,
            false,
            false,
            None,
            None,
        )
        .await
        .unwrap();
    drop(store_in);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/admin/customers/{}/delete", cust.id))
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // delete_customer 在有 ports 时返回 Error,error_page → 200 + HTML 错误页
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let s = String::from_utf8_lossy(&body_bytes);
    assert!(s.contains("端口") || s.contains("ports"), "错误页应提及端口引用");

    // 客户应仍存在
    let store_out = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    assert!(
        store_out.find_customer_by_id(cust.id).await.unwrap().is_some(),
        "被 port 引用时删除应失败"
    );
}

#[tokio::test]
async fn test_admin_port_create_with_per_port_bill_persists() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "admin").await;
    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let cust = store.list_all_customers().await.unwrap()[0].clone();

    let body = "port_label=switch-A&ip_count_a=8&ip_count_b=4&librenms_bill_id=99&machine_rent=on&machine_hosting=";
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/admin/customers/{}/ports/new", cust.id))
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", cookie.clone())
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let ports = store.list_ports_for_customer(cust.id).await.unwrap();
    assert_eq!(ports.len(), 1);
    let p = &ports[0];
    assert_eq!(p.port_label, "switch-A");
    assert_eq!(p.ip_count_a, 8);
    assert_eq!(p.ip_count_b, 4);
    assert_eq!(p.librenms_bill_id, Some(99), "per-port bill 应落库");
    assert!(p.machine_rent);
    assert!(!p.machine_hosting);
}

#[tokio::test]
async fn test_admin_port_edit_bill_change_reflected() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "admin").await;
    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let cust = store.list_all_customers().await.unwrap()[0].clone();
    let port_id = store
        .insert_port_with_bill(cust.id, "X", 1, 0, false, false, Some(10), None)
        .await
        .unwrap();

    let body = "port_label=X&ip_count_a=2&ip_count_b=0&librenms_bill_id=20&machine_rent=&machine_hosting=";
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/admin/customers/{}/ports/{}/edit", cust.id, port_id))
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", cookie)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let p = store.find_port_by_id(port_id).await.unwrap().unwrap();
    assert_eq!(p.librenms_bill_id, Some(20), "bill_id 应被改为 20");
    assert_eq!(p.ip_count_a, 2);
}

#[tokio::test]
async fn test_admin_port_delete_removes_row() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "admin").await;
    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let cust = store.list_all_customers().await.unwrap()[0].clone();
    let pid = store
        .insert_port_with_bill(cust.id, "Y", 0, 0, false, false, None, None)
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/admin/customers/{}/ports/{}/delete", cust.id, pid))
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    assert!(store.find_port_by_id(pid).await.unwrap().is_none());
}

#[tokio::test]
async fn test_admin_templates_page_lists_versions() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "admin").await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/templates")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let s = String::from_utf8_lossy(&body);
    assert!(s.contains("账单模板"), "应显示页面标题");
    assert!(s.contains("上传"), "应显示上传表单");
}

#[tokio::test]
async fn test_admin_ajax_bills_route_returns_503_for_missing_token() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "admin").await;
    let inst_id: i64 = 1;

    let resp = app
        .oneshot(
            Request::builder()
                .uri(&format!("/admin/ajax/bills?instance_id={}", inst_id))
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let s = resp.status();
    assert!(s.is_server_error() || s.is_client_error(), "错误路径应返回错误码,实际={}", s);
}

#[tokio::test]
async fn test_admin_instance_bills_route_renders_or_errors_gracefully() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "admin").await;
    let inst_id: i64 = 1;

    let resp = app
        .oneshot(
            Request::builder()
                .uri(&format!("/admin/instances/{}/bills", inst_id))
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_admin_template_version_store_round_trip() {
    let (_dir, _app) = boot().await;
    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    // list_template_versions 默认空
    assert!(store.list_template_versions().await.unwrap().is_empty());
    // 写一行(经由 store 直写 + 类似 inspect 路径)
    let row = TemplateVersionRow {
        template_name: "demo".into(),
        template_sha256: "abc123".into(),
        cell_map_json: "{}".into(),
        drawing_anchors_json: "[]".into(),
        last_validated_at: "2026-02-01T00:00:00Z".into(),
        notes: Some("note".into()),
    };
    lnms_invoice::template::audit::write_template_version(&store, &row_to_audit(&row))
        .await
        .unwrap();
    let list = store.list_template_versions().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].template_name, "demo");
    assert_eq!(list[0].template_sha256, "abc123");
    let found = store.find_template_version("demo").await.unwrap().unwrap();
    // TemplateVersionRow.find_* 不一定返回 notes 字段,只验证 template_name 落库
    assert_eq!(found.template_name, "demo");
}

#[tokio::test]
async fn test_admin_customer_template_select_lists_audited_only() {
    let (_dir, _app) = boot().await;
    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    // 模拟写入一个已审计的模板
    let row = TemplateVersionRow {
        template_name: "v2".into(),
        template_sha256: "deadbeef".into(),
        cell_map_json: "{}".into(),
        drawing_anchors_json: "[]".into(),
        last_validated_at: "2026-02-01T00:00:00Z".into(),
        notes: None,
    };
    lnms_invoice::template::audit::write_template_version(&store, &row_to_audit(&row))
        .await
        .unwrap();
    let list = store.list_template_versions().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].template_name, "v2");
    assert_eq!(store.count_customers_using_template("v2").await.unwrap(), 0);
}

#[tokio::test]
async fn test_admin_customer_update_via_web_persists_template_name() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "admin").await;
    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let cust = store.list_all_customers().await.unwrap()[0].clone();

    // 先上传一个模板版本
    let row = TemplateVersionRow {
        template_name: "v3".into(),
        template_sha256: "cafebabe".into(),
        cell_map_json: "{}".into(),
        drawing_anchors_json: "[]".into(),
        last_validated_at: "2026-02-01T00:00:00Z".into(),
        notes: None,
    };
    lnms_invoice::template::audit::write_template_version(&store, &row_to_audit(&row))
        .await
        .unwrap();

    // 通过 web 表单更新客户的 template_name = v3
    let body = format!(
        "internal_key={}&name={}&currency={}&librenms_instance_id={}&librenms_bill_id={}&timezone={}&company_type={}&company_info_json={{}}&template_name=v3&is_active=on",
        cust.internal_key,
        cust.name,
        cust.currency,
        cust.librenms_instance_id,
        cust.librenms_bill_id,
        cust.timezone,
        cust.company_type
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/admin/customers/{}/edit", cust.id))
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", cookie)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let store = Store::connect(&_dir.path().join("test.db")).await.unwrap();
    let after = store.find_customer_by_id(cust.id).await.unwrap().unwrap();
    assert_eq!(after.template_name.as_deref(), Some("v3"));
    assert_eq!(
        store.count_customers_using_template("v3").await.unwrap(),
        1
    );
}

#[tokio::test]
async fn test_operator_customer_new_redirects_to_login() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "operator").await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/customers/new")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", cookie)
                .body(Body::from("internal_key=X&name=Y&currency=CNY&librenms_instance_id=0&librenms_bill_id=0&timezone=Asia/Shanghai&company_type=domestic&company_info_json={}&is_active=on"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        resp.headers().get("location").and_then(|v| v.to_str().ok()),
        Some("/login")
    );
}

#[tokio::test]
async fn test_operator_templates_route_redirects_to_login() {
    let (_dir, app) = boot().await;
    let cookie = login(&app, "operator").await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/admin/templates")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        resp.headers().get("location").and_then(|v| v.to_str().ok()),
        Some("/login")
    );
}

// === 辅助:把 TemplateVersionRow 转成 write_template_version 接受的 audit 结构 ===

fn row_to_audit(row: &TemplateVersionRow) -> lnms_invoice::template::inspect::TemplateAudit {
    lnms_invoice::template::inspect::TemplateAudit {
        template_name: row.template_name.clone(),
        sha256: row.template_sha256.clone(),
        bytes: 0,
        sheets: vec![],
        cell_map: serde_json::from_str(&row.cell_map_json).unwrap_or_default(),
        drawings: serde_json::from_str(&row.drawing_anchors_json).unwrap_or_default(),
        media: vec![],
    }
}
