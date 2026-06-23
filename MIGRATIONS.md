# Migrations

Schema lives in `migrations/`, applied by `mnestic_store::run_migrations` (sqlx). Files
are versioned `NNNN_name.sql` and run in order, each in its own transaction.

## The rule: append-only

`migrations/0001_init.sql` is **frozen**. Do not edit any migration that has shipped.

sqlx records a SHA-384 checksum of each migration file in the `_sqlx_migrations` table the
first time it runs. The migrations are embedded into the binary at build time, so on every
later start sqlx compares those compiled-in checksums against `_sqlx_migrations` and refuses
to proceed if a previously-applied one changed. So editing `0001` in place after any real
database has applied it makes `run_migrations` fail at startup with a checksum mismatch, and
there is no clean recovery short of hand-editing `_sqlx_migrations` on every deployment.

During early development `0001` was edited in place many times. That stops here. From now on
every schema change is a **new** file:

```
migrations/0002_add_x.sql
migrations/0003_backfill_y.sql
```

Shipped migrations are guarded by a checksum tripwire test (`shipped_migrations_are_frozen`
in `crates/mnestic-store/src/lib.rs`). If you edit one, that test fails and points back here.

The MIT relicense was a one-time, deliberate exception to the append-only rule: it rewrote
every migration's SPDX header to `MIT` and recomputed the frozen checksums in the same commit.
A database created before that relicense applied the old-header migrations, so its recorded
checksums no longer match; re-migrate from a fresh database (or update the `_sqlx_migrations`
checksums) after upgrading. Going forward the rule holds: do not edit a shipped migration, add
a new one. The project license is set by `LICENSE` and the workspace `license` field.

## Writing a new migration

- One concern per file. Keep it forward-only; we do not run `down` migrations.
- Each file runs in its own transaction, so do not add `BEGIN`/`COMMIT` (an inner `BEGIN`
  commits sqlx's surrounding transaction early). Operations that cannot run inside a
  transaction (for example `CREATE INDEX CONCURRENTLY`) need `-- no-transaction` on the
  first line so sqlx runs the file outside one.
- Additive changes (new tables, new nullable columns, new indexes) are safe online. A new
  `NOT NULL` column needs a default or a backfill step first.
- New tenant-scoped tables must `ENABLE` and `FORCE ROW LEVEL SECURITY` and carry the same
  `tenant_isolation` policy as the tables in `0001`, or they leak across tenants.
- Run `cargo test -p mnestic-store` after adding a file; the Dockerized tests apply the full
  chain against a fresh Postgres.
- Once the new file has shipped, pin it: append its `(version, sha384)` to the `FROZEN` list
  in `shipped_migrations_are_frozen`. Get the digest with `shasum -a 384 migrations/NNNN_*.sql`.
