# lnms-invoice 详细设计(v0.4 冻结)

> 最后更新:2026-08-26
> 状态:**方案冻结,实施未开始**
> 协作披露:Claude(编排) + Codex 评审(2 轮,独立判断)

---

## 目录

1. [决策汇总(v0.4 冻结)](#决策汇总v04-冻结)
2. [架构总览](#架构总览)
3. [数据模型(SQLite schema)](#数据模型sqlite-schema)
4. [状态机](#状态机)
5. [模板与图表生成](#模板与图表生成)
6. [业务流程](#业务流程)
7. [部署形态](#部署形态)
8. [API 集成(LibreNMS)](#api-集成librenms)
9. [阶段划分(8 个)](#阶段划分8-个)
10. [风险与缓解](#风险与缓解)
11. [阶段 0 准备清单](#阶段-0-准备清单)
12. [协作披露](#协作披露)
13. [变更记录](#变更记录)

---

## 决策汇总(v0.4 冻结)

整合两轮 Codex 评审,共 22 项决策。

| # | 项 | 决策 | 来源 |
|---|---|---|---|
| 1 | 语言 | Rust 同步(业务)+ tokio 异步(Web) | 用户拍板 |
| 2 | Web 框架 | axum | 用户拍板 |
| 3 | ORM | sqlx + SQLite | 新 Codex Q5 |
| 4 | HTML 模板 | askama(SSR) | 用户拍板 |
| 5 | 认证 | tower-sessions + Argon2id,Cookie Secure+HttpOnly+SameSite,CSRF | 新 Codex Q6 |
| 6 | Excel 库 | **umya-spreadsheet 3.1.0 精确锁版** | 旧 Codex Q1 + crates.io 实时数据 |
| 7 | 依赖整改 | reqwest `default-features=false, features=["blocking","json","rustls-tls"]` 显式;**serde_yaml → TOML**;Decimal 冻结舍入 | 旧 Codex Q2 |
| 8 | 图表库 | plotters bitmap backend;无需 Cairo/Pango/Chromium | 新 Codex Q9 |
| 9 | 图替换策略 | **保留 drawing XML,直接替换 OOXML 包内同名 PNG** | 新 Codex Q9 |
| 10 | 字体 | Noto Sans/Serif CJK SC 显式加载;`fonts-noto-cjk fontconfig` | 新 Codex Q9 |
| 11 | 时区 | 客户档案 IANA timezone 字段;默认 Asia/Shanghai / Asia/Hong_Kong;内部存 UTC | 新 Codex Q9 |
| 12 | 模块划分 | `config / store / librenms / domain / template / runner`(砍 i18n/notifier;加 store) | 旧 Codex Q3 |
| 13 | 数据模型 | 加 `invoice_lines / invoice_runs / source_snapshot_json / template_version / invoice_no UNIQUE`;总额 INTEGER 分;公司字段 JSON + schema_version | 新 Codex Q3 |
| 14 | 状态机 | `generating → preview → confirming → final` + failed/rejected;条件更新 `WHERE status='preview'`;事务 | 新 Codex Q4 |
| 15 | PDF 输出 | 临时文件 + 原子 rename;**正式文件永不覆盖**;soffice 独立 UserInstallation | 旧 Q4 + 新 Q4 |
| 16 | API 限流 | 429 / Retry-After / 指数退避 / 分页 / 复用 Client | 旧 Codex Q4 |
| 17 | soffice 调度 | 不逐单启动;**分块转换**;并发只在压测后开 | 旧 Codex Q4 |
| 18 | 调度 | 内部 tokio 循环,启动时补跑缺失账期;不引 cron DSL | 新 Codex Q7 |
| 19 | 部署 | **systemd oneshot + timer 替代 cron**;`Persistent=true`、`flock`、journald、`LoadCredential=` | 旧 Codex Q5 |
| 20 | 数据血缘 | 模板哈希 + 单元格清单 → 内部客户键 ↔ LNMS `bill_id` 映射;漂移立即失败 | 旧 Codex Q6 |
| 21 | 预览校验 | soffice 重算,自动校验总额/页数/图片/公式/占位符 | 旧 Codex Q6 |
| 22 | 系统动作 | `actor_user_id=NULL` 允许(系统自跑);拒绝必须带原因 | 新 Codex Q4 |

### crates.io 实时数据(2026-08-17 抓取)

- umya-spreadsheet **3.1.0**(2026-08-17 更新)
- downloads 1,020,005;recent_downloads 355,482
- num_versions 89,created 2020-08-23
- repository: https://github.com/MathNya/umya-spreadsheet
- keywords: reader, writer, excel, xlsx, spreadsheet
- 备注:3.1.0 MSRV / 3.x API 差异 / 已知 issue **依据不足**(联网受限),留到阶段 1 实施期 `cargo check --locked` 实测

### reqwest 显式 features(必须)

```toml
reqwest = { version = "0.12", default-features = false, features = ["blocking", "json", "rustls-tls"] }
```

不写 `default-features = false` 默认会拉 OpenSSL,与"避系统依赖"目标冲突。

### serde_yaml 停止维护

来源:https://docs.rs/serde_yaml/latest/serde_yaml/(顶部"deprecated since 0.9")。替代为 `toml` crate(Rust 生态标准)。

---

## 架构总览

### 一句话

`内部 tokio 调度 → spawn_blocking 跑同步账单核心(API/Excel/图表/soffice)→ axum Web 暴露管理界面 + 预览/确认/重跑接口 → SQLite 持久化 → systemd 守护`

### 模块划分(6 个)

```
src/
├── config/         # TOML 配置 + env 注入(API token 走 systemd LoadCredential)
├── store/          # SQLite 访问层 + schema 迁移;管内部客户键 ↔ LNMS bill_id 映射 + 事务化序号
├── librenms/       # LibreNMS API 客户端(reqwest 同步,限流 + 退避)
├── domain/         # 业务模型(Bill / Customer / Invoice / Period / Rate)
├── template/       # 模板填充(umya 3.1.0)+ 图表生成(plotters)+ OOXML PNG 替换
└── runner/         # 调度循环(账期计算 + 启动补跑)+ 状态机 + soffice 编排
```

Web 层(axum + askama)作为顶层入口,调用 `runner` 暴露的内部 API;`runner` 不依赖 axum(便于未来拆分 `serve` / `run-billing` 子命令)。

### 数据流(月度一次)

```
[内部 tokio 调度 每分钟检查] → 到 10:00 触发 run_billing()
  ↓
[librenms] 拉所有 active 客户所属 LNMS 实例的 bills 列表
  ↓
[store]    按 (customer_id, year, month) 检查唯一性,跳过已生成
  ↓
[librenms] 逐客户拉 bills/{id} 详情 + 95th 标量 + /bills/{id}/history 时间序列
  ↓
[domain]   组合"客户档案 + LNMS 数据 + 适用 rates"成 InvoiceDraft
  ↓
[template] plotters 渲染该客户当月 95th 曲线 PNG (2069×713)
  ↓
[template] 复制模板 → umya 3.1.0 读 → 填输入单元格 → 保存 .xlsx
  ↓
[template] OOXML 解压 → 替换 xl/media/image1.png → 重打包 .xlsx
  ↓
[runner]   soffice --headless --convert-to pdf(独立 UserInstallation,超时 60s)
  ↓
[runner]   临时文件 + 原子 rename → output/YYYY/MM/preview/<customer_id>.pdf
  ↓
[store]    invoices.status = 'preview', 写 invoice_actions(running, preview_generated)
  ↓
[runner]   soffice 重算校验:总额 / 页数 / 图片存在 / 公式无 #VALUE! / 占位符 0
  ↓
[Web 界面]  运营登录 → 看到预览列表 → 重跑 / 确认 / 拒绝
  ↓
[runner 确认]  WHERE status='preview' 条件更新 → 受影响=1 → 原子 rename 至 final/
```

---

## 数据模型(SQLite schema)

**7 张表**。所有金额用 INTEGER(微观单位:分)。时间戳存 UTC ISO8601 字符串。JSON 字段用 TEXT(JSON1 验证)。

### `librenms_instances`

```sql
CREATE TABLE librenms_instances (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,                -- "main" / "secondary-dc"
  url TEXT NOT NULL,                        -- https://librenms.example.com
  api_token_enc BLOB NOT NULL,              -- 系统密钥加密的 token(libsodium secretbox)
  is_active INTEGER NOT NULL DEFAULT 1,
  created_at TEXT NOT NULL                  -- UTC ISO8601
);
```

### `customers`

```sql
CREATE TABLE customers (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,                       -- 内部显示名,如 "湖南XX网络"
  currency TEXT NOT NULL CHECK(currency IN ('CNY','HKD')),
  librenms_instance_id INTEGER NOT NULL REFERENCES librenms_instances(id),
  librenms_bill_id INTEGER NOT NULL,        -- LNMS 侧 bill_id
  timezone TEXT NOT NULL DEFAULT 'Asia/Shanghai',  -- IANA
  company_type TEXT NOT NULL CHECK(company_type IN ('domestic','hk')),
  company_info_json TEXT NOT NULL,          -- JSON,见下 schema_version
  company_info_schema_version INTEGER NOT NULL,    -- 公司信息 schema 版本
  billing_address TEXT,
  contact_email TEXT,
  is_active INTEGER NOT NULL DEFAULT 1,
  created_at TEXT NOT NULL,
  UNIQUE(librenms_instance_id, librenms_bill_id)
);
```

**company_info_json schema**:
- `domestic`:`{tax_id, address, phone, email, bank_name, bank_account}`
- `hk`:`{beneficiary_name, bank_name, bank_address, account_hkd, account_other, bank_code, swift_code}`

### `ports`

```sql
CREATE TABLE ports (
  id INTEGER PRIMARY KEY,
  customer_id INTEGER NOT NULL REFERENCES customers(id),
  port_label TEXT NOT NULL,                 -- "华为BGP 3段"
  ip_count_a INTEGER NOT NULL DEFAULT 0,
  ip_count_b INTEGER NOT NULL DEFAULT 0,
  machine_rent INTEGER NOT NULL DEFAULT 0,  -- 是否租用
  machine_hosting INTEGER NOT NULL DEFAULT 0,  -- 是否托管
  notes TEXT,
  UNIQUE(customer_id, port_label)
);
```

> **注意**:95th Mbps **不存 ports 表**——它是 LNMS 实时数据,快照在 `invoices.source_snapshot_json` 留底。新 Codex Q3 异议:"ports.mbps_95th 会被覆盖,确认后无法还原"。

### `rates`

```sql
CREATE TABLE rates (
  id INTEGER PRIMARY KEY,
  customer_id INTEGER NOT NULL REFERENCES customers(id),
  effective_from TEXT NOT NULL,             -- YYYY-MM-DD
  effective_to TEXT,                        -- NULL = 当前有效
  mbps_unit_price_cents INTEGER NOT NULL,   -- 每 Mbps 单价(分)
  ip_unit_price_cents INTEGER NOT NULL,
  machine_rent_cents INTEGER NOT NULL,
  machine_hosting_cents INTEGER NOT NULL,
  currency TEXT NOT NULL,
  CHECK(effective_to IS NULL OR effective_to > effective_from)
);
```

### `invoices`(核心)

```sql
CREATE TABLE invoices (
  id INTEGER PRIMARY KEY,
  customer_id INTEGER NOT NULL REFERENCES customers(id),
  period_year INTEGER NOT NULL,
  period_month INTEGER NOT NULL CHECK(period_month BETWEEN 1 AND 12),
  status TEXT NOT NULL CHECK(status IN ('generating','preview','confirming','final','failed','rejected')),
  invoice_no TEXT NOT NULL UNIQUE,          -- 见 #14
  template_version TEXT NOT NULL,           -- 模板 SHA256 哈希
  source_snapshot_json TEXT NOT NULL,       -- 不可变原始数据快照(95th、history、rates 引用)
  total_cents INTEGER,                      -- 预览后填;NULL 表示还没算出来
  currency TEXT NOT NULL,
  pdf_path_preview TEXT,                    -- 相对 OUTPUT_DIR
  pdf_path_final TEXT,                      -- 仅 status='final' 后填
  created_at TEXT NOT NULL,
  confirmed_at TEXT,
  confirmed_by INTEGER REFERENCES users(id),  -- NULL = 系统
  rejected_reason TEXT,
  UNIQUE(customer_id, period_year, period_month),  -- 幂等关键
  CHECK((status='final') = (pdf_path_final IS NOT NULL))
);
```

### `invoice_lines`(明细)

```sql
CREATE TABLE invoice_lines (
  id INTEGER PRIMARY KEY,
  invoice_id INTEGER NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
  line_no INTEGER NOT NULL,                 -- 1-based 行号
  port_label TEXT NOT NULL,
  mbps_95th INTEGER,                        -- 微观,NULL = 0
  ip_count_a INTEGER NOT NULL DEFAULT 0,
  ip_count_b INTEGER NOT NULL DEFAULT 0,
  machine_rent INTEGER NOT NULL DEFAULT 0,
  machine_hosting INTEGER NOT NULL DEFAULT 0,
  rate_id INTEGER NOT NULL REFERENCES rates(id),  -- 该行用的费率
  line_total_cents INTEGER,                 -- 该行合计(预览后填)
  UNIQUE(invoice_id, line_no)
);
```

### `invoice_runs`(生成批次)

```sql
CREATE TABLE invoice_runs (
  id INTEGER PRIMARY KEY,
  run_type TEXT NOT NULL CHECK(run_type IN ('scheduled','manual_replay','recovery')),
  scheduled_for TEXT NOT NULL,              -- 期望运行时间(账期)
  started_at TEXT NOT NULL,
  finished_at TEXT,
  customers_total INTEGER NOT NULL,
  customers_succeeded INTEGER NOT NULL DEFAULT 0,
  customers_failed INTEGER NOT NULL DEFAULT 0,
  error_summary TEXT
);
```

### `invoice_actions`(操作日志,不可变)

```sql
CREATE TABLE invoice_actions (
  id INTEGER PRIMARY KEY,
  invoice_id INTEGER NOT NULL REFERENCES invoices(id),
  invoice_run_id INTEGER REFERENCES invoice_runs(id),
  action TEXT NOT NULL CHECK(action IN ('running','preview_generated','preview_regenerated','confirmed','rejected','failed')),
  actor_user_id INTEGER REFERENCES users(id),  -- NULL = 系统
  reason TEXT,                              -- 拒绝/失败必填
  at TEXT NOT NULL                          -- UTC ISO8601
);
```

### `users`

```sql
CREATE TABLE users (
  id INTEGER PRIMARY KEY,
  username TEXT NOT NULL UNIQUE,
  password_hash TEXT NOT NULL,              -- Argon2id
  role TEXT NOT NULL CHECK(role IN ('admin','operator','viewer')),
  is_active INTEGER NOT NULL DEFAULT 1,
  created_at TEXT NOT NULL,
  last_login_at TEXT
);
```

### 索引

```sql
CREATE INDEX idx_invoices_status ON invoices(status);
CREATE INDEX idx_invoices_period ON invoices(period_year, period_month);
CREATE INDEX idx_invoice_actions_invoice ON invoice_actions(invoice_id);
CREATE INDEX idx_invoice_actions_at ON invoice_actions(at);
```

### SQLite 配置

```sql
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;
PRAGMA foreign_keys = ON;
```

---

## 状态机

```
                ┌──────────┐
       start →  │generating│
                └─────┬────┘
                      │ fill + soffice + validate
                ┌─────▼────┐         ┌────────┐
       (auto)→  │ preview  │  ───→   │ failed │ (回退,记录 reason)
                └─────┬────┘         └────────┘
                      │
            ┌─────────┼─────────┐
            │         │         │
       regenerate   confirm   reject
            │         │         │
       ┌────▼────┐    │    ┌────▼────┐
       │ preview │    │    │rejected │
       └─────────┘    │    └─────────┘
                      │
                ┌─────▼────┐
                │confirming│ ← 短暂态,WHERE status='preview' UPDATE 期间
                └─────┬────┘
                      │ 事务:UPDATE + action + 原子 rename
                ┌─────▼────┐
                │  final   │ (不可变,永不覆盖)
                └──────────┘
```

### 关键约束

- **状态转换原子**:`UPDATE invoices SET status='confirming' WHERE id=? AND status='preview'` 必须受影响 1 行
- **正式文件永不覆盖**:`final/` 下文件 write 时先写同目录临时文件 + `fsync` + 原子 `rename(2)`
- **拒绝/失败必填 reason**:`invoice_actions.reason` NOT NULL when action ∈ {rejected, failed}
- **幂等键**:`(customer_id, period_year, period_month)` 唯一约束,调度循环靠此补跑

---

## 模板与图表生成

### 模板(CNY / HKD 两份)

| 模板 | 文件 | 维度 | 合并格 | 公式 | 嵌入图 |
|---|---|---|---|---|---|
| CNY | `模板.xlsx` | A1:R13 + 2 空 sheet | 7 | 8 (O3-O10) | image1.png 185 KB |
| HKD | `模板2.xlsx` | A1:I10 | 7 | 4 (G4-G7) | image1.png 185 KB |

完整预检见 [`scripts_template_audit/summary.md`](./scripts_template_audit/summary.md)。

### 占位符约定

- **CNY 模板**:E 列字面值 `"读取LNMS"` 表示"该单元格从 LibreNMS API 拉 95th Mbps 填入"
- **HKD 模板**:E 列同样字面值

### 模板哈希 + 单元格映射(`store.template_versions` 思路)

启动时计算 `sha256(模板文件)`,记录每个有值/有公式的单元格:

```text
template_version = "a1b2c3d4..."
cell_map = {
  "A1": { kind: "title", value: "业务合作对账单({year}.{month:02d})" },
  "B3": { kind: "literal", value: "{period_start_excel_serial}" },
  "E3": { kind: "lnms", field: "ports[0].mbps_95th" },
  "F3": { kind: "literal", value: 43.5 },   // 单价
  "G3": { kind: "literal", value: 8 },     // IP 数
  "O3": { kind: "formula", value: "=(E3*F3)+G3*H3+K3*L3+N3" },  // 模板里,不动
  ...
}
```

**漂移检测**:启动时若模板哈希与上次不同,记录告警但仍尝试;**任何"读取LNMS"字面值未替换** 或 **公式含 #VALUE!** → 立即失败,不进入 preview 状态。

### 图表生成(plotters bitmap)

```text
数据源:LNMS /bills/{id}/history 端点(待阶段 4 实测,2026-08 Codex 提示"很可能是账期历史而非 5min 序列")
采样:    5 分钟一个点(一天 288 点,一月 ≈ 8640 点)
画法:    折线图(原始流量,bit/s)+ 横线 P95 标量参考
输出:    PNG 2069×713 RGB,144 DPI,文件 < 200 KB
字体:    Noto Sans CJK SC(显式 path,避开 fontconfig 解析问题)
时区:    X 轴标签按 customer.timezone 渲染
```

### 图表插入(OOXML PNG 替换,不动 umya 图 API)

```text
1. umya 3.1.0 填数据 → 保存 .xlsx(此时图仍是原图)
2. unzip 拷贝到临时目录
3. cp 渲染好的 PNG → xl/media/image1.png(同名覆盖)
4. 重新 zip 打包 .xlsx
5. 验证:重新打开 .xlsx,确认 xl/media/image1.png 是新图,drawing XML 不变
```

理由:umya 3.x 的"图增删 API"未实测,**保留 drawing XML + 替换 PNG 是最稳妥的方案**(新 Codex Q9)。

---

## 业务流程

### 月度调度(每月 1 日 10:00 客户本地时区)

```
tokio 循环(每 60s 检查):
  1. 算 next_run = 下一个 (year, month, 1, 10:00) in Asia/Shanghai
  2. 启动时:SELECT 缺失的 (period_year, period_month) from invoice_runs → 补跑
  3. 到点:创建 invoice_runs(run_type='scheduled', scheduled_for=next_run)
  4. 遍历所有 active 客户:
       对每个客户:
         a. SELECT invoices WHERE (customer_id, year, month) → 若 status='final' 跳过
         b. librenms 拉 bill 详情 + history
         c. domain 组合 InvoiceDraft
         d. 写 invoices(status='generating', source_snapshot_json=...)
         e. template 填 .xlsx + 替换 PNG
         f. soffice 转 PDF(独立 UserInstallation,临时输出)
         g. 写 invoices(status='preview', pdf_path_preview=...)
         h. runner 自动校验(总额/页数/图片/公式/占位符)
            通过: invoice_actions(preview_generated)
            失败: status='failed', 写 reason
  5. 标记 invoice_runs.finished_at,更新 succeeded/failed 计数
```

### 运营操作(Web 界面)

| 操作 | 路径 | 权限 | 关键事务 |
|---|---|---|---|
| 列出当月预览 | `GET /invoices?period=YYYY-MM` | operator+ | - |
| 查看单张预览 | `GET /invoices/:id` | operator+ | - |
| 下载预览 PDF | `GET /invoices/:id/pdf?type=preview` | operator+ | - |
| 重跑 | `POST /invoices/:id/regenerate` | operator+ | WHERE status='preview',覆写 preview/ + action |
| 确认 | `POST /invoices/:id/confirm` | operator+ | 事务:UPDATE status='final' WHERE status='preview' + 原子 rename + action |
| 拒绝 | `POST /invoices/:id/reject` | operator+ | WHERE status='preview',status='rejected',reason 必填 |
| LNMS 实例管理 | `/admin/instances` | admin | CRUD |
| 客户档案管理 | `/admin/customers` | admin | CRUD |
| 费率管理 | `/admin/rates` | admin | CRUD(带 effective 区间校验) |

所有写操作走 CSRF token。

---

## 部署形态

### systemd oneshot + timer(替代 cron)

```ini
# /etc/systemd/system/lnms-invoice-run.service
[Unit]
Description=lnms-invoice monthly billing run
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
User=lnms-invoice
Group=lnms-invoice
WorkingDirectory=/var/lib/lnms-invoice
ExecStart=/usr/local/bin/lnms-invoice run-billing
LoadCredentialEncrypted=lnms_api_token:/etc/lnms-invoice/token.cred
# 防并发
ExecStartPre=/usr/bin/flock -n /var/lock/lnms-invoice.run /bin/true
# 硬化
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/lnms-invoice /var/log/lnms-invoice
StateDirectory=lnms-invoice
LogsDirectory=lnms-invoice
TimeoutStartSec=2h
```

```ini
# /etc/systemd/system/lnms-invoice-run.timer
[Unit]
Description=lnms-invoice monthly billing timer

[Timer]
OnCalendar=*-*-01 10:00:00 Asia/Shanghai
Persistent=true
AccuracySec=60s
Unit=lnms-invoice-run.service

[Install]
WantedBy=timers.target
```

**`Persistent=true`**:系统关机时错过的时间点,开机后补跑(配合 `runner` 启动补跑逻辑,双重保险)。

### 目录与权限

```
/usr/local/bin/lnms-invoice          root:root 0755    # 二进制
/etc/lnms-invoice/                   root:lnms-invoice 0750
├── lnms-invoice.toml                root:lnms-invoice 0640   # 主配置
├── token.cred                       root:lnms-invoice 0640   # API token(LoadCredential 注入)
/var/lib/lnms-invoice/               lnms-invoice:lnms-invoice 0750
├── db.sqlite                        0640
├── output/                          0750
│   ├── 2026/
│   │   └── 08/
│   │       ├── preview/             0750
│   │       └── final/               0750
└── soffice-profile/                 0750  # soffice UserInstallation
/var/log/lnms-invoice/               lnms-invoice:lnms-invoice 0755
```

### install_ubuntu24.sh 关键包

```bash
apt install -y --no-install-recommends \
  libreoffice-core libreoffice-common libreoffice-writer \
  fonts-noto-cjk fonts-noto-cjk-extra fonts-noto \
  fontconfig \
  ca-certificates curl
```

`rustup` 装 stable;`soffice` 用 `libreoffice-calc`(xlsx 转换实际依赖 calc)。

---

## API 集成(LibreNMS)

### 端点(待阶段 4 实测)

| 端点 | 用途 | 备注 |
|---|---|---|
| `GET /api/v0/bills` | 列所有 bills | 支持 `?customer_id=` |
| `GET /api/v0/bills/{id}` | 单 bill 详情 | 含 `rate_95th`、`total_data` |
| `GET /api/v0/bills/{id}/history` | 95th 时间序列 | **2026-08 Codex 提示"很可能是账期历史,不是 5min 序列"**,待实测 |
| `GET /api/v0/ports/{id}/traffic` | 单端口流量 | 备选数据源 |

`/bills/{id}/history` 若非 5min 序列,降级用 `/ports/{id}/traffic` 拉端口级数据,自己按 5min 聚合。

### 客户端约束

- reqwest `Client` 单例,长生命周期,连接复用
- 每请求后 `sleep(50ms)`(粗防限流;实际限流处理见下)
- 限流处理:`429` 读 `Retry-After`;指数退避(`200ms × 2^n`,上限 30s);最多 5 次重试
- 分页:API 默认 30 req/s,10-20 客户基本不踩;后续若客户量大,加分页参数
- 超时:连接 5s,响应 30s,总 60s

### API token 注入

不走环境变量(易泄漏),走 systemd `LoadCredentialEncrypted=`,代码侧从 `$CREDENTIALS_DIRECTORY/lnms_api_token` 读。Store 中 `librenms_instances.api_token_enc` 用 libsodium secretbox 加密,密钥从 `/etc/lnms-invoice/master.key` 读(仅 root 可读)。

---

## 阶段划分(8 个)

| 阶段 | 内容 | 验收 |
|---|---|---|
| **0 准备** | 模板 ✓;LNMS URL+token;1-2 客户档案;1-2 客户已配 LNMS bills;目标服务器信息 | 资产齐 |
| **1 骨架** | Cargo 项目 + TOML 配置 + `store` 迁移 + smoke | `cargo run --bin dev_smoke` 成功 |
| **2 数据模型** | 7 表 + 索引/约束 + 单元测试 | `cargo test store` 通过 |
| **3 模板预检** | 解析 drawing XML 锁定锚点;锁模板哈希 + 单元格映射;空数据填一次,PDF 金样 | 金样 PDF 通过 |
| **4 LNMS 客户端** | reqwest 同步 + 退避;实测 `/bills/{id}/history` 响应;10-20 客户跑通拉取 | 真实 LNMS 跑通 |
| **4.5 图表生成** | plotters bitmap + Noto 字体;OOXML PNG 直接替换;中文 X 轴;soffice 校验 | 真实客户 1 张图进 PDF |
| **5 模板填充** | umya 3.1.0 填数据;保留原图(走 OOXML);公式自算 | 真实客户 1 张 xlsx + PDF |
| **6 状态机** | 4 态 + 失败/拒绝;条件更新 + 事务;操作日志;CSRF | 单元测试覆盖 |
| **7 部署** | `install_ubuntu24.sh` + systemd oneshot+timer + flock + LoadCredentialEncrypted | 月底 dry-run |
| **8 端到端** | 真实环境跑一个月(预览阶段);失败路径 + 重跑 + 数据血缘拦截 | 归档 + 审计可追溯 |

每阶段产出由 Codex 交叉验证(协作协议:高危领域 + 新业务)。

---

## 风险与缓解

| 风险 | 缓解 | 来源 |
|---|---|---|
| 数据血缘错配(客户↔bill) | 模板哈希+单元格映射+预览校验;漂移立即失败 | 旧 Codex 独立观察 |
| 确认后无法追溯原始数据 | `source_snapshot_json` 不可变 + 模板版本 + 图快照 | 新 Codex 独立观察 |
| soffice 启动慢 / 进程残留 | 独立 UserInstallation + 超时杀进程树 + 续跑状态 | 旧 Codex Q4 |
| 中文字体缺失 | 显式装 Noto CJK + plotters 显式字体路径 | 新 Codex Q9 |
| LibreNMS API 限流 | 429 + Retry-After + 指数退避 | 旧 Codex Q4 |
| LibreNMS 时区(UTC)与客户时区不一致 | 客户档案 IANA 字段,内部 UTC | 新 Codex Q9 |
| umya 3.1.0 实际兼容性 | 阶段 1 `cargo check --locked` + 阶段 3 PDF 金样测试 | 旧 Codex Q1 |
| SQLite 单写者竞争 | WAL + busy_timeout 5s + 外键 | 新 Codex Q5 |
| 并发双确认 | 条件更新 WHERE status='preview',受影响行=1 | 新 Codex Q4 |
| 正式 PDF 被覆盖 | 临时文件 + 原子 rename;final/ 永不覆盖 | 旧 Q4 + 新 Q4 |
| 跨 LNMS 实例客户归属错 | `customers.librenms_instance_id` 外键 + UNIQUE | 业务设计 |
| API token 泄漏 | systemd `LoadCredentialEncrypted` + libsodium secretbox 存 store | 旧 Codex Q5 |
| 失败账单留半截 PDF | 临时文件 + 校验通过才 rename;失败不写 final/ | 新 Codex Q4 |

---

## 阶段 0 准备清单

进阶段 1 之前必须齐:

- [x] **LibreNMS URL + API token**(2026-08-26 用户提供测试 token;**仅测试用,不入文档,实施时走 env / systemd `LoadCredentialEncrypted` 注入**;测试结束后作废)
- [ ] **1-2 个真实客户档案样本**(姓名 / 地址 / 银行 / 端口列表 / 单价)
- [ ] **目标服务器信息**(运行用户 / 输出绝对路径 / hostname)
- [ ] **1-2 个客户已在 LibreNMS 配好 bills**(`bill_id` + 95th 配置)
- [ ] **cron 触发时间确认**(默认每月 1 日 10:00 客户本地时区)

### Token 安全约束(强制)

- **不入文档** / **不入代码** / **不入 git** / **不入聊天回显**
- 实施时:开发期用 `.env`(加入 `.gitignore`);生产期走 systemd `LoadCredentialEncrypted=`
- `store.librenms_instances.api_token_enc` 用 libsodium secretbox 加密,密钥从 `/etc/lnms-invoice/master.key`(root 0600)读
- 测试结束后作废测试 token,新生产 token 单独走安全通道传递

---

## 协作披露

### 方案阶段(2026-08-26)

- **编排**:Claude(与用户对话)
- **评审**:Codex(2 轮,基于 v1 / v2 派单)
- **结论一致**:两轮独立观察都强调 **"数据血缘/不可变快照是 v2 核心防线"**,其它异议均接受并整合
- **依据不足项**(留到实施期实测):
  - umya-spreadsheet 3.1.0 MSRV(最低 rustc 版本)
  - umya 3.x 与 1.x API 差异
  - LibreNMS `/bills/{id}/history` 精确响应结构
  - plotters 实际渲染耗时(数量级估算:几十~数百 ms/图)

### Codex 异议分布

- 旧 Codex(6 个):全部异议(选型/依赖/架构/限流/部署/数据血缘)
- 新 Codex(9 个):3 同意 / 3 建议 / 3 异议

### 派发执行层

- **状态**:**未冻结**(用户决定,2026-08-26)
- 影响:目前不动项目代码;资产齐后才会进入阶段 1 实施

---

## 变更记录

| 版本 | 日期 | 主要变更 |
|---|---|---|
| v0.4 | 2026-08-26 | 冻结:Web 服务 + 两阶段审批流 + 多 LNMS + 95th 曲线图替换 + 完整数据血缘防御 |
| v0.3 | 2026-08-26 | Rust 同步 + Ubuntu 24 + umya 1.0(后修正 3.1.0) |
| v0.2 | 2026-08-26 | Python → Rust |
| v0.1 | 2026-08-26 | 基础流程(LibreNMS API + Excel 模板 + soffice 转 PDF) |
