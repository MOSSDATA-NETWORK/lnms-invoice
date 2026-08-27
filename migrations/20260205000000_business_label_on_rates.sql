-- v0.6.4:费率表加业务名称/备注列(纯元数据,不参与计费)
-- 区别于 effective_from/to 的纯时段切分,business_label 让运营能标注这条费率覆盖的具体业务线/项目名(如「IDC-A 区」「BGP 主用」等)
ALTER TABLE rates ADD COLUMN business_label TEXT;