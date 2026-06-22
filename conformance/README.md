# Conformance

Proves the official supermemory SDKs work as drop-in clients against a live pg_mnestic, by
driving the real SDK through the memory lifecycle (not the SDK's own mock-based tests). See
`../docs/05-clients.md` for the client compatibility surface.

## Run it

```bash
./run.sh
```

It builds the `mnestic-pg` image, starts a throwaway Postgres and a keyless server
(`MNESTIC_MOCK_PROVIDERS=1`), mints a tenant key, and runs `sdk-ts/conformance.mjs`. CI runs the
same suite (the `sdk conformance` job).

## sdk-ts

`sdk-ts/conformance.mjs` uses the npm `supermemory` package against `SUPERMEMORY_BASE_URL` +
`SUPERMEMORY_API_KEY`, asserting `add`, `search.memories`, `profile`, `search.documents`,
`memories.updateMemory` (versioning), and `memories.forget`.
