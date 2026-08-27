//! Web 层(阶段 6)
//!
//! axum 路由:
//! - GET  /login                       登录表单
//! - POST /login                       提交用户名/密码
//! - POST /logout                      注销
//! - GET  /                            dashboard(列出全部 active 客户)
//! - GET  /customers/:id               单客户的所有发票
//! - GET  /invoices/:id                单发票预览(显示 status / total / preview PDF)
//! - POST /invoices/:id/confirm        生成 final(原子 rename)
//! - POST /invoices/:id/reject         拒绝并要求 reason
//! - POST /invoices/:id/regenerate     重生成(仅 preview/rejected/failed)
//! - GET  /invoices/:id/file/preview   走文件流返回 preview PDF(需登录)
//! - GET  /invoices/:id/file/final     走文件流返回 final PDF(需登录)
//!
//! 鉴权:HMAC-SHA256 签名 cookie,载荷 `{user_id}` + `expires_at`,key 来自 `Config.web.session_secret`。
//! 阶段 7 部署时,secret 走 systemd LoadCredential,不会写进配置文件。

use crate::config::Config;
use crate::error::{Error, Result};
use crate::librenms::LibreNmsClient;
use crate::runner::InvoiceService;
use crate::store::{
    Customer, CustomerFullUpdate, Invoice, InvoiceStatus, NewCustomerFull, NewRate, User,
};
use crate::template::audit::write_template_version;
use crate::template::inspect::inspect;
use argon2::password_hash::{PasswordHash, PasswordVerifier};
use argon2::Argon2;
use askama::Template;
use axum::{
    extract::{Form, Multipart, Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct WebState {
    pub svc: InvoiceService,
    pub session_secret: Vec<u8>,
}

impl WebState {
    pub fn from_config(svc: InvoiceService, cfg: &Config) -> Self {
        Self {
            svc,
            session_secret: cfg.web.session_secret.clone().into_bytes(),
        }
    }
}

pub fn router(state: WebState) -> Router {
    Router::new()
        .route("/login", get(get_login).post(post_login))
        .route("/logout", post(post_logout))
        .route("/", get(get_dashboard))
        .route("/customers/:id", get(get_customer))
        .route("/invoices/:id", get(get_invoice))
        .route("/invoices/:id/confirm", post(post_invoice_confirm))
        .route("/invoices/:id/reject", post(post_invoice_reject))
        .route("/invoices/:id/regenerate", post(post_invoice_regenerate))
        .route("/invoices/:id/file/preview", get(get_invoice_file_preview))
        .route("/invoices/:id/file/final", get(get_invoice_file_final))
        // 管理后台(仅 admin 角色)
        .route("/admin", get(get_admin_home))
        .route("/admin/instances", get(get_admin_instances))
        .route("/admin/instances/new", post(post_admin_instance_create))
        .route("/admin/instances/:id/edit", post(post_admin_instance_update))
        .route("/admin/instances/:id/toggle-active", post(post_admin_instance_toggle_active))
        .route("/admin/instances/:id/delete", post(post_admin_instance_delete))
        .route("/admin/instances/:id/bills", get(get_admin_instance_bills))
        .route("/admin/ajax/bills", get(get_admin_ajax_bills))
        .route("/admin/customers", get(get_admin_customers))
        .route("/admin/customers/new", get(get_admin_customer_new).post(post_admin_customer_create))
        .route("/admin/customers/:id", get(get_admin_customer_detail))
        .route("/admin/customers/:id/edit", get(get_admin_customer_edit).post(post_admin_customer_update))
        .route("/admin/customers/:id/delete", post(post_admin_customer_delete))
        .route("/admin/customers/:id/ports/new", post(post_admin_port_create))
        .route("/admin/customers/:id/ports/:port_id/edit", post(post_admin_port_update))
        .route("/admin/customers/:id/ports/:port_id/delete", post(post_admin_port_delete))
        .route("/admin/customers/:id/toggle-active", post(post_admin_customer_toggle_active))
        .route("/admin/rates", get(get_admin_rates))
        .route("/admin/rates/new", post(post_admin_rate_create))
        .route("/admin/rates/:id/delete", post(post_admin_rate_delete))
        .route("/admin/templates", get(get_admin_templates))
        .route("/admin/templates/upload", post(post_admin_template_upload))
        .route("/admin/settings", get(get_admin_settings).post(post_admin_settings))
        .with_state(Arc::new(state))
}

// ============================================================
// Session cookie(HMAC-SHA256 签名,不依赖外部 session 中间件)
// ============================================================

const COOKIE_NAME: &str = "lnms_inv_session";
const COOKIE_TTL_SECS: i64 = 8 * 3600;

#[derive(Debug, Serialize, Deserialize)]
struct SessionPayload {
    user_id: i64,
    role: String,
    expires_at: i64,
    nonce: i64,
}

fn b64_encode(b: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(b)
}
fn b64_decode(s: &str) -> Result<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|e| Error::Config(format!("bad session b64: {e}")))
}

fn sign_cookie(state: &WebState, payload: &SessionPayload) -> Result<String> {
    let json = serde_json::to_vec(payload)
        .map_err(|e| Error::Internal(format!("session json: {e}")))?;
    let mut mac = HmacSha256::new_from_slice(&state.session_secret)
        .map_err(|e| Error::Config(format!("hmac key: {e}")))?;
    mac.update(&json);
    let tag = mac.finalize().into_bytes();
    let mut cookie = String::with_capacity(json.len() * 2 + 32);
    cookie.push_str(&b64_encode(&json));
    cookie.push('.');
    cookie.push_str(&b64_encode(&tag));
    Ok(cookie)
}

fn verify_cookie(state: &WebState, cookie: &str) -> Result<Option<SessionPayload>> {
    let (json_b64, tag_b64) = match cookie.split_once('.') {
        Some((j, t)) => (j, t),
        None => return Ok(None),
    };
    let json = b64_decode(json_b64)?;
    let tag = b64_decode(tag_b64)?;
    let mut mac = HmacSha256::new_from_slice(&state.session_secret)
        .map_err(|e| Error::Config(format!("hmac key: {e}")))?;
    mac.update(&json);
    let expected = mac.finalize().into_bytes();
    if expected.ct_eq(&tag).unwrap_u8() != 1 {
        return Ok(None);
    }
    let payload: SessionPayload = serde_json::from_slice(&json)
        .map_err(|e| Error::Config(format!("bad session json: {e}")))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| Error::Internal(format!("clock: {e}")))?
        .as_secs() as i64;
    if payload.expires_at < now {
        return Ok(None);
    }
    Ok(Some(payload))
}

fn extract_cookie(headers: &HeaderMap) -> Option<&str> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for kv in raw.split(';') {
        if let Some((k, v)) = kv.trim().split_once('=') {
            if k == COOKIE_NAME {
                return Some(v);
            }
        }
    }
    None
}

async fn current_user_or_redirect(state: &WebState, headers: &HeaderMap) -> Result<Option<User>> {
    let cookie = match extract_cookie(headers) {
        Some(c) => c,
        None => return Ok(None),
    };
    let p = match verify_cookie(state, cookie) {
        Ok(Some(p)) => p,
        _ => return Ok(None),
    };
    let store = state.svc.store();
    store.find_user_by_id(p.user_id).await
}

// ============================================================
// askama 模板
// ============================================================

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTpl<'a> {
    error: Option<&'a str>,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTpl<'a> {
    username: &'a str,
    role: &'a str,
    customers: Vec<CustomerRow>,
}

struct CustomerRow {
    id: i64,
    internal_key: String,
    name: String,
    currency: String,
}

#[derive(Template)]
#[template(path = "customer.html")]
struct CustomerTpl<'a> {
    customer_name: &'a str,
    customer_id: i64,
    invoice_count: usize,
    invoices: Vec<InvoiceRow>,
}

struct InvoiceRow {
    id: i64,
    period: String,
    status: String,
    invoice_no: String,
    total_cents: Option<i64>,
    currency: String,
}

#[derive(Template)]
#[template(path = "invoice.html")]
struct InvoiceTpl<'a> {
    invoice: &'a Invoice,
    customer_name: &'a str,
    period_label: String,
    total_yuan: String,
    total_frac: String,
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorTpl<'a> {
    message: &'a str,
}

// ============================================================
// 路由处理
// ============================================================

async fn get_login() -> Html<String> {
    Html(
        LoginTpl { error: None }
            .render()
            .unwrap_or_else(|_| "template error".into()),
    )
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

async fn post_login(
    State(state): State<Arc<WebState>>,
    Form(form): Form<LoginForm>,
) -> Response {
    let store = state.svc.store();
    let user_opt = store.find_user_by_username(&form.username).await.ok().flatten();
    let user = match user_opt {
        Some(u) if u.is_active => u,
        _ => {
            return render_login_error("用户名或密码错误");
        }
    };
    let parsed = match PasswordHash::new(&user.password_hash) {
        Ok(p) => p,
        Err(_) => return render_login_error("密码哈希格式错误(联系运维)"),
    };
    let argon = Argon2::default();
    if argon
        .verify_password(form.password.as_bytes(), &parsed)
        .is_err()
    {
        return render_login_error("用户名或密码错误");
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut nonce_bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = i64::from_le_bytes(nonce_bytes);
    let payload = SessionPayload {
        user_id: user.id,
        role: user.role.clone(),
        expires_at: now + COOKIE_TTL_SECS,
        nonce,
    };
    let cookie = sign_cookie(&state, &payload).expect("sign cookie");
    let _ = store.update_user_last_login(user.id).await;
    let cookie_header = format!(
        "{COOKIE_NAME}={cookie}; HttpOnly; SameSite=Strict; Path=/; Max-Age={COOKIE_TTL_SECS}"
    );
    (
        StatusCode::SEE_OTHER,
        [(header::SET_COOKIE, cookie_header), (header::LOCATION, "/".into())],
        "",
    )
        .into_response()
}

fn render_login_error(msg: &str) -> Response {
    Html(
        LoginTpl {
            error: Some(msg),
        }
        .render()
        .unwrap_or_else(|_| msg.to_string()),
    )
    .into_response()
}

async fn post_logout() -> Response {
    let cookie_header = format!("{COOKIE_NAME}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
    (
        StatusCode::SEE_OTHER,
        [(header::SET_COOKIE, cookie_header), (header::LOCATION, "/".into())],
        "",
    )
        .into_response()
}

async fn get_dashboard(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
) -> Response {
    let user = match current_user_or_redirect(&state, &headers)
        .await
        .ok()
        .flatten()
    {
        Some(u) => u,
        None => return Redirect::to("/login").into_response(),
    };
    let store = state.svc.store();
    let customers = match store.list_active_customers().await {
        Ok(cs) => cs,
        Err(e) => return error_page(&e.to_string()),
    };
    let rows = customers
        .into_iter()
        .map(|c| CustomerRow {
            id: c.id,
            internal_key: c.internal_key,
            name: c.name,
            currency: c.currency,
        })
        .collect();
    Html(
        DashboardTpl {
            username: &user.username,
            role: &user.role,
            customers: rows,
        }
        .render()
        .unwrap_or_else(|_| "template error".into()),
    )
    .into_response()
}

async fn get_customer(
    State(state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    if current_user_or_redirect(&state, &headers)
        .await
        .ok()
        .flatten()
        .is_none()
    {
        return Redirect::to("/login").into_response();
    }
    let store = state.svc.store();
    let customer = match store.find_customer_by_id(id).await {
        Ok(Some(c)) => c,
        Ok(None) => return error_page("客户不存在"),
        Err(e) => return error_page(&e.to_string()),
    };
    let invoices = match store.list_invoices_for_customer(id).await {
        Ok(v) => v,
        Err(e) => return error_page(&e.to_string()),
    };
    let invoice_count = invoices.len();
    let rows = invoices
        .into_iter()
        .map(|inv| InvoiceRow {
            id: inv.id,
            period: format!("{:04}-{:02}", inv.period_year, inv.period_month),
            status: inv.status.as_str().to_string(),
            invoice_no: inv.invoice_no,
            total_cents: inv.total_cents,
            currency: inv.currency,
        })
        .collect();
    Html(
        CustomerTpl {
            customer_name: &customer.name,
            customer_id: id,
            invoice_count,
            invoices: rows,
        }
        .render()
        .unwrap_or_else(|_| "template error".into()),
    )
    .into_response()
}

async fn get_invoice(
    State(state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    if current_user_or_redirect(&state, &headers)
        .await
        .ok()
        .flatten()
        .is_none()
    {
        return Redirect::to("/login").into_response();
    }
    let store = state.svc.store();
    let inv = match store.find_invoice(id).await {
        Ok(Some(i)) => i,
        Ok(None) => return error_page("发票不存在"),
        Err(e) => return error_page(&e.to_string()),
    };
    let customer = match store.find_customer_by_id(inv.customer_id).await {
        Ok(Some(c)) => c,
        _ => return error_page("客户不存在"),
    };
    let period_label = format!("{:04}-{:02}", inv.period_year, inv.period_month);
    let (total_yuan, total_frac) = match inv.total_cents {
        Some(c) => (format!("{}", c / 100), format!("{:02}", c % 100)),
        None => (String::new(), String::new()),
    };
    Html(
        InvoiceTpl {
            invoice: &inv,
            customer_name: &customer.name,
            period_label,
            total_yuan,
            total_frac,
        }
        .render()
        .unwrap_or_else(|_| "template error".into()),
    )
    .into_response()
}

#[derive(Deserialize)]
struct RejectForm {
    reason: String,
}

async fn post_invoice_confirm(
    State(state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let user = match current_user_or_redirect(&state, &headers)
        .await
        .ok()
        .flatten()
    {
        Some(u) => u,
        None => return Redirect::to("/login").into_response(),
    };
    match state.svc.confirm(id, user.id).await {
        Ok(_) => Redirect::to(&format!("/invoices/{id}")).into_response(),
        Err(e) => error_page(&format!("确认失败: {e}")),
    }
}

async fn post_invoice_reject(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<RejectForm>,
) -> Response {
    let user = match current_user_or_redirect(&state, &headers)
        .await
        .ok()
        .flatten()
    {
        Some(u) => u,
        None => return Redirect::to("/login").into_response(),
    };
    if form.reason.trim().is_empty() {
        return error_page("请填写拒绝原因");
    }
    match state.svc.reject(id, user.id, &form.reason).await {
        Ok(_) => Redirect::to(&format!("/invoices/{id}")).into_response(),
        Err(e) => error_page(&format!("拒绝失败: {e}")),
    }
}

async fn post_invoice_regenerate(
    State(state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let _user = match current_user_or_redirect(&state, &headers)
        .await
        .ok()
        .flatten()
    {
        Some(u) => u,
        None => return Redirect::to("/login").into_response(),
    };
    match state.svc.regenerate(id).await {
        Ok(new_id) => Redirect::to(&format!("/invoices/{new_id}")).into_response(),
        Err(e) => error_page(&format!("重生成失败: {e}")),
    }
}

async fn get_invoice_file_preview(
    State(state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    if current_user_or_redirect(&state, &headers)
        .await
        .ok()
        .flatten()
        .is_none()
    {
        return Redirect::to("/login").into_response();
    }
    stream_pdf(state, id, true).await
}

async fn get_invoice_file_final(
    State(state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    if current_user_or_redirect(&state, &headers)
        .await
        .ok()
        .flatten()
        .is_none()
    {
        return Redirect::to("/login").into_response();
    }
    stream_pdf(state, id, false).await
}

async fn stream_pdf(state: Arc<WebState>, id: i64, preview: bool) -> Response {
    let store = state.svc.store();
    let inv = match store.find_invoice(id).await {
        Ok(Some(i)) => i,
        _ => return error_page("发票不存在"),
    };
    let path = if preview {
        inv.pdf_path_preview.clone()
    } else {
        inv.pdf_path_final.clone()
    };
    let path = match path {
        Some(p) if std::path::Path::new(&p).exists() => p,
        _ => return error_page("PDF 文件不存在"),
    };
    match std::fs::read(&path) {
        Ok(bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/pdf".to_string())],
            bytes,
        )
            .into_response(),
        Err(e) => error_page(&format!("读 PDF 失败: {e}")),
    }
}

fn error_page(msg: &str) -> Response {
    Html(
        ErrorTpl { message: msg }
            .render()
            .unwrap_or_else(|_| msg.to_string()),
    )
    .into_response()
}

// ============================================================
// 管理后台(阶段 8,仅 admin 角色)
// ============================================================

/// 解析 cookie 并要求 role == "admin",否则 403
async fn require_admin(state: &WebState, headers: &HeaderMap) -> Result<User> {
    let cookie = extract_cookie(headers).ok_or_else(|| Error::NotFound("no session cookie".into()))?;
    let p = verify_cookie(state, cookie)?
        .ok_or_else(|| Error::InvalidTransition("invalid/expired session".into()))?;
    if p.role != "admin" {
        return Err(Error::InvalidTransition(format!(
            "role '{}' is not admin",
            p.role
        )));
    }
    let store = state.svc.store();
    store
        .find_user_by_id(p.user_id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("user {}", p.user_id)))
}

#[derive(Template)]
#[template(path = "admin/home.html")]
struct AdminHomeTpl<'a> {
    username: &'a str,
    counts: AdminCounts,
}

struct AdminCounts {
    instances: usize,
    customers: usize,
    rates: usize,
    users: usize,
}

#[derive(Template)]
#[template(path = "admin/instances.html")]
struct AdminInstancesTpl<'a> {
    username: &'a str,
    instances: Vec<InstanceRow>,
    error: Option<String>,
}

struct InstanceRow {
    id: i64,
    name: String,
    url: String,
    is_active: bool,
    token_set: bool,
}

#[derive(Template)]
#[template(path = "admin/customers.html")]
struct AdminCustomersTpl<'a> {
    username: &'a str,
    customers: Vec<AdminCustomerRow>,
}

struct AdminCustomerRow {
    id: i64,
    internal_key: String,
    name: String,
    currency: String,
    is_active: bool,
}

#[derive(Template)]
#[template(path = "admin/customer_detail.html")]
struct AdminCustomerDetailTpl<'a> {
    username: &'a str,
    customer: Customer,
    ports: Vec<AdminPortRow>,
    rates: Vec<AdminRateRow>,
}

struct AdminPortRow {
    id: i64,
    label: String,
    machine_rent: bool,
    machine_hosting: bool,
    librenms_bill_id: Option<i64>,
}

struct AdminRateRow {
    id: i64,
    effective_from: String,
    effective_to: Option<String>,
    mbps_unit_price_cents: i64,
    ip_unit_price_cents: i64,
    ip_quantity: i64,
    machine_rent_cents: i64,
    machine_hosting_cents: i64,
    currency: String,
    librenms_bill_id: Option<i64>,
    business_label: Option<String>,
    notes: String,
}

#[derive(Template)]
#[template(path = "admin/rates.html")]
struct AdminRatesTpl<'a> {
    username: &'a str,
    rates: Vec<AdminRateWithCustomerRow>,
    customer_options: Vec<AdminCustomerOption>,
    new_form_effective_from: &'a str,
    new_form_effective_to: &'a str,
    new_form_mbps_unit_price_cents: &'a str,
    new_form_ip_unit_price_cents: &'a str,
    new_form_ip_quantity: &'a str,
    new_form_machine_rent_cents: &'a str,
    new_form_machine_hosting_cents: &'a str,
    new_form_currency: &'a str,
    new_form_business_label: &'a str,
    new_form_notes: &'a str,
    error: Option<String>,
}

struct AdminRateWithCustomerRow {
    id: i64,
    customer_internal_key: String,
    effective_from: String,
    effective_to: Option<String>,
    mbps_unit_price_cents: i64,
    ip_unit_price_cents: i64,
    ip_quantity: i64,
    machine_rent_cents: i64,
    machine_hosting_cents: i64,
    currency: String,
    librenms_bill_id: Option<i64>,
    business_label: Option<String>,
    notes: String,
}

struct AdminCustomerOption {
    id: i64,
    internal_key: String,
    name: String,
}

async fn get_admin_home(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
) -> Response {
    let user = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    let store = state.svc.store();
    let counts = AdminCounts {
        instances: store
            .list_active_libre_nms_instances()
            .await
            .map(|v| v.len())
            .unwrap_or(0),
        customers: store
            .list_all_customers()
            .await
            .map(|v| v.len())
            .unwrap_or(0),
        rates: store.list_all_rates().await.map(|v| v.len()).unwrap_or(0),
        users: store.count_users().await.unwrap_or(0) as usize,
    };
    Html(
        AdminHomeTpl {
            username: &user.username,
            counts,
        }
        .render()
        .unwrap_or_else(|_| "template error".into()),
    )
    .into_response()
}

async fn get_admin_instances(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
) -> Response {
    let user = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    let store = state.svc.store();
    let list = match store.list_all_libre_nms_instances().await {
        Ok(v) => v,
        Err(e) => return error_page(&e.to_string()),
    };
    let mut rows = Vec::with_capacity(list.len());
    for i in list {
        let token_set = store
            .librenms_instance_token_set(i.id)
            .await
            .unwrap_or(false);
        rows.push(InstanceRow {
            id: i.id,
            name: i.name,
            url: i.url,
            is_active: i.is_active,
            token_set,
        });
    }
    Html(
        AdminInstancesTpl {
            username: &user.username,
            instances: rows,
            error: None,
        }
        .render()
        .unwrap_or_else(|_| "template error".into()),
    )
    .into_response()
}

async fn get_admin_customers(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
) -> Response {
    let user = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    let store = state.svc.store();
    let list = match store.list_all_customers().await {
        Ok(v) => v,
        Err(e) => return error_page(&e.to_string()),
    };
    let rows = list
        .into_iter()
        .map(|c| AdminCustomerRow {
            id: c.id,
            internal_key: c.internal_key,
            name: c.name,
            currency: c.currency,
            is_active: c.is_active,
        })
        .collect();
    Html(
        AdminCustomersTpl {
            username: &user.username,
            customers: rows,
        }
        .render()
        .unwrap_or_else(|_| "template error".into()),
    )
    .into_response()
}

async fn get_admin_customer_detail(
    State(state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let user = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    let store = state.svc.store();
    let customer = match store.find_customer_by_id(id).await {
        Ok(Some(c)) => c,
        Ok(None) => return error_page("客户不存在"),
        Err(e) => return error_page(&e.to_string()),
    };
    let ports: Vec<AdminPortRow> = store
        .list_ports_for_customer(id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|p| AdminPortRow {
            id: p.id,
            label: p.port_label,
            machine_rent: p.machine_rent,
            machine_hosting: p.machine_hosting,
            librenms_bill_id: p.librenms_bill_id,
        })
        .collect();
    let rates: Vec<AdminRateRow> = store
        .list_rates_for_customer(id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| AdminRateRow {
            id: r.id,
            effective_from: r.effective_from,
            effective_to: r.effective_to,
            mbps_unit_price_cents: r.mbps_unit_price_cents,
            ip_unit_price_cents: r.ip_unit_price_cents,
            ip_quantity: r.ip_quantity,
            machine_rent_cents: r.machine_rent_cents,
            machine_hosting_cents: r.machine_hosting_cents,
            currency: r.currency,
            librenms_bill_id: r.librenms_bill_id,
            business_label: r.business_label,
            notes: r.notes,
        })
        .collect();
    let customer_row = customer;
    Html(
        AdminCustomerDetailTpl {
            username: &user.username,
            customer: customer_row,
            ports,
            rates,
        }
        .render()
        .unwrap_or_else(|_| "template error".into()),
    )
    .into_response()
}

async fn post_admin_customer_toggle_active(
    State(state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let _ = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    let store = state.svc.store();
    let c = match store.find_customer_by_id(id).await {
        Ok(Some(c)) => c,
        Ok(None) => return error_page("客户不存在"),
        Err(e) => return error_page(&e.to_string()),
    };
    if let Err(e) = store.set_customer_active(id, !c.is_active).await {
        return error_page(&e.to_string());
    }
    Redirect::to(&format!("/admin/customers/{id}")).into_response()
}

async fn get_admin_rates(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
) -> Response {
    let user = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    let store = state.svc.store();
    let rates = match store.list_all_rates().await {
        Ok(v) => v,
        Err(e) => return error_page(&e.to_string()),
    };
    let customers = match store.list_all_customers().await {
        Ok(v) => v,
        Err(e) => return error_page(&e.to_string()),
    };
    // 拼 internal_key 出来
    let key_for_id: std::collections::HashMap<i64, String> =
        customers.iter().map(|c| (c.id, c.internal_key.clone())).collect();
    let rate_rows: Vec<AdminRateWithCustomerRow> = rates
        .into_iter()
        .map(|r| AdminRateWithCustomerRow {
            id: r.id,
            customer_internal_key: key_for_id
                .get(&r.customer_id)
                .cloned()
                .unwrap_or_else(|| format!("id={}", r.customer_id)),
            effective_from: r.effective_from,
            effective_to: r.effective_to,
            mbps_unit_price_cents: r.mbps_unit_price_cents,
            ip_unit_price_cents: r.ip_unit_price_cents,
            ip_quantity: r.ip_quantity,
            machine_rent_cents: r.machine_rent_cents,
            machine_hosting_cents: r.machine_hosting_cents,
            currency: r.currency,
            librenms_bill_id: r.librenms_bill_id,
            business_label: r.business_label,
            notes: r.notes,
        })
        .collect();
    let customer_options: Vec<AdminCustomerOption> = customers
        .into_iter()
        .map(|c| AdminCustomerOption {
            id: c.id,
            internal_key: c.internal_key,
            name: c.name,
        })
        .collect();
    Html(
        AdminRatesTpl {
            username: &user.username,
            rates: rate_rows,
            customer_options,
            new_form_effective_from: "",
            new_form_effective_to: "",
            new_form_mbps_unit_price_cents: "",
            new_form_ip_unit_price_cents: "",
            new_form_ip_quantity: "0",
            new_form_machine_rent_cents: "0",
            new_form_machine_hosting_cents: "0",
            new_form_currency: "CNY",
            new_form_business_label: "",
            new_form_notes: "",
            error: None,
        }
        .render()
        .unwrap_or_else(|_| "template error".into()),
    )
    .into_response()
}

#[derive(Deserialize)]
struct NewRateFormIn {
    customer_id: i64,
    effective_from: String,
    effective_to: Option<String>,
    mbps_unit_price_cents: i64,
    ip_unit_price_cents: i64,
    /// IP 数量(v0.6.3 起在费用表单上直接维护,不再从端口累加)
    ip_quantity: i64,
    machine_rent_cents: i64,
    machine_hosting_cents: i64,
    currency: String,
    librenms_bill_id: Option<String>,
    /// v0.6.4: 业务名称/备注(纯元数据,不参与计费)
    #[serde(default)]
    business_label: Option<String>,
    /// 用户自定义备注
    #[serde(default)]
    notes: Option<String>,
}

async fn post_admin_rate_create(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
    Form(form): Form<NewRateFormIn>,
) -> Response {
    let _ = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    let store = state.svc.store();
    let customer = match store.find_customer_by_id(form.customer_id).await {
        Ok(Some(c)) => c,
        Ok(None) => return error_page("客户不存在"),
        Err(e) => return error_page(&e.to_string()),
    };
    // 表单空字符串视同 NULL
    let effective_to = form
        .effective_to
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    // 可选 bill id:留空 = 不指定(回落客户默认);填了则端口未绑定时优先用它
    let librenms_bill_id = match form
        .librenms_bill_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None => None,
        Some(s) => match s.parse::<i64>() {
            Ok(n) if n > 0 => Some(n),
            _ => return error_page("LNMS bill_id 必须是正整数或留空"),
        },
    };
    let business_label = form
        .business_label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let notes = form
        .notes
        .as_deref()
        .map(str::trim)
        .unwrap_or("");
    let nr = NewRate {
        customer_id: customer.id,
        effective_from: &form.effective_from,
        effective_to,
        mbps_unit_price_cents: form.mbps_unit_price_cents,
        ip_unit_price_cents: form.ip_unit_price_cents,
        ip_quantity: form.ip_quantity.max(0),
        machine_rent_cents: form.machine_rent_cents,
        machine_hosting_cents: form.machine_hosting_cents,
        currency: &form.currency,
        librenms_bill_id,
        business_label,
        notes,
    };
    if let Err(e) = store.insert_rate(&nr).await {
        return error_page(&e.to_string());
    }
    Redirect::to("/admin/rates").into_response()
}

async fn post_admin_rate_delete(
    State(state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let _ = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    if let Err(e) = state.svc.store().delete_rate(id).await {
        return error_page(&e.to_string());
    }
    Redirect::to("/admin/rates").into_response()
}

// ============================================================
// LibreNMS 实例 CRUD(token 仍走 sudo set-instance-token)
// ============================================================

#[derive(Deserialize)]
struct NewInstanceFormIn {
    name: String,
    url: String,
    /// API token(可选)。非空时随实例一起存入;留空等价于创建后再注入。
    token: Option<String>,
}

async fn post_admin_instance_create(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
    Form(form): Form<NewInstanceFormIn>,
) -> Response {
    let _ = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    let name = form.name.trim();
    let url = form.url.trim();
    if name.is_empty() || url.is_empty() {
        return error_page("名称与 URL 都不能为空");
    }
    let token_bytes: &[u8] = match form.token.as_deref().map(str::trim) {
        Some(t) if !t.is_empty() => t.as_bytes(),
        _ => b"",
    };
    let store = state.svc.store();
    if let Err(e) = store.insert_libre_nms_instance(name, url, token_bytes).await {
        return error_page(&e.to_string());
    }
    Redirect::to("/admin/instances").into_response()
}

#[derive(Deserialize)]
struct EditInstanceFormIn {
    name: String,
    url: String,
    is_active: Option<String>,
    /// API token(可选)。留空保留当前值;非空会覆盖。
    token: Option<String>,
}

async fn post_admin_instance_update(
    State(state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<EditInstanceFormIn>,
) -> Response {
    let _ = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    let name = form.name.trim();
    let url = form.url.trim();
    if name.is_empty() || url.is_empty() {
        return error_page("名称与 URL 都不能为空");
    }
    let is_active = matches!(form.is_active.as_deref(), Some("on") | Some("1") | Some("true"));
    let store = state.svc.store();
    if let Err(e) = store.update_librenms_instance(id, name, url, is_active).await {
        return error_page(&e.to_string());
    }
    // token 字段留空保留当前值,非空才覆盖(避免误清空)
    if let Some(raw) = form.token.as_deref().map(str::trim) {
        if !raw.is_empty() {
            if let Err(e) = store.update_librenms_instance_token(id, raw.as_bytes()).await {
                return error_page(&e.to_string());
            }
        }
    }
    Redirect::to("/admin/instances").into_response()
}

async fn post_admin_instance_toggle_active(
    State(state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let _ = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    let store = state.svc.store();
    let inv = match store.find_librenms_instance(id).await {
        Ok(Some(i)) => i,
        Ok(None) => return error_page("实例不存在"),
        Err(e) => return error_page(&e.to_string()),
    };
    if let Err(e) = store
        .set_librenms_instance_active(id, !inv.is_active)
        .await
    {
        return error_page(&e.to_string());
    }
    Redirect::to("/admin/instances").into_response()
}

async fn post_admin_instance_delete(
    State(state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let _ = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    if let Err(e) = state.svc.store().delete_librenms_instance(id).await {
        return error_page(&e.to_string());
    }
    Redirect::to("/admin/instances").into_response()
}

// ============================================================
// 阶段 8f:客户 CRUD + per-port bill + 模板管理
// ============================================================

/// 客户表单模板(新增/编辑共用)。customer 总是填值,新增时是零值客户。
#[derive(Template)]
#[template(path = "admin/customer_form.html")]
struct AdminCustomerFormTpl<'a> {
    username: &'a str,
    mode: &'a str, // "new" | "edit"
    customer: Customer,
    instances: Vec<crate::store::LibreNmsInstance>,
    templates: Vec<crate::store::TemplateVersionRow>,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "admin/instance_bills.html")]
struct AdminInstanceBillsTpl<'a> {
    username: &'a str,
    instance_name: String,
    bills: Vec<crate::librenms::BillSummary>,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "admin/templates.html")]
struct AdminTemplatesTpl<'a> {
    username: &'a str,
    versions: Vec<crate::store::TemplateVersionRow>,
    customer_counts: std::collections::HashMap<String, i64>,
    error: Option<String>,
}

// ---- 后台设置(出账日 / 出账时刻 / 发票号模板;run-billing 自检 + 发票渲染都从这里读)----

#[derive(Deserialize)]
struct AdminSettingsForm {
    billing_day: String,
    billing_hour: String,
    invoice_no_template: String,
}

#[derive(Template)]
#[template(path = "admin/settings.html")]
struct AdminSettingsTpl<'a> {
    username: &'a str,
    billing_day: u32,
    billing_hour: u32,
    invoice_no_template: &'a str,
    default_invoice_no_template: &'a str,
    saved: bool,
    error: Option<String>,
}

// ---- 客户 CRUD ----

#[derive(Deserialize)]
struct CustomerFullForm {
    internal_key: String,
    name: String,
    currency: String,
    librenms_instance_id: i64,
    librenms_bill_id: String,            // 表单是文本,handler 转 i64
    timezone: String,
    company_type: String,
    billing_address: Option<String>,
    contact_email: Option<String>,
    company_info_json: Option<String>,
    template_name: Option<String>,
    is_active: Option<String>,
}

fn validate_currency(c: &str) -> Result<()> {
    if c == "CNY" || c == "HKD" {
        Ok(())
    } else {
        Err(Error::Config(format!("currency 必须是 CNY 或 HKD,收到: {c}")))
    }
}
fn validate_company_type(c: &str) -> Result<()> {
    if c == "domestic" || c == "hk" {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "company_type 必须是 domestic 或 hk,收到: {c}"
        )))
    }
}

async fn render_customer_form(
    state: &WebState,
    username: &str,
    mode: &str,
    customer: Customer,
    error: Option<String>,
) -> Result<Html<String>> {
    let instances = state.svc.store().list_active_libre_nms_instances().await?;
    let templates = state.svc.store().list_template_versions().await?;
    let tpl = AdminCustomerFormTpl {
        username,
        mode,
        customer,
        instances,
        templates,
        error,
    };
    Ok(Html(tpl.render().map_err(|e| {
        Error::Internal(format!("render customer form: {e}"))
    })?))
}

fn empty_customer() -> Customer {
    Customer {
        id: 0,
        internal_key: String::new(),
        name: String::new(),
        currency: "CNY".to_string(),
        librenms_instance_id: 0,
        librenms_bill_id: 0,
        timezone: "Asia/Shanghai".to_string(),
        company_type: "domestic".to_string(),
        company_info_json: "{}".to_string(),
        company_info_schema_version: 1,
        billing_address: None,
        contact_email: None,
        template_name: None,
        is_active: true,
        created_at: String::new(),
    }
}

async fn get_admin_customer_new(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
) -> Response {
    let user = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    match render_customer_form(&state, &user.username, "new", empty_customer(), None).await {
        Ok(html) => html.into_response(),
        Err(e) => error_page(&e.to_string()),
    }
}

async fn post_admin_customer_create(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
    Form(form): Form<CustomerFullForm>,
) -> Response {
    let user = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    if let Err(e) = validate_currency(&form.currency) {
        return render_customer_form(&state, &user.username, "new", empty_customer(), Some(e.to_string()))
            .await
            .map(|h| h.into_response())
            .unwrap_or_else(|e| error_page(&e.to_string()));
    }
    if let Err(e) = validate_company_type(&form.company_type) {
        return render_customer_form(&state, &user.username, "new", empty_customer(), Some(e.to_string()))
            .await
            .map(|h| h.into_response())
            .unwrap_or_else(|e| error_page(&e.to_string()));
    }
    let bill_id: i64 = match form.librenms_bill_id.trim().parse() {
        Ok(n) => n,
        Err(_) => {
            return render_customer_form(
                &state,
                &user.username,
                "new",
                empty_customer(),
                Some("librenms_bill_id 必须是正整数".into()),
            )
            .await
            .map(|h| h.into_response())
            .unwrap_or_else(|e| error_page(&e.to_string()));
        }
    };
    let info_json = form.company_info_json.clone().unwrap_or_else(|| "{}".into());
    let new_c = NewCustomerFull {
        internal_key: form.internal_key.trim(),
        name: form.name.trim(),
        currency: form.currency.trim(),
        librenms_instance_id: form.librenms_instance_id,
        librenms_bill_id: bill_id,
        timezone: form.timezone.trim(),
        company_type: form.company_type.trim(),
        company_info_json: &info_json,
        company_info_schema_version: 1,
        billing_address: form.billing_address.as_deref().map(str::trim).filter(|s| !s.is_empty()),
        contact_email: form.contact_email.as_deref().map(str::trim).filter(|s| !s.is_empty()),
        template_name: form.template_name.as_deref().map(str::trim).filter(|s| !s.is_empty()),
        is_active: matches!(form.is_active.as_deref(), Some("on") | Some("1") | Some("true")),
    };
    let store = state.svc.store();
    let id = match store.insert_customer_full(&new_c).await {
        Ok(i) => i,
        Err(e) => {
            return render_customer_form(&state, &user.username, "new", empty_customer(), Some(e.to_string()))
                .await
                .map(|h| h.into_response())
                .unwrap_or_else(|e| error_page(&e.to_string()));
        }
    };
    Redirect::to(&format!("/admin/customers/{id}")).into_response()
}

async fn get_admin_customer_edit(
    State(state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let user = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    let store = state.svc.store();
    let customer = match store.find_customer_by_id(id).await {
        Ok(Some(c)) => c,
        Ok(None) => return error_page("客户不存在"),
        Err(e) => return error_page(&e.to_string()),
    };
    match render_customer_form(&state, &user.username, "edit", customer, None).await {
        Ok(html) => html.into_response(),
        Err(e) => error_page(&e.to_string()),
    }
}

async fn post_admin_customer_update(
    State(state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<CustomerFullForm>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return Redirect::to("/login").into_response();
    }
    if let Err(e) = validate_currency(&form.currency) {
        return error_page(&e.to_string());
    }
    if let Err(e) = validate_company_type(&form.company_type) {
        return error_page(&e.to_string());
    }
    let bill_id: i64 = match form.librenms_bill_id.trim().parse() {
        Ok(n) => n,
        Err(_) => return error_page("librenms_bill_id 必须是正整数"),
    };
    let info_json = form.company_info_json.clone().unwrap_or_else(|| "{}".into());
    let upd = CustomerFullUpdate {
        internal_key: form.internal_key.trim(),
        name: form.name.trim(),
        currency: form.currency.trim(),
        librenms_instance_id: form.librenms_instance_id,
        librenms_bill_id: bill_id,
        timezone: form.timezone.trim(),
        company_type: form.company_type.trim(),
        company_info_json: &info_json,
        company_info_schema_version: 1,
        billing_address: form.billing_address.as_deref().map(str::trim).filter(|s| !s.is_empty()),
        contact_email: form.contact_email.as_deref().map(str::trim).filter(|s| !s.is_empty()),
        template_name: form.template_name.as_deref().map(str::trim).filter(|s| !s.is_empty()),
        is_active: matches!(form.is_active.as_deref(), Some("on") | Some("1") | Some("true")),
    };
    let store = state.svc.store();
    if let Err(e) = store.update_customer(id, &upd).await {
        return error_page(&e.to_string());
    }
    Redirect::to(&format!("/admin/customers/{id}")).into_response()
}

async fn post_admin_customer_delete(
    State(state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let _ = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    if let Err(e) = state.svc.store().delete_customer(id).await {
        return error_page(&e.to_string());
    }
    Redirect::to("/admin/customers").into_response()
}

// ---- 端口 CRUD(per-port bill) ----

#[derive(Deserialize)]
struct PortForm {
    port_label: String,
    /// v0.6.3 起 IP 数量由费用表单维护,端口不再输入;保留字段以兼容旧表单(若仍提交会被忽略)。
    #[serde(default)]
    ip_count_a: i64,
    #[serde(default)]
    ip_count_b: i64,
    machine_rent: Option<String>,
    machine_hosting: Option<String>,
    librenms_bill_id: Option<String>,
    notes: Option<String>,
}

async fn post_admin_port_create(
    State(state): State<Arc<WebState>>,
    Path(customer_id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<PortForm>,
) -> Response {
    let _ = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    let bill: Option<i64> = form
        .librenms_bill_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok());
    let store = state.svc.store();
    if let Err(e) = store
        .insert_port_with_bill(
            customer_id,
            form.port_label.trim(),
            form.ip_count_a,
            form.ip_count_b,
            matches!(form.machine_rent.as_deref(), Some("on") | Some("1") | Some("true")),
            matches!(form.machine_hosting.as_deref(), Some("on") | Some("1") | Some("true")),
            bill,
            form.notes.as_deref().map(str::trim).filter(|s| !s.is_empty()),
        )
        .await
    {
        return error_page(&e.to_string());
    }
    Redirect::to(&format!("/admin/customers/{customer_id}")).into_response()
}

async fn post_admin_port_update(
    State(state): State<Arc<WebState>>,
    Path((customer_id, port_id)): Path<(i64, i64)>,
    headers: HeaderMap,
    Form(form): Form<PortForm>,
) -> Response {
    let _ = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    let bill: Option<i64> = form
        .librenms_bill_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok());
    let store = state.svc.store();
    if let Err(e) = store
        .update_port(
            port_id,
            form.port_label.trim(),
            form.ip_count_a,
            form.ip_count_b,
            matches!(form.machine_rent.as_deref(), Some("on") | Some("1") | Some("true")),
            matches!(form.machine_hosting.as_deref(), Some("on") | Some("1") | Some("true")),
            bill,
            form.notes.as_deref().map(str::trim).filter(|s| !s.is_empty()),
        )
        .await
    {
        return error_page(&e.to_string());
    }
    Redirect::to(&format!("/admin/customers/{customer_id}")).into_response()
}

async fn post_admin_port_delete(
    State(state): State<Arc<WebState>>,
    Path((customer_id, port_id)): Path<(i64, i64)>,
    headers: HeaderMap,
) -> Response {
    let _ = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    if let Err(e) = state.svc.store().delete_port(port_id).await {
        return error_page(&e.to_string());
    }
    Redirect::to(&format!("/admin/customers/{customer_id}")).into_response()
}

// ---- LNMS bills(per-instance 浏览 + AJAX 级联) ----

/// 在阻塞线程池里完成「构造 LNMS 客户端 + 调用」。
/// reqwest::blocking 的构造和请求都不能在 tokio worker 上执行(会 panic),
/// 所以整个操作必须包进 spawn_blocking。
fn spawn_lnms_op<T>(
    url: String,
    token_bytes: Vec<u8>,
    instance_id: i64,
    op: impl FnOnce(LibreNmsClient) -> crate::error::Result<T> + Send + 'static,
) -> tokio::task::JoinHandle<crate::error::Result<T>>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let token = String::from_utf8(token_bytes).map_err(|_| {
            Error::Database(format!(
                "instance {instance_id} token 不是 UTF-8(可能 LoadCredential 未解密?)"
            ))
        })?;
        let client = LibreNmsClient::new(&url, &token)?;
        op(client)
    })
}

async fn get_admin_instance_bills(
    State(state): State<Arc<WebState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let user = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    let store = state.svc.store();
    let inst = match store.find_librenms_instance(id).await {
        Ok(Some(i)) => i,
        Ok(None) => return error_page("实例不存在"),
        Err(e) => return error_page(&e.to_string()),
    };
    // 构造客户端 + 拉取 bills 整体放到阻塞线程池(reqwest::blocking 不能在 tokio worker 上跑)
    let bills_result = spawn_lnms_op(inst.url.clone(), inst.api_token_enc.clone(), inst.id, |c| {
        c.list_bills()
    })
    .await
    .map_err(|e| Error::Internal(format!("spawn_blocking join: {e}")))
    .and_then(|r| r);
    let (bills, error) = match bills_result {
        Ok(b) => (b, None),
        Err(e) => (vec![], Some(format!("拉取 NMS 账单列表失败: {e}"))),
    };
    Html(
        AdminInstanceBillsTpl {
            username: &user.username,
            instance_name: inst.name.clone(),
            bills,
            error,
        }
        .render()
        .unwrap_or_else(|e| format!("render error: {e}")),
    )
    .into_response()
}

async fn get_admin_ajax_bills(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    if require_admin(&state, &headers).await.is_err() {
        return Redirect::to("/login").into_response();
    }
    let id: i64 = match q.get("instance_id").and_then(|v| v.parse().ok()) {
        Some(n) => n,
        None => return (StatusCode::BAD_REQUEST, "missing instance_id").into_response(),
    };
    let store = state.svc.store();
    let inst = match store.find_librenms_instance(id).await {
        Ok(Some(i)) => i,
        Ok(None) => return (StatusCode::NOT_FOUND, "instance not found").into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    // 构造客户端 + 拉取 bills 整体放到阻塞线程池,同上
    let bills_result = spawn_lnms_op(inst.url.clone(), inst.api_token_enc.clone(), inst.id, |c| {
        c.list_bills()
    })
    .await
    .map_err(|e| Error::Internal(format!("spawn_blocking join: {e}")))
    .and_then(|r| r);
    match bills_result {
        Ok(bills) => {
            let json = serde_json::json!({
                "bills": bills.iter().map(|b| serde_json::json!({
                    "id": b.id,
                    "bill_name": b.bill_name,
                    "bill_type": b.bill_type,
                    "active": b.active,
                })).collect::<Vec<_>>()
            });
            (StatusCode::OK, [("content-type", "application/json")], json.to_string())
                .into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("NMS error: {e}")).into_response(),
    }
}

// ---- 模板管理(列表 + 上传) ----

async fn get_admin_templates(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
) -> Response {
    let user = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    let store = state.svc.store();
    let versions = match store.list_template_versions().await {
        Ok(v) => v,
        Err(e) => return error_page(&e.to_string()),
    };
    let mut counts = std::collections::HashMap::new();
    for v in &versions {
        match store.count_customers_using_template(&v.template_name).await {
            Ok(n) => {
                counts.insert(v.template_name.clone(), n);
            }
            Err(e) => return error_page(&e.to_string()),
        }
    }
    Html(
        AdminTemplatesTpl {
            username: &user.username,
            versions,
            customer_counts: counts,
            error: None,
        }
        .render()
        .unwrap_or_else(|e| format!("render error: {e}")),
    )
    .into_response()
}

async fn post_admin_template_upload(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    let _ = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    // 解析:先找 template_name 字段,再��文件字���
    let mut template_name: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;
    while let Some(field) = match multipart.next_field().await {
        Ok(f) => f,
        Err(e) => return error_page(&format!("multipart: {e}")),
    } {
        let name = field.name().unwrap_or("").to_string();
        if name == "template_name" {
            template_name = Some(field.text().await.unwrap_or_default());
        } else if name == "file" {
            bytes = Some(match field.bytes().await {
                Ok(b) => b.to_vec(),
                Err(e) => return error_page(&format!("读取上传文件失败: {e}")),
            });
        }
    }
    let template_name = match template_name {
        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
        _ => return error_page("必须提供 template_name 字段"),
    };
    let bytes = match bytes {
        Some(b) => b,
        None => return error_page("必须上传 xlsx 文件"),
    };
    if !bytes.starts_with(b"PK") {
        return error_page("上传文件不是合法 xlsx(zip 容器应以 PK 开头)");
    }
    // 写到 template_root/<name>.xlsx
    let dest = state.svc.template_root().join(format!("{template_name}.xlsx"));
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&dest, &bytes) {
        return error_page(&format!("写模板文件失败: {e}"));
    }
    // inspect + 落库
    let audit = match inspect(&dest, &template_name) {
        Ok(a) => a,
        Err(e) => {
            return error_page(&format!("inspect 失败: {e}"));
        }
    };
    if let Err(e) = write_template_version(state.svc.store(), &audit).await {
        return error_page(&format!("写 template_versions 失败: {e}"));
    }
    Redirect::to("/admin/templates").into_response()
}

// ============================================================
// Invoice / InvoiceStatus 的 askama 桥接
// ============================================================

impl Invoice {
    pub fn period_label(&self) -> String {
        format!("{:04}-{:02}", self.period_year, self.period_month)
    }
    pub fn total_yuan(&self) -> String {
        match self.total_cents {
            Some(c) => format!("{}.{:02}", c / 100, c % 100),
            None => "-".to_string(),
        }
    }
}

// ============================================================
// 后台 /admin/settings(出账日 / 出账时刻 / 发票号模板)
// ============================================================

const ADMIN_SETTING_BILLING_DAY: &str = "billing_day";
const ADMIN_SETTING_BILLING_HOUR: &str = "billing_hour";
const ADMIN_SETTING_INVOICE_NO_TEMPLATE: &str = "invoice_no_template";
const ADMIN_DEFAULT_INVOICE_NO_TEMPLATE: &str = "INV-{KEY}-{YYYY}-{MM}-{SEQ}";

async fn read_admin_setting_u32(store: &crate::store::Store, key: &str) -> u32 {
    match store.get_setting(key).await {
        Ok(Some(v)) => v.trim().parse::<u32>().ok(),
        _ => None,
    }
    .unwrap_or(if key == ADMIN_SETTING_BILLING_DAY { 1 } else { 10 })
    .clamp(if key == ADMIN_SETTING_BILLING_DAY { 1 } else { 0 },
           if key == ADMIN_SETTING_BILLING_DAY { 28 } else { 23 })
}

async fn read_admin_invoice_template(store: &crate::store::Store) -> String {
    match store.get_setting(ADMIN_SETTING_INVOICE_NO_TEMPLATE).await {
        Ok(Some(v)) if !v.trim().is_empty() => v.trim().to_string(),
        _ => ADMIN_DEFAULT_INVOICE_NO_TEMPLATE.to_string(),
    }
}

async fn get_admin_settings(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
) -> Response {
    let user = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    let store = state.svc.store();
    let billing_day = read_admin_setting_u32(store, ADMIN_SETTING_BILLING_DAY).await;
    let billing_hour = read_admin_setting_u32(store, ADMIN_SETTING_BILLING_HOUR).await;
    let invoice_no_template = read_admin_invoice_template(store).await;
    Html(
        AdminSettingsTpl {
            username: &user.username,
            billing_day,
            billing_hour,
            invoice_no_template: &invoice_no_template,
            default_invoice_no_template: ADMIN_DEFAULT_INVOICE_NO_TEMPLATE,
            saved: false,
            error: None,
        }
        .render()
        .unwrap_or_else(|e| format!("render error: {e}")),
    )
    .into_response()
}

async fn post_admin_settings(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
    Form(form): Form<AdminSettingsForm>,
) -> Response {
    let user = match require_admin(&state, &headers).await {
        Ok(u) => u,
        Err(_) => return Redirect::to("/login").into_response(),
    };
    let store = state.svc.store();

    // 1. 解析并 clamp:day ∈ [1,28](>28 的月份会溢出,统一限到 28);hour ∈ [0,23]
    let day: u32 = match form.billing_day.trim().parse::<u32>() {
        Ok(n) if n >= 1 && n <= 28 => n,
        _ => {
            let invoice_no_template = read_admin_invoice_template(store).await;
            return render_admin_settings_error(
                &user,
                &invoice_no_template,
                "出账日必须为 1–28 的整数(避免 2 月/30 天月份日期溢出)",
            );
        }
    };
    let hour: u32 = match form.billing_hour.trim().parse::<u32>() {
        Ok(n) if n <= 23 => n,
        _ => {
            let invoice_no_template = read_admin_invoice_template(store).await;
            return render_admin_settings_error(
                &user,
                &invoice_no_template,
                "出账时刻必须为 0–23 的整数",
            );
        }
    };
    let tpl = form.invoice_no_template.trim().to_string();
    if tpl.is_empty() {
        let invoice_no_template = read_admin_invoice_template(store).await;
        return render_admin_settings_error(
            &user,
            &invoice_no_template,
            "发票号模板不能为空",
        );
    }
    // 至少需要包含 {SEQ} 占位符，其他占位符可选
    if !tpl.contains("{SEQ}") {
        let invoice_no_template = read_admin_invoice_template(store).await;
        return render_admin_settings_error(
            &user,
            &invoice_no_template,
            "发票号模板必须包含 {SEQ} 占位符",
        );
    }

    // 2. 写 settings 表(失败回表单)
    if let Err(e) = store.set_setting(ADMIN_SETTING_BILLING_DAY, &day.to_string()).await {
        return render_admin_settings_error(
            &user,
            &tpl,
            &format!("保存出账日失败: {e}"),
        );
    }
    if let Err(e) = store.set_setting(ADMIN_SETTING_BILLING_HOUR, &hour.to_string()).await {
        return render_admin_settings_error(
            &user,
            &tpl,
            &format!("保存出账时刻失败: {e}"),
        );
    }
    if let Err(e) = store.set_setting(ADMIN_SETTING_INVOICE_NO_TEMPLATE, &tpl).await {
        return render_admin_settings_error(
            &user,
            &tpl,
            &format!("保存发票号模板失败: {e}"),
        );
    }

    // 3. 成功 → 重新渲染表单(saved=true 给前端一个轻提示)
    Html(
        AdminSettingsTpl {
            username: &user.username,
            billing_day: day,
            billing_hour: hour,
            invoice_no_template: &tpl,
            default_invoice_no_template: ADMIN_DEFAULT_INVOICE_NO_TEMPLATE,
            saved: true,
            error: None,
        }
        .render()
        .unwrap_or_else(|e| format!("render error: {e}")),
    )
    .into_response()
}

fn render_admin_settings_error(
    user: &crate::store::User,
    current_tpl: &str,
    msg: &str,
) -> Response {
    Html(
        AdminSettingsTpl {
            username: &user.username,
            // 错误情况下显示 1 / 10 是因为我们没回读数据库,避免再炸一次
            billing_day: 1,
            billing_hour: 10,
            invoice_no_template: current_tpl,
            default_invoice_no_template: ADMIN_DEFAULT_INVOICE_NO_TEMPLATE,
            saved: false,
            error: Some(msg.to_string()),
        }
        .render()
        .unwrap_or_else(|e| format!("render error: {e}")),
    )
    .into_response()
}

impl InvoiceStatus {
    pub fn label_zh(&self) -> &'static str {
        match self {
            Self::Generating => "生成中",
            Self::Preview => "预览待确认",
            Self::Confirming => "确认中",
            Self::Final => "已生效",
            Self::Failed => "失败",
            Self::Rejected => "已拒绝",
        }
    }
}

impl std::fmt::Display for InvoiceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}