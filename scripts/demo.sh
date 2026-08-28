#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Andrew Stevens
#
# Narrated end-to-end demo of Tollgate, zero infrastructure required.
# Boots `tollgate demo` (in-memory, mock provider), issues a demo key with a
# small per-key budget, sends requests until the budget hard-stops, and shows
# the spend ledger. No Postgres, Redis, or cloud credentials needed.
#
# Usage:  ./scripts/demo.sh
set -euo pipefail

PORT="${PORT:-8088}"
REQUESTS="${REQUESTS:-5}"
BODY='{"model":"demo","prompt":"hello tollgate","max_output_tokens":1000}'

echo "Building tollgate..."
cargo build --quiet

echo "Starting demo server on 127.0.0.1:${PORT} ..."
TOLLGATE_HTTP__BIND="127.0.0.1:${PORT}" cargo run --quiet -- demo >/tmp/tollgate-demo.out 2>/tmp/tollgate-demo.err &
SERVER_PID=$!
trap 'kill "${SERVER_PID}" 2>/dev/null || true' EXIT

# Wait for it to come up.
for _ in $(seq 1 30); do
  curl -sf "localhost:${PORT}/healthz" >/dev/null 2>&1 && break
  sleep 0.3
done

echo
sed -n '1,20p' /tmp/tollgate-demo.out
KEY="$(grep -oE 'tgk_[0-9a-f]+_[0-9a-f]+' /tmp/tollgate-demo.out | head -1)"

echo
echo "=== Sending ${REQUESTS} requests (per-key budget hard-stops after ~3) ==="
for i in $(seq 1 "${REQUESTS}"); do
  code="$(curl -s -o /tmp/tollgate-demo-resp.json -w '%{http_code}' \
    "localhost:${PORT}/v1/mock/generate" \
    -H "x-tollgate-key: ${KEY}" \
    -H 'content-type: application/json' \
    -d "${BODY}")"
  echo "--- request ${i} -> HTTP ${code} ---"
  if command -v jq >/dev/null 2>&1; then jq . /tmp/tollgate-demo-resp.json; else cat /tmp/tollgate-demo-resp.json; echo; fi
done

echo
echo "=== /admin/budgets (admin endpoints require the key) ==="
curl -s -H "x-tollgate-key: ${KEY}" "localhost:${PORT}/admin/budgets"; echo
echo
echo "=== /admin/usage ==="
curl -s -H "x-tollgate-key: ${KEY}" "localhost:${PORT}/admin/usage"; echo
echo
echo "=== /metrics (Prometheus) ==="
curl -s "localhost:${PORT}/metrics"; echo
echo
echo "Demo complete. Server shutting down."
