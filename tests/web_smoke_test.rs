//! 阶段 6 Web 层冒烟测试
//!
//! - /login GET 返回 200 + 表单
//! - /login POST 错密码 → 200 + 错误提示
//! - /login POST 正确密码 → 303 + Set-Cookie
//! - 持 cookie 访问 / → 200 + 用户名出现在 HTML

use lnms_invoice::config::Config;
use lnms_invoice::runner::InvoiceService;
use lnms_invoice::store::{NewCustomer, Store};
use lnms_invoice::web::{router, WebState};
use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
use argon2::Argon2;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use std::path::PathBuf;

async fn boot() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("test.db");
    let store = Store::connect(&db).await.unwrap();

    // 注入一个 LibreNMS 实例 + 一个客户
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

    // 用 Argon2 算个真哈希
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(b"hunter2", &salt)
        .unwrap()
        .to_string();
    store.insert_user("admin", &hash, "admin").await.unwrap();

    // 构造 service + state
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

#[tokio::test]
async fn test_login_get_renders_form() {
    let (_dir, app) = boot().await;
    let resp = app
        .oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let s = String::from_utf8_lossy(&body);
    assert!(s.contains("登录"));
    assert!(s.contains(r#"name="username""#));
}

#[tokio::test]
async fn test_login_post_wrong_password_returns_form_with_error() {
    let (_dir, app) = boot().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("username=admin&password=wrong"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let s = String::from_utf8_lossy(&body);
    assert!(s.contains("用户名或密码错误"));
}

#[tokio::test]
async fn test_login_post_correct_sets_cookie_and_redirects() {
    let (_dir, app) = boot().await;
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("username=admin&password=hunter2"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let cookie = resp
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap();
    assert!(cookie.contains("lnms_inv_session="));
}

#[tokio::test]
async fn test_dashboard_without_cookie_redirects_to_login() {
    let (_dir, app) = boot().await;
    let resp = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        resp.headers().get("location").and_then(|v| v.to_str().ok()),
        Some("/login")
    );
}

#[tokio::test]
async fn test_dashboard_with_valid_cookie_renders_customer_list() {
    let (_dir, app) = boot().await;
    // 先登录拿 cookie
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("username=admin&password=hunter2"))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie_val = resp
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    // 用 cookie 访问 /
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/")
                .header("cookie", cookie_val)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let s = String::from_utf8_lossy(&body);
    assert!(s.contains("客户列表"));
    assert!(s.contains("测试客户"));
}