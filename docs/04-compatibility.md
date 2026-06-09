# Mnestic: supermemory Compatibility

> **Document 4 of 4** · Status: Draft v0.2 · Date: 2026-06-07
> Companion documents: `01-high-level-plan.md`, `02-architecture.md`, `03-low-level-design.md`

This document is the contract for running the existing supermemory open-source shells against Mnestic. It is a **Phase 2 deliverable**, but it is written now so the core schema (LLD §2) and SDK (LLD §7) are designed to map onto it without a later migration.

The goal is narrow and concrete: a user installs `claude-supermemory` (or points an MCP client) at Mnestic by changing a base URL and an API key, and it works. We do not fork the shells.

Everything below is grounded in supermemory's published OpenAPI (`api.supermemory.ai/v3/openapi`), the SDK packages (`supermemory` on npm and PyPI), and the `supermemoryai/*` repos. Items marked *(unverified)* were listed in their reference index but not confirmed field-by-field; close them by pulling the full OpenAPI before implementing.

---

## 1. What the target shells actually call

### `claude-supermemory` (Claude Code plugin)
- It is a Claude Code **plugin** using lifecycle hooks (`SessionStart` injects context, `Stop` saves the session), not an MCP server.
- It depends on the `supermemory` npm SDK and instantiates the client with `baseURL = process.env.SUPERMEMORY_API_URL || 'https://api.supermemory.ai'`.
- Env it reads: `SUPERMEMORY_CC_API_KEY` (required, `sm_...`), `SUPERMEMORY_API_URL` (base-URL override, this is our seam), `SUPERMEMORY_DEBUG` (optional).
- SDK calls it makes: `client.add({ content, containerTag, metadata, customId, entityContext })`, `client.search.memories({ q, containerTag, limit })`, `client.profile({ containerTag, q })`.

So the REST subset we must serve for this shell is small:

| Endpoint | Used for |
|---|---|
| `POST /v3/documents` | `client.add` (save a memory/session summary) |
| `POST /v4/search` | `client.search.memories` (recall) |
| `POST /v4/profile` | `client.profile` (SessionStart context) |
| `GET /v3/session` | API-key validation (returns `userId`/`email`/`name`) |
| `GET /v3/projects` *(source-derived, not in public docs)* | list container tags |

### MCP clients (Claude Desktop, Cursor, etc.)
The supermemory MCP server exposes these tools. Mnestic's MCP server speaks the same names so clients work unchanged:

| Tool | Input (abridged) | Behavior |
|---|---|---|
| `memory` | `{ content (<=200000), action: "save"\|"forget", containerTag? }` | Save (full pipeline) or forget. |
| `recall` | `{ query (<=1000), includeProfile=true, containerTag? }` | Hybrid search; returns ranked memories, optionally the profile. |
| `listProjects` | `{ refresh=false }` | List container tags (projects). |
| `whoAmI` | `{}` | Returns `userId`, `email`, `name`, session info. |
| `memory-graph` *(optional)* | `{ containerTag? }` | Summary + `structuredContent { documents[], totalCount }`. |

Plus the resources `supermemory://profile` (markdown profile) and `supermemory://projects` (JSON), and a prompt named `context` (a system message injecting the profile). Note: `context` is a **prompt**, not a tool. All tool returns use the standard MCP shape `{ content: [{ type: "text", text }], isError? }`.

MCP auth: a bearer token where `token.startsWith("sm_")`, validated by `GET /v3/session`. OAuth is a later option; key-based is enough for Phase 2.

---

## 2. The scoping mapping (the part their model hides and ours must make explicit)

supermemory has **one** scoping key: `containerTag` (a string, pattern `^[a-zA-Z0-9_:-]+$`, max 100, colon-hierarchical, e.g. `org:123:user:456`). It doubles as user id, project id, and org id. The field is sometimes singular (`containerTag`) and sometimes a plural array (`containerTags`); the compat layer must accept both.

Mnestic has **three** orthogonal concepts: `tenant_id` (RLS security boundary), `actor_id` (who the memory is about), and `container_tags[]` (convenience filters). The compat layer resolves the mapping:

```
API key (sm_...)  ──►  tenant_id        (one tenant per key; the security boundary)
containerTag      ──►  actor_id         (default: the whole tag is the actor)
containerTag with ':' ──► parsed        (e.g. "org:123:user:456": org/project -> container_tags[],
                                          trailing user segment -> actor_id)
containerTags[]   ──►  container_tags[] (filters within the tenant)
```

Rules:
- The API key is the only thing that can change `tenant_id`. A request can never reach another tenant's rows, because RLS keys on the session GUC the engine sets from the resolved key. This is the isolation upgrade over supermemory: their `containerTag` is a filter; our `tenant_id` is enforced by the database.
- The parse convention for colon-hierarchical tags is configurable. The default treats the last `user:<id>` (or final segment) as `actor_id` and the rest as `container_tags[]`. A deployment that uses `containerTag` purely as a user id gets `actor_id = containerTag` and no tags.
- Round-trip: responses echo the original `containerTag` string the caller sent, reconstructed from `(actor_id, container_tags)` so the shells see what they expect.

---

## 3. Field map: Mnestic schema ↔ supermemory wire

The LLD §2.4 columns are named so this map is mechanical.

| supermemory wire field | Mnestic column | Notes |
|---|---|---|
| `content` | `mnestic_memory.content` | Canonical natural-language memory. |
| `customId` | `mnestic_memory.custom_id` | Idempotency/dedup, unique per tenant. |
| `isStatic` | `mnestic_memory.is_static` | Durable trait. |
| `forgetAfter` | `mnestic_memory.forget_after` | Ephemeral expiry. |
| `forgetReason` | `mnestic_memory.forget_reason` | |
| `temporalContext.documentDate` | `mnestic_memory.document_date` | |
| `temporalContext.eventDate` | `mnestic_memory.event_date` | |
| `metadata` | `mnestic_memory.metadata` (jsonb) | Filterable. |
| `entityContext` | per-container extraction prompt | Stored in container settings. |
| `containerTag` / `containerTags` | `actor_id` + `container_tags[]` | Via §2 mapping. |
| memory `version` | supersession chain (`supersedes_id`, `is_latest`) | Their "new version" = our new row superseding the prior. |
| search `memory` field | rendered `content` of a hit | |
| search `chunk` field | `mnestic_chunk.content` of a hit | Document path. |
| search `similarity` | normalized vector score | |
| `searchMode: hybrid\|memories\|documents` | recall `mode` | Same three modes (LLD §5.4). |
| `threshold` (default 0.6) | recall similarity cutoff | |
| `rerank` (bool) | recall reranker toggle | |
| `taskType: memory\|superrag` | ingest routing | `memory` -> extraction pipeline; `superrag` -> document/chunk path. |
| `dreaming: instant\|dynamic` | extraction sync mode | `instant` = synchronous extract; `dynamic` = enqueue + extract out-of-band. |

The `filters` grammar (nested AND/OR up to 5 levels; leaf `{ key, value, filterType, numericOperator, negate, ignoreCase }`) maps onto SQL predicates over `metadata` and typed columns.

---

## 4. REST contract (the subset we serve in Phase 2)

Base path mirrors theirs: `/v3` (documents) and `/v4` (memories). Auth: `Authorization: Bearer sm_...`. Accept both `containerTag` and `containerTags`.

```
POST /v3/documents      { content, containerTag?, containerTags?, customId?, metadata?,
                          entityContext?, taskType?, dreaming? }  -> { id, status }
POST /v3/search         { q, containerTag?/containerTags?, filters?, limit?, ... }  -> document hits
POST /v4/search         { q, containerTag?, searchMode?, limit?, threshold?, rerank?, filters? }
                        -> { results: [{ id, memory, chunk?, similarity, metadata, updatedAt, version }],
                             timing, total }
POST /v4/profile        { containerTag?, q?, threshold?, filters? }  -> profile (+ optional results)
POST /v4/memories       { memories: [{ content, isStatic?, metadata?, forgetAfter?, forgetReason?,
                          temporalContext? }], containerTag }  -> { documentId, memories: [...] }
POST /v4/conversations  { conversationId, messages: [...], containerTags?, metadata? }
GET  /v3/session        -> { userId, email, name }            # key validation for MCP/plugins
GET  /v3/projects       -> [ container tags ]                 # (unverified path)
```

Endpoints we deliberately skip in Phase 2 (not called by the target shells): connectors (`/v3/connections/*`), file upload, batch/bulk document ops, container-tag merge, the Memory Router proxy. They can follow if a shell needs them.

> The Memory Router (the transparent LLM proxy that scopes by the `x-sm-user-id` header, base-URL form `.../v3/https://api.openai.com/v1`) is a **separate product** from the memory CRUD API. It is out of scope. If we ever add it, `x-sm-user-id` maps to `actor_id` the same way `containerTag` does.

---

## 5. How a user flips a shell onto Mnestic

```bash
# claude-supermemory against a local Mnestic compat server
export SUPERMEMORY_API_URL="http://localhost:8080"
export SUPERMEMORY_CC_API_KEY="sm_localtenant_xxx"   # resolves tenant_id on our side
```

```jsonc
// MCP client config pointing at Mnestic's MCP endpoint
{ "url": "http://localhost:8080/mcp", "headers": { "Authorization": "Bearer sm_localtenant_xxx" } }
```

No code change in the shells. The `sm_` key prefix is required because their auth path checks `startsWith("sm_")`; Mnestic issues keys in that format and resolves them to a tenant.

---

## 6. Open items to close before building the compat layer

- Pull the full OpenAPI JSON and the SDK `api.md` files to confirm the *(unverified)* paths: `/v3/projects`, document update/delete verbs, the exact `/v4/search` response field set, and the `client.memories.*` method names.
- Decide the colon-hierarchy parse convention for `containerTag` (§2) and make it a documented setting.
- Decide whether `taskType: superrag` is supported at all in Phase 2 or returns a clear "not supported" so shells degrade predictably.
- Pin the supermemory API generation we target (the `/v3`+`/v4` split as of 2026-06) and record it, since their surface evolves.

---

*This surface is intentionally a thin translation over the core engine. If it ever needs the core schema to change, that is a signal the core design drifted from §3; fix the core, not the translation.*
