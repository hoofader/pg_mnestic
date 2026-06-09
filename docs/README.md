# Mnestic: Design Document Set

Postgres-native long-term memory engine for AI agents. Memory lives in the user's own Postgres, not a hosted service, with strict RLS tenant isolation, bitemporal fact resolution, and in-database hybrid search. Rust core, Python-first SDK (TypeScript fast-follow).

This set is for **review before implementation**. Read in order:

1. **[01-high-level-plan.md](01-high-level-plan.md)**: vision, scope, competitive wedge, target users, roadmap, risks, open decisions.
2. **[02-architecture.md](02-architecture.md)**: layered architecture, components, deployment topologies, technology rationale, data flow, security, portability matrix.
3. **[03-low-level-design.md](03-low-level-design.md)**: schema DDL, RLS policies, encryption, pipeline algorithms (extraction, resolution, hybrid recall), Rust module layout, SDK and MCP contracts, testing and eval.
4. **[04-compatibility.md](04-compatibility.md)**: the supermemory-compatible surface (REST subset, MCP tools, `containerTag` mapping, field map). A Phase 2 deliverable, documented now so the core schema maps cleanly with no later migration.

> Status: Draft v0.2 · 2026-06-07. DDL and pseudocode are a design reference for review, not final migrations. The memory model is content-primary with optional structured fields. Compatibility is library-first (ships Phase 2). Vector dimension and embedding provider remain open (see HLP §9).
>
> Correction baked into v0.2: supermemory's production backend is reportedly Postgres + pgvector already. The wedge is ownership and isolation (your database, your transaction, your RLS), not "Postgres vs a vector DB."
