-- SPDX-License-Identifier: AGPL-3.0-only

-- The ctid-keyed index path tied graph provenance to a row's physical location, which the
-- UPDATEs mnestic runs (dedup confidence bumps, supersession, forget) move, so a mention could
-- point at a row that no longer holds the text. Keying the watch on the stable `id` survives
-- those rewrites, so a memory's entities stay attached to it across its lifecycle. maintain()
-- (driven by the worker) resolves the watch just as it resolved the index.
DROP INDEX IF EXISTS mnestic_memory_kg;
-- watch only catches future writes through its trigger, so reindex over the rows already
-- present (an upgrade from 0007 has them) to seed the graph; on a fresh database it is a no-op.
SELECT graphwright.reindex(graphwright.watch('mnestic_memory', 'content', 'id'));
