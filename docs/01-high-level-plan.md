# Mnestic: High-Level Plan

> **Document 1 of 4** · Status: Draft v0.2 · Date: 2026-06-07
> Companion documents: `02-architecture.md`, `03-low-level-design.md`, `04-compatibility.md`

---

## 1. Vision

AI agents forget everything between sessions. The current memory products (supermemory, mem0, Letta, Zep) solve this, but they do it inside *their* infrastructure, behind *their* API. Your users' memories become data you rent, not data you own.

**Mnestic is a long-term memory engine for AI agents that lives entirely inside the user's own Postgres.** Same category as the incumbents, opposite trust model.

One honest correction up front, because it shapes the whole positioning: supermemory's production backend is reportedly Postgres + pgvector already, with Cloudflare Workers in front and a fact-graph on top. The differentiator is therefore not "Postgres vs a vector DB." It is *whose* Postgres. Memory sits in the database you already run, in the same transaction as your application data, under your RLS, in your region. The incumbents put the same substrate behind a multi-tenant hosted boundary and a per-call bill.

The one-line positioning:

> Persistent memory for AI agents, in the Postgres you already run. Not another hosted memory service, and not their multi-tenant database pretending to be yours.

## 2. Scope

### In scope
- A memory engine: extract facts from conversations and documents, resolve contradictions, expire stale facts, recall the right context on demand.
- Auto-maintained user/agent profiles (stable facts plus recent context).
- Hybrid retrieval: semantic (pgvector) plus lexical (BM25 via `pg_search`, with a `tsvector` floor) fused in-database, reranked.
- Strict multi-tenant isolation via Postgres Row-Level Security (RLS).
- Optional at-rest encryption of sensitive memory payloads.
- A Python-first SDK, with a TypeScript SDK as a fast-follow.
- An MCP server and adapters for the major agent frameworks.
- **A supermemory-compatible layer (Phase 2): a wire-compatible REST subset plus a supermemory-shaped MCP server, so the existing open-source shells (`claude-supermemory`, MCP clients) run against Mnestic with only a base-URL change.** See `04-compatibility.md`.
- An optional `pg_mnestic` Postgres extension (Rust/pgrx) for accelerating hot paths.

### Explicitly out of scope (initially)
- Building or forking a consumer-facing app or browser extension. We do not own that surface and do not want to. The compatibility layer lets the *existing* shells point at us instead; we do not reimplement them.
- A broad connector catalog. One or two connectors deep, not ten shallow.
- A hosted/managed offering. The thesis is "runs in your Postgres." A managed option can come later; it is not the product.
- Multi-modal ingestion (PDF/OCR/video) at launch. Added once the text path is strong.

### Resolving the "reuse their shells" question
We do not build the shells and we do not fork them. We become a drop-in backend. `claude-supermemory` is a Claude Code plugin that calls the `supermemory` SDK and already reads a `SUPERMEMORY_API_URL` base-URL override; the SDKs are generated from a published OpenAPI spec. Serving the subset of endpoints those shells call, with `sm_`-prefixed keys, makes them work unchanged. This is a Phase 2 deliverable. The core schema is designed now to map cleanly to that wire format, so Phase 2 is a translation layer over a stable core, not a migration.

## 3. Competitive landscape & the wedge

| Product | Model | Language | Where memory lives |
|---|---|---|---|
| supermemory | Hosted API | TS (Cloudflare) + PG | Their hosted Postgres + pgvector |
| mem0 | Library + hosted | Python | Pluggable (incl. pgvector) |
| Letta (MemGPT) | Framework + server | Python | Their server |
| Zep | Service | Go core + SDKs | Their service (temporal KG) |
| **Mnestic** | **Library / embeddable** | **Rust core, Python/TS SDK** | **Your own Postgres** |

The incumbents compete on recall quality. supermemory reports ~85% on LongMemEval-S at sub-300ms retrieval. Their viral "99%" is an 8-prompt ensemble with any-of-8 scoring and a dozen frontier-model calls per query, explicitly not the production engine, so treat ~85% as the real bar. Recall quality is a function of extraction, resolution, and reranking, not storage. **We do not win the benchmark by being Postgres-native.** The honest claim is at-parity recall with decisively better ownership, operations, security, and cost. Parity is earned in the pipeline, not granted by the datastore (see Risks).

### Why this wedge is defensible
1. **Your database, your transaction.** Memories sit in the same database, and the same commit, as application data. No sync pipeline, atomic writes, and you can JOIN business tables against memory.
2. **Real isolation.** RLS is a boundary the database enforces, not a `containerTag` filter the application has to remember to pass. This is the strongest honest differentiator, because the incumbents scope by a request field the caller must get right every time.
3. **SQL-native transparency.** Audit, query, and run BI over memory directly instead of trusting a black-box endpoint.
4. **Data residency.** Your cloud, your region, your compliance regime. Closes regulated-industry deals the hosted players cannot.
5. **Cost.** No second datastore and no per-call memory-service bill.
6. **Open source.** No API lock-in.
7. **Drop-in for existing clients (Phase 2).** The shells people already use can point at your database without code changes.

## 4. Target users

- **Primary:** developers building AI agents/apps who already run Postgres and don't want a second stateful system or a third-party data processor.
- **Secondary:** platform/infra teams with data-residency or compliance constraints (fintech, health, gov, EU).
- **Tertiary:** the self-hosting / open-source crowd who reject hosted-only tools on principle.

Representative use cases: a coding agent that remembers a developer's stack and preferences; a customer-support agent with per-account memory under strict tenant isolation; a personal-assistant product that must keep user data in-region.

## 5. Strategic principles

1. **Postgres does the work.** Push memory logic into the database where it belongs (temporal correctness, isolation, search). The app layer orchestrates; it does not re-implement the database.
2. **Python is the front door.** That is where AI builders evaluate and adopt. TypeScript is a planned second door, not an afterthought.
3. **Rust is the engine, not the interface.** One core, exposed through thin bindings.
4. **Runs anywhere by default.** The core must work on any managed Postgres with `pgvector`. The `pg_mnestic` extension is an optional accelerator, never a hard dependency.
5. **Measure recall from day one.** An evaluation harness is core infrastructure, not a Phase-3 nicety. Run supermemory's open `memorybench` so numbers are comparable.
6. **Design for compatibility now, ship it later.** The core schema and SDK are shaped so the Phase 2 supermemory-compatible layer is a thin translation, not a rewrite. We do not let library-first become "compat is impossible later."

## 6. Success metrics

- **Adoption:** GitHub stars, PyPI installs/week, reported production deployments.
- **Activation:** time-to-first-recall in the quickstart (target under 5 minutes).
- **Quality:** LongMemEval-S / LoCoMo / ConvoMem run through supermemory's open `memorybench` harness (MemScore = accuracy / latency / context tokens) so comparisons are apples-to-apples. Target: within the parity band of mem0 by end of Phase 1, on a credible path to ~85% LongMemEval-S.
- **Performance:** p50/p95 recall latency on a reference dataset. Target sub-300ms recall, profile fetch under ~50 ms.
- **Portability:** verified on vanilla Postgres + pgvector on the major managed providers.

## 7. Roadmap

### Phase 0: MVP (weeks)
Core schema + RLS, content-primary memory model, `add` / `search` / `recall`, pgvector semantic search, single-call LLM fact extraction, Python SDK. Runs on any Postgres. Goal: prove the loop end-to-end.

### Phase 1: Parity core
Supersession + expiry via `supersedes_id` / `is_latest`, `static`/`dynamic` profiles, hybrid search (pgvector + `pg_search` BM25, `tsvector` floor) fused with RRF, **a cross-encoder reranker, query rewriting, and entity/attribute resolution** (the pieces that actually move recall toward the bar), a native MCP server, TypeScript SDK, **and the evaluation harness with first published memorybench numbers.** Parity is claimed only once the harness shows it.

### Phase 2: Compatibility + ecosystem
The supermemory-compatible REST subset plus a supermemory-shaped MCP server, so `claude-supermemory` and MCP clients run against Mnestic via a base-URL change (see `04-compatibility.md`). Framework adapters (LangChain, LlamaIndex, Vercel AI SDK, OpenAI Agents SDK, CrewAI). Connectors one at a time, GitHub first. Begin multi-modal ingestion (PDF/OCR) once text quality is strong.

### Phase 3: Moat
`pg_mnestic` Rust/pgrx extension for in-database acceleration, row-level/payload encryption, hardened self-host story, analytics surfaces over memory.

## 8. Risks & mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| "Postgres-native" is not a moat by itself | High | supermemory already runs on Postgres. Lead with ownership, RLS, and residency; earn recall parity in the pipeline, not the datastore. |
| Recall quality lags incumbents | High | Eval harness in Phase 1 via `memorybench`; invest in extraction/resolution/rerank and the attribute ontology; publish numbers honestly. |
| Attribute/entity normalization is the real crux | High | Supersession only fires when the same fact resolves to the same key. Use embedding-based attribute matching plus a small ontology; measure in the harness. |
| Connector/multi-modal surface area is bottomless | High | Do not compete on breadth. Go deep on the self-host/Postgres wedge; grow connectors with demand. |
| Wire-compat drift | Medium | Their API has two generations (`/v3` documents, `/v4` memories) and evolves. Pin to the subset the target shells actually call; track their OpenAPI. |
| Managed Postgres can't install extensions | Medium | Core is pure SQL + pgvector; `pg_mnestic` and `pg_search` are optional. Never gate features on an extension. |
| Embedding-inversion leaks from plaintext vectors | Medium | Document in threat model; offer payload encryption; note embeddings are not zero-risk. |
| Bitemporal modeling adds complexity | Low/Med | Encapsulate in the engine; expose a simple API; lean on Postgres constraints for the single-valued cases. |

## 9. Open & settled decisions

Settled in v0.2:
- **Compatibility posture:** library-first. The supermemory-compatible layer is Phase 2, targeting `claude-supermemory` and the MCP server first. The core schema is designed to map cleanly now (see `04-compatibility.md`).
- **Memory model:** content-primary hybrid. Natural-language `content` is canonical; optional structured subject/attribute/value fields are populated when extraction is clean; supersession via `supersedes_id` + `is_latest`. See LLD §2.4.

Still open (settle before/early in implementation):
1. **Embedding provider & dimension**: recommend a pluggable trait with a pinned default (OpenAI `text-embedding-3-small`, 1536 dims, stored as `halfvec` for space). The dimension is templated at install so larger models (e.g. `text-embedding-3-large` at 3072) and Matryoshka truncation are supported.
2. **Extraction model**: which LLM(s) to target for extraction, and how to keep it swappable behind the `Extractor` trait.
3. **License**: MIT vs Apache-2.0 vs source-available for any future hosted offering. Note: reusing the supermemory shells needs no fork (base-URL override), so their license does not constrain ours.
4. **Naming lock**: `mnestic` (brand/SDK) + `pg_mnestic` (extension). Reserve the domain and GitHub org before public commit.

---

*Next: see `02-architecture.md` for the system architecture, `03-low-level-design.md` for schema and algorithms, and `04-compatibility.md` for the supermemory-compatible surface.*
