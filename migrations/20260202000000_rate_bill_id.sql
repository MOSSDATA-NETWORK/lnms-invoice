-- 费用(费率)级 LNMS bill 绑定:端口未绑定时回落到费用指定的 bill,再回落客户默认
ALTER TABLE rates ADD COLUMN librenms_bill_id INTEGER;
