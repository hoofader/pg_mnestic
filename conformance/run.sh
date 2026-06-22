#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# Runs the sdk-ts conformance suite against a throwaway, keyless pg_mnestic: builds the image,
# starts Postgres and the server (mock providers, no API keys), mints a tenant key, and drives
# the real supermemory SDK through the memory lifecycle. Cleans up on exit.
set -euo pipefail
cd "$(dirname "$0")/.."

PG="mnestic_conf_pg_$$"
PORT_PG=55440
PORT_API=8090
SERVE_PID=""
cleanup() {
  [ -n "$SERVE_PID" ] && kill "$SERVE_PID" 2>/dev/null || true
  docker rm -f "$PG" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "==> building mnestic-pg image (cached after first run)"
docker build -t mnestic-pg:dev docker/pg >/dev/null

echo "==> starting Postgres"
docker run -d --name "$PG" -e POSTGRES_PASSWORD=postgres -p "$PORT_PG:5432" mnestic-pg:dev >/dev/null
until docker exec "$PG" pg_isready -U postgres >/dev/null 2>&1; do sleep 1; done

export DATABASE_URL="postgres://postgres:postgres@localhost:$PORT_PG/postgres"
echo "==> building server + issue-key"
cargo build --features serve --bin serve
cargo build --features cli --bin issue-key

echo "==> starting server (MNESTIC_MOCK_PROVIDERS=1)"
MNESTIC_MOCK_PROVIDERS=1 MNESTIC_BIND="127.0.0.1:$PORT_API" \
  ./target/debug/serve >/tmp/mnestic_conf_serve.log 2>&1 &
SERVE_PID=$!
until curl -fsS "http://127.0.0.1:$PORT_API/health" >/dev/null 2>&1; do sleep 1; done

echo "==> minting a tenant key"
TOKEN=$(./target/debug/issue-key conformance local | sed -n 's/^token.*: //p')

echo "==> running sdk-ts conformance"
cd conformance/sdk-ts
npm ci --no-audit --no-fund >/dev/null
SUPERMEMORY_BASE_URL="http://127.0.0.1:$PORT_API" SUPERMEMORY_API_KEY="$TOKEN" node conformance.mjs
