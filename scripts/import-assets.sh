#!/usr/bin/env bash
# 资产导入脚本(阶段 7)
#
# 用法:
#   sudo ./scripts/import-assets.sh <customers.json>
#
# 作用:
# 1. 拷贝 customers.json 到 /etc/lnms-invoice/customers.json(root:APP_USER, 0640)
# 2. 调用 import-customers 把客户/端口/费率写入 SQLite
# 3. 提示运维用 set-instance-token 为每个 LNMS 实例设 token
#    (token 走 stdin,**绝不**写到磁盘明文)
#
# 安全约束:
# - 永远不接受命令行传 token
# - 不修改 customers.json 内任何 token 相关字段
# - 数据库里 api_token_enc 占位是 "env:<ENV_VAR_NAME>",set-instance-token
#   后会被覆写为真实字节

set -euo pipefail

APP_USER="lnms-invoice"
APP_HOME="/var/lib/lnms-invoice"
DB="$APP_HOME/db/lnms-invoice.sqlite"
ETC_DIR="/etc/lnms-invoice"

if [[ $EUID -ne 0 ]]; then
    echo "请用 sudo 跑" >&2
    exit 1
fi
if [[ $# -ne 1 ]]; then
    echo "用法: $0 <customers.json>" >&2
    exit 2
fi
SRC="$1"
if [[ ! -r "$SRC" ]]; then
    echo "找不到或不可读: $SRC" >&2
    exit 2
fi

# 1. 拷贝
echo "[import-assets] 拷贝 $SRC 到 $ETC_DIR/customers.json..."
install -m 0640 -o root -g "$APP_USER" "$SRC" "$ETC_DIR/customers.json"

# 2. import-customers
echo "[import-assets] 写入 SQLite..."
sudo -u "$APP_USER" -- \
    LNMS_INVOICE_DB="$DB" \
    LNMS_INVOICE_CONFIG="$ETC_DIR/lnms-invoice.toml" \
    /usr/local/bin/import-customers "$DB" "$ETC_DIR/customers.json"

# 3. 提示 token 设置
INSTANCES=$(grep -o '"name": *"[^"]*"' "$ETC_DIR/customers.json" \
    | head -20 \
    | sed -E 's/.*"name": *"([^"]+)".*/\1/')

cat <<EOF

[import-assets] 客户/端口/费率已写入 $DB

下一步:为每个 LibreNMS 实例设置 API token(token 从 stdin 读,不会写磁盘):

EOF
echo "$INSTANCES" | while read -r name; do
    [[ -z "$name" ]] && continue
    cat <<INST
  echo -n '<token>' | sudo /usr/local/bin/set-instance-token $DB '$name'

INST
done

cat <<EOF

注意:
  - token 写入数据库的 librenms_instances.api_token_enc 列(plaintext,阶段 8 加密)
  - 建议生产用 systemd-creds encrypt 把 token 加密后存 /etc/lnms-invoice/,而不是入库
EOF