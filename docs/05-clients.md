# Supermemory clients on pg_mnestic

pg_mnestic serves the supermemory wire API (see `04-compatibility.md`), so the official
supermemory SDKs and any tool built on them work against it unchanged. You point the client at
your pg_mnestic server and authenticate with a tenant key instead of a supermemory cloud key.

## Pointing a client at pg_mnestic

Every supermemory SDK takes a base URL and an API key. Set the base URL to your pg_mnestic
server and the key to a tenant key minted by `issue-key`.

TypeScript (`supermemory` on npm):

```ts
import Supermemory from 'supermemory';

const client = new Supermemory({
  baseURL: 'https://memory.your-host.example', // your pg_mnestic server
  apiKey: process.env.SUPERMEMORY_API_KEY,     // a tenant key from issue-key
});
```

Or by environment, with no code change: `SUPERMEMORY_BASE_URL=https://memory.your-host.example`
and `SUPERMEMORY_API_KEY=sm_...`. The Python SDK is the same with `base_url=` / `api_key=` (or
the same env vars).

Mint a key:

```bash
issue-key <tenant-external-id> [label]
# prints: token (shown once, store it now): sm_...
```

## What works

These SDK methods map to endpoints pg_mnestic implements:

| SDK call | Endpoint | Notes |
|---|---|---|
| `client.add(...)` | `POST /v3/documents` | Stores a memory (`taskType: memory`, default) or a chunked document (`taskType: superrag`). |
| `client.search.memories(...)` | `POST /v4/search` | `q`, `containerTag`, `limit`, `filters`, `searchMode`, `threshold`, `include`, `rerank`, `aggregate`. |
| `client.search.documents(...)` / `.execute(...)` | `POST /v3/search` | Document-chunk search, grouped per document. |
| `client.profile(...)` | `POST /v4/profile` | `profile.static` / `profile.dynamic`, plus optional query-scoped results. |
| `client.memories.updateMemory(...)` | `PATCH /v4/memories` | Versioned supersede (`newContent`); needs `containerTag`. |
| `client.memories.forget(...)` | `DELETE /v4/memories` | By `id` or `content`; needs `containerTag`. |

Anything built on these (an agent framework, an MCP wrapper, a script) inherits the
compatibility. pg_mnestic also speaks MCP directly at `/mcp` (tools `memory`, `recall`,
`memory-graph`, ...), so an MCP client can use it without the SDK.

## What does not work

The SaaS-platform surface of the SDK is out of scope and returns an error (see `04-compatibility.md`):
`client.connections.*` (OAuth/connectors), `client.settings.*`, document CRUD/list/batch/file
upload (`client.documents.update/list/delete/get/batchAdd/uploadFile/...`), and scoped keys.
pg_mnestic is the self-hosted memory engine, not the hosted product.

## Conformance

The supermemory SDK repos ship their own tests, but those run against a `prism` mock generated
from the OpenAPI spec, so they check the SDK against the spec, not against a live backend.
Running them "against pg_mnestic" is therefore not meaningful.

Instead, `conformance/sdk-ts` drives the real `supermemory` SDK against a live pg_mnestic and
asserts the memory lifecycle (add, search, profile, document search, versioned update, forget).
Run it locally:

```bash
conformance/run.sh        # builds the image, starts a keyless server, runs the SDK suite
```

CI runs the same suite on every push (the `sdk conformance` job), so SDK compatibility is a
gate, not a claim. The server there runs with `MNESTIC_MOCK_PROVIDERS=1` (network-free mock
embedder/extractor) so it needs no API keys; that mode is for conformance and local demos, never
production.
