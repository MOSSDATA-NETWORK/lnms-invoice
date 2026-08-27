-- 阶段 8f:per-port LNMS bill 绑定 + 客户模板选择
--
-- 背景:
--   customers.librenms_bill_id 历史上是「客户级别一个 bill」,所有 port 共享同一份 95th。
--   现在允许每个 port 绑自己的 LNMS bill,客户级别降级为「可选默认」。
--   customers 加 template_name 外键,允许选择已审计的模板版本。
--
-- 数据迁移策略:
--   新列均允许 NULL(ALTER ADD COLUMN 默认即可);既有的 customer.librenms_bill_id NOT NULL 保留
--   —— store 层在 port.bill_id 为空时 fallback 到 customer.bill_id,创建客户时仍必填(向后兼容)。

-- 1. ports 加 per-port bill
ALTER TABLE ports ADD COLUMN librenms_bill_id INTEGER;
CREATE INDEX IF NOT EXISTS idx_ports_bill ON ports(librenms_bill_id);

-- 2. customers 加 template_name 外键(可空 = 不绑)
ALTER TABLE customers ADD COLUMN template_name TEXT
  REFERENCES template_versions(template_name);
CREATE INDEX IF NOT EXISTS idx_customers_template ON customers(template_name);