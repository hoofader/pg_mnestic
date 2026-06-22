# Deployment

## Postgres image

The database must carry `vector`, `pgcrypto`, `btree_gist`, and `pg_graphwright`. The first
three are in `pgvector/pgvector:pg16`; `pg_graphwright` (the knowledge-graph extension) is not
in any public registry, so run the image built by `docker/pg/Dockerfile` (pgvector plus
`pg_graphwright` from source) or an equivalent. See `docker/pg/README.md`. The integration
tests and CI use this same image, so the test database matches production.

Migration `0007` runs `CREATE EXTENSION pg_graphwright`, which Postgres only allows a
**superuser**, so the role that runs the migrations (`run_migrations` at startup, or whatever
applies them) must be a superuser, or the extension must be pre-created. The application's
own runtime role stays non-superuser and RLS-bound. The worker calls `graphwright.maintain()`
each cycle to resolve the graph; grant that role `EXECUTE` on the maintenance functions (see
the `pg_graphwright` README) when it is not the superuser.

## Knowledge graph extractor (GLiNER, optional)

The graph resolves entities from memory content. By default it uses `pg_graphwright`'s built-in
tokenizer, which is coarse (common words become entities). For real named-entity extraction, run
the GLiNER sidecar (`services/onnx-extractor`, a small Node service wrapping `graphwright-onnx`)
and point the extractor seam at it. It is opt-in: without it, the graph keeps resolving with the
built-in tokenizer.

```bash
docker build -t mnestic-onnx services/onnx-extractor
docker run -d --name onnx -p 8081:8081 mnestic-onnx   # fetches the GLiNER model on first start
```

Then, as a superuser, activate it for the database (migration `0009` already installed the
`mnestic_gliner_extract` function and the `http` extension it uses):

```sql
ALTER DATABASE mnestic SET mnestic.gliner_url   = 'http://onnx:8081/extract';
ALTER DATABASE mnestic SET graphwright.extractor = 'mnestic_gliner_extract';
```

The next `graphwright.maintain()` (the worker, each cycle) re-resolves through GLiNER. The sidecar
keeps memory text on your own infrastructure (no third-party call). The extractor runs in the
maintenance pass, off the write path, so a slow model never blocks a write. To revert, `RESET`
`graphwright.extractor` and the graph falls back to the built-in tokenizer.

## TLS is mandatory

The server (`mnestic-server`, `--features serve`) speaks plain HTTP. Auth is a bearer token
in the `Authorization` header, so any request that crosses a network without TLS leaks a
credential that grants full access to a tenant's memories. The server does **not** terminate
TLS itself by design; terminate it at a reverse proxy or load balancer in front of the app.

To stop an accidental plaintext exposure, the binary refuses to bind a non-loopback address
unless you assert that TLS is handled upstream:

- `MNESTIC_BIND` defaults to `127.0.0.1:8080`. A loopback bind always starts. It must be an
  `ip:port`; hostnames (including `localhost`) are rejected, since loopback cannot be checked
  before name resolution and resolution can be spoofed.
- A non-loopback bind (`0.0.0.0:8080`, a LAN/interface IP) fails to start unless you set
  `MNESTIC_TRUST_PROXY=1`, which is your statement that a proxy terminates TLS before traffic
  reaches this socket.

This is a guard, not encryption: setting `MNESTIC_TRUST_PROXY=1` without an actual TLS proxy
in front still exposes cleartext. The flag exists so exposing the port is a deliberate act.

## Recommended topologies

**Proxy on the same host (simplest).** Bind the app to loopback and point the proxy at it.
No flag needed.

```
client --TLS--> proxy (:443 on the host) --HTTP--> 127.0.0.1:8080 (mnestic-server)
```

**App on a private network behind a load balancer.** The LB or ingress terminates TLS; the
app listens on all interfaces inside a network nothing untrusted can reach. Set
`MNESTIC_TRUST_PROXY=1` and keep the app's port closed to the public internet at the firewall.

```
client --TLS--> LB (:443) --HTTP (private net)--> mnestic-server 0.0.0.0:8080
```

## Example: Caddy (automatic certificates)

```caddy
memory.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

## Example: nginx

```nginx
server {
    listen 443 ssl;
    server_name memory.example.com;

    ssl_certificate     /etc/ssl/memory.example.com/fullchain.pem;
    ssl_certificate_key /etc/ssl/memory.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}
```

## Health checks

`GET /health` returns `200 ok` with no auth and no database call. Use it for the proxy's
upstream check and for liveness/readiness probes. It does not assert database connectivity;
a deeper readiness check is a later roadmap item.

## Logging and metrics

The server logs through `tracing`. `RUST_LOG` sets levels (default `info`), and
`MNESTIC_LOG_FORMAT=json` switches to structured JSON for a log aggregator. Each request logs
one info line with method, path, status, and latency; the request span omits headers and
bodies, so bearer tokens and memory content never reach the logs.

Provider token spend is logged on the `mnestic::tokens` target, one event per OpenAI or
Anthropic call with `provider`, `model`, `op`, and input/output token counts. Route that
target to a metrics sink (or grep the JSON logs) to track cost per tenant or per operation.
Errors that produce a 500 log their cause at the error level; the HTTP body stays generic.

## Backups and recovery

All state is in Postgres, so backup and recovery are standard Postgres operations. There is no
separate state to back up. A managed Postgres (RDS, Cloud SQL, Crunchy, etc.) gives automated
snapshots plus point-in-time recovery; self-hosted, configure WAL archiving for PITR and take
periodic base backups.

- Set the retention window to your recovery and compliance needs. It also bounds how long an
  erased subject's data lingers in backups (see GDPR.md).
- A logical `pg_dump` is a useful periodic export, but PITR via WAL archiving is what bounds
  data loss (RPO) on a failure.
- Restore drill: practice a restore into a scratch instance and bring the server up against it
  (`DATABASE_URL` pointed at the restored database). Migrations are frozen and append-only
  (MIGRATIONS.md), so a restored database at an older schema is brought current by
  `run_migrations` on the next start. Restoring a backup from a newer schema than the binary
  knows about is the unsupported direction; deploy the matching (or newer) binary.
- After restoring a backup, re-apply any erasures (`purge-actor`) that were honored after the
  backup was taken, or the restore silently reintroduces deleted data.

## Rate limiting

Each API key gets a token bucket: `MNESTIC_RATE_LIMIT_PER_MIN` requests per minute (default
600), bursting up to that many, refilling steadily. A key over its budget gets `429`. The
limit is per key, so one tenant's traffic can't starve another's, and the check runs after the
key is authenticated, so unauthenticated requests never consume a bucket. Set
`MNESTIC_RATE_LIMIT_PER_MIN=0` to disable it.

The state is in-process. With more than one replica each counts independently, so the
effective limit is about (replicas x the configured rate); divide the per-key budget by the
replica count, or front the deployment with a proxy/gateway that enforces a shared limit, if
you need an exact cluster-wide cap. The bucket map holds one entry per active key; it is not
evicted, which is bounded by the number of issued keys.

## Async ingestion (the worker)

By default `POST /v4/memories` extracts and embeds synchronously, so the call blocks on model
latency. A client that wants a fast accept can send `"dreaming": "dynamic"`: the request
persists the raw source and returns `"status": "queued"` without calling the models. A
separate **worker** process then drains the queue out of band.

Run the worker alongside the server (same image, `--features serve`, same DATABASE_URL and
provider keys):

```bash
cargo run -p mnestic-server --features serve --bin worker
```

- `MNESTIC_WORKER_POLL_SECS` (default 5): idle poll interval. When a cycle finds work it loops
  immediately to drain the backlog; it sleeps only when nothing is pending.
- `MNESTIC_WORKER_LEASE_SECS` (default 300): how long a claimed source is reserved. Set it
  above the slowest extraction, or a still-running claim is reclaimed and the work is redone
  (the duplicate is dropped at commit, so this is wasted effort, not corruption).
- `MNESTIC_WORKER_BATCH` (default 16): max sources processed per tenant per cycle.
- Run one or many workers: claims use `FOR UPDATE SKIP LOCKED` plus the lease, so workers take
  distinct sources. A source whose extraction errors is logged and retried after its lease
  lapses. On SIGTERM/SIGINT the worker stops after the current cycle.

The worker shares `MNESTIC_DB_MAX_CONNECTIONS`, `MNESTIC_EXTRACT_MODEL`, and the log env with
the server. Without a worker running, `dreaming: dynamic` sources stay queued and never become
recallable, so deploy the worker whenever any client uses dynamic mode.

## Reranker

Set `MNESTIC_RERANK_URL` to a TEI (HuggingFace Text Embeddings Inference) rerank service to
turn on reranking. The service is self-hosted, so the candidate text stays on your
infrastructure. When set, recall pulls a larger candidate pool from hybrid retrieval and
reranks it against the query before returning the top `limit`; a per-request `rerank: false`
on `/v4/search` opts out for that call. Without the env var, recall ranks on RRF (vector +
lexical) plus recency and confidence only.

```bash
MNESTIC_RERANK_URL=http://reranker:8080 \
cargo run -p mnestic-server --features serve --bin serve
```

The reranker adds a network hop and its latency to every recall that uses it, so size the
service for the search QPS and keep it close to the server (same network).

## Encryption at rest

All persistent state is in Postgres, so encryption at rest is a database/storage-layer
concern, not an application one. The application stores `content` in cleartext columns
because recall searches it through a plaintext `tsvector` index and a semantic embedding;
encrypting the column would either leak the content through those derived artifacts or make
the row unsearchable. So encrypt the storage underneath the database:

- **Managed Postgres** (RDS, Cloud SQL, Azure Database): encryption at rest is transparent and
  KMS-backed. On Cloud SQL and Azure it is on by default; what you choose at creation is the
  key (a customer-managed key for rotation and revocation control). On RDS, encryption itself
  is set at instance creation and cannot be enabled in place, so create the instance encrypted
  and migrate into it. A customer-managed key is an availability dependency: lose or revoke it
  and the database becomes unreadable, so guard and back up the key, not just the data.
- **Self-hosted**: put the data directory, WAL, and any tablespaces on an encrypted block
  device (LUKS/dm-crypt) or filesystem. Community PostgreSQL has no built-in cluster TDE;
  transparent encryption inside the database needs a TDE-enabled distribution (for example
  EDB or Cybertec) if you require it there rather than under it.
- **Backups and WAL**: encrypt them too. Managed snapshots inherit the instance's encryption;
  `pg_dump` output and WAL archives must be written to encrypted storage. An unencrypted
  backup undoes at-rest encryption.
- **In transit** is separate and already required (see TLS above): the client to proxy and
  proxy to app to database links should all be TLS.

Application-level column encryption (the unused `content_enc`/`raw_enc` columns) is
deliberately not used: it conflicts with hybrid search as described. It remains an option for
a future, separate class of non-searchable sensitive memories, which would need a key strategy
and would keep those rows out of recall.

## Process lifecycle and tuning

- `MNESTIC_DB_MAX_CONNECTIONS` sizes the Postgres pool (default 16). Size it to the database's
  connection budget across all server replicas, not to request rate; a pooled connection that
  can't be acquired within 10s fails the request rather than hanging it.
- `MNESTIC_EXTRACT_MODEL` overrides the extraction model (default `claude-opus-4-8`). A
  cost-sensitive deployment can set a cheaper tier such as `claude-sonnet-4-6` (same 1M
  context window) or `claude-haiku-4-5` (cheapest, but a 200K window, so a large ingest that
  fits Opus or Sonnet can fail extraction on Haiku). The request schema and pipeline are
  unchanged; quality and price are the tradeoff. An invalid model id is not caught at
  startup; it surfaces as a provider error on every ingest, visible in the logs. Watch the
  `mnestic::tokens` metrics to compare cost. Embeddings are not configurable (the dimension is
  fixed in the schema).
- On `SIGTERM` (the orchestrator's stop signal) or `SIGINT`, the server stops accepting new
  connections and drains in-flight requests before exiting. There is no internal drain
  deadline yet, so the orchestrator's termination grace period is the only bound: set it above
  the longest expected request (extraction/embedding can take seconds) so a rolling deploy
  doesn't cut a request short, but keep it finite so a stuck request can't block the rollout.

## Scaling pgvector

Recall uses an HNSW index over the `halfvec(1536)` embeddings. The knobs that matter as the
data grows:

- **`hnsw.ef_search`** (default 40) trades recall for latency: higher widens the search and
  finds more of the true nearest neighbors, slower. The server does not set it per query, so
  set it on the role the app connects as and it applies to every recall:
  `ALTER ROLE <app_role> SET hnsw.ef_search = 100;`. Raise it if recall quality matters more
  than latency at your scale; measure with the request-latency logs.
- **Container-filtered recall** already turns on `hnsw.iterative_scan = 'relaxed_order'` per
  query: the `container_tags` filter is a residual predicate on the HNSW top-k, so without
  iterative scan a selective filter can return fewer than the requested limit while matching
  rows sit deeper in the index. The final ranking re-sorts, so the looser scan order is fine.
  A very selective filter on a large corpus can still under-return when iterative scan hits
  `hnsw.max_scan_tuples` (default 20,000); raise it (and `hnsw.scan_mem_multiplier`) at the
  role level to trade latency for completeness.
- **Index build parameters** (`m`, `ef_construction`) take pgvector's defaults: the `0001`
  index DDL does not set them. For a very large corpus, a higher `m`/`ef_construction` improves
  recall at the cost of build time and index size; changing them means a new migration that
  rebuilds the index with `WITH (...)`, not a runtime setting. Raise `maintenance_work_mem` on
  the session that builds or reindexes so the build stays in memory.
- Keep planner statistics fresh (autovacuum on, or periodic `ANALYZE`) so the planner picks
  good plans for the lexical (tsvector) and filtered (tenant/actor/container) paths as row
  counts grow.

## Notes

- In-process TLS (rustls in the app) is intentionally out of scope. Terminating at the edge
  keeps certificate rotation, HSTS, and ALPN with the proxy where ops already manage them.
- Do not expose the app port to the public internet even with `MNESTIC_TRUST_PROXY=1`; the
  proxy is the only intended ingress.
