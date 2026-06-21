# Mnestic: supermemory Compatibility

> **Document 4 of 4** · Status: v0.3, core memory surface implemented · Date: 2026-06-20
> Companion documents: `01-high-level-plan.md`, `02-architecture.md`, `03-low-level-design.md`

This document is the contract for running the existing supermemory open-source shells against Mnestic. It started as a Phase 2 deliverable; the core memory surface is now built and verified against the official SDK. See §4a for the per-endpoint status.

The goal is narrow and concrete: a user installs `claude-supermemory` (or points an MCP client) at Mnestic by changing a base URL and an API key, and it works. We do not fork the shells.

The authoritative contract is the official TypeScript SDK `github.com/supermemoryai/sdk-ts` (Stainless-generated from supermemory's OpenAPI), cross-checked against `supermemoryai/python-sdk` and the Rust DTOs in `supermemoryai/smfs`. The published docs prose (`supermemory.ai/docs`) is summarized and was imprecise on response shapes, so the SDK types are the source of truth. §4a records the verified shapes and what was built.

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

The shapes below are the verified `sdk-ts` ones. Responses keep an additive `containerTag` echo (the SDK ignores unknown keys), which the table omits.

```
POST   /v3/documents     { content, containerTag?, containerTags?, customId?, metadata?,
                           title?, uri? }  -> { id, status, chunks }
POST   /v3/search        { q, containerTag?/containerTags?, limit?, filters? }
                         -> { results: [{ documentId, chunks: [{ content, isRelevant, score }],
                              score, title, type, metadata, createdAt, updatedAt }], timing, total }
POST   /v4/search        { q, containerTag?, limit?, filters? }
                         -> { results: [{ id, memory, similarity, updatedAt, metadata }], timing, total }
POST   /v4/profile       { containerTag?, q?, limit?, filters? }
                         -> { profile: { static: [...], dynamic: [...] },
                              searchResults?: { results: [...], timing, total } }
POST   /v4/memories      { content, containerTag?/containerTags?, customId?, metadata?, dreaming? }
                         -> { id, containerTag, status }
DELETE /v4/memories      { containerTag, id?, content?, reason? }  -> { id, forgotten }
PATCH  /v4/memories      { containerTag, newContent, id?, metadata?, forgetAfter?, forgetReason?,
                           temporalContext? }
                         -> { id, createdAt, memory, version, parentMemoryId, rootMemoryId,
                              forgetAfter, forgetReason }
POST   /v4/conversations { conversationId, messages: [...], containerTag?/containerTags?, metadata? }
                         -> { conversationId, id, status }
GET    /v3/session       -> { userId, email, name }     # key validation for MCP/plugins
GET    /v3/projects      -> [ container tags ]           # convenience; not in the SDK
```

Note: the SDK's `client.add` adds content via `POST /v3/documents` (not `/v4/memories`), and its `/v4/memories` surface is forget (`DELETE`) and update (`PATCH`) only. Mnestic also serves `POST /v4/memories` as a direct single-memory add (supermemory's "Create Memories Directly"). `/v4/conversations`, `/v3/session`, and `/v3/projects` are not in `sdk-ts`; they are additive conveniences the shells use.

> The Memory Router (the transparent LLM proxy that scopes by the `x-sm-user-id` header, base-URL form `.../v3/https://api.openai.com/v1`) is a **separate product** from the memory CRUD API. It is out of scope. If we ever add it, `x-sm-user-id` maps to `actor_id` the same way `containerTag` does.

---

## 4a. Implementation status (verified against `sdk-ts`)

Every SDK method that targets the memory core is served and asserted by an integration test that checks the response deserializes into the SDK's shape.

| SDK method | Endpoint | Status |
|---|---|---|
| `client.add` | `POST /v3/documents` | Done. Stores `metadata`; returns `{ id, status, chunks }`. |
| `client.search.documents` / `.execute` | `POST /v3/search` | Done. Per-document grouping with `chunks[]`; `limit` bounds documents. |
| `client.search.memories` | `POST /v4/search` | Done. `results`/`timing`/`total`, per-result `metadata`. |
| `client.profile` | `POST /v4/profile` | Done. `profile.static`/`dynamic` + optional `searchResults`. |
| `client.memories.forget` | `DELETE /v4/memories` | Done. By `id` (actor-scoped) or `content`. |
| `client.memories.updateMemory` | `PATCH /v4/memories` | Done. Versioned supersede; carries the memory class forward. |
| `filters` (the three read endpoints) | n/a | Done. OR/AND tree over `metadata`. Pushed into SQL on the memory path (`/v4/search` memories, `/v4/profile`); Rust over the candidate pool on the document path. |
| `searchMode` / `threshold` (`/v4/search`) | n/a | Done. `memories`/`documents`/`hybrid`; `threshold` is a relative cutoff (our score is fused RRF, not a 0-1 cosine). |

Out of scope, by design (the SaaS platform surface, not the self-hosted memory engine):

- Connectors / OAuth (`/v3/connections/*`), and the per-provider sync/import/resources.
- File upload and storage (`/v3/documents/file`, `/file-url`), and document CRUD/list/batch/bulk/chunks/processing (`GET`/`PATCH`/`DELETE /v3/documents/*`, `/v3/documents/list`, etc.).
- Organization settings (`/v3/settings`), container-tag settings/merge/delete (`/v3/container-tags/*`), and scoped API keys (`/v3/auth/scoped-key`). Mnestic issues tenant-scoped keys through its own CLI.
- The Memory Router LLM proxy (a separate product, see §4).

Known limitations:

- `filters` on the document path (`/v3/search`, the document half of `hybrid`) is still evaluated in Rust over an over-fetched pool (best-effort at extreme scale), because document metadata lives on a joined table; the memory path is exact in SQL. Two narrow semantic divergences between the two: an `array_contains` leaf only matches string array elements in SQL (the Rust path also matches a number rendered as a string), and a `metadata`-equality leaf compares the stored text, so a JSON number with trailing zeros (`1.50`) does not string-equal `"1.5"`.
- `rerank`, `aggregate`, and `include.forgottenMemories` on `/v4/search` are accepted (ignored): `rerank` needs a reranker model wired into the server, and recall is always over latest active memories.
- `taskType` on `/v3/documents` is accepted and ignored (`superrag` routing is not branched); `entityContext` is accepted and ignored (documents are not run through memory extraction).
- `threshold` is a cutoff relative to the strongest hit for the query, not supermemory's absolute score, because our `similarity` is a fused RRF value.

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

## 6. Resolved, and what remains

Resolved (verified against `sdk-ts`, 2026-06-20):

- The SDK surface was confirmed field-by-field. `client.memories.*` is `forget` (`DELETE /v4/memories`) and `updateMemory` (`PATCH /v4/memories`); both are served. The `/v4/search` and `/v4/profile` response shapes are the verified ones in §4. `/v3/session` and `/v3/projects` are not in the SDK; kept as conveniences.
- The colon-hierarchy parse default is shipped (`parse_container_tag`): the trailing `user:<id>` (or final segment) is the actor, the rest are container tags.

What remains (tracked in §4a "Known limitations"):

- Async-path `metadata` (needs a `metadata` column on `mnestic_source` and worker threading).
- Honoring `searchMode`/`threshold`/`rerank` rather than accepting-and-ignoring them.
- `taskType: superrag` routing (today documents take the chunk path and memories the extraction path; `superrag` is not branched).
- Pushing `filters` into SQL for exact results past the over-fetch pool, and accepting the `"true"`/`"false"` string forms of `negate`/`ignoreCase`.

---

*This surface is intentionally a thin translation over the core engine. If it ever needs the core schema to change, that is a signal the core design drifted from §3; fix the core, not the translation.*
