#!/usr/bin/env bash
set -euo pipefail

# 1. 获取最新配置与 Admin API Key
PG_PASS=$(grep -oP "password:\s*&postgres_password\s*'\K[^']+" /opt/codex-proxy-rs/deploy/config.yaml || true)
if [ -z "$PG_PASS" ]; then
    echo "无法从 config.yaml 解析 postgres password" >&2
    exit 1
fi

ADMIN_KEY=$(docker exec -e PGPASSWORD="$PG_PASS" codex-proxy-rs-postgres-1 psql -h 127.0.0.1 -U codex_proxy -d codex_proxy -t -c "SELECT admin_api_key FROM runtime_settings;" | tr -d '[:space:]')
if [ -z "$ADMIN_KEY" ]; then
    echo "无法获取 admin_api_key" >&2
    exit 1
fi

# 2. 查询当前已启用的 codex-candy-eval 扩展目标 (instanceId/artifactSha256/revision)
EXT_INFO=$(curl -s -k -H "x-api-key: $ADMIN_KEY" https://127.0.0.1:47675/api/admin/plugins/extensions | python3 -c '
import sys, json
try:
    data = json.load(sys.stdin)
    for v in data.get("data", []):
        t = v.get("target", {})
        inst = t.get("instanceId")
        sha = t.get("artifactSha256")
        rev = t.get("revision")
        if inst and sha and rev is not None:
            print(inst + "/" + sha + "/" + str(rev))
            break
except Exception:
    pass
')

if [ -z "$EXT_INFO" ]; then
    echo "未发现活动的 codex-candy-eval 扩展实例" >&2
    exit 1
fi

# 3. 触发一键巡检
URL="https://127.0.0.1:47675/api/admin/plugins/extensions/${EXT_INFO}/api/api/inspect"
RESP=$(curl -s -k -X POST -H "Content-Type: application/json" -H "x-api-key: $ADMIN_KEY" -d "{}" "$URL")

echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] CPR Candy Inspect: $RESP"
