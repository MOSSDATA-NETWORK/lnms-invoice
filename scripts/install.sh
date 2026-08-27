#!/usr/bin/env bash
# lnms-invoice 部署脚本(阶段 7,Ubuntu 24.04 LTS)
#
# 作用:
# 1. apt 装 LibreOffice + Noto CJK + ca-certificates
# 2. 创建系统用户 lnms-invoice(非登录 shell,nologin)
# 3. 创建设备目录 /var/lib/lnms-invoice/{templates,db,output,soffice-profile}
# 4. 安装二进制到 /usr/local/bin/lnms-invoice
# 5. 安装 systemd unit(常驻 web + oneshot billing + billing.timer)
# 6. 写 /etc/lnms-invoice/lnms-invoice.toml 占位(运维再编辑)
# 7. systemd-creds 加密 session_secret(从 $LNMS_INVOICE_SESSION_SECRET 读)
#
# 用法:
#   sudo LNMS_INVOICE_SESSION_SECRET="$(openssl rand -hex 32)" \
#        ./scripts/install.sh [--no-build] [--binary <path>]
#
# 前提:
# - 当前用户有 sudo 权限
# - 二进制已 cargo build --release(或 --no-build 跳过)
# - 配置文件 /etc/lnms-invoice/lnms-invoice.toml 已就绪(或本脚本生成占位)

set -euo pipefail

APP_USER="lnms-invoice"
APP_HOME="/var/lib/lnms-invoice"
ETC_DIR="/etc/lnms-invoice"
SYSTEMD_DIR="/etc/systemd/system"
BIN_DEST="/usr/local/bin/lnms-invoice"

NO_BUILD=0
BIN_PATH=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-build) NO_BUILD=1; shift ;;
        --binary)   BIN_PATH="$2"; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

if [[ $EUID -ne 0 ]]; then
    echo "请用 sudo 跑" >&2
    exit 1
fi

# 1. apt
echo "[install] apt 依赖..."
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y --no-install-recommends \
    libreoffice-core libreoffice-common libreoffice-calc \
    fonts-noto-cjk fonts-noto-cjk-extra \
    ca-certificates systemd

# 2. 系统用户
if ! id "$APP_USER" >/dev/null 2>&1; then
    echo "[install] 创建用户 $APP_USER..."
    adduser --system --no-create-home --shell /usr/sbin/nologin --group "$APP_USER"
fi

# 3. 设备目录
echo "[install] 设备目录 $APP_HOME ..."
mkdir -p "$APP_HOME"/{templates,db,output,soffice-profile}
chown -R "$APP_USER":"$APP_USER" "$APP_HOME"
chmod 0750 "$APP_HOME"

# 4. 二进制
if [[ -z "$BIN_PATH" ]]; then
    BIN_PATH="$APP_HOME/lnms-invoice"
fi
if [[ $NO_BUILD -eq 0 ]]; then
    echo "[install] cargo build --release..."
    (cd "$(dirname "$0")/.." && cargo build --release)
    install -m 0755 target/release/lnms-invoice "$BIN_PATH"
fi
if [[ ! -x "$BIN_PATH" ]]; then
    echo "[install] 拷贝二进制到 $BIN_DEST ..."
    install -m 0755 "$BIN_PATH" "$BIN_DEST"
fi
# 让 lnms-invoice 命令也可被非 APP_USER 用户调用(可选)
ln -sf "$BIN_DEST" /usr/local/bin/lnms-invoice >/dev/null 2>&1 || true

# 5. 配置目录
if [[ ! -d "$ETC_DIR" ]]; then
    echo "[install] 配置目录 $ETC_DIR ..."
    mkdir -p "$ETC_DIR"
    chmod 0750 "$ETC_DIR"
fi
if [[ ! -f "$ETC_DIR/lnms-invoice.toml" ]]; then
    echo "[install] 生成占位配置(运维请随后编辑)..."
    install -m 0640 -o root -g "$APP_USER" config/default.toml "$ETC_DIR/lnms-invoice.toml"
fi

# 6. systemd unit
echo "[install] systemd unit..."
install -m 0644 scripts/systemd/lnms-invoice-web.service "$SYSTEMD_DIR/"
install -m 0644 scripts/systemd/lnms-invoice-billing.service "$SYSTEMD_DIR/"

# 6a. timer 周期固定 hourly(self-check 模式)
# 说明:由于出账日 / 时刻 / 发票号模板已经搬到 settings 表 + /admin/settings UI 可改,
# timer 不再需要按 config 渲染 OnCalendar。run-billing 每小时被拉起后读 settings 自检,
# 到期才真正出账;未到期直接退出。已出账客户由 invoices 表幂等,不会重复跑号。
echo "[install] timer OnCalendar=hourly (run-billing 自检是否到期)"
install -m 0644 scripts/systemd/lnms-invoice-billing.timer "$SYSTEMD_DIR/"

# 7. session_secret 加密(从环境变量读)
if [[ -z "${LNMS_INVOICE_SESSION_SECRET:-}" ]]; then
    echo "[install] 警告:未提供 LNMS_INVOICE_SESSION_SECRET,跳过 systemd-creds 加密" >&2
else
    echo "[install] 加密 session_secret 到 systemd credential..."
    echo -n "$LNMS_INVOICE_SESSION_SECRET" \
        | systemd-creds encrypt --name=lnms_session_secret - "$ETC_DIR/session.secret" >/dev/null
    chown root:"$APP_USER" "$ETC_DIR/session.secret"
    chmod 0640 "$ETC_DIR/session.secret"
    rm -f /tmp/.lnms_secret.$$
fi

# 8. 启用服务
systemctl daemon-reload
systemctl enable lnms-invoice-web.service
systemctl enable lnms-invoice-billing.timer

echo "[install] 完成。"
echo "  - 启动 web:  systemctl start lnms-invoice-web"
echo "  - 触发账期: systemctl start lnms-invoice-billing.service"
echo "  - 看定时器: systemctl list-timers 'lnms*'"