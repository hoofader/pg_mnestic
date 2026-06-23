# Changelog

## 0.1.0

First release. A self-hosted, drop-in supermemory-compatible long-term memory engine over the
customer's own Postgres. `0.x`: the wire API is conformance-gated and stable, but the library and
schema may still change.

### supermemory compatibility
- Serves the supermemory wire API: `POST /v3/documents` (add), `POST /v3/search` (document
  search), `POST /v4/search` (memory search), `POST /v4/profile`, `PATCH`/`DELETE /v4/memories`
  (versioned update, forget).
- `searchMode` (`memories`/`documents`/`hybrid`), `threshold`, metadata `filters` pushed into
  SQL, `include.forgottenMemories`, `taskType` (`memory`/`superrag`).
- `aggregate`: per-result `context` of `updates` (supersession chain), `extends`/`derives`
  (LLM-classified relations), and `documents`.
- MCP server at `/mcp` (`memory`, `recall`, `listProjects`, `whoAmI`, `memory-graph`).
- A conformance suite drives the real `supermemory` npm SDK against a live instance; CI runs it
  on every push.

### Engine
- Hybrid recall (pgvector + lexical), RRF fusion, recency decay on event time with as-of.
- Bitemporal storage, supersession chain (versioned edits), dedup, single-valued attributes.
- Tenant isolation via Postgres RLS (FORCE), keyed on a per-transaction GUC.
- Async ingestion (`dreaming: dynamic`) drained by a worker, off the request path; relation
  classification runs post-commit on both the sync and async paths (best-effort).
- Optional self-hosted reranker (HuggingFace TEI); recall falls back to retrieval order if it is
  unavailable.
- RLS-aware knowledge graph (`pg_graphwright`): entities resolved from memory content feed
  `aggregate.related` and the `memory-graph` MCP tool; an optional GLiNER (`graphwright-onnx`)
  extractor sharpens entity quality.
- Encryption-at-rest support at the storage layer.

### Operations
- One-command Docker quickstart (`quickstart.sh`) and `docker-compose.yml` (Postgres + server +
  worker), with optional `rerank` and `graph` profiles.
- Custom Postgres image (`pgvector` + `pg_graphwright` + `pgsql-http`).
- Operator CLIs: `issue-key`, `list-keys`, `revoke-key`, `export-actor`, `purge-actor`. GDPR
  export/purge.
- Migrations are frozen (checksum-pinned) once shipped.
- A memorybench-style evaluation harness and a supermemory-vs-Mnestic comparison harness.

### Not yet
- Recall-quality parity vs supermemory (the ~85% LongMemEval target) is not yet benchmarked;
  the comparison harness exists but has not been run for published numbers.
- `mnestic-py` (PyO3) and `mnestic-node` (napi-rs) bindings are not built.
- `pg_graphwright` is an early-stage extension dependency on a core path.
