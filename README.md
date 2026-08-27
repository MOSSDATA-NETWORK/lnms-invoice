# lnms-invoice

> 基于 LibreNMS API 自动生成带宽账单(95th percentile),从 Excel 模板填数据,soffice 转 PDF,Web 审批确认后归档。
>
> 状态:**方案 v0.4 已冻结,实施未开始**。详见 [DESIGN.md](./DESIGN.md)。

---

## 项目目标

每月 1 日 10:00(客户本地时区)系统自动从 LibreNMS 拉客户当月 95th percentile 带宽,按客户档案(姓名 / 银行 / 单价)填入 Excel 模板,生成 95th 曲线图(替换模板占位图),soffice 转 PDF。运营人员在 Web 界面:

1. **预览**(PDF 落到 `output/YYYY/MM/preview/`)
2. **重跑**(覆盖预览,允许多次直到满意)
3. **确认**(原子写入 `output/YYYY/MM/final/`,永不覆盖)

确认后的账单连同原始采样、费率、模板版本、图快照一起不可变归档,任何时候可追溯。

## 技术栈速览

| 层 | 选型 | 版本/说明 |
|---|---|---|
| 语言 | Rust | 1.75+ (rustup stable) |
| Web 框架 | axum | 异步 |
| ORM | sqlx | SQLite,WAL + busy_timeout |
| HTML 模板 | askama | SSR |
| Excel | umya-spreadsheet | **3.1.0**(锁版) |
| 图表 | plotters | bitmap backend,无系统 Chromium 依赖 |
| PDF | LibreOffice soffice | `--headless --convert-to pdf` |
| 认证 | tower-sessions + Argon2id | 持久 Session |
| 数据库 | SQLite | 单文件,UTF-8 |
| 部署 | systemd oneshot + timer | 替代 cron;`flock` 防并发;`LoadCredential=` 注 API token |
| 目标系统 | Ubuntu 24.04 LTS | Noble Numbat |

完整决策与理由见 [DESIGN.md §决策汇总](./DESIGN.md#决策汇总-v04-冻结)。

## 目录结构

```
lnms-invoice/
├── README.md                    ← 你正在看
├── DESIGN.md                    ← 详细设计(v0.4 冻结)
├── 模板.xlsx                    ← CNY 客户账单模板(用户提供)
├── 模板2.xlsx                   ← HKD 客户账单模板(用户提供)
├── scripts_template_audit/      ← 模板预检工具与报告(出方案时做的)
│   ├── audit.py                 ←  模板结构预检脚本
│   ├── summarize.py             ←  汇总脚本
│   ├── summary.md               ←  预检汇总报告
│   ├── report.json              ←  预检原始 JSON
│   ├── template1_image1.png     ←  模板 1 嵌入图样本
│   └── template1_image1.jpg     ←  同上 jpg 版(Read 工具用)
└── (待实施)src/、bin/、deploy/、tests/、config/、templates/
```

## 阶段状态

| 阶段 | 状态 |
|---|---|
| 0 准备(资产 + 凭据) | 🟡 模板 ✓,其余待提供 |
| 1 骨架(Cargo + 配置 + smoke) | ✅ |
| 2 数据模型(SQLite schema + store) | ✅ |
| 3 模板预检(锁定 drawing 锚点 + PDF 金样) | ✅ |
| 4 LNMS 客户端(reqwest + 退避) | ✅ |
| 4.5 图表生成(plotters + OOXML PNG 替换) | ✅ |
| 5 模板填充(umya 3.1.0 填数据 + 公式) | ✅ |
| 6 状态机(4 态 + 事务) | ✅ |
| 7 部署(install_ubuntu24.sh + systemd) | ✅ |
| 8 Web 管理后台 + Apple HIG UI | ✅ |
| 8f 客户 CRUD + per-port bill + 模板管理 | ✅ |
| 8g 后台设置(出账日/时刻/发票号模板)+ 定时自检 | ✅ |
| 9 端到端(真实环境跑一个月) | ⏳ 待资产齐全 |

实施期共 99 个测试通过(本轮 v0.6.5 全量改单位 + 撤回 v0.6.4 保底/business_label,未新增测试)。Codex 评审暂未走用户授权的双签流程(本轮单位重构属失效模式封闭的改动,按用户授权独立处理)。

## 阶段 0 准备清单(进生产前必须齐)

- [x] **LibreNMS URL + API token**(已提供,仅测试;token 不入文档,实施时走 env / LoadCredential)
- [ ] **1-2 个真实客户档案样本**(姓名 / 地址 / 银行 / 端口列表 / 单价)
- [ ] **目标服务器信息**(运行用户 / 输出绝对路径 / hostname)
- [ ] **1-2 个客户已在 LibreNMS 配好 bills**(`bill_id` + 95th 配置)
- [ ] **cron 触发时间确认**(默认每月 1 日 10:00 客户本地时区)

资产齐后,跑 `./scripts/install.sh` 完成生产部署,然后用真实 LNMS 端到端跑一个月(阶段 9)。

## 文档

- [DESIGN.md](./DESIGN.md) — 详细设计(架构 / 数据模型 / 状态机 / 部署 / 阶段 / 风险 / 协作披露)

## 运维速查

### 本地开发

```bash
# 一次性填充示例数据(admin/admin123 / operator/admin123 + 1 客户 2 端口 1 费率)
cargo run --bin dev-bootstrap -- /tmp/lnms-invoice/dev.db

# 改完 templates/*.html 后:重新 build 二进制 + 重启 serve
./scripts/dev-reload.sh

# 或只 build 不启动
./scripts/dev-reload.sh --no-run
```

> ⚠️ **askama 是编译时模板嵌入**——改 `templates/*.html` 后必须 `cargo build --bin lnms-invoice`,
> 不能只 `cargo build --lib`,否则新模板不会生效(`dev-reload.sh` 已经替你做了这一步)。

### 生产部署(Ubuntu 24)

```bash
# 1. 上传代码到服务器
sudo cp -r . /opt/lnms-invoice/

# 2. 编译 release 二进制
cd /opt/lnms-invoice && cargo build --release

# 3. 安装 systemd unit + timer + 资产��入脚本
sudo ./scripts/install.sh

# 4. 导入资产(不含 token)
sudo /usr/local/bin/import-assets /etc/lnms-invoice/customers.json

# 5. 注入每个 LNMS 实例的 token(走 sudo,不进 web)
echo -n '<token>' | sudo /usr/local/bin/set-instance-token /var/lib/lnms-invoice/db.sqlite '<instance-name>'

# 6. 启动 web + 启用定时跑账
sudo systemctl enable --now lnms-invoice-web.service lnms-invoice-billing.timer
```

### 手工触发跑账(不等定时)

```bash
sudo /usr/local/bin/run-billing --force /etc/lnms-invoice/config.toml
```

> v0.6.2 起,定时器(`lnms-invoice-billing.timer`)改为 `OnCalendar=hourly`,由 `run-billing` 读 `settings` 表自检是否到「出账日/时」。
> 手工触发仍支持 `--force` 跳过自检立即跑;未带 `--force` 时不达出账日会立即退出,不会重复出账。

### 修改出账时间 / 发票号模板

后台 → `/admin/settings`:

- **出账日**(1–28,避免 2 月/30 天月份日期溢出)
- **出账时刻**(0–23,server 本地时区,默认 Asia/Shanghai)
- **发票号模板**(必含 `{KEY}` `{YYYY}` `{MM}` `{SEQ}`;`{SEQ}` 自动补 0 到 4 位)

设置写入 `settings` 表,下次 `run-billing` 拉起立即生效。

### 数据库备份

```bash
# DB 单文件,直接拷(注意 WAL:拷前先 sqlite3 db.sqlite ".backup /tmp/db.bak")
sudo -u lnms-invoice sqlite3 /var/lib/lnms-invoice/db.sqlite ".backup /var/backups/lnms-invoice-$(date +%F).db"
```

### 常见故障

| 现象 | 排查 |
|---|---|
| 服务起来后 502 | 看 `journalctl -u lnms-invoice-web`;多半是 `LNMS_INVOICE_SESSION_SECRET` 没注入 |
| `template_version` mismatch | `./target/release/bin/template-audit /etc/lnms-invoice/config.toml` 重做模板预检 |
| 模板里图替换没生效 | 看 `run-billing` stdout 里有没有 `image_replace ok`;确认 drawing 锚点 row 还在 |
| 改 `.html` 后界面没变化 | 99% 是忘了 `cargo build --bin`——跑 `./scripts/dev-reload.sh` |
| `customer_internal_key` 找不到 | 检查 `customers.json` 里这个字段是否漏了;`import-customers` 会报错行号 |
| 到出账日没生成账单 | 1) systemctl status lnms-invoice-billing.timer 看是否 hourly 拉起;2) `/admin/settings` 看 billing_day/billing_hour 设置;3) 看 `run-billing` 日志自检输出 `not due yet ... skip`(未到期)或 done 统计 |

## 协作披露

- 方案阶段(2026-08):Claude 与 Codex 双签共识
- **Codex 评审**:2 轮(基于方案 v1 / v2 派单)
- **结论一致**:两轮独立观察都强调"**数据血缘/不可变快照** 是 v2 核心防线"
- **遗留"依据不足"**:3.1.0 MSRV / 3.x API 差异 / LibreNMS 端点精确响应结构 — 留到阶段 1/3/4 实施期实测验证
- **派发执行层**:**未冻结**(用户决定),目前不写项目代码

---

## 变更记录

- **2026-08-27 v0.6.5**:金额单位从「分(整数)」改为「元(REAL,保留 2 位小数)」——`mbps_unit_price` / `ip_unit_price` / `machine_rent` / `machine_hosting` / `invoices.total` / `invoice_lines.line_total` 全部改元,列名 `*_cents → *_yuan`,类型 `INTEGER → REAL`,迁移 `20260208000000_amount_unit_to_yuan.sql`(建新表 + `INSERT ... SELECT ... / 100.0` + DROP + RENAME,空表 noop,非空表正确 ÷100);表单 `<input step="0.01">`,placeholder/说明文案「¥分」→「¥元」,展示用 `format!("{:.2}", ...)`;渲染 Excel `D5` 单元格直接 `set_value_number(data.total_yuan)`(不再 `× 100`)。**v0.6.5 同期撤回 v0.6.4 的保底与业务标注**:`rates.monthly_guarantee_yuan` / `guarantee_floor_mbps` / `business_label` 三列 DROP(`20260209000000_drop_guarantee_and_business_label.sql`),billing 回到 v0.6.3 简单求和(`mbps_95th × 单价 + 机柜费 + IP 费`),表单/列表去掉 3 栏;运营如需手工调整金额,后续单独提需求(本期不实现「出账预览手动覆盖」);测试 99 通过、0 忽略
- **2026-08-26 v0.6.4**:**已撤回**(见 v0.6.5);曾加「保底金额」字段 + 超额自动计算 + `business_label`,运营反馈表单变复杂且「超额自动算」不直观
- **2026-08-26 v0.6.2**:后台设置页 `/admin/settings`(出账日 1–28 / 出账时刻 0–23 / 可配发票号模板,占位符 `{KEY}` `{YYYY}` `{MM}` `{SEQ}` 必填),写 `settings` 表(settings 表迁移 `20260203000000_settings.sql`);`run-billing` 每次启动读 settings 自检「是否到出账日时」,未到期直接退出;`systemd timer` 由「每月 1 日 10:00」改为 `OnCalendar=hourly`(`install.sh` 同步),由 `run-billing` 自检 + `has_invoice_for_period` 幂等保证不会重复出账;新增 `--force` CLI 参数跳过自检(手动补账/测试用);测试 93 通过、0 忽略
- **2026-08-26 v0.6.1**:「费率」更名「费用」并去掉「(分)」单位后缀;费用表单新增可选 LNMS bill_id(读取优先级:端口 > 费用 > 客户默认,迁移 `20260202000000_rate_bill_id`);**修复查看 bills 必崩问题**——LibreNmsClient(reqwest::blocking)的构造与请求整体移入 `spawn_blocking`,此前在 axum 异步上下文直接 panic,run-billing 同步修复;测试 85 通过、0 忽略
- **2026-08-26 v0.6**:Web 后台客户 CRUD(新增/编辑/删除,删除前自动检查 ports + invoices 引用);per-port LNMS bill 绑定(`ports.librenms_bill_id` 可空,fallback 到客户默认 bill);`/admin/templates` 上传/审计(xlsx 自动 inspect 落 `template_versions`);`/admin/instances/:id/bills` + `/admin/ajax/bills` 把 NMS bills 接到客户表单下拉;模板管理(admin 可给客户绑模板);`build_invoice_lines` 改为 per-port 取 95th,`run-billing` 按 port 各自拉 history
- **2026-08-26 v0.5.1**:Web 表单支持直接注入 LibreNMS API token(`type=password` + autocomplete 关闭);同步要求 **部署强制 HTTPS**,DB 备份视为敏感文件;测试 66 通过
- **2026-08-26 v0.5**:Web 管理后台(实例/客户/费率 CRUD)+ Apple HIG UI 重构 + dev-reload 脚本 + 64 集成测试通过
- **2026-08-26 v0.4(冻结)**:Web 服务 + 两阶段审批流 + 多 LNMS + 95th 曲线图替换 + 完整数据血缘防御
- **2026-08-26 v0.3**:Rust 同步 + Ubuntu 24 + umya 1.0(后修正为 3.1.0)
- **2026-08-26 v0.2**:从 Python 改为 Rust
- **2026-08-26 v0.1**:从 LibreNMS 拉数据 + Excel 模板 + soffice 转 PDF(基础流程)
