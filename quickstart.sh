#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only
#
# One-command local pg_mnestic: build and start Postgres, the server, and the worker, then mint
# a tenant key and print how to use it. Re-runnable.
set -euo pipefail
cd "$(dirname "$0")"

# Load .env (provider keys) if present, to pick real vs mock providers.
set -a
[ -f .env ] && . ./.env
set +a

if [ -z "${OPENAI_API_KEY:-}" ] || [ -z "${ANTHROPIC_API_KEY:-}" ]; then
  echo "No OPENAI_API_KEY/ANTHROPIC_API_KEY found (set them in .env for real memory)."
  echo "Starting with mock providers: the API works, but recall is non-semantic."
  export MNESTIC_MOCK_PROVIDERS=1
else
  echo "Provider keys found: using real embeddings + extraction."
fi

echo "==> building images and starting the stack"
echo "    (the first build compiles the Postgres extensions and the server in release; it is slow)"
docker compose up --build -d

echo "==> waiting for the server to be ready"
until curl -fsS http://localhost:8080/health >/dev/null 2>&1; do sleep 2; done

echo "==> minting a tenant key (tenant 'me')"
TOKEN=$(docker compose exec -T server issue-key me | sed -n 's/^token.*: //p')

cat <<EOF

pg_mnestic is up at http://localhost:8080  (tenant: me)
API key: ${TOKEN}

Try it:
  curl -s localhost:8080/v3/documents -H "authorization: Bearer ${TOKEN}" \\
    -H 'content-type: application/json' \\
    -d '{"content":"I prefer window seats.","containerTag":"me"}'

  curl -s localhost:8080/v4/search -H "authorization: Bearer ${TOKEN}" \\
    -H 'content-type: application/json' \\
    -d '{"q":"seat preference","containerTag":"me"}'

Or point a supermemory SDK / MCP client at http://localhost:8080 (see docs/05-clients.md).
Logs:  docker compose logs -f server
Stop:  docker compose down            (add -v to also wipe the data volume)
EOF
