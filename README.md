# Mnestic

Mnestic is a Postgres-native long-term memory engine. The customer's own Postgres is the single
datastore: vector + lexical search, tenant isolation via RLS, bitemporal correctness, and an
RLS-aware knowledge graph. Use it as an embedded Rust library (`mnestic-engine`) or run the
server (`mnestic-server`), which speaks the supermemory wire API and MCP, so the official
supermemory SDKs work against it unchanged (see [`docs/05-clients.md`](docs/05-clients.md)).

Design docs are the source of truth, see [`docs/`](docs/):

- [`docs/01-high-level-plan.md`](docs/01-high-level-plan.md)
- [`docs/02-architecture.md`](docs/02-architecture.md)
- [`docs/03-low-level-design.md`](docs/03-low-level-design.md)
- [`docs/04-compatibility.md`](docs/04-compatibility.md) - the supermemory wire surface, what is and isn't implemented.
- [`docs/05-clients.md`](docs/05-clients.md) - pointing supermemory SDK clients at pg_mnestic.

Operational guides: [`DEPLOYMENT.md`](DEPLOYMENT.md), [`MIGRATIONS.md`](MIGRATIONS.md),
[`SECRETS.md`](SECRETS.md), [`GDPR.md`](GDPR.md).

## Crates

- `mnestic-core` - domain types, provider traits, and the pure resolution logic (`decide`). No DB, no network.
- `mnestic-store` - Postgres access over `sqlx`, embedded migrations, RLS policies, and the SQL for recall and the metadata-filter builder.
- `mnestic-model` - provider impls. The mock impls are always built and network-free; the cloud providers sit behind features: `openai`, `anthropic`, and `rerank` (a self-hosted TEI reranker).
- `mnestic-engine` - the orchestration library: the write path (extract, embed, resolve), recall, the supersession chain, relation classification, and the graph maintenance hooks.
- `mnestic-server` - the REST + MCP server (`serve` feature) and the operator CLIs (`cli` feature): `serve`, `worker`, `issue-key`, `list-keys`, `revoke-key`, `export-actor`, `purge-actor`.
- `mnestic-eval` - a memorybench-style evaluation harness: ingest a benchmark's conversations, answer its questions from recall, and grade the answers (accuracy, recall latency, context size). The `real` feature adds the Claude-backed providers and the `memorybench` binary.

Still deferred (in the LLD module layout, not built): `mnestic-py` (PyO3 wheel) and
`mnestic-node` (napi-rs npm package).

## Database

Postgres 16 with `pgvector` (`halfvec(1536)`), `pg_graphwright` (the knowledge graph), and
`pgsql-http` (the optional GLiNER extractor bridge). These are not all in any public image, so
the tests, CI, and a deploy run the image built by [`docker/pg/Dockerfile`](docker/pg/Dockerfile)
(pgvector plus the extensions, built from source). Build it once:

```sh
docker build -t mnestic-pg:dev docker/pg
```

`migrations/` holds the SQL schema and RLS policies. Shipped migrations are frozen (see
`MIGRATIONS.md`).

## Running tests

The integration tests start a throwaway `mnestic-pg:dev` container via testcontainers, so build
that image first (above) and have Docker running. Then:

```sh
cargo test --workspace --all-features
```

`--all-features` so the feature-gated provider, `serve`, `rerank`, and CLI code is exercised. The
live cloud-provider tests are `#[ignore]`, so no API keys are needed.

The supermemory SDK conformance suite drives the real `supermemory` npm SDK against a live
pg_mnestic; CI runs it on every push and you can run it locally:

```sh
conformance/run.sh
```

## Running the server

```sh
cargo run --features serve --bin serve     # needs DATABASE_URL and provider keys
cargo run --features cli --bin issue-key <tenant-external-id>   # mint a tenant key
```

Set `MNESTIC_MOCK_PROVIDERS=1` to run keyless with mock providers (conformance and local demos
only, never production). See `DEPLOYMENT.md` for TLS, the worker, the reranker, and the graph
extractor.

## License

AGPL-3.0-only. See [`LICENSE`](LICENSE).
