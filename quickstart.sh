#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
#
# One-command local pg_mnestic: build and start Postgres, the server, and the worker, then mint
# a tenant key and print how to use it. Re-runnable.
#
# Optional sidecars:
#   --rerank   also start the TEI reranker and point recall at it
#   --graph    also start the GLiNER extractor and activate it for the knowledge graph
set -euo pipefail
cd "$(dirname "$0")"

PROFILES=()
WANT_RERANK=0
WANT_GRAPH=0
for arg in "$@"; do
  case "$arg" in
    --rerank) WANT_RERANK=1; PROFILES+=(--profile rerank) ;;
    --graph)  WANT_GRAPH=1;  PROFILES+=(--profile graph) ;;
    *) echo "unknown flag: $arg (use --rerank and/or --graph)"; exit 2 ;;
  esac
done

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

# The reranker URL is set only when its sidecar is enabled, so recall never points at a
# reranker that is not running.
[ "$WANT_RERANK" = 1 ] && export MNESTIC_RERANK_URL="http://rerank:80"

echo "==> building images and starting the stack"
echo "    (the first build compiles the Postgres extensions and the server in release; it is slow)"
docker compose "${PROFILES[@]}" up --build -d

echo "==> waiting for the server to be ready"
until curl -fsS http://localhost:8080/health >/dev/null 2>&1; do sleep 2; done

if [ "$WANT_RERANK" = 1 ]; then
  echo "==> waiting for the reranker (downloads the model on first start, can take minutes)"
  until curl -fsS http://localhost:8082/health >/dev/null 2>&1; do sleep 3; done
fi

if [ "$WANT_GRAPH" = 1 ]; then
  echo "==> activating the GLiNER graph extractor"
  # Set the seam at the database level so every new connection (incl. the worker's) sees it,
  # then restart the worker so its pool reconnects. The model loads in the background; until it
  # is ready the worker's maintain logs a warning and retries, which is harmless.
  docker compose "${PROFILES[@]}" exec -T db psql -U postgres -v ON_ERROR_STOP=1 -c \
    "ALTER DATABASE postgres SET mnestic.gliner_url = 'http://onnx:8081/extract'; \
     ALTER DATABASE postgres SET graphwright.extractor = 'mnestic_gliner_extract';"
  docker compose "${PROFILES[@]}" restart worker
fi

echo "==> minting a tenant key (tenant 'me')"
TOKEN=$(docker compose "${PROFILES[@]}" exec -T server issue-key me | sed -n 's/^token.*: //p')

cat <<EOF

pg_mnestic is up at http://localhost:8080  (tenant: me)
API key: ${TOKEN}
$([ "$WANT_RERANK" = 1 ] && echo "Reranker: on (TEI at http://localhost:8082)")
$([ "$WANT_GRAPH" = 1 ] && echo "Graph extractor: GLiNER (onnx at http://localhost:8083); the model loads in the background")

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
