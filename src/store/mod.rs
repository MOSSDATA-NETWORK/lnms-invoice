//! 数据访问层(SQLite via sqlx)。
//!
//! 阶段 2 实装:
//! - 9 张表 schema 迁移(详见 `migrations/20260101000000_init.sql`)
//! - PRAGMA `journal_mode=WAL` / `busy_timeout=5000` / `foreign_keys=ON`
//! - 内部客户键 ↔ LNMS `bill_id` 映射(决策 #20,数据血缘)
//! - 事务化 INVOICE NO 序号(决策 #13/#14)
//! - 基础 CRUD:`librenms_instances` / `customers` / `ports` / `rates`
//! - 后续阶段补:`invoices` / `invoice_lines` / `invoice_runs` / `invoice_actions` / `users` 业务方法

use crate::error::{Error, Result};
use chrono::Utc;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::path::Path;
use std::str::FromStr;

/// 迁移列表:按时间戳顺序排列,每个文件可独立引用。
///
/// 早期版本只用单一 include_str,只支持幂等 SQL(IF NOT EXISTS);
/// 阶段 8f 起需要 ALTER TABLE ADD COLUMN 等非幂等 DDL,因此引入应用记录表。
const MIGRATIONS: &[(&str, &str)] = &[
    (
        "20260101000000_init",
        include_str!("../../migrations/20260101000000_init.sql"),
    ),
    (
        "20260201000000_per_port_bill_and_templates",
        include_str!("../../migrations/20260201000000_per_port_bill_and_templates.sql"),
    ),
    (
        "20260202000000_rate_bill_id",
        include_str!("../../migrations/20260202000000_rate_bill_id.sql"),
    ),
    (
        "20260203000000_settings",
        include_str!("../../migrations/20260203000000_settings.sql"),
    ),
    (
        "20260204000000_ip_quantity_on_rates",
        include_str!("../../migrations/20260204000000_ip_quantity_on_rates.sql"),
    ),
    (
        "20260205000000_business_label_on_rates",
        include_str!("../../migrations/20260205000000_business_label_on_rates.sql"),
    ),
];

/// 数据库连接池
#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// 打开 SQLite(必要时创建),设置 PRAGMA,跑迁移
    pub async fn connect(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let url = format!("sqlite://{}?mode=rwc", path.display());
        let opts = SqliteConnectOptions::from_str(&url)
            .map_err(|e| Error::Database(format!("connect options: {e}")))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(16)
            .connect_with(opts)
            .await
            .map_err(|e| Error::Database(format!("connect: {e}")))?;

        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// 跑迁移:每个文件只在 `_lnms_migrations` 里没记录时执行,执行后写入记录。
    /// 老库会一次性补齐未跑的迁移;新库每条记录都新鲜。
    async fn migrate(&self) -> Result<()> {
        // 1. 建应用记录表(idempotent)
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS _lnms_migrations (
                name TEXT PRIMARY KEY,
                applied_at TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("create _lnms_migrations: {e}")))?;

        for (name, sql) in MIGRATIONS {
            let already: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM _lnms_migrations WHERE name = ?")
                    .bind(name)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(|e| Error::Database(format!("probe migration {name}: {e}")))?;
            if already > 0 {
                continue;
            }
            // 预处理:剥掉所有 `--` 行注释(到行尾)。避免 `-- foo; bar` 形式被 `;` 拆开
            // 后,后半截没 `--` 前缀漏过剥离、SQLite 把它当 SQL 解析失败。
            let stripped: String = sql
                .lines()
                .map(|l| match l.find("--") {
                    Some(i) => &l[..i],
                    None => l,
                })
                .collect::<Vec<_>>()
                .join("\n");
            for stmt in stripped.split(';') {
                let s = stmt.trim();
                if s.is_empty() {
                    continue;
                }
                sqlx::query(s)
                    .execute(&self.pool)
                    .await
                    .map_err(|e| {
                        Error::Database(format!(
                            "migration {name} failed: {e}\nstatement: {s}"
                        ))
                    })?;
            }
            sqlx::query("INSERT INTO _lnms_migrations (name, applied_at) VALUES (?, ?)")
                .bind(name)
                .bind(Utc::now().to_rfc3339())
                .execute(&self.pool)
                .await
                .map_err(|e| Error::Database(format!("record migration {name}: {e}")))?;
            log::info!("migration {name} applied");
        }
        Ok(())
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

// ============================================================
// LibreNMS 实例
// ============================================================

#[derive(Debug, Clone)]
pub struct LibreNmsInstance {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub api_token_enc: Vec<u8>,
    pub is_active: bool,
    pub created_at: String,
}

impl Store {
    /// 新增 LibreNMS 实例
    pub async fn insert_libre_nms_instance(
        &self,
        name: &str,
        url: &str,
        api_token_enc: &[u8],
    ) -> Result<i64> {
        let now = Utc::now().to_rfc3339();
        let res = sqlx::query(
            "INSERT INTO librenms_instances (name, url, api_token_enc, is_active, created_at)
             VALUES (?, ?, ?, 1, ?)",
        )
        .bind(name)
        .bind(url)
        .bind(api_token_enc)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("insert librenms_instances: {e}")))?;
        Ok(res.last_insert_rowid())
    }

    pub async fn list_active_libre_nms_instances(&self) -> Result<Vec<LibreNmsInstance>> {
        let rows = sqlx::query(
            "SELECT id, name, url, api_token_enc, is_active, created_at
             FROM librenms_instances WHERE is_active = 1 ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("list librenms_instances: {e}")))?;

        rows.into_iter()
            .map(|r| {
                let active: i64 = r.try_get("is_active").map_err(sqlx_err)?;
                Ok(LibreNmsInstance {
                    id: r.try_get("id").map_err(sqlx_err)?,
                    name: r.try_get("name").map_err(sqlx_err)?,
                    url: r.try_get("url").map_err(sqlx_err)?,
                    api_token_enc: r.try_get("api_token_enc").map_err(sqlx_err)?,
                    is_active: active != 0,
                    created_at: r.try_get("created_at").map_err(sqlx_err)?,
                })
            })
            .collect()
    }

    pub async fn find_librenms_instance(&self, id: i64) -> Result<Option<LibreNmsInstance>> {
        let row = sqlx::query(
            "SELECT id, name, url, api_token_enc, is_active, created_at
             FROM librenms_instances WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("find librenms_instance: {e}")))?;
        row.map(|r| {
            let active: i64 = r.try_get("is_active").map_err(sqlx_err)?;
            Ok(LibreNmsInstance {
                id: r.try_get("id").map_err(sqlx_err)?,
                name: r.try_get("name").map_err(sqlx_err)?,
                url: r.try_get("url").map_err(sqlx_err)?,
                api_token_enc: r.try_get("api_token_enc").map_err(sqlx_err)?,
                is_active: active != 0,
                created_at: r.try_get("created_at").map_err(sqlx_err)?,
            })
        })
        .transpose()
    }

    /// 列出所有 LibreNMS 实例(含已停用),admin 后台用
    pub async fn list_all_libre_nms_instances(&self) -> Result<Vec<LibreNmsInstance>> {
        let rows = sqlx::query(
            "SELECT id, name, url, api_token_enc, is_active, created_at
             FROM librenms_instances ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("list all librenms_instances: {e}")))?;
        rows.into_iter()
            .map(|r| {
                let active: i64 = r.try_get("is_active").map_err(sqlx_err)?;
                Ok(LibreNmsInstance {
                    id: r.try_get("id").map_err(sqlx_err)?,
                    name: r.try_get("name").map_err(sqlx_err)?,
                    url: r.try_get("url").map_err(sqlx_err)?,
                    api_token_enc: r.try_get("api_token_enc").map_err(sqlx_err)?,
                    is_active: active != 0,
                    created_at: r.try_get("created_at").map_err(sqlx_err)?,
                })
            })
            .collect()
    }

    /// 修改 LibreNMS 实例的 name/url/is_active(token 不动)
    pub async fn update_librenms_instance(
        &self,
        id: i64,
        name: &str,
        url: &str,
        is_active: bool,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE librenms_instances SET name = ?, url = ?, is_active = ? WHERE id = ?",
        )
        .bind(name)
        .bind(url)
        .bind(if is_active { 1 } else { 0 })
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("update librenms_instance: {e}")))?;
        Ok(())
    }

    /// 切换 LibreNMS 实例激活状态
    pub async fn set_librenms_instance_active(&self, id: i64, is_active: bool) -> Result<()> {
        sqlx::query("UPDATE librenms_instances SET is_active = ? WHERE id = ?")
            .bind(if is_active { 1 } else { 0 })
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::Database(format!("toggle librenms_instance: {e}")))?;
        Ok(())
    }

    /// 替换 LibreNMS 实例的 API token(直接覆盖 api_token_enc 列)
    ///
    /// 注:列名虽叫 `_enc`,实际存的是原始字节 —— 跟 `sudo set-instance-token`
    /// 走的是同一个写入路径。Web 注入与 sudo 注入产生的数据格式完全一致。
    pub async fn update_librenms_instance_token(&self, id: i64, api_token_enc: &[u8]) -> Result<()> {
        sqlx::query("UPDATE librenms_instances SET api_token_enc = ? WHERE id = ?")
            .bind(api_token_enc)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::Database(format!("update librenms_instance token: {e}")))?;
        Ok(())
    }

    /// 删除 LibreNMS 实例(仅当没有任何 customer 引用它)
    pub async fn delete_librenms_instance(&self, id: i64) -> Result<()> {
        let refs: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM customers WHERE librenms_instance_id = ?")
                .bind(id)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| Error::Database(format!("count customer refs: {e}")))?;
        if refs > 0 {
            return Err(Error::Database(format!(
                "instance {id} 仍被 {refs} 个 customer 引用,先迁移或停用"
            )));
        }
        sqlx::query("DELETE FROM librenms_instances WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::Database(format!("delete librenms_instance: {e}")))?;
        Ok(())
    }
}

// ============================================================
// 客户
// ============================================================

#[derive(Debug, Clone)]
pub struct Customer {
    pub id: i64,
    pub internal_key: String,
    pub name: String,
    pub currency: String,
    pub librenms_instance_id: i64,
    pub librenms_bill_id: i64,
    pub timezone: String,
    pub company_type: String,
    pub company_info_json: String,
    pub company_info_schema_version: i64,
    pub billing_address: Option<String>,
    pub contact_email: Option<String>,
    pub template_name: Option<String>,
    pub is_active: bool,
    pub created_at: String,
}

impl Store {
    pub async fn insert_customer(
        &self,
        c: &NewCustomer<'_>,
    ) -> Result<i64> {
        let now = Utc::now().to_rfc3339();
        let res = sqlx::query(
            "INSERT INTO customers
                (internal_key, name, currency, librenms_instance_id, librenms_bill_id,
                 timezone, company_type, company_info_json, company_info_schema_version,
                 billing_address, contact_email, is_active, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?)",
        )
        .bind(c.internal_key)
        .bind(c.name)
        .bind(c.currency)
        .bind(c.librenms_instance_id)
        .bind(c.librenms_bill_id)
        .bind(c.timezone)
        .bind(c.company_type)
        .bind(c.company_info_json)
        .bind(c.company_info_schema_version)
        .bind(c.billing_address)
        .bind(c.contact_email)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("insert customers: {e}")))?;
        Ok(res.last_insert_rowid())
    }

    pub async fn find_customer_by_internal_key(&self, key: &str) -> Result<Option<Customer>> {
        let row = sqlx::query(
            "SELECT id, internal_key, name, currency, librenms_instance_id, librenms_bill_id,
                    timezone, company_type, company_info_json, company_info_schema_version,
                    billing_address, contact_email, template_name, is_active, created_at
             FROM customers WHERE internal_key = ?",
        )
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("find customer: {e}")))?;
        row.map(row_to_customer).transpose()
    }

    pub async fn find_customer_by_id(&self, id: i64) -> Result<Option<Customer>> {
        let row = sqlx::query(
            "SELECT id, internal_key, name, currency, librenms_instance_id, librenms_bill_id,
                    timezone, company_type, company_info_json, company_info_schema_version,
                    billing_address, contact_email, template_name, is_active, created_at
             FROM customers WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("find customer by id: {e}")))?;
        row.map(row_to_customer).transpose()
    }

    pub async fn list_active_customers(&self) -> Result<Vec<Customer>> {
        let rows = sqlx::query(
            "SELECT id, internal_key, name, currency, librenms_instance_id, librenms_bill_id,
                    timezone, company_type, company_info_json, company_info_schema_version,
                    billing_address, contact_email, template_name, is_active, created_at
             FROM customers WHERE is_active = 1 ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("list customers: {e}")))?;
        rows.into_iter().map(row_to_customer).collect()
    }

    pub async fn list_all_customers(&self) -> Result<Vec<Customer>> {
        let rows = sqlx::query(
            "SELECT id, internal_key, name, currency, librenms_instance_id, librenms_bill_id,
                    timezone, company_type, company_info_json, company_info_schema_version,
                    billing_address, contact_email, template_name, is_active, created_at
             FROM customers ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("list all customers: {e}")))?;
        rows.into_iter().map(row_to_customer).collect()
    }

    pub async fn set_customer_active(&self, id: i64, active: bool) -> Result<()> {
        sqlx::query("UPDATE customers SET is_active = ? WHERE id = ?")
            .bind(if active { 1i64 } else { 0i64 })
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::Database(format!("toggle customer active: {e}")))?;
        Ok(())
    }
}

impl Customer {
    /// 模板友好 getter:Option 字段为空时返回空字符串(避免 askama 模板里
    /// 写一堆 `{% if %}` 嵌套)
    pub fn billing_address_or(&self) -> &str {
        self.billing_address.as_deref().unwrap_or("")
    }
    pub fn contact_email_or(&self) -> &str {
        self.contact_email.as_deref().unwrap_or("")
    }
    pub fn template_name_or(&self) -> &str {
        self.template_name.as_deref().unwrap_or("")
    }
    pub fn company_info_json_or(&self) -> &str {
        if self.company_info_json.is_empty() {
            "{}"
        } else {
            &self.company_info_json
        }
    }
}

impl Store {
    /// 全字段创建客户(阶段 8f)
    pub async fn insert_customer_full(&self, c: &NewCustomerFull<'_>) -> Result<i64> {
        let now = Utc::now().to_rfc3339();
        let res = sqlx::query(
            "INSERT INTO customers
                (internal_key, name, currency, librenms_instance_id, librenms_bill_id,
                 timezone, company_type, company_info_json, company_info_schema_version,
                 billing_address, contact_email, template_name, is_active, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(c.internal_key)
        .bind(c.name)
        .bind(c.currency)
        .bind(c.librenms_instance_id)
        .bind(c.librenms_bill_id)
        .bind(c.timezone)
        .bind(c.company_type)
        .bind(c.company_info_json)
        .bind(c.company_info_schema_version)
        .bind(c.billing_address)
        .bind(c.contact_email)
        .bind(c.template_name)
        .bind(if c.is_active { 1i64 } else { 0i64 })
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("insert customers (full): {e}")))?;
        Ok(res.last_insert_rowid())
    }

    /// 全字段更新客户(阶段 8f)
    pub async fn update_customer(&self, id: i64, c: &CustomerFullUpdate<'_>) -> Result<()> {
        sqlx::query(
            "UPDATE customers SET
                internal_key = ?, name = ?, currency = ?, librenms_instance_id = ?,
                librenms_bill_id = ?, timezone = ?, company_type = ?,
                company_info_json = ?, company_info_schema_version = ?,
                billing_address = ?, contact_email = ?, template_name = ?,
                is_active = ?
             WHERE id = ?",
        )
        .bind(c.internal_key)
        .bind(c.name)
        .bind(c.currency)
        .bind(c.librenms_instance_id)
        .bind(c.librenms_bill_id)
        .bind(c.timezone)
        .bind(c.company_type)
        .bind(c.company_info_json)
        .bind(c.company_info_schema_version)
        .bind(c.billing_address)
        .bind(c.contact_email)
        .bind(c.template_name)
        .bind(if c.is_active { 1i64 } else { 0i64 })
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("update customers: {e}")))?;
        Ok(())
    }

    /// 删除客户:必须先清空 ports 与 invoices 引用(参照 delete_librenms_instance 的语义)
    pub async fn delete_customer(&self, id: i64) -> Result<()> {
        let ports: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ports WHERE customer_id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| Error::Database(format!("count customer ports: {e}")))?;
        if ports > 0 {
            return Err(Error::Database(format!(
                "customer {id} 仍被 {ports} 个 ports 引用,先删除 ports"
            )));
        }
        let invs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM invoices WHERE customer_id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| Error::Database(format!("count customer invoices: {e}")))?;
        if invs > 0 {
            return Err(Error::Database(format!(
                "customer {id} 仍被 {invs} 个 invoices 引用,不可删除(归档需要)"
            )));
        }
        sqlx::query("DELETE FROM customers WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::Database(format!("delete customer: {e}")))?;
        Ok(())
    }

    /// 统计绑定了某模板的客户数(模板管理 UI 用)
    pub async fn count_customers_using_template(&self, name: &str) -> Result<i64> {
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM customers WHERE template_name = ?")
                .bind(name)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| Error::Database(format!("count template users: {e}")))?;
        Ok(n)
    }

    pub async fn delete_rate(&self, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM rates WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::Database(format!("delete rate: {e}")))?;
        Ok(())
    }

    /// 检查 librenms_instances.api_token_enc 是否被设置过(非空、非占位 "env:")
    pub async fn librenms_instance_token_set(&self, id: i64) -> Result<bool> {
        let row = sqlx::query("SELECT api_token_enc FROM librenms_instances WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| Error::Database(format!("token probe: {e}")))?;
        match row {
            None => Ok(false),
            Some(r) => {
                let v: Vec<u8> = r.try_get("api_token_enc").map_err(sqlx_err)?;
                // 占位是 "env:..." 形式,空或占位视为未设
                if v.is_empty() {
                    Ok(false)
                } else if v.starts_with(b"env:") {
                    // env:NAME 形式,且 NAME 对应环境变量不存在时视为未设
                    if let Ok(name) = std::str::from_utf8(&v[4..]) {
                        Ok(std::env::var(name).is_ok())
                    } else {
                        Ok(false)
                    }
                } else {
                    Ok(true)
                }
            }
        }
    }
}

pub struct NewCustomer<'a> {
    pub internal_key: &'a str,
    pub name: &'a str,
    pub currency: &'a str,
    pub librenms_instance_id: i64,
    pub librenms_bill_id: i64,
    pub timezone: &'a str,
    pub company_type: &'a str,
    pub company_info_json: &'a str,
    pub company_info_schema_version: i64,
    pub billing_address: Option<&'a str>,
    pub contact_email: Option<&'a str>,
}

/// 阶段 8f:全字段客户(新增/编辑表单用)。模板可选,默认激活。
pub struct NewCustomerFull<'a> {
    pub internal_key: &'a str,
    pub name: &'a str,
    pub currency: &'a str,
    pub librenms_instance_id: i64,
    pub librenms_bill_id: i64,
    pub timezone: &'a str,
    pub company_type: &'a str,
    pub company_info_json: &'a str,
    pub company_info_schema_version: i64,
    pub billing_address: Option<&'a str>,
    pub contact_email: Option<&'a str>,
    pub template_name: Option<&'a str>,
    pub is_active: bool,
}

pub struct CustomerFullUpdate<'a> {
    pub internal_key: &'a str,
    pub name: &'a str,
    pub currency: &'a str,
    pub librenms_instance_id: i64,
    pub librenms_bill_id: i64,
    pub timezone: &'a str,
    pub company_type: &'a str,
    pub company_info_json: &'a str,
    pub company_info_schema_version: i64,
    pub billing_address: Option<&'a str>,
    pub contact_email: Option<&'a str>,
    pub template_name: Option<&'a str>,
    pub is_active: bool,
}

fn row_to_customer(r: sqlx::sqlite::SqliteRow) -> Result<Customer> {
    let active: i64 = r.try_get("is_active").map_err(sqlx_err)?;
    Ok(Customer {
        id: r.try_get("id").map_err(sqlx_err)?,
        internal_key: r.try_get("internal_key").map_err(sqlx_err)?,
        name: r.try_get("name").map_err(sqlx_err)?,
        currency: r.try_get("currency").map_err(sqlx_err)?,
        librenms_instance_id: r.try_get("librenms_instance_id").map_err(sqlx_err)?,
        librenms_bill_id: r.try_get("librenms_bill_id").map_err(sqlx_err)?,
        timezone: r.try_get("timezone").map_err(sqlx_err)?,
        company_type: r.try_get("company_type").map_err(sqlx_err)?,
        company_info_json: r.try_get("company_info_json").map_err(sqlx_err)?,
        company_info_schema_version: r.try_get("company_info_schema_version").map_err(sqlx_err)?,
        billing_address: r.try_get("billing_address").map_err(sqlx_err)?,
        contact_email: r.try_get("contact_email").map_err(sqlx_err)?,
        template_name: r.try_get("template_name").map_err(sqlx_err)?,
        is_active: active != 0,
        created_at: r.try_get("created_at").map_err(sqlx_err)?,
    })
}

// ============================================================
// 端口
// ============================================================

#[derive(Debug, Clone)]
pub struct Port {
    pub id: i64,
    pub customer_id: i64,
    pub port_label: String,
    pub ip_count_a: i64,
    pub ip_count_b: i64,
    pub machine_rent: bool,
    pub machine_hosting: bool,
    pub librenms_bill_id: Option<i64>,
    pub notes: Option<String>,
}

impl Store {
    pub async fn insert_port(
        &self,
        customer_id: i64,
        port_label: &str,
        ip_count_a: i64,
        ip_count_b: i64,
        machine_rent: bool,
        machine_hosting: bool,
        notes: Option<&str>,
    ) -> Result<i64> {
        let res = sqlx::query(
            "INSERT INTO ports (customer_id, port_label, ip_count_a, ip_count_b,
                                machine_rent, machine_hosting, notes)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(customer_id)
        .bind(port_label)
        .bind(ip_count_a)
        .bind(ip_count_b)
        .bind(machine_rent as i64)
        .bind(machine_hosting as i64)
        .bind(notes)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("insert ports: {e}")))?;
        Ok(res.last_insert_rowid())
    }

    pub async fn list_ports_for_customer(&self, customer_id: i64) -> Result<Vec<Port>> {
        let rows = sqlx::query(
            "SELECT id, customer_id, port_label, ip_count_a, ip_count_b,
                    machine_rent, machine_hosting, librenms_bill_id, notes
             FROM ports WHERE customer_id = ? ORDER BY id",
        )
        .bind(customer_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("list ports: {e}")))?;
        rows.into_iter()
            .map(|r| {
                let mr: i64 = r.try_get("machine_rent").map_err(sqlx_err)?;
                let mh: i64 = r.try_get("machine_hosting").map_err(sqlx_err)?;
                Ok(Port {
                    id: r.try_get("id").map_err(sqlx_err)?,
                    customer_id: r.try_get("customer_id").map_err(sqlx_err)?,
                    port_label: r.try_get("port_label").map_err(sqlx_err)?,
                    ip_count_a: r.try_get("ip_count_a").map_err(sqlx_err)?,
                    ip_count_b: r.try_get("ip_count_b").map_err(sqlx_err)?,
                    machine_rent: mr != 0,
                    machine_hosting: mh != 0,
                    librenms_bill_id: r.try_get("librenms_bill_id").map_err(sqlx_err)?,
                    notes: r.try_get("notes").map_err(sqlx_err)?,
                })
            })
            .collect()
    }

    /// 阶段 8f:per-port bill 绑定的新增端口
    pub async fn insert_port_with_bill(
        &self,
        customer_id: i64,
        port_label: &str,
        ip_count_a: i64,
        ip_count_b: i64,
        machine_rent: bool,
        machine_hosting: bool,
        librenms_bill_id: Option<i64>,
        notes: Option<&str>,
    ) -> Result<i64> {
        let res = sqlx::query(
            "INSERT INTO ports (customer_id, port_label, ip_count_a, ip_count_b,
                                machine_rent, machine_hosting, librenms_bill_id, notes)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(customer_id)
        .bind(port_label)
        .bind(ip_count_a)
        .bind(ip_count_b)
        .bind(machine_rent as i64)
        .bind(machine_hosting as i64)
        .bind(librenms_bill_id)
        .bind(notes)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("insert ports (with bill): {e}")))?;
        Ok(res.last_insert_rowid())
    }

    pub async fn find_port_by_id(&self, id: i64) -> Result<Option<Port>> {
        let row = sqlx::query(
            "SELECT id, customer_id, port_label, ip_count_a, ip_count_b,
                    machine_rent, machine_hosting, librenms_bill_id, notes
             FROM ports WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("find port: {e}")))?;
        match row {
            None => Ok(None),
            Some(r) => {
                let mr: i64 = r.try_get("machine_rent").map_err(sqlx_err)?;
                let mh: i64 = r.try_get("machine_hosting").map_err(sqlx_err)?;
                Ok(Some(Port {
                    id: r.try_get("id").map_err(sqlx_err)?,
                    customer_id: r.try_get("customer_id").map_err(sqlx_err)?,
                    port_label: r.try_get("port_label").map_err(sqlx_err)?,
                    ip_count_a: r.try_get("ip_count_a").map_err(sqlx_err)?,
                    ip_count_b: r.try_get("ip_count_b").map_err(sqlx_err)?,
                    machine_rent: mr != 0,
                    machine_hosting: mh != 0,
                    librenms_bill_id: r.try_get("librenms_bill_id").map_err(sqlx_err)?,
                    notes: r.try_get("notes").map_err(sqlx_err)?,
                }))
            }
        }
    }

    pub async fn update_port(
        &self,
        id: i64,
        port_label: &str,
        ip_count_a: i64,
        ip_count_b: i64,
        machine_rent: bool,
        machine_hosting: bool,
        librenms_bill_id: Option<i64>,
        notes: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE ports SET port_label = ?, ip_count_a = ?, ip_count_b = ?,
                              machine_rent = ?, machine_hosting = ?,
                              librenms_bill_id = ?, notes = ?
             WHERE id = ?",
        )
        .bind(port_label)
        .bind(ip_count_a)
        .bind(ip_count_b)
        .bind(machine_rent as i64)
        .bind(machine_hosting as i64)
        .bind(librenms_bill_id)
        .bind(notes)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("update port: {e}")))?;
        Ok(())
    }

    pub async fn delete_port(&self, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM ports WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::Database(format!("delete port: {e}")))?;
        Ok(())
    }
}

// ============================================================
// 费率
// ============================================================

#[derive(Debug, Clone)]
pub struct Rate {
    pub id: i64,
    pub customer_id: i64,
    pub effective_from: String,
    pub effective_to: Option<String>,
    pub mbps_unit_price_cents: i64,
    pub ip_unit_price_cents: i64,
    pub ip_quantity: i64,
    pub machine_rent_cents: i64,
    pub machine_hosting_cents: i64,
    pub currency: String,
    pub librenms_bill_id: Option<i64>,
    pub business_label: Option<String>,
    pub notes: String,
}

impl Store {
    pub async fn insert_rate(
        &self,
        r: &NewRate<'_>,
    ) -> Result<i64> {
        let res = sqlx::query(
            "INSERT INTO rates
                (customer_id, effective_from, effective_to,
                 mbps_unit_price_cents, ip_unit_price_cents, ip_quantity,
                 machine_rent_cents, machine_hosting_cents, currency, librenms_bill_id,
                 business_label, notes)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(r.customer_id)
        .bind(r.effective_from)
        .bind(r.effective_to)
        .bind(r.mbps_unit_price_cents)
        .bind(r.ip_unit_price_cents)
        .bind(r.ip_quantity)
        .bind(r.machine_rent_cents)
        .bind(r.machine_hosting_cents)
        .bind(r.currency)
        .bind(r.librenms_bill_id)
        .bind(r.business_label)
        .bind(r.notes)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("insert rates: {e}")))?;
        Ok(res.last_insert_rowid())
    }

    pub async fn find_rate_for_customer_at(
        &self,
        customer_id: i64,
        period_yyyymm: &str, // "YYYY-MM-01" 形式
    ) -> Result<Option<Rate>> {
        let row = sqlx::query(
            "SELECT id, customer_id, effective_from, effective_to,
                    mbps_unit_price_cents, ip_unit_price_cents, ip_quantity,
                    machine_rent_cents, machine_hosting_cents, currency, librenms_bill_id,
                    business_label, notes
             FROM rates
             WHERE customer_id = ?
               AND effective_from <= ?
               AND (effective_to IS NULL OR effective_to > ?)
             ORDER BY effective_from DESC LIMIT 1",
        )
        .bind(customer_id)
        .bind(period_yyyymm)
        .bind(period_yyyymm)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("find rate: {e}")))?;
        row.map(|r| {
            Ok(Rate {
                id: r.try_get("id").map_err(sqlx_err)?,
                customer_id: r.try_get("customer_id").map_err(sqlx_err)?,
                effective_from: r.try_get("effective_from").map_err(sqlx_err)?,
                effective_to: r.try_get("effective_to").map_err(sqlx_err)?,
                mbps_unit_price_cents: r.try_get("mbps_unit_price_cents").map_err(sqlx_err)?,
                ip_unit_price_cents: r.try_get("ip_unit_price_cents").map_err(sqlx_err)?,
                ip_quantity: r.try_get("ip_quantity").map_err(sqlx_err)?,
                machine_rent_cents: r.try_get("machine_rent_cents").map_err(sqlx_err)?,
                machine_hosting_cents: r.try_get("machine_hosting_cents").map_err(sqlx_err)?,
                currency: r.try_get("currency").map_err(sqlx_err)?,
                librenms_bill_id: r.try_get("librenms_bill_id").map_err(sqlx_err)?,
                business_label: r.try_get("business_label").map_err(sqlx_err)?,
                notes: r.try_get("notes").map_err(sqlx_err)?,
            })
        })
        .transpose()
    }

    pub async fn list_rates_for_customer(&self, customer_id: i64) -> Result<Vec<Rate>> {
        let rows = sqlx::query(
            "SELECT id, customer_id, effective_from, effective_to,
                    mbps_unit_price_cents, ip_unit_price_cents, ip_quantity,
                    machine_rent_cents, machine_hosting_cents, currency, librenms_bill_id,
                    business_label, notes
             FROM rates WHERE customer_id = ? ORDER BY effective_from DESC",
        )
        .bind(customer_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("list rates: {e}")))?;
        rows.into_iter()
            .map(|r| {
                Ok(Rate {
                    id: r.try_get("id").map_err(sqlx_err)?,
                    customer_id: r.try_get("customer_id").map_err(sqlx_err)?,
                    effective_from: r.try_get("effective_from").map_err(sqlx_err)?,
                    effective_to: r.try_get("effective_to").map_err(sqlx_err)?,
                    mbps_unit_price_cents: r.try_get("mbps_unit_price_cents").map_err(sqlx_err)?,
                    ip_unit_price_cents: r.try_get("ip_unit_price_cents").map_err(sqlx_err)?,
                    ip_quantity: r.try_get("ip_quantity").map_err(sqlx_err)?,
                    machine_rent_cents: r.try_get("machine_rent_cents").map_err(sqlx_err)?,
                    machine_hosting_cents: r.try_get("machine_hosting_cents").map_err(sqlx_err)?,
                    currency: r.try_get("currency").map_err(sqlx_err)?,
                    librenms_bill_id: r.try_get("librenms_bill_id").map_err(sqlx_err)?,
                    business_label: r.try_get("business_label").map_err(sqlx_err)?,
                    notes: r.try_get("notes").map_err(sqlx_err)?,
                })
            })
            .collect()
    }

    pub async fn list_all_rates(&self) -> Result<Vec<Rate>> {
        let rows = sqlx::query(
            "SELECT id, customer_id, effective_from, effective_to,
                    mbps_unit_price_cents, ip_unit_price_cents, ip_quantity,
                    machine_rent_cents, machine_hosting_cents, currency, librenms_bill_id,
                    business_label, notes
             FROM rates ORDER BY customer_id, effective_from DESC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("list all rates: {e}")))?;
        rows.into_iter()
            .map(|r| {
                Ok(Rate {
                    id: r.try_get("id").map_err(sqlx_err)?,
                    customer_id: r.try_get("customer_id").map_err(sqlx_err)?,
                    effective_from: r.try_get("effective_from").map_err(sqlx_err)?,
                    effective_to: r.try_get("effective_to").map_err(sqlx_err)?,
                    mbps_unit_price_cents: r.try_get("mbps_unit_price_cents").map_err(sqlx_err)?,
                    ip_unit_price_cents: r.try_get("ip_unit_price_cents").map_err(sqlx_err)?,
                    ip_quantity: r.try_get("ip_quantity").map_err(sqlx_err)?,
                    machine_rent_cents: r.try_get("machine_rent_cents").map_err(sqlx_err)?,
                    machine_hosting_cents: r.try_get("machine_hosting_cents").map_err(sqlx_err)?,
                    currency: r.try_get("currency").map_err(sqlx_err)?,
                    librenms_bill_id: r.try_get("librenms_bill_id").map_err(sqlx_err)?,
                    business_label: r.try_get("business_label").map_err(sqlx_err)?,
                    notes: r.try_get("notes").map_err(sqlx_err)?,
                })
            })
            .collect()
    }

    pub async fn count_users(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM users")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| Error::Database(format!("count users: {e}")))?;
        row.try_get("n").map_err(sqlx_err)
    }

    // ============================================================
    // 全局设置(键值;后台 /admin/settings 可改,run-billing 运行时读取)
    // ============================================================

    /// 读单个设置;不存在返回 None(调用方用默认值兜底)
    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT value FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| Error::Database(format!("get setting {key}: {e}")))?;
        match row {
            Some(r) => Ok(Some(r.try_get("value").map_err(sqlx_err)?)),
            None => Ok(None),
        }
    }

    /// 写设置(upsert)
    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO settings (key, value, updated_at) VALUES (?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("set setting {key}: {e}")))?;
        Ok(())
    }

    /// 该客户该账期是否已生成过发票(任意状态)。
    /// 定时自检模式用它做幂等:每小时被 timer 拉起时,已出过账的客户直接跳过。
    pub async fn has_invoice_for_period(
        &self,
        customer_id: i64,
        period_year: i64,
        period_month: u32,
    ) -> Result<bool> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM invoices
             WHERE customer_id = ? AND period_year = ? AND period_month = ?",
        )
        .bind(customer_id)
        .bind(period_year)
        .bind(period_month as i64)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("has_invoice_for_period: {e}")))?;
        Ok(n > 0)
    }
}

// ============================================================
// 模板版本(读侧;写侧见 template::audit::write_template_version)
// ============================================================

#[derive(Debug, Clone)]
pub struct TemplateVersionRow {
    pub template_name: String,
    pub template_sha256: String,
    pub cell_map_json: String,
    pub drawing_anchors_json: String,
    pub last_validated_at: String,
    pub notes: Option<String>,
}

impl Store {
    /// 列出所有已审计的模板版本(阶段 8f admin 模板管理)
    pub async fn list_template_versions(&self) -> Result<Vec<TemplateVersionRow>> {
        let rows = sqlx::query(
            "SELECT template_name, template_sha256, cell_map_json,
                    drawing_anchors_json, last_validated_at, notes
             FROM template_versions ORDER BY template_name",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("list template_versions: {e}")))?;
        rows.into_iter()
            .map(|r| {
                Ok(TemplateVersionRow {
                    template_name: r.try_get("template_name").map_err(sqlx_err)?,
                    template_sha256: r.try_get("template_sha256").map_err(sqlx_err)?,
                    cell_map_json: r.try_get("cell_map_json").map_err(sqlx_err)?,
                    drawing_anchors_json: r.try_get("drawing_anchors_json").map_err(sqlx_err)?,
                    last_validated_at: r.try_get("last_validated_at").map_err(sqlx_err)?,
                    notes: r.try_get("notes").map_err(sqlx_err)?,
                })
            })
            .collect()
    }

    pub async fn find_template_version(&self, name: &str) -> Result<Option<TemplateVersionRow>> {
        let row = sqlx::query(
            "SELECT template_name, template_sha256, cell_map_json,
                    drawing_anchors_json, last_validated_at, notes
             FROM template_versions WHERE template_name = ?",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("find template_version: {e}")))?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some(TemplateVersionRow {
                template_name: r.try_get("template_name").map_err(sqlx_err)?,
                template_sha256: r.try_get("template_sha256").map_err(sqlx_err)?,
                cell_map_json: r.try_get("cell_map_json").map_err(sqlx_err)?,
                drawing_anchors_json: r.try_get("drawing_anchors_json").map_err(sqlx_err)?,
                last_validated_at: r.try_get("last_validated_at").map_err(sqlx_err)?,
                notes: r.try_get("notes").map_err(sqlx_err)?,
            })),
        }
    }
}

pub struct NewRate<'a> {
    pub customer_id: i64,
    pub effective_from: &'a str,
    pub effective_to: Option<&'a str>,
    pub mbps_unit_price_cents: i64,
    pub ip_unit_price_cents: i64,
    pub ip_quantity: i64,
    pub machine_rent_cents: i64,
    pub machine_hosting_cents: i64,
    pub currency: &'a str,
    pub librenms_bill_id: Option<i64>,
    pub business_label: Option<&'a str>,
    pub notes: &'a str,
}

// ============================================================
// 序号(事务化 INVOICE NO,决策 #14)
// ============================================================

impl Store {
    /// 取下一个序号(原子,SQLite 事务)。
    /// 若 name 不存在,初始化为 `from`(默认 1)。
    /// 应对 SQLITE_BUSY / SQLITE_LOCKED / SQLITE_CANTOPEN(transient,大量并发初始化连接时)
    /// 最多 50 次退避,每次 50ms;覆盖批量并发跑号(测试/生产 20 客户同跑)。
    pub async fn next_sequence(&self, name: &str, from: i64) -> Result<i64> {
        const MAX_RETRIES: usize = 50;
        const BACKOFF_MS: u64 = 50;

        for attempt in 0..MAX_RETRIES {
            match self.try_next_sequence(name, from).await {
                Ok(v) => return Ok(v),
                Err(e) if is_transient(&e) => {
                    tokio::time::sleep(std::time::Duration::from_millis(BACKOFF_MS)).await;
                    log::debug!("next_sequence({name}): retry {attempt} after transient err: {e}");
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Err(Error::Database(format!(
            "next_sequence({name}): exhausted {MAX_RETRIES} retries on transient errors"
        )))
    }

    async fn try_next_sequence(&self, name: &str, from: i64) -> Result<i64> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| Error::Database(format!("begin tx: {e}")))?;

        let existing: Option<(String, i64)> = sqlx::query_as(
            "SELECT name, next_value FROM sequences WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| Error::Database(format!("select sequence: {e}")))?;

        let next = match existing {
            None => {
                sqlx::query("INSERT INTO sequences (name, next_value) VALUES (?, ?)")
                    .bind(name)
                    .bind(from)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| Error::Database(format!("insert sequence: {e}")))?;
                from
            }
            Some((_, current)) => {
                let incremented = current + 1;
                sqlx::query("UPDATE sequences SET next_value = ? WHERE name = ?")
                    .bind(incremented)
                    .bind(name)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| Error::Database(format!("update sequence: {e}")))?;
                incremented
            }
        };

        tx.commit()
            .await
            .map_err(|e| Error::Database(format!("commit sequence: {e}")))?;
        Ok(next)
    }
}

/// SQLITE_BUSY(5) / SQLITE_LOCKED(6) / SQLITE_CANTOPEN(14,大量并发 open 瞬态)。
fn is_transient(e: &Error) -> bool {
    let msg = e.to_string();
    msg.contains("database is locked")
        || msg.contains("code: 5")
        || msg.contains("code: 6")
        || msg.contains("code: 14")
}

fn sqlx_err(e: sqlx::Error) -> Error {
    Error::Database(format!("{e}"))
}

// ============================================================
// Invoice(账单 + 状态机,决策 #13 / #14)
// ============================================================

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvoiceStatus {
    Generating,
    Preview,
    Confirming,
    Final,
    Failed,
    Rejected,
}

impl InvoiceStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Generating => "generating",
            Self::Preview => "preview",
            Self::Confirming => "confirming",
            Self::Final => "final",
            Self::Failed => "failed",
            Self::Rejected => "rejected",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "generating" => Ok(Self::Generating),
            "preview" => Ok(Self::Preview),
            "confirming" => Ok(Self::Confirming),
            "final" => Ok(Self::Final),
            "failed" => Ok(Self::Failed),
            "rejected" => Ok(Self::Rejected),
            other => Err(Error::Database(format!("unknown invoice status: {other}"))),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Invoice {
    pub id: i64,
    pub customer_id: i64,
    pub period_year: i64,
    pub period_month: i64,
    pub status: InvoiceStatus,
    pub invoice_no: String,
    pub template_version: String,
    pub source_snapshot_json: String,
    pub total_cents: Option<i64>,
    pub currency: String,
    pub pdf_path_preview: Option<String>,
    pub pdf_path_final: Option<String>,
    pub created_at: String,
    pub confirmed_at: Option<String>,
    pub confirmed_by: Option<i64>,
    pub rejected_reason: Option<String>,
}

impl Store {
    pub async fn upsert_invoice_generating(
        &self,
        customer_id: i64,
        period_year: i64,
        period_month: i64,
        invoice_no: &str,
        template_version: &str,
        source_snapshot_json: &str,
        currency: &str,
    ) -> Result<i64> {
        let now = chrono::Utc::now().to_rfc3339();
        let res = sqlx::query(
            "INSERT INTO invoices
                (customer_id, period_year, period_month, status,
                 invoice_no, template_version, source_snapshot_json, currency, created_at)
             VALUES (?, ?, ?, 'generating', ?, ?, ?, ?, ?)
             ON CONFLICT(customer_id, period_year, period_month) DO UPDATE SET
                status = 'generating',
                invoice_no = excluded.invoice_no,
                template_version = excluded.template_version,
                source_snapshot_json = excluded.source_snapshot_json,
                currency = excluded.currency,
                created_at = excluded.created_at",
        )
        .bind(customer_id)
        .bind(period_year)
        .bind(period_month)
        .bind(invoice_no)
        .bind(template_version)
        .bind(source_snapshot_json)
        .bind(currency)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("upsert invoice generating: {e}")))?;
        Ok(res.last_insert_rowid())
    }

    pub async fn update_invoice_preview(
        &self,
        id: i64,
        total_cents: i64,
        pdf_path_preview: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE invoices
             SET status = 'preview', total_cents = ?, pdf_path_preview = ?
             WHERE id = ?",
        )
        .bind(total_cents)
        .bind(pdf_path_preview)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("update invoice preview: {e}")))?;
        Ok(())
    }

    pub async fn update_invoice_confirmed(
        &self,
        id: i64,
        pdf_path_final: &str,
        actor_user_id: i64,
    ) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE invoices
             SET status = 'final',
                 pdf_path_final = ?,
                 confirmed_at = ?,
                 confirmed_by = ?
             WHERE id = ?",
        )
        .bind(pdf_path_final)
        .bind(&now)
        .bind(actor_user_id)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("update invoice final: {e}")))?;
        Ok(())
    }

    pub async fn update_invoice_rejected(
        &self,
        id: i64,
        reason: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE invoices
             SET status = 'rejected', rejected_reason = ?
             WHERE id = ?",
        )
        .bind(reason)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("update invoice rejected: {e}")))?;
        Ok(())
    }

    pub async fn update_invoice_failed(&self, id: i64) -> Result<()> {
        sqlx::query("UPDATE invoices SET status = 'failed' WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::Database(format!("update invoice failed: {e}")))?;
        Ok(())
    }

    pub async fn find_invoice(&self, id: i64) -> Result<Option<Invoice>> {
        let row = sqlx::query(
            "SELECT id, customer_id, period_year, period_month, status,
                    invoice_no, template_version, source_snapshot_json,
                    total_cents, currency, pdf_path_preview, pdf_path_final,
                    created_at, confirmed_at, confirmed_by, rejected_reason
             FROM invoices WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("find invoice: {e}")))?;
        row.map(row_to_invoice).transpose()
    }

    pub async fn find_invoice_for_customer_month(
        &self,
        customer_id: i64,
        year: i64,
        month: i64,
    ) -> Result<Option<Invoice>> {
        let row = sqlx::query(
            "SELECT id, customer_id, period_year, period_month, status,
                    invoice_no, template_version, source_snapshot_json,
                    total_cents, currency, pdf_path_preview, pdf_path_final,
                    created_at, confirmed_at, confirmed_by, rejected_reason
             FROM invoices WHERE customer_id = ? AND period_year = ? AND period_month = ?",
        )
        .bind(customer_id)
        .bind(year)
        .bind(month)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("find invoice by period: {e}")))?;
        row.map(row_to_invoice).transpose()
    }

    pub async fn list_invoices_for_customer(&self, customer_id: i64) -> Result<Vec<Invoice>> {
        let rows = sqlx::query(
            "SELECT id, customer_id, period_year, period_month, status,
                    invoice_no, template_version, source_snapshot_json,
                    total_cents, currency, pdf_path_preview, pdf_path_final,
                    created_at, confirmed_at, confirmed_by, rejected_reason
             FROM invoices WHERE customer_id = ?
             ORDER BY period_year DESC, period_month DESC",
        )
        .bind(customer_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("list invoices: {e}")))?;
        rows.into_iter().map(row_to_invoice).collect()
    }

    pub async fn record_action(
        &self,
        invoice_id: i64,
        action: &str,
        actor_user_id: Option<i64>,
        reason: Option<&str>,
    ) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO invoice_actions
                (invoice_id, action, actor_user_id, reason, at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(invoice_id)
        .bind(action)
        .bind(actor_user_id)
        .bind(reason)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("record action: {e}")))?;
        Ok(())
    }
}

fn row_to_invoice(r: sqlx::sqlite::SqliteRow) -> Result<Invoice> {
    let status_s: String = r.try_get("status").map_err(sqlx_err)?;
    let status = InvoiceStatus::parse(&status_s)?;
    Ok(Invoice {
        id: r.try_get("id").map_err(sqlx_err)?,
        customer_id: r.try_get("customer_id").map_err(sqlx_err)?,
        period_year: r.try_get("period_year").map_err(sqlx_err)?,
        period_month: r.try_get("period_month").map_err(sqlx_err)?,
        status,
        invoice_no: r.try_get("invoice_no").map_err(sqlx_err)?,
        template_version: r.try_get("template_version").map_err(sqlx_err)?,
        source_snapshot_json: r.try_get("source_snapshot_json").map_err(sqlx_err)?,
        total_cents: r.try_get("total_cents").map_err(sqlx_err)?,
        currency: r.try_get("currency").map_err(sqlx_err)?,
        pdf_path_preview: r.try_get("pdf_path_preview").map_err(sqlx_err)?,
        pdf_path_final: r.try_get("pdf_path_final").map_err(sqlx_err)?,
        created_at: r.try_get("created_at").map_err(sqlx_err)?,
        confirmed_at: r.try_get("confirmed_at").map_err(sqlx_err)?,
        confirmed_by: r.try_get("confirmed_by").map_err(sqlx_err)?,
        rejected_reason: r.try_get("rejected_reason").map_err(sqlx_err)?,
    })
}

// ============================================================
// 用户(阶段 6 Web 登录用)
// ============================================================

#[derive(Debug, Clone)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    pub role: String,
    pub is_active: bool,
}

impl Store {
    pub async fn insert_user(
        &self,
        username: &str,
        password_hash: &str,
        role: &str,
    ) -> Result<i64> {
        let now = chrono::Utc::now().to_rfc3339();
        let res = sqlx::query(
            "INSERT INTO users (username, password_hash, role, is_active, created_at)
             VALUES (?, ?, ?, 1, ?)",
        )
        .bind(username)
        .bind(password_hash)
        .bind(role)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("insert user: {e}")))?;
        Ok(res.last_insert_rowid())
    }

    pub async fn find_user_by_id(&self, id: i64) -> Result<Option<User>> {
        let row = sqlx::query(
            "SELECT id, username, password_hash, role, is_active FROM users WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("find user by id: {e}")))?;
        row.map(|r| {
            let active: i64 = r.try_get("is_active").map_err(sqlx_err)?;
            Ok(User {
                id: r.try_get("id").map_err(sqlx_err)?,
                username: r.try_get("username").map_err(sqlx_err)?,
                password_hash: r.try_get("password_hash").map_err(sqlx_err)?,
                role: r.try_get("role").map_err(sqlx_err)?,
                is_active: active != 0,
            })
        })
        .transpose()
    }

    pub async fn find_user_by_username(&self, username: &str) -> Result<Option<User>> {
        let row = sqlx::query(
            "SELECT id, username, password_hash, role, is_active FROM users WHERE username = ?",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::Database(format!("find user: {e}")))?;
        row.map(|r| {
            let active: i64 = r.try_get("is_active").map_err(sqlx_err)?;
            Ok(User {
                id: r.try_get("id").map_err(sqlx_err)?,
                username: r.try_get("username").map_err(sqlx_err)?,
                password_hash: r.try_get("password_hash").map_err(sqlx_err)?,
                role: r.try_get("role").map_err(sqlx_err)?,
                is_active: active != 0,
            })
        })
        .transpose()
    }

    pub async fn update_user_last_login(&self, id: i64) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query("UPDATE users SET last_login_at = ? WHERE id = ?")
            .bind(&now)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::Database(format!("update last_login: {e}")))?;
        Ok(())
    }
}
