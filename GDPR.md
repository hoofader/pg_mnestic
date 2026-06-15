# Data subject rights

Mnestic stores personal data (the memories, documents, and profile derived from a subject's
conversations), so a deployment has to answer access and erasure requests. The subject is
identified by their `containerTag`, which resolves to an `actor_id` within a tenant the same
way the serving path resolves it.

Two CLIs cover the rights, built with `--features cli`:

```bash
# Right to access / portability: export everything held for a subject as JSON
cargo run -p mnestic-server --features cli --bin export-actor -- <tenant> <containerTag>

# Right to erasure: permanently delete everything held for a subject
cargo run -p mnestic-server --features cli --bin purge-actor -- <tenant> <containerTag>
```

## Erasure is a hard delete, distinct from forget

The `memory`/`forget` API path is a soft tombstone: it sets a row's status to `forgotten` so
it stops surfacing in recall, but the row stays for the audit trail and supersession history.
That is not erasure. `purge-actor` is the real thing: it deletes the subject's memories,
chunks, documents, sources, and cached profile in one transaction. There is no undo, so take a
backup first (see DEPLOYMENT.md) and run `export-actor` beforehand if the same subject also
asked for a copy of their data.

Scope: erasure is per actor within one tenant. The `containerTag` resolves to an actor, and the
purge covers that actor across **all** containers; the `container_tags` part of a hierarchical
tag (the `org:1` in `org:1:user:2`) does not narrow the scope. That is the right scope for
erasing a natural person, but it means a tag with no `user:` segment (a bare `org:123`) resolves
to a phantom actor and purges nothing. The CLIs print the resolved actor so the operator can
confirm the subject before a destructive run. The tenant is the security boundary, so a purge
never reaches another tenant's rows.

## What the export contains

The export is the subject's natural-language content and metadata: each memory (content,
structured triple, confidence, timestamps, status), each document and its chunks, the raw
sources, and the current profile. The opaque retrieval columns (the embedding vector and the
search `tsvector`) are dropped; they are derived data, not the subject's information.

## Open items

- Backups taken before an erasure still contain the data until they age out. Document your
  backup retention so an erasure is honored within the retention window, and re-apply a purge
  if a backup is restored.
- Erasure and export are operator-run today, not a tenant self-service endpoint. A REST
  surface for them would need its own admin authorization, separate from the per-tenant
  bearer keys.
- All content is cleartext today (the envelope-encryption columns are unused). If
  encryption-at-rest is enabled later, the export carries those payloads as ciphertext the
  subject cannot read; an intelligible export then needs a decrypt-on-export step.
