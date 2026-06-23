-- SPDX-License-Identifier: MIT

-- Carry the request's metadata through the async (dreaming: dynamic) path: the worker reads
-- it back off the source row so the memories it extracts get the same metadata the sync path
-- stores. Constant default, so this applies online.
ALTER TABLE mnestic_source ADD COLUMN metadata jsonb NOT NULL DEFAULT '{}';
