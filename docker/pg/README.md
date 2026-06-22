# mnestic-pg image

The Postgres image the engine runs against: `pgvector/pgvector:pg16` plus `pg_graphwright`
(the RLS-aware knowledge-graph extension), built from source via pgrx. The integration tests
(testcontainers), CI, and a production deploy all use it, so the database carries the
extensions the engine expects (`vector`, `pgcrypto`, `btree_gist`, `pg_graphwright`).

`pg_graphwright` is not in any public registry, so this image must be built locally before
the Dockerized tests can run. Build it once and the tests reuse it:

```bash
docker build -t mnestic-pg:dev docker/pg
```

The build compiles a Rust/pgrx extension, so the first build is slow (several minutes); it is
cached afterward. `PG_GRAPHWRIGHT_REF` pins the extension commit; bump it to upgrade.

In production, run this image (or one built the same way) as your Postgres, and grant the
maintenance functions to your operator role (see the `pg_graphwright` README). The engine's
`worker` calls `graphwright.maintain()` to resolve the graph off the write path.
