# Mnestic: Architecture

> **Document 2 of 4** · Status: Draft v0.2 · Date: 2026-06-07
> Companion documents: `01-high-level-plan.md`, `03-low-level-design.md`, `04-compatibility.md`

---

## 1. Overview

Mnestic is structured as four layers. Reading top to bottom: client applications call a thin SDK; the SDK calls a Rust memory engine; the engine reads and writes the user's own Postgres, which does the heavy lifting (vector + lexical search, tenant isolation, temporal correctness).

```
┌─────────────────────────────────────────────────────────────────┐
│  Clients:  AI apps & agents  ·  MCP clients  ·  Frameworks        │
│            (Phase 2: existing supermemory shells via base-URL)    │
└───────────────────────────────┬─────────────────────────────────┘
                                 │
┌───────────────────────────────▼─────────────────────────────────┐
│  SDK layer:  mnestic (Python · PyO3)  ·  mnestic (TS · napi)     │
│  Phase 2 compat layer: supermemory-wire REST subset + MCP server │
└───────────────────────────────┬─────────────────────────────────┘
                                 │
┌───────────────────────────────▼─────────────────────────────────┐
│  Memory engine (Rust core):  ingest · extract · resolve ·        │
│                              profile · recall                     │
│            └── calls external Model APIs (LLM + embeddings) ──┐   │
└───────────────────────────────┬──────────────────────────────┼──┘
                                 │                              │
┌───────────────────────────────▼─────────────────────────────────┐
│  Your Postgres (single datastore, runs anywhere):                │
│    pgvector (HNSW)  ·  pg_search (BM25)  ·  RLS  ·  pgcrypto      │
│    optional: pg_mnestic extension (Rust/pgrx accelerator)        │
└───────────────────────────────────────────────────────────────────┘
```

The defining choice: the bottom layer is **the customer's database**, not infrastructure Mnestic operates. Everything above it is a library they embed. The Phase 2 compatibility layer is an optional translation extension point, not a change to the core.

## 2. Components

### 2.1 SDK layer
Thin, idiomatic clients in Python and TypeScript. They do not contain memory logic; they marshal calls to the Rust core and shape results into native objects. The Python SDK is the flagship (PyO3/maturin); the TypeScript SDK reuses the same core via napi-rs rather than reimplementing it. The native surface (`add`, `search`, `profile`) is close to supermemory's by intent, which keeps switching costs low and makes the Phase 2 compat layer a thin map rather than a rewrite (see `04-compatibility.md`).

### 2.2 Memory engine (Rust core)
The single source of truth for behavior. Responsibilities:
- **Ingest**: persist raw items to `source` atomically.
- **Extract**: call an LLM to turn text into candidate memories (entity-centric natural-language `content`, plus optional structured subject/attribute/value, confidence, temporal hints); embed them.
- **Resolve**: dedup, detect contradictions, apply supersession (`supersedes_id` + `is_latest`) and expiry. Additive facts coexist; only single-valued attributes are mutually exclusive.
- **Profile**: maintain per-actor static/dynamic profiles incrementally.
- **Recall**: run hybrid retrieval, rerank, and rank; return memories + profile.

The engine is provider-agnostic: LLM and embedding access sit behind traits so models are swappable. It speaks to Postgres over a connection pool (`sqlx` or `tokio-postgres`).

### 2.3 Postgres layer
Where correctness and performance live:
- `pgvector` with HNSW indexes for semantic search.
- `pg_search` (ParadeDB) for true BM25 lexical search where it can be installed; a `tsvector` path is the portable floor (see §9 for the honest quality gap).
- RLS policies for per-tenant isolation.
- `pgcrypto` and/or app-layer envelope encryption for sensitive payloads.
- Bitemporal columns plus an `EXCLUDE` constraint that fires only for attributes declared single-valued.

### 2.4 MCP server
Exposes the engine to MCP-compatible clients (Claude Desktop, Cursor, Claude Code, etc.). It speaks the supermemory tool names so those clients work unchanged: `memory` (save/forget), `recall` (hybrid search, optional profile), `listProjects`, `whoAmI`, and an optional `memory-graph`. It also serves the resources `supermemory://profile` and `supermemory://projects` and a `context` prompt (a prompt, not a tool). Key-based auth (`sm_...`) validated via `GET /v3/session`. Full schemas in `04-compatibility.md` §1.

### 2.5 Compatibility layer (Phase 2)
A wire-compatible REST subset (the `/v3`+`/v4` endpoints the target shells call) plus the MCP server above, run as a service so existing supermemory shells point at Mnestic with a base-URL change. It is a translation over the core, with no logic of its own. See `04-compatibility.md`.

### 2.6 Framework adapters
Drop-in wrappers for LangChain, LlamaIndex, Vercel AI SDK, OpenAI Agents SDK, and CrewAI. Each is a thin shim over the SDK.

### 2.7 Connectors (later)
Source-sync modules (GitHub first). Each connector writes into `source`/`document` and triggers the ingest pipeline. Built as an extensible interface so the community can add more.

### 2.8 pg_mnestic extension (optional)
A Rust/pgrx extension that pushes hot paths (ranking fusion, decay scoring, batch resolution) into the database for users who can install custom extensions. **Never required**; every feature has a pure-SQL path.

## 3. Deployment topologies

In order of simplicity:

1. **Embedded library (default).** The Rust core runs in-process inside the application via the Python/TS binding. The app connects to Postgres directly. Simplest, no extra service. Best for single-app deployments and the primary Phase 0/1 target.
2. **Service.** The engine runs as a standalone process exposing an HTTP/gRPC API, shared by multiple apps/languages. This is also the host for the Phase 2 compatibility layer (the supermemory-wire REST subset + MCP server live here).
3. **In-database acceleration.** The `pg_mnestic` extension co-locates hot logic with the data. Layered on top of either topology above.

These are not mutually exclusive. A deployment can start embedded and adopt the service or extension as scale or compatibility needs demand.

## 4. Technology choices & rationale

| Choice | Why |
|---|---|
| Rust core | Performance; one implementation for both SDKs; aligns with existing pgrx/ParadeDB experience. |
| PyO3 / maturin | Proven path for Rust-backed Python wheels (cf. `pydantic-core`, `tokenizers`). |
| napi-rs | Same core reused for Node without a rewrite. |
| pgvector + HNSW | The portable, ubiquitous vector index on Postgres; available on all major managed providers. |
| pg_search (BM25) | Real lexical search inside Postgres, enabling true in-DB hybrid retrieval rather than app-layer stitching. Optional, because managed providers often lack it. |
| pgcrypto / envelope | Column/payload encryption without leaving Postgres; envelope keeps keys in a KMS. |
| Bitemporal + EXCLUDE | Temporal correctness as a database invariant for single-valued attributes, not fragile application logic. |
| RLS | Tenant isolation enforced by the engine of record, holds even when the application has a bug. |
| Cross-encoder reranker | The piece that closes the gap between RRF and the recall bar; a swappable model behind a trait. |

## 5. Data flow

### 5.1 Write path (`add`)
1. SDK calls engine `add(content, container_tag, actor_id)`.
2. Engine opens a transaction, sets the tenant context (`SET LOCAL`), writes raw to `source`. An optional `custom_id` makes the write idempotent per tenant.
3. **Extract:** the LLM produces candidate memories: entity-centric `content`, optional `(subject, attribute, value)`, `confidence`, temporal hints (`valid_time`, `document_date`, `event_date`), `is_static`, and `forget_after` for ephemeral facts. Each candidate is embedded.
4. **Resolve:** match each candidate against existing latest memories for the actor (vector + lexical +, when present, attribute key). Dedup duplicates. For a contradicting single-valued attribute, close the prior row's `valid_time`, set it `superseded` / `is_latest = false`, and insert the new row with `supersedes_id`. Additive facts simply insert as new latest rows. The `EXCLUDE` constraint guards only single-valued attributes, so multi-valued facts (languages, skills, interests) coexist.
5. **Profile:** update the actor's static/dynamic profile incrementally from the rows touched in this transaction, not by full recomputation. Full rebuild is an out-of-band job, so write latency stays bounded.
6. Commit. Everything above is atomic; a failure rolls back cleanly with no partial state and no external system to reconcile.

For high ingest throughput, extraction can run out-of-band (enqueue raw with a `needs_extraction` flag, extract asynchronously). This mirrors supermemory's `dreaming: instant|dynamic` modes (see `04-compatibility.md`).

### 5.2 Read path (`recall` / `profile`)
1. SDK calls engine `recall(q, container_tag, mode)`.
2. Engine sets tenant context and runs hybrid retrieval:
   - optional query rewrite/expansion,
   - vector similarity (pgvector cosine) over latest memories and/or document chunks,
   - lexical relevance (BM25 via `pg_search`, or `tsvector` on the floor),
   - fused via reciprocal-rank fusion, reranked by a cross-encoder, then re-scored by recency/confidence decay,
   - RLS filters to the tenant automatically, with `WHERE is_latest AND status = 'active' AND (forget_after IS NULL OR forget_after > now())`.
3. Returns ranked memories plus the cached profile. The profile read is a single indexed lookup (the ~50 ms target); reranking adds latency only when enabled.

## 6. Multi-tenancy & security architecture

- **Isolation:** every row carries `tenant_id`; every table enables RLS with a policy keyed on a session setting (`mnestic.tenant_id`). The engine sets this per transaction from the resolved API key. An application bug that forgets a filter cannot leak across tenants; the database refuses.
- **Scoping within a tenant:** `actor_id` (who the memory is about) and `container_tags[]` (work/personal, per-client, per-repo) are filters, distinct from `tenant_id` (security). The compat layer maps supermemory's single `containerTag` onto this triple (see `04-compatibility.md` §2); the API key is the only thing that selects a tenant.
- **Encryption:** sensitive memory `content` and source `raw` can be envelope-encrypted (per-tenant data key from a KMS) and stored as ciphertext; the engine decrypts on read. **Embeddings and the resolution keys (`subject`/`attribute`) remain plaintext** so similarity search and supersession work. This is a documented trade-off: embeddings can leak information about source text (inversion attacks). RLS controls who reads a row; encryption controls what a leaked row reveals.
- **Key management:** keys never live in Postgres. The engine fetches data keys from a KMS; only ciphertext and key references are stored.

## 7. Scalability & performance

- **Vector search** scales with HNSW; tune `m` / `ef_search` per workload. Partition large tenants by `tenant_id` if needed.
- **Write throughput** is bounded by the extraction LLM call, not the database. Extraction can be batched and made asynchronous when ingest latency matters more than immediate recall freshness.
- **Profiles are precomputed** and updated incrementally, so the hot read path avoids recomputation.
- **Connection pooling** (PgBouncer-friendly); the engine holds a bounded pool.
- The optional `pg_mnestic` extension removes round trips for ranking/decay on very hot paths.

## 8. Observability

- Structured logs and tracing (OpenTelemetry) from the engine: per-stage timings (extract/resolve/rerank/recall), token usage, cache hit rates.
- Because state is in SQL, operators get observability for free: counts of latest vs superseded vs expired memories, per-tenant volumes, profile freshness, all queryable directly.
- Eval-harness metrics (memorybench MemScore) tracked over time as a first-class signal.

## 9. Portability matrix

| Capability | Vanilla PG + pgvector | + pg_search | + pg_mnestic |
|---|---|---|---|
| Semantic recall | yes | yes | yes (faster) |
| Lexical hybrid | `tsvector` floor (no true IDF) | true BM25 | true BM25 |
| RLS isolation | yes | yes | yes |
| Payload encryption | yes (pgcrypto/app) | yes | yes |
| In-DB ranking/decay | app-layer | app-layer | yes (in-DB) |
| Runs on managed PG (RDS, Cloud SQL) | yes | provider-dependent | needs extension install |

The leftmost column is the floor: Mnestic must be fully functional there. Be honest about one thing: `tsvector` + `ts_rank` is not BM25. It lacks proper IDF weighting, so lexical quality on managed Postgres without `pg_search` is genuinely lower, not merely a drop-in fallback. The reranker recovers some of that gap. Everything to the right of the floor is progressive enhancement.

---

*Next: see `03-low-level-design.md` for concrete schema DDL, RLS policies, pipeline algorithms, and API contracts, and `04-compatibility.md` for the supermemory-compatible surface.*
