-- v0.6.5 修订:金额单位从「分」改「元」(保留 2 位小数)
-- 列名 *_cents → *_yuan;类型 INTEGER → REAL。
-- 空表 noop;非空表把现有数据 ÷100。
--
-- SQLite 不支持 ALTER COLUMN TYPE,标准做法是「建新表 → 拷数据(转换) → DROP → 改名 → 重建索引」。
-- rates 表与 invoices / invoice_lines 表分别处理。
-- rates 表其他列太多,这里用「拷全表」+「在 SELECT 里改字段名+转换」。

-- ============ rates ============
CREATE TABLE IF NOT EXISTS rates_new (
  id                     INTEGER PRIMARY KEY AUTOINCREMENT,
  customer_id            INTEGER NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
  effective_from         TEXT    NOT NULL,
  effective_to           TEXT,
  mbps_unit_price_yuan   REAL    NOT NULL DEFAULT 0,
  ip_unit_price_yuan     REAL    NOT NULL DEFAULT 0,
  ip_quantity            INTEGER NOT NULL DEFAULT 0,
  machine_rent_yuan      REAL    NOT NULL DEFAULT 0,
  machine_hosting_yuan   REAL    NOT NULL DEFAULT 0,
  currency               TEXT    NOT NULL,
  librenms_bill_id       INTEGER,
  business_label         TEXT,
  notes                  TEXT    NOT NULL DEFAULT '',
  monthly_guarantee_yuan REAL    NOT NULL DEFAULT 0,
  guarantee_floor_mbps   INTEGER NOT NULL DEFAULT 0,
  CHECK(effective_to IS NULL OR effective_to > effective_from)
);
INSERT INTO rates_new (
  id, customer_id, effective_from, effective_to,
  mbps_unit_price_yuan, ip_unit_price_yuan, ip_quantity,
  machine_rent_yuan, machine_hosting_yuan,
  currency, librenms_bill_id, business_label, notes,
  monthly_guarantee_yuan, guarantee_floor_mbps
)
SELECT
  id, customer_id, effective_from, effective_to,
  CAST(mbps_unit_price_cents   AS REAL) / 100.0,
  CAST(ip_unit_price_cents     AS REAL) / 100.0,
  ip_quantity,
  CAST(machine_rent_cents      AS REAL) / 100.0,
  CAST(machine_hosting_cents   AS REAL) / 100.0,
  currency, librenms_bill_id, business_label, notes,
  CAST(COALESCE(monthly_guarantee_cents, 0) AS REAL) / 100.0,
  COALESCE(guarantee_floor_mbps, 0)
FROM rates;
DROP TABLE rates;
-- DROP TABLE 不会清 sqlite_sequence,AUTOINCREMENT 表改名会因记录残留撞名,先手动删
DELETE FROM sqlite_sequence WHERE name = 'rates';
ALTER TABLE rates_new RENAME TO rates;
CREATE INDEX IF NOT EXISTS idx_rates_customer_effective ON rates(customer_id, effective_from);

-- ============ invoices ============
CREATE TABLE IF NOT EXISTS invoices_new (
  id                     INTEGER PRIMARY KEY AUTOINCREMENT,
  customer_id            INTEGER NOT NULL REFERENCES customers(id),
  period_year            INTEGER NOT NULL,
  period_month           INTEGER NOT NULL CHECK(period_month BETWEEN 1 AND 12),
  status                 TEXT    NOT NULL CHECK(status IN ('generating','preview','confirming','final','failed','rejected')),
  invoice_no             TEXT    NOT NULL UNIQUE,
  template_version       TEXT    NOT NULL,
  source_snapshot_json   TEXT    NOT NULL,
  total_yuan             REAL,
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
INSERT INTO invoices_new (
  id, customer_id, period_year, period_month, status, invoice_no,
  template_version, source_snapshot_json, total_yuan, currency,
  pdf_path_preview, pdf_path_final, created_at, confirmed_at, confirmed_by, rejected_reason
)
SELECT
  id, customer_id, period_year, period_month, status, invoice_no,
  template_version, source_snapshot_json,
  CAST(total_cents AS REAL) / 100.0,
  currency, pdf_path_preview, pdf_path_final,
  created_at, confirmed_at, confirmed_by, rejected_reason
FROM invoices;
DROP TABLE invoices;
DELETE FROM sqlite_sequence WHERE name = 'invoices';
ALTER TABLE invoices_new RENAME TO invoices;
CREATE INDEX IF NOT EXISTS idx_invoices_status ON invoices(status);
CREATE INDEX IF NOT EXISTS idx_invoices_period ON invoices(period_year, period_month);

-- ============ invoice_lines ============
CREATE TABLE IF NOT EXISTS invoice_lines_new (
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
  line_total_yuan   REAL,
  UNIQUE(invoice_id, line_no)
);
INSERT INTO invoice_lines_new (
  id, invoice_id, line_no, port_label, mbps_95th,
  ip_count_a, ip_count_b, machine_rent, machine_hosting,
  rate_id, line_total_yuan
)
SELECT
  id, invoice_id, line_no, port_label, mbps_95th,
  ip_count_a, ip_count_b, machine_rent, machine_hosting,
  rate_id, CAST(line_total_cents AS REAL) / 100.0
FROM invoice_lines;
DROP TABLE invoice_lines;
DELETE FROM sqlite_sequence WHERE name = 'invoice_lines';
ALTER TABLE invoice_lines_new RENAME TO invoice_lines;
CREATE INDEX IF NOT EXISTS idx_invoice_lines_invoice ON invoice_lines(invoice_id);
