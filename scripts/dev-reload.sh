#!/usr/bin/env bash
# scripts/dev-reload.sh — 本地开发:重编二进制 + 重启 serve
#
# 重要:askama 是编译时模板嵌入,改 templates/*.html 后只跑 cargo build --lib
# 是不够的,必须 cargo build --bin lnms-invoice,否则新模板不会生效。
#
# 用法:
#   ./scripts/dev-reload.sh           # build + 重启 serve
#   ./scripts/dev-reload.sh --no-run  # 只 build,不启动

set -euo pipefail

cd "$(dirname "$0")/.."

RUN_AFTER=1
if [[ "${1:-}" == "--no-run" ]]; then
  RUN_AFTER=0
fi

export PATH="$HOME/.cargo/bin:$PATH"

echo "▶ cargo build --bin lnms-invoice (askama 模板必须重新编译进二进制)"
cargo build --bin lnms-invoice

if [[ $RUN_AFTER -eq 0 ]]; then
  echo "✓ build 完成(--no-run)"
  exit 0
fi

echo "▶ 停掉旧 serve"
pkill -f "target/debug/lnms-invoice serve" 2>/dev/null || true
# 等旧进程真退、端口真释放
for _ in $(seq 1 10); do
  if ! pgrep -f "target/debug/lnms-invoice serve" > /dev/null 2>&1; then
    break
  fi
  sleep 0.3
done

# dev 环境��量(不要在生产用这套 SECRET)
export LNMS_INVOICE_CONFIG="${LNMS_INVOICE_CONFIG:-config/dev.toml}"
export LNMS_INVOICE_TEMPLATE_ROOT="${LNMS_INVOICE_TEMPLATE_ROOT:-/tmp/lnms-invoice/templates}"
export LNMS_INVOICE_OUTPUT_ROOT="${LNMS_INVOICE_OUTPUT_ROOT:-/tmp/lnms-invoice/output}"
export LNMS_INVOICE_DB="${LNMS_INVOICE_DB:-/tmp/lnms-invoice/dev.db}"
export LNMS_INVOICE_SESSION_SECRET="${LNMS_INVOICE_SESSION_SECRET:-dev-only-not-real-secret-just-for-local-preview-do-not-use-in-prod-32bytes}"

mkdir -p "$LNMS_INVOICE_TEMPLATE_ROOT" "$LNMS_INVOICE_OUTPUT_ROOT" "$(dirname "$LNMS_INVOICE_DB")"

echo "▶ 启动 serve (后台)"
nohup ./target/debug/lnms-invoice serve > /tmp/lnms-invoice-dev.log 2>&1 &
SERVER_PID=$!

PORT=$(awk -F'=' '/^[[:space:]]*port[[:space:]]*=/ {gsub(/[[:space:]]/, "", $2); print $2; exit}' "$LNMS_INVOICE_CONFIG")
PORT="${PORT:-18765}"

# 给服务 2 秒 bind,然后用 --max-time 限时探测(避免 curl hang)
sleep 2
if curl -sS --max-time 3 -o /dev/null "http://127.0.0.1:${PORT}/login" 2>/dev/null; then
  echo "✓ serve 已在 http://127.0.0.1:${PORT}/login"
  echo "  admin    / admin123"
  echo "  operator / admin123"
  echo "  日志: tail -f /tmp/lnms-invoice-dev.log"
else
  echo "✗ serve 启动失败,看日志:"
  tail -30 /tmp/lnms-invoice-dev.log
  exit 1
fi