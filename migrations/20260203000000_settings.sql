-- 全局设置(键值):出账日 / 出账时刻 / 发票号模板,后台可改,run-billing 每次运行时读取
CREATE TABLE IF NOT EXISTS settings (
  key        TEXT PRIMARY KEY,
  value      TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
