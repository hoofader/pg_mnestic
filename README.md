# Mnestic

Mnestic is a Postgres-native long-term memory engine. The customer's own
Postgres is the single datastore (vector + lexical search, tenant isolation via
RLS, bitemporal correctness); everything above it is a library they embed.

Design docs are the source of truth, see [`docs/`](docs/):

- [`docs/01-high-level-plan.md`](docs/01-high-level-plan.md)
- [`docs/02-architecture.md`](docs/02-architecture.md)
- [`docs/03-low-level-design.md`](docs/03-low-level-design.md)
- [`docs/04-compatibility.md`](docs/04-compatibility.md)

## Phase 0 crates

- `crates/mnestic-core` - domain types, provider traits, and the pure
  resolution logic (`decide`). No DB, no network.
- `crates/mnestic-model` - provider impls. Mock impls are always built and
  network-free; OpenAI impls sit behind the `openai` feature.
- `crates/mnestic-store` - Postgres access over `sqlx`, embedded migrations,
  and the Dockerized integration test.

`migrations/` holds the SQL schema and RLS policies. Target is Postgres 16 with
pgvector 0.8 (`halfvec(1536)`).

## Deferred crates (later phases)

Not scaffolded yet. They appear in the LLD module layout and land in later
phases:

- `mnestic-py` - PyO3 bindings (the flagship Python wheel).
- `mnestic-node` - napi-rs bindings (npm package).
- `mnestic-mcp` - MCP server with supermemory-shaped tools.
- `mnestic-compat` - Phase 2 supermemory-wire REST subset.
- `pg_mnestic` - Phase 3 optional pgrx extension (accelerators).

TODO: scaffold the above in their respective phases.

## Running tests

The unit tests need nothing external:

```sh
cargo test
```

The store integration test starts a `pgvector/pgvector:pg16` container via
testcontainers, so Docker must be running. It runs as part of the same command.
The default build is offline; the OpenAI provider impls only compile with the
feature on:

```sh
cargo build -p mnestic-model --features openai
```

## License

Apache-2.0. See [`LICENSE`](LICENSE).
