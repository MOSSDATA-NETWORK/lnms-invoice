-- lnms-invoice 初始化 schema
-- 阶段 2:9 张表 + 索引 + 约束

-- 1. LibreNMS 实例
CREATE TABLE IF NOT EXISTS librenms_instances (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  name            TEXT    NOT NULL UNIQUE,
  url             TEXT    NOT NULL,
  api_token_enc   BLOB    NOT NULL,
  is_active       INTEGER NOT NULL DEFAULT 1,
  created_at      TEXT    NOT NULL
);

-- 2. 客户(内部客户键 + LNMS bill_id 双键)
CREATE TABLE IF NOT EXISTS customers (
  id                          INTEGER PRIMARY KEY AUTOINCREMENT,
  internal_key                TEXT    NOT NULL UNIQUE,
  name                        TEXT    NOT NULL,
  currency                    TEXT    NOT NULL CHECK(currency IN ('CNY','HKD')),
  librenms_instance_id        INTEGER NOT NULL REFERENCES librenms_instances(id),
  librenms_bill_id            INTEGER NOT NULL,
  timezone                    TEXT    NOT NULL DEFAULT 'Asia/Shanghai',
  company_type                TEXT    NOT NULL CHECK(company_type IN ('domestic','hk')),
  company_info_json           TEXT    NOT NULL DEFAULT '{}',
  company_info_schema_version INTEGER NOT NULL DEFAULT 1,
  billing_address             TEXT,
  contact_email               TEXT,
  is_active                   INTEGER NOT NULL DEFAULT 1,
  created_at                  TEXT    NOT NULL,
  UNIQUE(librenms_instance_id, librenms_bill_id)
);
CREATE INDEX IF NOT EXISTS idx_customers_active ON customers(is_active);

-- 3. 端口(95th Mbps 不入库,快照在 invoices.source_snapshot_json)
CREATE TABLE IF NOT EXISTS ports (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  customer_id     INTEGER NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
  port_label      TEXT    NOT NULL,
  ip_count_a      INTEGER NOT NULL DEFAULT 0,
  ip_count_b      INTEGER NOT NULL DEFAULT 0,
  machine_rent    INTEGER NOT NULL DEFAULT 0,
  machine_hosting INTEGER NOT NULL DEFAULT 0,
  notes           TEXT,
  UNIQUE(customer_id, port_label)
);
CREATE INDEX IF NOT EXISTS idx_ports_customer ON ports(customer_id);

-- 4. 费率(历史区间,多版本)
CREATE TABLE IF NOT EXISTS rates (
  id                     INTEGER PRIMARY KEY AUTOINCREMENT,
  customer_id            INTEGER NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
  effective_from         TEXT    NOT NULL,
  effective_to           TEXT,
  mbps_unit_price_cents  INTEGER NOT NULL,
  ip_unit_price_cents    INTEGER NOT NULL,
  machine_rent_cents     INTEGER NOT NULL,
  machine_hosting_cents  INTEGER NOT NULL,
  currency               TEXT    NOT NULL,
  CHECK(effective_to IS NULL OR effective_to > effective_from)
);
CREATE INDEX IF NOT EXISTS idx_rates_customer_effective ON rates(customer_id, effective_from);

-- 5. 账单
CREATE TABLE IF NOT EXISTS invoices (
  id                     INTEGER PRIMARY KEY AUTOINCREMENT,
  customer_id            INTEGER NOT NULL REFERENCES customers(id),
  period_year            INTEGER NOT NULL,
  period_month           INTEGER NOT NULL CHECK(period_month BETWEEN 1 AND 12),
  status                 TEXT    NOT NULL CHECK(status IN ('generating','preview','confirming','final','failed','rejected')),
  invoice_no             TEXT    NOT NULL UNIQUE,
  template_version       TEXT    NOT NULL,
  source_snapshot_json   TEXT    NOT NULL,
  total_cents            INTEGER,
  currency               TEXT    NOT NULL,
  pdf_path_preview       TEXT,
  pdf_path_final         TEXT,
  created_at             TEXT    NOT NULL,
  confirmed_at           TEXT,
  confirmed_by           INTEGER REFERENCES users(id),
  rejected_reason        TEXT,
  UNIQUE(customer_id, period_year, period_month),
  CHECK((status='final') = (pdf_path_final IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS idx_invoices_status ON invoices(status);
CREATE INDEX IF NOT EXISTS idx_invoices_period ON invoices(period_year, period_month);

-- 6. 账单行
CREATE TABLE IF NOT EXISTS invoice_lines (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  invoice_id        INTEGER NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
  line_no           INTEGER NOT NULL,
  port_label        TEXT    NOT NULL,
  mbps_95th         INTEGER,
  ip_count_a        INTEGER NOT NULL DEFAULT 0,
  ip_count_b        INTEGER NOT NULL DEFAULT 0,
  machine_rent      INTEGER NOT NULL DEFAULT 0,
  machine_hosting   INTEGER NOT NULL DEFAULT 0,
  rate_id           INTEGER NOT NULL REFERENCES rates(id),
  line_total_cents  INTEGER,
  UNIQUE(invoice_id, line_no)
);
CREATE INDEX IF NOT EXISTS idx_invoice_lines_invoice ON invoice_lines(invoice_id);

-- 7. 生成批次
CREATE TABLE IF NOT EXISTS invoice_runs (
  id                  INTEGER PRIMARY KEY AUTOINCREMENT,
  run_type            TEXT    NOT NULL CHECK(run_type IN ('scheduled','manual_replay','recovery')),
  scheduled_for       TEXT    NOT NULL,
  started_at          TEXT    NOT NULL,
  finished_at         TEXT,
  customers_total     INTEGER NOT NULL,
  customers_succeeded INTEGER NOT NULL DEFAULT 0,
  customers_failed    INTEGER NOT NULL DEFAULT 0,
  error_summary       TEXT
);

-- 8. 操作日志(不可变,审计用)
CREATE TABLE IF NOT EXISTS invoice_actions (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  invoice_id      INTEGER NOT NULL REFERENCES invoices(id),
  invoice_run_id  INTEGER REFERENCES invoice_runs(id),
  action          TEXT    NOT NULL CHECK(action IN ('running','preview_generated','preview_regenerated','confirmed','rejected','failed')),
  actor_user_id   INTEGER REFERENCES users(id),
  reason          TEXT,
  at              TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_invoice_actions_invoice ON invoice_actions(invoice_id);
CREATE INDEX IF NOT EXISTS idx_invoice_actions_at ON invoice_actions(at);

-- 9. 用户
CREATE TABLE IF NOT EXISTS users (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  username        TEXT    NOT NULL UNIQUE,
  password_hash   TEXT    NOT NULL,
  role            TEXT    NOT NULL CHECK(role IN ('admin','operator','viewer')),
  is_active       INTEGER NOT NULL DEFAULT 1,
  created_at      TEXT    NOT NULL,
  last_login_at   TEXT
);

-- 9.x 序号表(事务化 INVOICE NO,决策 #14)
CREATE TABLE IF NOT EXISTS sequences (
  name        TEXT    PRIMARY KEY,
  next_value  INTEGER NOT NULL
);

-- 模板版本 + 单元格映射(决策 #20,数据血缘)
CREATE TABLE IF NOT EXISTS template_versions (
  template_name        TEXT    PRIMARY KEY,
  template_sha256      TEXT    NOT NULL,
  cell_map_json        TEXT    NOT NULL,
  drawing_anchors_json TEXT    NOT NULL DEFAULT '[]',
  last_validated_at    TEXT    NOT NULL,
  notes                TEXT
);
