-- IP 数量从「端口累加」改为「费用层面直填」(v0.6.3)
-- 原因:LibreNMS 端读 IP 数不可靠;数量在后台费用表单上直接维护
ALTER TABLE rates ADD COLUMN ip_quantity INTEGER NOT NULL DEFAULT 0;
