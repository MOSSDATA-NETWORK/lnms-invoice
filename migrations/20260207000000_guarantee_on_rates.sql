-- v0.6.5:保底段支持
--   monthly_guarantee_cents: 当月预付的保底固定金额(¥分)。0 = 无保底
--   guarantee_floor_mbps:   保底覆盖的 Mbps 阈值。0 = 无阈值,按全量计费
--
-- 计费公式(每条 rate):
--   overage_mbps = max(0, bill_95th_mbps - guarantee_floor_mbps)
--   overage_cents = overage_mbps * mbps_unit_price_cents
--   total_cents   = monthly_guarantee_cents + overage_cents
--
-- 旧数据(无保底):默认 0,行为完全等价于改前。
ALTER TABLE rates ADD COLUMN monthly_guarantee_cents INTEGER NOT NULL DEFAULT 0;
ALTER TABLE rates ADD COLUMN guarantee_floor_mbps   INTEGER NOT NULL DEFAULT 0;