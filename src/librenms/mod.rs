//! LibreNMS API 客户端(reqwest 同步,限流 + 退避)。
//!
//! 阶段 4 实装:
//! - `GET /api/v0/bills` 列表(账期历史/状态)
//! - `GET /api/v0/bills/{id}` 详情(含 `rate_95th` 字段)
//! - `GET /api/v0/bills/{id}/history` 时间序列(可能返 5min series,实际形态待验)
//! - 401/403/404/429/5xx 处理:
//!   - 401/403/404 → 立即失败(错误里带 URL)
//!   - 429 → 按 `Retry-After` 头等待(默认 5s)
//!   - 5xx + 网络错 → 指数退避 3 次(0.5s / 2s / 8s)
//! - 长生命周期 `Client` + 连接复用(决策 #16)

use crate::error::{Error, Result};
use reqwest::blocking::Client as HttpClient;
use serde::Deserialize;
use std::time::Duration;

const MAX_RETRIES: usize = 3;
const BASE_BACKOFF: Duration = Duration::from_millis(500);

/// LibreNMS 账单概要(来自 `GET /api/v0/bills`)
#[derive(Debug, Clone, Deserialize)]
pub struct BillSummary {
    pub id: i64,
    #[serde(default)]
    pub bill_name: Option<String>,
    #[serde(default)]
    pub bill_type: Option<String>,
    #[serde(default)]
    pub port_id: Option<i64>,
    #[serde(default)]
    pub customer_id: Option<i64>,
    #[serde(default)]
    pub period: Option<u32>,
    #[serde(default)]
    pub active: Option<u8>,
}

/// LibreNMS 账单详情(来自 `GET /api/v0/bills/{id}`),含 95th 计算结果
#[derive(Debug, Clone, Deserialize)]
pub struct BillDetail {
    pub id: i64,
    #[serde(default)]
    pub bill_name: Option<String>,
    #[serde(default)]
    pub bill_type: Option<String>,
    #[serde(default)]
    pub rate_95th: Option<f64>,
    #[serde(default)]
    pub dir_95th: Option<String>,
    #[serde(default)]
    pub in_avg: Option<f64>,
    #[serde(default)]
    pub out_avg: Option<f64>,
    #[serde(default)]
    pub total_data: Option<f64>,
    #[serde(default)]
    pub ports: Option<Vec<BillPort>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BillPort {
    pub port_id: i64,
    #[serde(default)]
    pub label: Option<String>,
}

/// 5min 数据序列点(来自 `/bills/{id}/history`,具体字段待实测)
#[derive(Debug, Clone, Deserialize)]
pub struct HistoryPoint {
    #[serde(default)]
    pub timestamp: Option<i64>,
    #[serde(default)]
    pub period: Option<i64>,
    #[serde(default)]
    pub in_delta: Option<f64>,
    #[serde(default)]
    pub out_delta: Option<f64>,
    #[serde(default)]
    pub total: Option<f64>,
}

/// LibreNMS /bills 列表响应包装
#[derive(Debug, Deserialize)]
struct BillsListResponse {
    #[allow(dead_code)]
    status: String,
    #[serde(default)]
    bills: Vec<BillSummary>,
}

/// LibreNMS /bills/{id} 详情响应包装
#[derive(Debug, Deserialize)]
struct BillDetailResponse {
    #[allow(dead_code)]
    status: String,
    bill: BillDetail,
}

#[derive(Clone)]
pub struct LibreNmsClient {
    base_url: String,
    token: String,
    http: HttpClient,
}

impl LibreNmsClient {
    pub fn new(base_url: &str, token: &str) -> Result<Self> {
        let http = HttpClient::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("lnms-invoice/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::LibreNms(format!("build http client: {e}")))?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            http,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `GET /api/v0/bills`
    pub fn list_bills(&self) -> Result<Vec<BillSummary>> {
        let url = format!("{}/api/v0/bills", self.base_url);
        let env: BillsListResponse = self.get_json(&url)?;
        Ok(env.bills)
    }

    /// `GET /api/v0/bills/{id}` — 取 95th 等账单结算数据
    pub fn get_bill(&self, id: i64) -> Result<BillDetail> {
        let url = format!("{}/api/v0/bills/{}", self.base_url, id);
        let env: BillDetailResponse = self.get_json(&url)?;
        Ok(env.bill)
    }

    /// `GET /api/v0/bills/{id}/history` — 5min 序列(或账期历史,待实测)
    /// 返回原始 JSON 数组(因为真实字段形态待实测,阶段 4.5 拿数据时再确认)
    pub fn get_bill_history_raw(&self, id: i64) -> Result<serde_json::Value> {
        let url = format!("{}/api/v0/bills/{}/history", self.base_url, id);
        self.get_json_raw(&url)
    }

    /// 通用 GET JSON,带重试
    fn get_json<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T> {
        let raw = self.get_json_raw(url)?;
        serde_json::from_value(raw).map_err(|e| {
            Error::LibreNms(format!("parse JSON from {url}: {e}"))
        })
    }

    fn get_json_raw(&self, url: &str) -> Result<serde_json::Value> {
        let mut attempt = 0usize;
        loop {
            let resp = self.http.get(url).header("X-Auth-Token", &self.token).send();

            match resp {
                Ok(r) => {
                    let status = r.status();
                    if status.is_success() {
                        return r.json::<serde_json::Value>().map_err(|e| {
                            Error::LibreNms(format!("decode JSON from {url}: {e}"))
                        });
                    }

                    if status.as_u16() == 401 || status.as_u16() == 403 {
                        let body = r.text().unwrap_or_default();
                        return Err(Error::LibreNms(format!(
                            "auth failed ({status}) for {url}: {body}"
                        )));
                    }
                    if status.as_u16() == 404 {
                        return Err(Error::LibreNms(format!(
                            "not found (404) for {url}"
                        )));
                    }
                    if status.as_u16() == 429 {
                        let wait = r
                            .headers()
                            .get("Retry-After")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok())
                            .map(Duration::from_secs)
                            .unwrap_or(Duration::from_secs(5));
                        log::warn!("429 from {url}, sleep {wait:?}");
                        std::thread::sleep(wait);
                        attempt += 1;
                        if attempt > MAX_RETRIES {
                            return Err(Error::LibreNms(format!(
                                "429 retries exhausted for {url}"
                            )));
                        }
                        continue;
                    }
                    // 5xx 等:重试 + 退避
                    let body = r.text().unwrap_or_default();
                    attempt += 1;
                    if attempt > MAX_RETRIES {
                        return Err(Error::LibreNms(format!(
                            "HTTP {status} after {MAX_RETRIES} retries for {url}: {body}"
                        )));
                    }
                    let backoff = BASE_BACKOFF * 4u32.pow((attempt - 1) as u32);
                    log::warn!(
                        "HTTP {status} from {url}, retry {attempt}/{MAX_RETRIES} in {backoff:?}: {body}"
                    );
                    std::thread::sleep(backoff);
                }
                Err(e) => {
                    attempt += 1;
                    if attempt > MAX_RETRIES {
                        return Err(Error::LibreNms(format!(
                            "transport error after {MAX_RETRIES} retries for {url}: {e}"
                        )));
                    }
                    let backoff = BASE_BACKOFF * 4u32.pow((attempt - 1) as u32);
                    log::warn!(
                        "transport error to {url}: {e}; retry {attempt}/{MAX_RETRIES} in {backoff:?}"
                    );
                    std::thread::sleep(backoff);
                }
            }
        }
    }
}
