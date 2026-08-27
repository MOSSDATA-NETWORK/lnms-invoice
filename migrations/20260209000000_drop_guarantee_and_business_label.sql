-- v0.6.5:撤掉 v0.6.4 的「保底金额/保底 Mbps」;保留 business_label(业务名称,纯元数据)
ALTER TABLE rates DROP COLUMN monthly_guarantee_yuan;
ALTER TABLE rates DROP COLUMN guarantee_floor_mbps;
-- business_label 迁移已在 20260205000000 加过,此处不再重复 ADD;